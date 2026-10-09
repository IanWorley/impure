//! Where the runtime keeps what it makes for the player: saves (src/kernel/content.rs), the
//! disc files and game library prepared from their own copy of the game (src/launcher.rs,
//! src/builder/), and which game to open next time.

use std::path::PathBuf;

const ENVIRONMENT_OVERRIDE: &str = "PURE_DATA_DIR";
const DIRECTORY_NAME: &str = "pure-runtime";
const GAMES_DIRECTORY: &str = "games";

/// `$XDG_DATA_HOME/pure-runtime` (or `~/.local/share/pure-runtime`) on Linux,
/// `~/Library/Application Support/pure-runtime` on macOS, `%APPDATA%\pure-runtime` on Windows;
/// `PURE_DATA_DIR` overrides it, for example to keep test runs away from real saves.
pub fn root() -> PathBuf {
    if let Some(directory) = std::env::var_os(ENVIRONMENT_OVERRIDE) {
        return PathBuf::from(directory);
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local").join("share"))
    };
    base.join(DIRECTORY_NAME)
}

/// What the runtime prepared from one copy of the game, named by its `default.xex`'s SHA-256.
pub fn game(xex_sha256: &str) -> PathBuf {
    root().join(GAMES_DIRECTORY).join(xex_sha256)
}
