//! Rust runtime for a static recompilation of Pure (Xbox 360).
//!
//! Usage: pure-runtime <path/to/default.xex> [--exit-on-stop]
//!
//! Built with the `recompiled` feature, it runs the code tools/recompile.sh generated from your
//! own default.xex. Without it, it only opens the window, so it builds anywhere without the game.

#[cfg(feature = "recompiled")]
mod guest;
#[cfg(feature = "recompiled")]
mod kernel;
#[cfg(any(feature = "recompiled", test))]
mod memory;
mod status;

use std::env;
use std::path::PathBuf;

use bevy::prelude::*;

const WINDOW_TITLE: &str = "Pure (static recompilation)";
const BACKGROUND_COLOR: Color = Color::srgb(0.05, 0.05, 0.08);
const USAGE: &str = "usage: pure-runtime <path/to/default.xex> [--exit-on-stop]";
const EXIT_ON_STOP_FLAG: &str = "--exit-on-stop";
const USAGE_EXIT_CODE: u8 = 2;

#[derive(Resource)]
struct Options {
    xex: PathBuf,
    /// Close the app once guest execution stops, for scripted runs.
    exit_on_stop: bool,
}

impl Options {
    fn parse(arguments: impl Iterator<Item = String>) -> Option<Self> {
        let mut xex = None;
        let mut exit_on_stop = false;
        for argument in arguments {
            match argument.as_str() {
                EXIT_ON_STOP_FLAG => exit_on_stop = true,
                _ if xex.is_none() && !argument.starts_with('-') => {
                    xex = Some(PathBuf::from(argument))
                }
                _ => return None,
            }
        }
        Some(Self {
            xex: xex?,
            exit_on_stop,
        })
    }
}

fn main() -> AppExit {
    let Some(options) = Options::parse(env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return AppExit::from_code(USAGE_EXIT_CODE);
    };

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: WINDOW_TITLE.to_string(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(ClearColor(BACKGROUND_COLOR))
        .insert_resource(options)
        .add_systems(Startup, (spawn_camera, start_guest))
        .add_systems(Update, exit_on_stop)
        .run()
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn start_guest(options: Res<Options>) {
    #[cfg(feature = "recompiled")]
    if let Err(error) = guest::start(&options.xex) {
        status::report_stop(error);
    }

    #[cfg(not(feature = "recompiled"))]
    status::report_stop(format!(
        "built without the `recompiled` feature, so {} was not loaded; see docs/tooling.md",
        options.xex.display()
    ));
}

fn exit_on_stop(options: Res<Options>, mut exit: MessageWriter<AppExit>) {
    if options.exit_on_stop && status::stop_reason().is_some() {
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use super::{EXIT_ON_STOP_FLAG, Options};

    fn parse(arguments: &[&str]) -> Option<Options> {
        Options::parse(arguments.iter().map(|argument| argument.to_string()))
    }

    #[test]
    fn accepts_executable_with_exit_flag_in_any_order() {
        let options = parse(&[EXIT_ON_STOP_FLAG, "default.xex"]).unwrap();
        assert_eq!(options.xex.to_str(), Some("default.xex"));
        assert!(options.exit_on_stop);
    }

    #[test]
    fn rejects_missing_executable_and_unknown_flags() {
        assert!(parse(&[EXIT_ON_STOP_FLAG]).is_none());
        assert!(parse(&["default.xex", "--unknown"]).is_none());
    }
}
