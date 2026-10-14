//! Rust runtime for a static recompilation of Pure (Xbox 360).
//!
//! Usage: pure-runtime [<disc image (.iso) or default.xex>] [--choose] [--build-game]
//!                     [--exit-on-stop] [--input-script <file>]
//!
//! With no game named, it opens the game it ran last time, or asks for one in the window. The
//! game's code is never part of the runtime: the first time, it is translated and compiled from
//! the player's own copy into a library the runtime then loads (src/builder/, src/game_module.rs).

mod address_ranges;
#[cfg(feature = "audio")]
mod audio_output;
mod builder;
mod command_processor;
mod controller;
#[cfg(unix)]
mod crash;
mod critical_section;
mod data_dir;
mod dispatcher;
mod display;
mod edram;
mod game_module;
mod gpu;
mod guest;
mod handles;
mod input_script;
mod kernel;
mod keyboard_dialog;
mod launcher;
mod memory;
mod physical_memory;
mod prepare;
mod progress;
mod renderer;
mod shader;
mod spin_lock;
mod status;
mod submitter;
mod texture;
mod time;
mod timebase;
mod tls;
mod translate;
mod vfs;
mod virtual_memory;
mod xex_header;
mod xiso;
mod xma;
#[cfg(feature = "audio")]
mod xma_ffmpeg;

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};

const WINDOW_TITLE: &str = "Pure (static recompilation)";
const BACKGROUND_COLOR: Color = Color::srgb(0.05, 0.05, 0.08);
const USAGE: &str = "usage: pure-runtime [<disc image (.iso) or default.xex>] [--choose] [--build-game] [--exit-on-stop] [--input-script <file>]";
const CHOOSE_FLAG: &str = "--choose";
const BUILD_GAME_FLAG: &str = "--build-game";
const EXIT_ON_STOP_FLAG: &str = "--exit-on-stop";
const INPUT_SCRIPT_FLAG: &str = "--input-script";
const USAGE_EXIT_CODE: u8 = 2;
const FAILURE_EXIT_CODE: u8 = 1;
/// The window redraws at the console's 60 Hz. Without vertical sync (under Xvfb, for one) Bevy
/// would otherwise redraw as fast as it can, taking the cores the guest threads need.
const WINDOW_REFRESH_HZ: u32 = 60;
const WINDOW_UPDATE_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / WINDOW_REFRESH_HZ as u64);

#[derive(Resource)]
struct Options {
    /// The player's disc image or `default.xex`.
    game: Option<PathBuf>,
    /// Ask for a game even if one was opened before.
    choose: bool,
    /// Prepare the game (copy the disc, build the game library) in the terminal, then exit.
    build_game: bool,
    /// Close the app once guest execution stops, for scripted runs.
    exit_on_stop: bool,
    /// Keys and screenshots to play back on chosen frames; see input_script.rs.
    input_script: Option<PathBuf>,
}

impl Options {
    fn parse(mut arguments: impl Iterator<Item = String>) -> Option<Self> {
        let mut options = Self {
            game: None,
            choose: false,
            build_game: false,
            exit_on_stop: false,
            input_script: None,
        };
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                CHOOSE_FLAG => options.choose = true,
                BUILD_GAME_FLAG => options.build_game = true,
                EXIT_ON_STOP_FLAG => options.exit_on_stop = true,
                INPUT_SCRIPT_FLAG => options.input_script = Some(PathBuf::from(arguments.next()?)),
                _ if options.game.is_none() && !argument.starts_with('-') => {
                    options.game = Some(PathBuf::from(argument))
                }
                _ => return None,
            }
        }
        Some(options)
    }

    /// The game opened last time, unless the player asked to choose again.
    fn remembered(&self) -> Option<PathBuf> {
        (!self.choose).then(prepare::remembered).flatten()
    }
}

fn main() -> AppExit {
    let Some(options) = Options::parse(env::args().skip(1)) else {
        eprintln!("{USAGE}");
        return AppExit::from_code(USAGE_EXIT_CODE);
    };
    if options.build_game {
        return build_game(&options);
    }
    // Loaded before the window opens, so a mistake in the script is reported at once.
    let scripted_input = match options
        .input_script
        .as_deref()
        .map(input_script::InputScript::load)
        .transpose()
    {
        Ok(script) => script.map(ScriptedInput::new),
        Err(error) => {
            eprintln!("{error}");
            return AppExit::from_code(USAGE_EXIT_CODE);
        }
    };

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: WINDOW_TITLE.to_string(),
            ..default()
        }),
        ..default()
    }))
    .insert_resource(WinitSettings {
        focused_mode: UpdateMode::reactive(WINDOW_UPDATE_INTERVAL),
        unfocused_mode: UpdateMode::reactive(WINDOW_UPDATE_INTERVAL),
    })
    .insert_resource(ClearColor(BACKGROUND_COLOR))
    .insert_resource(launcher::Startup {
        game: options.game.clone(),
        remembered: options.remembered(),
    })
    .insert_resource(options)
    .add_plugins((
        display::DisplayPlugin,
        launcher::LauncherPlugin,
        keyboard_dialog::KeyboardDialogPlugin,
    ))
    .add_systems(
        Update,
        (
            play_input_script,
            keyboard_dialog::type_into_dialog,
            read_controller,
        )
            .chain(),
    )
    .add_systems(Update, exit_on_stop);
    if let Some(scripted_input) = scripted_input {
        app.insert_resource(scripted_input);
    }
    app.run()
}

/// `--build-game`: prepares the game with progress in the terminal, for scripts and for anyone
/// who would rather wait there than in the window.
fn build_game(options: &Options) -> AppExit {
    let Some(source) = options.game.clone().or_else(|| options.remembered()) else {
        eprintln!("{BUILD_GAME_FLAG} needs a disc image or default.xex\n{USAGE}");
        return AppExit::from_code(USAGE_EXIT_CODE);
    };
    match prepare::prepare(&source, &progress::Progress::new(true)) {
        Ok(prepared) => {
            eprintln!("The game library is ready: {}", prepared.library.display());
            AppExit::Success
        }
        Err(error) => {
            eprintln!("{error}");
            AppExit::from_code(FAILURE_EXIT_CODE)
        }
    }
}

/// An input script being played back, and the keys it holds down.
#[derive(Resource)]
pub(crate) struct ScriptedInput {
    script: input_script::InputScript,
    pub(crate) held: ButtonInput<KeyCode>,
}

impl ScriptedInput {
    fn new(script: input_script::InputScript) -> Self {
        Self {
            script,
            held: ButtonInput::default(),
        }
    }
}

/// Takes the input script's steps that are due at the frame the title has reached.
fn play_input_script(scripted: Option<ResMut<ScriptedInput>>, mut exit: MessageWriter<AppExit>) {
    use input_script::Action;

    let Some(mut scripted) = scripted else {
        return;
    };
    let frame = kernel::frames_presented();
    for action in scripted.script.take_due(frame) {
        info!(target: "input_script", "frame {frame}: {action}");
        match action {
            Action::Down(key) => scripted.held.press(key),
            Action::Up(key) => scripted.held.release(key),
            Action::Screenshot(path) => renderer::capture_next_frame(path),
            Action::Resolves(prefix) => renderer::dump_next_frame_resolves(prefix),
            Action::Exit => {
                exit.write(AppExit::Success);
            }
        }
    }
}

/// Feeds the host keyboard, the first connected gamepad and any input script's held keys to the
/// first controller each frame. While the keyboard dialog is up, the controller is at rest.
fn read_controller(
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&bevy::input::gamepad::Gamepad>,
    scripted: Option<Res<ScriptedInput>>,
    dialog: Res<keyboard_dialog::KeyboardDialog>,
) {
    if dialog.is_open() {
        controller::update(controller::Gamepad::default());
        return;
    }
    let mut state = controller::Gamepad::from_keys(&dialog.unswallowed(&keys));
    if let Some(host) = gamepads.iter().next() {
        state = state.combine(controller::Gamepad::from_host(host));
    }
    if let Some(scripted) = scripted {
        state = state.combine(controller::Gamepad::from_keys(
            &dialog.unswallowed(&scripted.held),
        ));
    }
    controller::update(state);
}

fn exit_on_stop(options: Res<Options>, mut exit: MessageWriter<AppExit>) {
    if options.exit_on_stop && status::stop_reason().is_some() {
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILD_GAME_FLAG, EXIT_ON_STOP_FLAG, INPUT_SCRIPT_FLAG, Options};

    fn parse(arguments: &[&str]) -> Option<Options> {
        Options::parse(arguments.iter().map(|argument| argument.to_string()))
    }

    #[test]
    fn takes_the_game_and_flags_in_any_order() {
        let options = parse(&[EXIT_ON_STOP_FLAG, "Pure.iso", BUILD_GAME_FLAG]).unwrap();
        assert_eq!(options.game.unwrap().to_str(), Some("Pure.iso"));
        assert!(options.exit_on_stop && options.build_game);
        assert!(options.input_script.is_none());
    }

    #[test]
    fn the_game_is_optional() {
        assert!(parse(&[]).unwrap().game.is_none());
    }

    #[test]
    fn takes_the_input_script_path_from_the_next_argument() {
        let options = parse(&["default.xex", INPUT_SCRIPT_FLAG, "boot.txt"]).unwrap();
        assert_eq!(options.input_script.unwrap().to_str(), Some("boot.txt"));
        assert!(parse(&["default.xex", INPUT_SCRIPT_FLAG]).is_none());
    }

    #[test]
    fn rejects_unknown_flags_and_a_second_game() {
        assert!(parse(&["default.xex", "--unknown"]).is_none());
        assert!(parse(&["default.xex", "other.xex"]).is_none());
    }
}
