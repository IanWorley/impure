//! XenonRecomp translates the player's executable into C++: XenonAnalyse finds its switch
//! tables, Pure's own additions (recomp/pure_switch_tables.toml) are appended, and XenonRecomp
//! writes the code with Pure's configuration (recomp/pure.toml).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Sdk, io, key};
use crate::progress::Progress;

const TRANSLATED_DIRECTORY: &str = "recomp";
const PARTIAL_SUFFIX: &str = "partial";
const STAMP_FILE: &str = "stamp";
const GENERATED_DIRECTORY: &str = "ppc";
const SWITCH_TABLES_FILE: &str = "switch_tables.toml";
const CONFIG_FILE: &str = "config.toml";
const ANALYSE_LOG: &str = "analyse.log";
const RECOMP_LOG: &str = "recomp.log";
const PURE_CONFIG: &str = "pure.toml";
const PURE_SWITCH_TABLES: &str = "pure_switch_tables.toml";
const CONTEXT_HEADER: &str = "ppc_context.h";

/// The translated code, and the key it was made with.
pub struct Translated {
    /// The generated sources and headers.
    pub generated: PathBuf,
    pub key: String,
}

/// The translated code for `xex`, translating it again only if the executable, the tools or the
/// configuration changed.
pub fn ensure(
    sdk: &Sdk,
    xex: &Path,
    xex_sha256: &str,
    game: &Path,
    progress: &Progress,
) -> Result<Translated, String> {
    let read = |path: &Path| fs::read(path).map_err(|error| format!("{}: {error}", path.display()));
    let pure_config = read(&sdk.config.join(PURE_CONFIG))?;
    let pure_switch_tables = read(&sdk.config.join(PURE_SWITCH_TABLES))?;
    let translation_key = key([
        xex_sha256.as_bytes(),
        &read(&sdk.analyse)?,
        &read(&sdk.recomp)?,
        &read(&sdk.xenon_utils.join(CONTEXT_HEADER))?,
        &pure_config,
        &pure_switch_tables,
    ]);
    let directory = game.join(TRANSLATED_DIRECTORY);
    let translated = Translated {
        generated: directory.join(GENERATED_DIRECTORY),
        key: translation_key.clone(),
    };
    if fs::read_to_string(directory.join(STAMP_FILE)).is_ok_and(|stamp| stamp == translation_key) {
        return Ok(translated);
    }

    progress.stage("Translating the game's code (a minute or two)");
    // Generate beside the destination and move it into place only once both tools succeed, so
    // an interrupted run never leaves a directory that looks complete.
    let partial = directory.with_extension(PARTIAL_SUFFIX);
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(io(&partial))?;
    }
    fs::create_dir_all(partial.join(GENERATED_DIRECTORY)).map_err(io(&partial))?;
    let xex = std::path::absolute(xex).map_err(io(xex))?;

    let switch_tables = partial.join(SWITCH_TABLES_FILE);
    run(
        Command::new(&sdk.analyse).arg(&xex).arg(&switch_tables),
        &partial.join(ANALYSE_LOG),
    )?;
    let mut tables = read(&switch_tables)?;
    tables.extend(&pure_switch_tables);
    fs::write(&switch_tables, tables).map_err(io(&switch_tables))?;

    // The generated paths are relative to the config, so the directory can move once complete.
    let config = format!(
        "[main]\nfile_path = {}\nout_directory_path = \"{GENERATED_DIRECTORY}\"\nswitch_table_file_path = \"{SWITCH_TABLES_FILE}\"\n{}",
        toml_string(&xex.to_string_lossy()),
        String::from_utf8_lossy(&pure_config)
    );
    let config_path = partial.join(CONFIG_FILE);
    fs::write(&config_path, config).map_err(io(&config_path))?;
    // XenonRecomp prefixes file_path with the config's directory even when it is absolute, so
    // it is given the config by bare name from inside the output directory.
    run(
        Command::new(&sdk.recomp)
            .arg(CONFIG_FILE)
            .arg(sdk.xenon_utils.join(CONTEXT_HEADER))
            .current_dir(&partial),
        &partial.join(RECOMP_LOG),
    )?;
    fs::write(partial.join(STAMP_FILE), &translation_key).map_err(io(&partial))?;

    if directory.exists() {
        fs::remove_dir_all(&directory).map_err(io(&directory))?;
    }
    fs::rename(&partial, &directory).map_err(io(&directory))?;
    Ok(translated)
}

/// Runs a tool with its output in `log`; a failure quotes the log's end.
fn run(command: &mut Command, log: &Path) -> Result<(), String> {
    const QUOTED_LINES: usize = 20;
    let file = File::create(log).map_err(|error| format!("{}: {error}", log.display()))?;
    let error_file = file
        .try_clone()
        .map_err(|error| format!("{}: {error}", log.display()))?;
    let status = command
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(error_file))
        .status()
        .map_err(|error| format!("cannot run {:?}: {error}", command.get_program()))?;
    if status.success() {
        return Ok(());
    }
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    Err(format!(
        "{:?} failed ({status}); the end of {}:\n{}",
        command.get_program(),
        log.display(),
        lines[lines.len().saturating_sub(QUOTED_LINES)..].join("\n")
    ))
}

/// A TOML basic string holding `text`.
fn toml_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::toml_string;

    #[test]
    fn windows_paths_survive_as_toml_strings() {
        assert_eq!(
            toml_string(r#"C:\Games\Pure "World"\default.xex"#),
            r#""C:\\Games\\Pure \"World\"\\default.xex""#
        );
    }
}
