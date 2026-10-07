//! Rust runtime for a static recompilation of Pure (Xbox 360).
//!
//! Usage: pure-runtime <path/to/default.xex> [--exit-on-stop]
//!
//! Built with the `recompiled` feature, it runs the code tools/recompile.sh generated from your
//! own default.xex. Without it, it only opens the window, so it builds anywhere without the game.

// Without `recompiled`, the guest-facing modules are compiled only for their tests, so their
// callers are missing. Dead-code analysis is meaningful in the full build, which CI also lints.
#![cfg_attr(not(feature = "recompiled"), allow(dead_code))]

#[cfg(any(feature = "recompiled", test))]
mod address_ranges;
#[cfg(any(feature = "recompiled", test))]
mod command_processor;
#[cfg(any(feature = "recompiled", test))]
mod controller;
#[cfg(all(feature = "recompiled", unix))]
mod crash;
#[cfg(any(feature = "recompiled", test))]
mod critical_section;
#[cfg(any(feature = "recompiled", test))]
mod dispatcher;
#[cfg(feature = "recompiled")]
mod display;
#[cfg(any(feature = "recompiled", test))]
mod edram;
#[cfg(any(feature = "recompiled", test))]
mod gpu;
#[cfg(feature = "recompiled")]
mod guest;
#[cfg(any(feature = "recompiled", test))]
mod handles;
#[cfg(feature = "recompiled")]
mod kernel;
#[cfg(any(feature = "recompiled", test))]
mod memory;
#[cfg(any(feature = "recompiled", test))]
mod physical_memory;
#[cfg(any(feature = "recompiled", test))]
mod renderer;
#[cfg(any(feature = "recompiled", test))]
mod shader;
#[cfg(any(feature = "recompiled", test))]
mod spin_lock;
mod status;
#[cfg(any(feature = "recompiled", test))]
mod texture;
#[cfg(any(feature = "recompiled", test))]
mod time;
#[cfg(any(feature = "recompiled", test))]
mod timebase;
#[cfg(any(feature = "recompiled", test))]
mod tls;
#[cfg(any(feature = "recompiled", test))]
mod translate;
#[cfg(any(feature = "recompiled", test))]
mod vfs;
#[cfg(any(feature = "recompiled", test))]
mod virtual_memory;
#[cfg(any(feature = "recompiled", test))]
mod xex_header;
#[cfg(any(feature = "recompiled", test))]
mod xma;

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

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: WINDOW_TITLE.to_string(),
            ..default()
        }),
        ..default()
    }))
    .insert_resource(ClearColor(BACKGROUND_COLOR))
    .insert_resource(options)
    .add_systems(Startup, start_guest)
    .add_systems(Update, exit_on_stop)
    .add_systems(Update, read_controller);
    // The recompiled game's frames are shown through the display; without it the window shows
    // the clear colour.
    #[cfg(feature = "recompiled")]
    app.add_plugins(display::DisplayPlugin);
    #[cfg(not(feature = "recompiled"))]
    app.add_systems(Startup, spawn_camera);
    app.run()
}

#[cfg(not(feature = "recompiled"))]
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

/// Feeds the host keyboard and the first connected gamepad to the first controller each frame.
fn read_controller(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&bevy::input::gamepad::Gamepad>,
) {
    #[cfg(feature = "recompiled")]
    {
        let keyboard = controller::Gamepad::from_keys(&keys);
        let state = match gamepads.iter().next() {
            Some(host) => keyboard.combine(controller::Gamepad::from_host(host)),
            None => keyboard,
        };
        controller::update(state);
    }
    #[cfg(not(feature = "recompiled"))]
    let _ = (keys, gamepads);
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
