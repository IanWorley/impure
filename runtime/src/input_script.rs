//! Scripted input for unattended runs (`--input-script <file>`): keys held and released and
//! screenshots taken on chosen frames, so a run can reach a menu or the tutorial and capture it
//! without a person at the keyboard or any operating system automation.
//!
//! Each line is `<frame> <action>`. The frame counts the frames the game has presented, its own
//! clock, so a script behaves the same on a software GPU at a frame every few seconds as on a
//! fast one at 60 a second:
//!
//! ```text
//! # Past the safety warning.
//! 120 down Enter
//! 126 up Enter
//! 300 screenshot title.png
//! 600 resolves title-passes
//! 900 exit
//! ```
//!
//! Keys are named as in the keyboard bindings (`Enter`, `KeyW`, `ArrowUp`; see controller.rs), so
//! a script presses exactly what a player would, through the same bindings. Screenshot paths are
//! relative to the script's directory. `resolves <prefix>` saves every resolve of the next frame
//! (the render-to-texture passes its picture is built from) as `<prefix>-<n>-<address>.png`. Blank lines and lines starting with `#` are ignored, and
//! steps must be in frame order.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use bevy::input::keyboard::KeyCode;

use crate::controller;

const COMMENT_PREFIX: char = '#';
const DOWN: &str = "down";
const UP: &str = "up";
const SCREENSHOT: &str = "screenshot";
const RESOLVES: &str = "resolves";
const EXIT: &str = "exit";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Down(KeyCode),
    Up(KeyCode),
    Screenshot(PathBuf),
    /// Save the next frame's resolves, with this path prefix.
    Resolves(PathBuf),
    Exit,
}

impl fmt::Display for Action {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Down(key) => write!(formatter, "{DOWN} {key:?}"),
            Self::Up(key) => write!(formatter, "{UP} {key:?}"),
            Self::Screenshot(path) => write!(formatter, "{SCREENSHOT} {}", path.display()),
            Self::Resolves(prefix) => write!(formatter, "{RESOLVES} {}", prefix.display()),
            Self::Exit => formatter.write_str(EXIT),
        }
    }
}

#[derive(Debug)]
struct Step {
    frame: u32,
    action: Action,
}

#[derive(Debug)]
pub struct InputScript {
    steps: Vec<Step>,
    /// The first step not yet taken.
    next: usize,
}

impl InputScript {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read input script {}: {error}", path.display()))?;
        let directory = path.parent().unwrap_or(Path::new(""));
        Self::parse(&text, directory)
            .map_err(|error| format!("input script {}: {error}", path.display()))
    }

    /// Parses a script whose screenshot paths are relative to `directory`.
    fn parse(text: &str, directory: &Path) -> Result<Self, String> {
        let mut steps: Vec<Step> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(COMMENT_PREFIX) {
                continue;
            }
            let error = |message: &str| format!("line {}: {message}: {line}", index + 1);
            let mut words = line.split_whitespace();
            let frame: u32 = words
                .next()
                .and_then(|word| word.parse().ok())
                .ok_or_else(|| error("expected a frame number"))?;
            if steps.last().is_some_and(|last| frame < last.frame) {
                return Err(error("steps must be in frame order"));
            }
            let key = |name: Option<&str>| {
                name.and_then(controller::key_named)
                    .ok_or_else(|| error("expected a bound key name"))
            };
            let action = match words.next() {
                Some(DOWN) => Action::Down(key(words.next())?),
                Some(UP) => Action::Up(key(words.next())?),
                Some(SCREENSHOT) => Action::Screenshot(
                    directory.join(words.next().ok_or_else(|| error("expected a file name"))?),
                ),
                Some(RESOLVES) => Action::Resolves(
                    directory.join(
                        words
                            .next()
                            .ok_or_else(|| error("expected a name prefix"))?,
                    ),
                ),
                Some(EXIT) => Action::Exit,
                _ => return Err(error("expected down, up, screenshot, resolves or exit")),
            };
            if words.next().is_some() {
                return Err(error("unexpected words after the action"));
            }
            steps.push(Step { frame, action });
        }
        Ok(Self { steps, next: 0 })
    }

    /// The actions due once `frame` frames have been presented, each returned only once. A
    /// release waits for the next call when this one presses a key, so a press the window
    /// polled late is still held for one poll instead of being released unseen.
    pub fn take_due(&mut self, frame: u32) -> Vec<Action> {
        let mut due = Vec::new();
        for step in self.steps[self.next..]
            .iter()
            .take_while(|step| step.frame <= frame)
        {
            let pressed = due.iter().any(|action| matches!(action, Action::Down(_)));
            if pressed && matches!(step.action, Action::Up(_)) {
                break;
            }
            due.push(step.action.clone());
        }
        self.next += due.len();
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECTORY: &str = "evidence";

    fn parse(text: &str) -> Result<InputScript, String> {
        InputScript::parse(text, Path::new(DIRECTORY))
    }

    #[test]
    fn steps_come_due_once_in_frame_order() {
        let mut script = parse(
            "# Past the warning.\n\
             \n\
             120 down Enter\n\
             126 up Enter\n\
             126 screenshot title.png\n\
             900 exit\n",
        )
        .unwrap();
        assert_eq!(script.take_due(119), []);
        assert_eq!(script.take_due(125), [Action::Down(KeyCode::Enter)]);
        assert_eq!(
            script.take_due(200),
            [
                Action::Up(KeyCode::Enter),
                Action::Screenshot(Path::new(DIRECTORY).join("title.png")),
            ]
        );
        assert_eq!(script.take_due(200), []);
        assert_eq!(script.take_due(u32::MAX), [Action::Exit]);
    }

    #[test]
    fn a_late_poll_holds_a_press_until_the_next_one() {
        let mut script = parse("10 down Enter\n12 up Enter\n12 screenshot after.png").unwrap();
        assert_eq!(script.take_due(20), [Action::Down(KeyCode::Enter)]);
        assert_eq!(
            script.take_due(20),
            [
                Action::Up(KeyCode::Enter),
                Action::Screenshot(Path::new(DIRECTORY).join("after.png")),
            ]
        );
    }

    #[test]
    fn rejects_unbound_keys_unknown_actions_and_steps_out_of_order() {
        assert!(parse("1 down F13").is_err());
        assert!(parse("1 jump").is_err());
        assert!(parse("1 down Enter now").is_err());
        assert!(parse("down Enter").is_err());
        assert!(parse("1 screenshot").is_err());
        assert!(parse("1 resolves").is_err());
        assert!(parse("10 exit\n5 exit").is_err());
    }
}
