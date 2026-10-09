//! Builds the game library (src/game_module.rs) from the player's own `default.xex`, on their
//! machine: XenonRecomp translates the executable into C++ (translate.rs), and the player's
//! Clang compiles that with the glue in cpp/ into a shared library (compile.rs). Releases never
//! contain game code; this is how a player gets it from their own disc.
//!
//! Everything is cached under the data directory (`games/<xex SHA-256>/`). A warm start checks
//! one stamp and loads the library; a runtime update that changes the glue recompiles only the
//! glue.

mod compile;
mod translate;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::progress::Progress;

/// Where a packaged SDK lives, if not beside the executable.
const SDK_ENVIRONMENT: &str = "PURE_SDK_DIR";
/// The release layout, beside the executable: see tools/package-release.sh.
const SDK_DIRECTORY: &str = "sdk";
const SDK_BINARIES: &str = "bin";
/// Developer checkouts use the XenonRecomp checkout docs/tooling.md describes.
const XENON_RECOMP_ENVIRONMENT: &str = "XENON_RECOMP_DIR";
const DEVELOPER_XENON_RECOMP: &str = "local/tools/XenonRecomp";
const DEVELOPER_BUILD: &str = "build-release";
const ANALYSE: &str = "XenonAnalyse";
const RECOMP: &str = "XenonRecomp";
const XENON_UTILS: &str = "XenonUtils";
const THIRDPARTY: &str = "thirdparty";
const GLUE: &str = "cpp";
const CONFIG: &str = "recomp";
/// A compiler other than the one found on PATH (or by xcrun on macOS).
const COMPILER_ENVIRONMENT: &str = "PURE_CXX";
const COMPILER: &str = "clang++";
const VERSION_FLAG: &str = "--version";

const LIBRARY_STEM: &str = "pure_game";
const STAMP_EXTENSION: &str = "stamp";

/// The tools and sources the build needs; none of them contain game code.
pub struct Sdk {
    analyse: PathBuf,
    recomp: PathBuf,
    xenon_utils: PathBuf,
    thirdparty: PathBuf,
    /// The glue and title fixes (runtime/cpp).
    glue: PathBuf,
    /// XenonRecomp's configuration for Pure (recomp/).
    config: PathBuf,
}

fn executable(directory: &Path, name: &str) -> PathBuf {
    directory
        .join(name)
        .with_extension(std::env::consts::EXE_EXTENSION)
}

impl Sdk {
    /// A packaged SDK (`PURE_SDK_DIR`, or `sdk/` beside the executable), or else this source
    /// checkout with its XenonRecomp build.
    pub fn locate() -> Result<Self, String> {
        let packaged = std::env::var_os(SDK_ENVIRONMENT)
            .map(PathBuf::from)
            .or_else(|| {
                let beside = std::env::current_exe().ok()?.parent()?.join(SDK_DIRECTORY);
                beside.is_dir().then_some(beside)
            });
        let sdk = match packaged {
            Some(root) => Self {
                analyse: executable(&root.join(SDK_BINARIES), ANALYSE),
                recomp: executable(&root.join(SDK_BINARIES), RECOMP),
                xenon_utils: root.join(XENON_UTILS),
                thirdparty: root.join(THIRDPARTY),
                glue: root.join(GLUE),
                config: root.join(CONFIG),
            },
            None => {
                let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("the runtime lives inside the repository");
                let xenon = std::env::var_os(XENON_RECOMP_ENVIRONMENT)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| repository.join(DEVELOPER_XENON_RECOMP));
                let build = xenon.join(DEVELOPER_BUILD);
                Self {
                    analyse: executable(&build.join(ANALYSE), ANALYSE),
                    recomp: executable(&build.join(RECOMP), RECOMP),
                    xenon_utils: xenon.join(XENON_UTILS),
                    thirdparty: xenon.join(THIRDPARTY),
                    glue: Path::new(env!("CARGO_MANIFEST_DIR")).join(GLUE),
                    config: repository.join(CONFIG),
                }
            }
        };
        for path in [
            &sdk.analyse,
            &sdk.recomp,
            &sdk.xenon_utils,
            &sdk.thirdparty,
            &sdk.glue,
            &sdk.config,
        ] {
            if !path.exists() {
                return Err(format!(
                    "{} is missing: this runtime's build tools are incomplete (see docs/tooling.md)",
                    path.display()
                ));
            }
        }
        Ok(sdk)
    }
}

/// The player's C++ compiler, and what it says it is (part of every cache key).
pub struct Compiler {
    path: PathBuf,
    /// On macOS, the SDK to build against: the Command Line Tools' Clang, run directly rather
    /// than through the /usr/bin shim, does not find the system headers without it.
    sysroot: Option<PathBuf>,
    identity: String,
}

impl Compiler {
    pub fn locate() -> Result<Self, String> {
        let path = std::env::var_os(COMPILER_ENVIRONMENT)
            .map(PathBuf::from)
            .or_else(|| xcrun(&["--find", COMPILER]))
            .unwrap_or_else(|| PathBuf::from(COMPILER));
        let sysroot = xcrun(&["--show-sdk-path"]);
        let output = Command::new(&path).arg(VERSION_FLAG).output();
        match output {
            Ok(output) if output.status.success() => Ok(Self {
                identity: format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    sysroot
                        .as_deref()
                        .map(Path::display)
                        .map(|path| path.to_string())
                        .unwrap_or_default()
                ),
                path,
                sysroot,
            }),
            _ => Err(format!(
                "Building the game needs Clang, which was not found. {}",
                install_hint()
            )),
        }
    }

    /// A compiler command, with the SDK it builds against.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        if let Some(sysroot) = &self.sysroot {
            command.arg("-isysroot").arg(sysroot);
        }
        command
    }
}

/// On macOS, a path `xcrun` prints: the Command Line Tools' Clang, which it finds even off PATH,
/// or the SDK.
fn xcrun(arguments: &[&str]) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("xcrun").args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Install Apple's Command Line Tools with `xcode-select --install`, then open the game again."
    } else if cfg!(windows) {
        "Install LLVM (`winget install LLVM.LLVM`, or from llvm.org) with the option to add it to PATH, then open the game again."
    } else {
        "Install Clang with your package manager (for example `sudo apt install clang`), then open the game again."
    }
}

/// Returns the game library for the executable at `xex` (whose SHA-256 the caller has checked),
/// building whatever is missing or stale.
pub fn ensure(xex: &Path, xex_sha256: &str, progress: &Progress) -> Result<PathBuf, String> {
    let sdk = Sdk::locate()?;
    let compiler = Compiler::locate()?;
    let game = crate::data_dir::game(xex_sha256);
    fs::create_dir_all(&game).map_err(|error| format!("{}: {error}", game.display()))?;

    let translated = translate::ensure(&sdk, xex, xex_sha256, &game, progress)?;
    let library = game
        .join(LIBRARY_STEM)
        .with_extension(std::env::consts::DLL_EXTENSION);
    let stamp = library.with_extension(STAMP_EXTENSION);
    let key = compile::library_key(&sdk, &compiler, &translated)?;
    if library.is_file() && fs::read_to_string(&stamp).is_ok_and(|stamp| stamp == key) {
        return Ok(library);
    }
    compile::build(&sdk, &compiler, &translated, &game, &library, progress)?;
    fs::write(&stamp, key).map_err(|error| format!("{}: {error}", stamp.display()))?;
    Ok(library)
}

/// Turns an I/O error on `path` into a message naming it.
fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |error| format!("{}: {error}", path.display())
}

/// A key for a set of inputs: hex SHA-256 over each part, length-prefixed so parts cannot run
/// together.
fn key<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

/// The contents of every file under `directory` whose extension is one of `extensions`, in path
/// order, as key parts (path, then contents).
fn file_parts(directory: &Path, extensions: &[&str]) -> Result<Vec<Vec<u8>>, String> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extensions.iter().any(|wanted| extension == *wanted))
            {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut parts = Vec::with_capacity(files.len() * 2);
    for file in files {
        let relative = file.strip_prefix(directory).unwrap_or(&file);
        parts.push(relative.to_string_lossy().into_owned().into_bytes());
        parts.push(fs::read(&file).map_err(|error| format!("{}: {error}", file.display()))?);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::key;

    #[test]
    fn keys_keep_parts_apart() {
        assert_ne!(key([b"ab".as_slice(), b"c"]), key([b"a".as_slice(), b"bc"]));
        assert_eq!(key([b"ab".as_slice()]), key([b"ab".as_slice()]));
    }
}
