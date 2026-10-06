//! With the `recompiled` feature, compiles the C++ that tools/recompile.sh generated from your own
//! default.xex, the glue in cpp/, and XenonUtils' XEX loader into one static library.
//! Nothing here contains game code; the generated sources live in the ignored local/ tree.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
// Generated code is huge; unoptimized builds are slow and need far deeper host stacks.
const GENERATED_OPT_LEVEL: u32 = 2;
const OBJECT_CACHE_DIR: &str = "objects";
const STAMP_FILE: &str = "stamp.txt";
const OBJECT_LIST_FILE: &str = "objects.txt";
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

/// The generated sources only change when tools/recompile.sh runs again, so their objects are
/// reused until the file list, sizes, or timestamps change. They are cached beside the sources
/// rather than in OUT_DIR, which Cargo replaces whenever this package's dependencies change.
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
    let stamp_path = objects_dir.join(STAMP_FILE);
    let object_list_path = objects_dir.join(OBJECT_LIST_FILE);
    let stamp: String = sources
        .iter()
        .map(|source| {
            let metadata = fs::metadata(source).unwrap();
            format!(
                "{} {} {:?}\n",
                source.display(),
                metadata.len(),
                metadata.modified().unwrap()
            )
        })
        .collect();

    if fs::read_to_string(&stamp_path).is_ok_and(|previous| previous == stamp) {
        return fs::read_to_string(&object_list_path)
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .collect();
    }

    fs::create_dir_all(&objects_dir).unwrap();
    let objects = target
        .cpp_build()
        .include(generated_dir)
        .include(thirdparty.join("simde"))
        .files(&sources)
        .out_dir(&objects_dir)
        .opt_level(GENERATED_OPT_LEVEL)
        .debug(false)
        .compile_intermediates();

    let object_list: String = objects
        .iter()
        .map(|object| format!("{}\n", object.display()))
        .collect();
    fs::write(&object_list_path, object_list).unwrap();
    fs::write(&stamp_path, stamp).unwrap();
    objects
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
        // Third-party and generated code: warnings are not actionable here.
        build.flag(if self.msvc { "/w" } else { "-w" });
        if self.x86_64 {
            // SIMDe maps the guest's VMX instructions onto SSE4.1 when it is available.
            build.flag("-msse4.1");
        }
        build
    }
}
