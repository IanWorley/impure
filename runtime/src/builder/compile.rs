//! Compiles the translated code, the glue in cpp/ and XenonUtils' XEX loader with the player's
//! Clang, in parallel, and links them into the game library.
//!
//! Each object is cached under a key of its source's contents and everything else that shapes
//! it (compiler, flags, headers), so a rebuild after a runtime update recompiles only what
//! changed: usually the glue, not the ~1,700 generated files.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::translate::Translated;
use super::{Compiler, Sdk, file_parts, io, key};
use crate::progress::Progress;

const BUILD_DIRECTORY: &str = "build";
const OBJECTS_DIRECTORY: &str = "objects";
const IMPORTS_SOURCE: &str = "imports.cpp";
const RESPONSE_FILE: &str = "link.rsp";
const OBJECT_EXTENSION: &str = "o";
const TEMPORARY_EXTENSION: &str = "tmp";
/// Hex digits of an object's key kept in its file name.
const OBJECT_KEY_DIGITS: usize = 16;
/// Lines of compiler output quoted when a file fails to compile.
const QUOTED_LINES: usize = 20;

const MAPPING_SOURCE: &str = "ppc_func_mapping.cpp";
const GENERATED_SOURCE_PREFIX: &str = "ppc_recomp.";
const IMPORT_PREFIX: &str = "__imp__";
const RECOMPILED_FUNCTION_PREFIX: &str = "__imp__sub_";
const GLUE_SOURCES: [&str; 2] = ["glue.cpp", "patches.cpp"];
/// XenonUtils' XEX loader: decryption, decompression and the image layout.
const XEX_LOADER_SOURCES: [&str; 4] = [
    "xex.cpp",
    "image.cpp",
    "xex_patcher.cpp",
    "memory_mapped_file.cpp",
];
const SIMDE: &str = "simde";
const MSPACK: &str = "libmspack/libmspack/mspack";
const TINY_AES: &str = "tiny-AES-c";
const TINY_SHA1: &str = "TinySHA1";
const LZX_DECODER: &str = "libmspack/libmspack/mspack/lzxd.c";
const AES: &str = "tiny-AES-c/aes.c";
const SOURCE_EXTENSIONS: [&str; 3] = ["h", "cpp", "c"];
const HEADER_EXTENSIONS: [&str; 1] = ["h"];

/// Generated code is huge; this keeps it fast without the build time of -O3.
const OPTIMIZATION: &str = "-O2";
/// Third-party and generated code: warnings are not actionable here.
const NO_WARNINGS: &str = "-w";
const CXX_STANDARD: &str = "-std=c++20";
/// SIMDe maps the guest's VMX instructions onto SSE4.1 on x86-64.
const X86_64_SIMD: &str = "-msse4.1";
const NOMINMAX: &str = "NOMINMAX";
/// Changes whenever `units` changes which include directories or defines a source compiles with,
/// so cached objects built the old way are not reused.
const RECIPE_VERSION: u32 = 1;

#[derive(Clone, Copy)]
enum Language {
    Cxx,
    C,
}

struct Unit {
    source: PathBuf,
    language: Language,
    includes: Vec<PathBuf>,
    defines: &'static [&'static str],
}

/// Flags every compile and the link share: the runtime's architecture (on macOS, where Clang
/// may default to another under Rosetta), and position-independent code with only the
/// library's exports visible.
fn target_flags() -> Vec<&'static str> {
    let mut flags = Vec::new();
    if cfg!(target_os = "macos") {
        flags.extend([
            "-arch",
            if cfg!(target_arch = "x86_64") {
                "x86_64"
            } else {
                "arm64"
            },
        ]);
    }
    if cfg!(target_arch = "x86_64") {
        flags.push(X86_64_SIMD);
    }
    if !cfg!(windows) {
        flags.extend(["-fPIC", "-fvisibility=hidden"]);
    }
    flags
}

/// The parts of every object's key besides its source: compiler, flags and shared headers.
fn context_parts(
    sdk: &Sdk,
    compiler: &Compiler,
    translated: &Translated,
) -> Result<Vec<Vec<u8>>, String> {
    let mut parts = vec![
        RECIPE_VERSION.to_le_bytes().to_vec(),
        compiler.identity.clone().into_bytes(),
        target_flags().join(" ").into_bytes(),
        [OPTIMIZATION, NO_WARNINGS, CXX_STANDARD]
            .join(" ")
            .into_bytes(),
        // The tools pin the third-party sources they were built with.
        fs::read(&sdk.recomp).map_err(|error| format!("{}: {error}", sdk.recomp.display()))?,
    ];
    parts.extend(file_parts(&translated.generated, &HEADER_EXTENSIONS)?);
    parts.extend(file_parts(&sdk.glue, &HEADER_EXTENSIONS)?);
    parts.extend(file_parts(&sdk.xenon_utils, &HEADER_EXTENSIONS)?);
    Ok(parts)
}

/// A key for the finished library that is quick to compute: the generated sources only change
/// when the translation does, so its key stands in for them.
pub fn library_key(
    sdk: &Sdk,
    compiler: &Compiler,
    translated: &Translated,
) -> Result<String, String> {
    let mut parts = context_parts(sdk, compiler, translated)?;
    parts.push(translated.key.clone().into_bytes());
    parts.extend(file_parts(&sdk.glue, &SOURCE_EXTENSIONS)?);
    parts.extend(file_parts(&sdk.xenon_utils, &SOURCE_EXTENSIONS)?);
    Ok(key(parts.iter().map(Vec::as_slice)))
}

/// The C++ that forwards every import the generated code calls to the runtime.
fn import_forwarders(mapping: &str) -> String {
    let mut source = String::from("#include \"host.h\"\n");
    for symbol in mapping.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if let Some(name) = symbol
            .strip_prefix(IMPORT_PREFIX)
            .filter(|_| !symbol.starts_with(RECOMPILED_FUNCTION_PREFIX))
        {
            source += &format!(
                "PPC_FUNC({symbol}) {{ pure_host->import_call(\"{name}\", &ctx, base); }}\n"
            );
        }
    }
    source
}

fn units(sdk: &Sdk, generated: &Path, imports: PathBuf) -> Result<Vec<Unit>, String> {
    let mut generated_sources: Vec<PathBuf> = fs::read_dir(generated)
        .map_err(|error| format!("{}: {error}", generated.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            path.extension().is_some_and(|extension| extension == "cpp")
                && (name.starts_with(GENERATED_SOURCE_PREFIX) || name == MAPPING_SOURCE)
        })
        .collect();
    generated_sources.sort();
    let simde = sdk.thirdparty.join(SIMDE);
    let mut units: Vec<Unit> = generated_sources
        .into_iter()
        .map(|source| Unit {
            source,
            language: Language::Cxx,
            includes: vec![generated.to_path_buf(), simde.clone()],
            defines: &[],
        })
        .collect();
    let glue_includes = vec![
        generated.to_path_buf(),
        sdk.glue.clone(),
        sdk.xenon_utils.clone(),
        simde,
        sdk.thirdparty.join(MSPACK),
        sdk.thirdparty.join(TINY_AES),
        sdk.thirdparty.join(TINY_SHA1),
    ];
    let glue_sources = GLUE_SOURCES
        .iter()
        .map(|name| sdk.glue.join(name))
        .chain([imports])
        .chain(
            XEX_LOADER_SOURCES
                .iter()
                .map(|name| sdk.xenon_utils.join(name)),
        );
    units.extend(glue_sources.map(|source| Unit {
        source,
        language: Language::Cxx,
        includes: glue_includes.clone(),
        defines: &[NOMINMAX],
    }));
    units.extend([LZX_DECODER, AES].map(|source| Unit {
        source: sdk.thirdparty.join(source),
        language: Language::C,
        includes: vec![sdk.thirdparty.join(MSPACK), sdk.thirdparty.join(TINY_AES)],
        defines: &[],
    }));
    Ok(units)
}

impl Unit {
    fn object_key(&self, context: &str) -> Result<String, String> {
        let contents = fs::read(&self.source)
            .map_err(|error| format!("{}: {error}", self.source.display()))?;
        // Include directories are absolute paths, so they are not part of the key: the cache
        // stays valid wherever the data directory is, and RECIPE_VERSION stands in for them.
        Ok(key([
            context.as_bytes(),
            match self.language {
                Language::Cxx => b"c++".as_slice(),
                Language::C => b"c",
            },
            self.defines.join(" ").as_bytes(),
            &contents,
        ]))
    }

    fn compile(&self, compiler: &Compiler, object: &Path) -> Result<(), String> {
        let partial = object.with_extension(TEMPORARY_EXTENSION);
        let mut command = compiler.command();
        command
            .args(["-c", OPTIMIZATION, NO_WARNINGS])
            .args(target_flags());
        match self.language {
            Language::Cxx => command.arg(CXX_STANDARD),
            Language::C => command.args(["-x", "c"]),
        };
        for include in &self.includes {
            command.arg("-I").arg(include);
        }
        for define in self.defines {
            command.arg(format!("-D{define}"));
        }
        command.arg(&self.source).arg("-o").arg(&partial);
        let output = command
            .output()
            .map_err(|error| format!("cannot run {}: {error}", compiler.path.display()))?;
        if !output.status.success() {
            let text = String::from_utf8_lossy(&output.stderr);
            let lines: Vec<&str> = text.lines().collect();
            return Err(format!(
                "compiling {} failed:\n{}",
                self.source.display(),
                lines[lines.len().saturating_sub(QUOTED_LINES)..].join("\n")
            ));
        }
        fs::rename(&partial, object).map_err(|error| format!("{}: {error}", object.display()))
    }
}

/// Compiles whatever is not cached and links the library at `library`.
pub fn build(
    sdk: &Sdk,
    compiler: &Compiler,
    translated: &Translated,
    game: &Path,
    library: &Path,
    progress: &Progress,
) -> Result<(), String> {
    let build = game.join(BUILD_DIRECTORY);
    let objects_directory = build.join(OBJECTS_DIRECTORY);
    fs::create_dir_all(&objects_directory).map_err(io(&objects_directory))?;
    let mapping_path = translated.generated.join(MAPPING_SOURCE);
    let mapping = fs::read_to_string(&mapping_path).map_err(io(&mapping_path))?;
    let imports = build.join(IMPORTS_SOURCE);
    fs::write(&imports, import_forwarders(&mapping)).map_err(io(&imports))?;

    progress.stage("Checking which parts of the game need compiling");
    let context_parts = context_parts(sdk, compiler, translated)?;
    let context = key(context_parts.iter().map(Vec::as_slice));
    let units = units(sdk, &translated.generated, imports)?;
    let mut objects = Vec::with_capacity(units.len());
    let mut stale = Vec::new();
    for unit in &units {
        let stem = unit
            .source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let object_key = unit.object_key(&context)?;
        // Generated stems contain dots (`ppc_recomp.12`), so the name is built whole rather
        // than with `with_extension`, which would cut it at the last dot.
        let object = objects_directory.join(format!(
            "{stem}-{}.{OBJECT_EXTENSION}",
            &object_key[..OBJECT_KEY_DIGITS]
        ));
        if !object.is_file() {
            stale.push((unit, object.clone()));
        }
        objects.push(object);
    }

    if !stale.is_empty() {
        progress.stage(format!(
            "Compiling the game ({} files; about 10 to 15 minutes the first time)",
            stale.len()
        ));
        compile_all(compiler, &stale, progress)?;
    }
    // Objects from earlier builds that nothing uses any more.
    for entry in fs::read_dir(&objects_directory)
        .map_err(io(&objects_directory))?
        .flatten()
    {
        if !objects.contains(&entry.path()) {
            let _ = fs::remove_file(entry.path());
        }
    }

    progress.stage("Linking the game library");
    link(compiler, &objects, &build.join(RESPONSE_FILE), library)
}

/// Compiles `stale` on every core, stopping at the first failure.
fn compile_all(
    compiler: &Compiler,
    stale: &[(&Unit, PathBuf)],
    progress: &Progress,
) -> Result<(), String> {
    let jobs = thread::available_parallelism().map_or(1, |count| count.get());
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failure: Mutex<Option<String>> = Mutex::new(None);
    thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= stale.len() || failure.lock().unwrap().is_some() {
                        return;
                    }
                    let (unit, object) = &stale[index];
                    if let Err(error) = unit.compile(compiler, object) {
                        failure.lock().unwrap().get_or_insert(error);
                        return;
                    }
                    let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress.count(finished as u64, stale.len() as u64);
                }
            });
        }
    });
    failure.into_inner().unwrap().map_or(Ok(()), Err)
}

/// Links the objects into the library through a response file: Windows limits a command line to
/// 32,767 characters, far fewer than ~1,700 object paths.
fn link(
    compiler: &Compiler,
    objects: &[PathBuf],
    response: &Path,
    library: &Path,
) -> Result<(), String> {
    let quoted: Vec<String> = objects
        .iter()
        // Forward slashes and quotes read the same under Clang's Windows and Unix quoting rules.
        .map(|object| format!("\"{}\"", object.to_string_lossy().replace('\\', "/")))
        .collect();
    fs::write(response, quoted.join("\n"))
        .map_err(|error| format!("{}: {error}", response.display()))?;
    let partial = library.with_extension(TEMPORARY_EXTENSION);
    let mut command = compiler.command();
    command.args(target_flags()).arg("-shared");
    if cfg!(target_os = "linux") {
        // The library must take everything from the runtime through its host table.
        command.arg("-Wl,--no-undefined");
    }
    command
        .arg("-o")
        .arg(&partial)
        .arg(format!("@{}", response.display()));
    let output = command
        .output()
        .map_err(|error| format!("cannot run {}: {error}", compiler.path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "linking the game library failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    fs::rename(&partial, library).map_err(|error| format!("{}: {error}", library.display()))
}

#[cfg(test)]
mod tests {
    use super::import_forwarders;

    #[test]
    fn forwards_imports_but_not_recompiled_functions() {
        let mapping = "{ 0x82000000, __imp__sub_example },\n{ 0x82000010, __imp__NtClose },";
        assert_eq!(
            import_forwarders(mapping),
            "#include \"host.h\"\n\
             PPC_FUNC(__imp__NtClose) { pure_host->import_call(\"NtClose\", &ctx, base); }\n"
        );
    }
}
