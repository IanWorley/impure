//! With the `recompiled` feature, compiles the C++ that tools/recompile.sh generated from your own
//! default.xex, the glue in cpp/, and XenonUtils' XEX loader into one static library.
//! Nothing here contains game code; the generated sources live in the ignored local/ tree.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const GENERATED_DIR_ENV: &str = "PURE_RECOMP_DIR";
const XENON_RECOMP_DIR_ENV: &str = "XENON_RECOMP_DIR";
const DEFAULT_GENERATED_DIR: &str = "../local/recomp/ppc";
const DEFAULT_XENON_RECOMP_DIR: &str = "../local/tools/XenonRecomp";
const GLUE_SOURCE: &str = "cpp/glue.cpp";
const MAPPING_SOURCE: &str = "ppc_func_mapping.cpp";
const GENERATED_SOURCE_PREFIX: &str = "ppc_recomp.";
const IMPORT_PREFIX: &str = "__imp__";
const RECOMPILED_FUNCTION_PREFIX: &str = "__imp__sub_";
const LIBRARY_NAME: &str = "pure_guest";
// Generated code is huge; unoptimized builds are slow and need far deeper host stacks. The XEX
// loader (decryption, LZX decompression) and glue use it too, or every debug launch spends over
// half a minute unpacking the executable.
const GENERATED_OPT_LEVEL: u32 = 2;
const OBJECT_CACHE_DIR: &str = "objects";
const MANIFEST_FILE: &str = "manifest.tsv";
/// cc names each object `<hash of the source directory>-<source stem>.o`.
const OBJECT_SUFFIX: &str = ".o";
const OBJECT_NAME_SEPARATOR: char = '-';
const SOURCE_SUFFIX: &str = ".cpp";
/// Set by GitHub Actions and most other CI services.
const CI_ENV: &str = "CI";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={GENERATED_DIR_ENV}");
    println!("cargo:rerun-if-env-changed={XENON_RECOMP_DIR_ENV}");
    println!("cargo:rerun-if-env-changed={CI_ENV}");
    if env::var_os("CARGO_FEATURE_RECOMPILED").is_none() {
        return;
    }
    // CI has no generated sources, but still type-checks and lints the feature's Rust code.
    // Skipping the C++ there means `cargo build` fails at link time, so CI can never produce a
    // binary containing game code. This guards against accidents; it is not a security boundary.
    if env::var_os(CI_ENV).is_some() {
        println!(
            "cargo:warning=CI is set: the generated C++ is not compiled, so linking with the `recompiled` feature will fail"
        );
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let generated_dir =
        directory_from_env(GENERATED_DIR_ENV, &manifest_dir.join(DEFAULT_GENERATED_DIR));
    let xenon_dir = directory_from_env(
        XENON_RECOMP_DIR_ENV,
        &manifest_dir.join(DEFAULT_XENON_RECOMP_DIR),
    );
    let target = Target::from_env();

    println!("cargo:rerun-if-changed={}", generated_dir.display());
    println!("cargo:rerun-if-changed={GLUE_SOURCE}");

    let imports_source = out_dir.join("imports.cpp");
    fs::write(&imports_source, import_forwarders(&generated_dir)).unwrap();

    let thirdparty = xenon_dir.join("thirdparty");
    let xenon_utils = xenon_dir.join("XenonUtils");
    let mut objects = compile_generated(&target, &generated_dir, &thirdparty);
    objects.extend(
        target
            .c_build()
            .include(thirdparty.join("libmspack/libmspack/mspack"))
            .include(thirdparty.join("tiny-AES-c"))
            .file(thirdparty.join("libmspack/libmspack/mspack/lzxd.c"))
            .file(thirdparty.join("tiny-AES-c/aes.c"))
            .compile_intermediates(),
    );

    target
        .cpp_build()
        .include(&generated_dir)
        .include(&xenon_utils)
        .include(thirdparty.join("simde"))
        .include(thirdparty.join("libmspack/libmspack/mspack"))
        .include(thirdparty.join("tiny-AES-c"))
        .include(thirdparty.join("TinySHA1"))
        .define("NOMINMAX", None)
        .file(manifest_dir.join(GLUE_SOURCE))
        .file(&imports_source)
        .files(
            [
                "xex.cpp",
                "image.cpp",
                "xex_patcher.cpp",
                "memory_mapped_file.cpp",
            ]
            .map(|file| xenon_utils.join(file)),
        )
        .objects(objects)
        .compile(LIBRARY_NAME);
}

fn directory_from_env(variable: &str, default: &Path) -> PathBuf {
    let directory = env::var_os(variable).map_or_else(|| default.to_path_buf(), PathBuf::from);
    if !directory.is_dir() {
        panic!(
            "{} not found. Run tools/recompile.sh with your own default.xex first, or set {variable}.",
            directory.display()
        );
    }
    directory
}

/// Every import the generated code calls becomes a function that forwards to the Rust dispatcher.
fn import_forwarders(generated_dir: &Path) -> String {
    let mapping = fs::read_to_string(generated_dir.join(MAPPING_SOURCE)).unwrap();
    let mut source = String::from(
        "#include \"ppc_recomp_shared.h\"\n\
         extern \"C\" void pure_import_call(const char* name, PPCContext* ctx, uint8_t* base);\n",
    );
    for symbol in mapping.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if let Some(name) = symbol
            .strip_prefix(IMPORT_PREFIX)
            .filter(|_| !symbol.starts_with(RECOMPILED_FUNCTION_PREFIX))
        {
            source +=
                &format!("PPC_FUNC({symbol}) {{ pure_import_call(\"{name}\", &ctx, base); }}\n");
        }
    }
    source
}

/// The generated sources only change when tools/recompile.sh runs again, and a rerun after a
/// recompiler fix changes only some of them, so each file's object is cached on its own. A file
/// whose size and timestamp match the manifest is not even read; otherwise its contents are
/// hashed, and only a file whose contents changed is recompiled. Objects are cached beside the
/// sources rather than in OUT_DIR, which Cargo replaces whenever this package's dependencies
/// change. The hash is only stable within one Rust toolchain; a new one recompiles once.
fn compile_generated(target: &Target, generated_dir: &Path, thirdparty: &Path) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = fs::read_dir(generated_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            path.extension().is_some_and(|extension| extension == "cpp")
                && (name.starts_with(GENERATED_SOURCE_PREFIX) || name == MAPPING_SOURCE)
        })
        .collect();
    sources.sort();

    let objects_dir = generated_dir
        .join("..")
        .join(OBJECT_CACHE_DIR)
        .join(env::var("TARGET").unwrap());
    fs::create_dir_all(&objects_dir).unwrap();
    let manifest_path = objects_dir.join(MANIFEST_FILE);
    let previous: HashMap<String, CachedObject> = fs::read_to_string(&manifest_path)
        .unwrap_or_default()
        .lines()
        .filter_map(CachedObject::parse)
        .map(|cached| (cached.source.clone(), cached))
        .collect();

    // Objects already in the directory but missing from the manifest (from a build before the
    // manifest existed, or one that was interrupted) are reused if newer than their source.
    let existing_objects: HashMap<String, PathBuf> = fs::read_dir(&objects_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let source_stem = name
                .strip_suffix(OBJECT_SUFFIX)?
                .split_once(OBJECT_NAME_SEPARATOR)?
                .1;
            Some((format!("{source_stem}{SOURCE_SUFFIX}"), path))
        })
        .collect();

    let mut entries: Vec<Option<CachedObject>> = Vec::with_capacity(sources.len());
    let mut stale: Vec<(usize, Fingerprint)> = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let name = source.file_name().unwrap().to_string_lossy().into_owned();
        let metadata = fs::metadata(source).unwrap();
        let stat = (metadata.len(), modified_nanos(&metadata));
        let cached = previous.get(&name).filter(|cached| cached.object.exists());
        if let Some(cached) = cached.filter(|cached| (cached.size, cached.modified) == stat) {
            entries.push(Some(cached.clone()));
            continue;
        }
        let content_hash = hash_file(source);
        let fingerprint = Fingerprint {
            name,
            size: stat.0,
            modified: stat.1,
            content_hash,
        };
        let reusable = cached
            .filter(|cached| cached.content_hash == content_hash)
            .map(|cached| cached.object.clone())
            .or_else(|| {
                let object = existing_objects.get(&fingerprint.name)?;
                let object_modified = modified_nanos(&fs::metadata(object).ok()?);
                (object_modified > fingerprint.modified).then(|| object.clone())
            });
        match reusable {
            Some(object) => entries.push(Some(fingerprint.with_object(object))),
            None => {
                entries.push(None);
                stale.push((index, fingerprint));
            }
        }
    }

    if !stale.is_empty() {
        println!(
            "cargo:warning=compiling {} of {} generated sources",
            stale.len(),
            sources.len()
        );
        let objects = target
            .cpp_build()
            .include(generated_dir)
            .include(thirdparty.join("simde"))
            .files(stale.iter().map(|(index, _)| &sources[*index]))
            .out_dir(&objects_dir)
            .debug(false)
            .compile_intermediates();
        // cc returns one object per file, in the order the files were given.
        for ((index, fingerprint), object) in stale.into_iter().zip(objects) {
            entries[index] = Some(fingerprint.with_object(object));
        }
    }

    let entries: Vec<CachedObject> = entries.into_iter().map(Option::unwrap).collect();
    let manifest: String = entries.iter().map(CachedObject::line).collect();
    fs::write(&manifest_path, manifest).unwrap();
    entries.into_iter().map(|cached| cached.object).collect()
}

/// What identifies a generated source's contents.
struct Fingerprint {
    name: String,
    size: u64,
    modified: u128,
    content_hash: u64,
}

impl Fingerprint {
    fn with_object(self, object: PathBuf) -> CachedObject {
        CachedObject {
            source: self.name,
            size: self.size,
            modified: self.modified,
            content_hash: self.content_hash,
            object,
        }
    }
}

/// One manifest line: a source's name, size, timestamp and content hash, and its object.
#[derive(Clone)]
struct CachedObject {
    source: String,
    size: u64,
    modified: u128,
    content_hash: u64,
    object: PathBuf,
}

impl CachedObject {
    const SEPARATOR: char = '\t';

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split(Self::SEPARATOR);
        Some(Self {
            source: fields.next()?.to_string(),
            size: fields.next()?.parse().ok()?,
            modified: fields.next()?.parse().ok()?,
            content_hash: u64::from_str_radix(fields.next()?, 16).ok()?,
            object: PathBuf::from(fields.next()?),
        })
    }

    fn line(&self) -> String {
        let separator = Self::SEPARATOR;
        format!(
            "{}{separator}{}{separator}{}{separator}{:016x}{separator}{}\n",
            self.source,
            self.size,
            self.modified,
            self.content_hash,
            self.object.display()
        )
    }
}

fn modified_nanos(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn hash_file(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    fs::read(path).unwrap().hash(&mut hasher);
    hasher.finish()
}

struct Target {
    msvc: bool,
    x86_64: bool,
}

impl Target {
    fn from_env() -> Self {
        Self {
            msvc: env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|value| value == "msvc"),
            x86_64: env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|value| value == "x86_64"),
        }
    }

    /// XenonRecomp's output relies on Clang builtins, so every target uses a Clang driver.
    fn cpp_build(&self) -> cc::Build {
        let mut build = self.common_build();
        build
            .cpp(true)
            .std("c++20")
            .compiler(if self.msvc { "clang-cl" } else { "clang++" });
        build
    }

    fn c_build(&self) -> cc::Build {
        let mut build = self.common_build();
        build.compiler(if self.msvc { "clang-cl" } else { "clang" });
        build
    }

    fn common_build(&self) -> cc::Build {
        let mut build = cc::Build::new();
        build.opt_level(GENERATED_OPT_LEVEL);
        // Third-party and generated code: warnings are not actionable here.
        build.flag(if self.msvc { "/w" } else { "-w" });
        if self.x86_64 {
            // SIMDe maps the guest's VMX instructions onto SSE4.1 when it is available.
            build.flag("-msse4.1");
        }
        build
    }
}
