//! The first controller, as the title reads it through XamInputGetState, driven by the host
//! keyboard and the first connected physical gamepad together. The keyboard bindings below are
//! the one place to change them; gamepad buttons map to the Xbox 360 pad's by position.

use std::sync::Mutex;

use bevy::input::ButtonInput;
use bevy::input::gamepad::{Gamepad as HostGamepad, GamepadButton};
use bevy::input::keyboard::KeyCode;
use bevy::log::debug;

/// XINPUT_GAMEPAD button bits.
pub const DPAD_UP: u16 = 0x0001;
pub const DPAD_DOWN: u16 = 0x0002;
pub const DPAD_LEFT: u16 = 0x0004;
pub const DPAD_RIGHT: u16 = 0x0008;
pub const START: u16 = 0x0010;
pub const BACK: u16 = 0x0020;
pub const LEFT_THUMB: u16 = 0x0040;
pub const RIGHT_THUMB: u16 = 0x0080;
pub const LEFT_SHOULDER: u16 = 0x0100;
pub const RIGHT_SHOULDER: u16 = 0x0200;
pub const A: u16 = 0x1000;
pub const B: u16 = 0x2000;
pub const X: u16 = 0x4000;
pub const Y: u16 = 0x8000;

/// A key held all the way: triggers report 0-255 and sticks -32768..=32767.
const TRIGGER_FULL: u8 = u8::MAX;
const STICK_FULL: i16 = i16::MAX;

/// What a key drives.
#[derive(Clone, Copy)]
enum Binding {
    Button(u16),
    LeftTrigger,
    RightTrigger,
    /// A stick axis pushed fully one way: which stick, which axis, and the sign.
    Stick(Stick, Axis, i16),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stick {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// The default bindings, laid out for a racer: W and S are the accelerator and brake triggers,
/// A and D steer, and the arrow keys are the D-pad for menus.
const BINDINGS: [(KeyCode, Binding); 21] = [
    (KeyCode::KeyW, Binding::RightTrigger),
    (KeyCode::KeyS, Binding::LeftTrigger),
    (
        KeyCode::KeyA,
        Binding::Stick(Stick::Left, Axis::Horizontal, -1),
    ),
    (
        KeyCode::KeyD,
        Binding::Stick(Stick::Left, Axis::Horizontal, 1),
    ),
    (
        KeyCode::KeyI,
        Binding::Stick(Stick::Right, Axis::Vertical, 1),
    ),
    (
        KeyCode::KeyK,
        Binding::Stick(Stick::Right, Axis::Vertical, -1),
    ),
    (
        KeyCode::KeyJ,
        Binding::Stick(Stick::Right, Axis::Horizontal, -1),
    ),
    (
        KeyCode::KeyL,
        Binding::Stick(Stick::Right, Axis::Horizontal, 1),
    ),
    (KeyCode::ArrowUp, Binding::Button(DPAD_UP)),
    (KeyCode::ArrowDown, Binding::Button(DPAD_DOWN)),
    (KeyCode::ArrowLeft, Binding::Button(DPAD_LEFT)),
    (KeyCode::ArrowRight, Binding::Button(DPAD_RIGHT)),
    (KeyCode::Enter, Binding::Button(A)),
    (KeyCode::Space, Binding::Button(A)),
    (KeyCode::Backspace, Binding::Button(B)),
    (KeyCode::KeyX, Binding::Button(X)),
    (KeyCode::KeyY, Binding::Button(Y)),
    (KeyCode::KeyQ, Binding::Button(LEFT_SHOULDER)),
    (KeyCode::KeyE, Binding::Button(RIGHT_SHOULDER)),
    (KeyCode::Escape, Binding::Button(START)),
    (KeyCode::Tab, Binding::Button(BACK)),
];

/// Physical gamepad buttons and the Xbox 360 buttons in the same places.
const GAMEPAD_BUTTONS: [(GamepadButton, u16); 14] = [
    (GamepadButton::South, A),
    (GamepadButton::East, B),
    (GamepadButton::West, X),
    (GamepadButton::North, Y),
    (GamepadButton::LeftTrigger, LEFT_SHOULDER),
    (GamepadButton::RightTrigger, RIGHT_SHOULDER),
    (GamepadButton::Select, BACK),
    (GamepadButton::Start, START),
    (GamepadButton::LeftThumb, LEFT_THUMB),
    (GamepadButton::RightThumb, RIGHT_THUMB),
    (GamepadButton::DPadUp, DPAD_UP),
    (GamepadButton::DPadDown, DPAD_DOWN),
    (GamepadButton::DPadLeft, DPAD_LEFT),
    (GamepadButton::DPadRight, DPAD_RIGHT),
];

/// XINPUT_GAMEPAD's contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gamepad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
}

impl Gamepad {
    /// The gamepad the held keys describe. Opposite stick directions cancel out.
    pub fn from_keys(keys: &ButtonInput<KeyCode>) -> Self {
        let mut gamepad = Self::default();
        let (mut left, mut right) = ((0i32, 0i32), (0i32, 0i32));
        for (key, binding) in BINDINGS {
            if !keys.pressed(key) {
                continue;
            }
            match binding {
                Binding::Button(bit) => gamepad.buttons |= bit,
                Binding::LeftTrigger => gamepad.left_trigger = TRIGGER_FULL,
                Binding::RightTrigger => gamepad.right_trigger = TRIGGER_FULL,
                Binding::Stick(stick, axis, sign) => {
                    let position = if stick == Stick::Left {
                        &mut left
                    } else {
                        &mut right
                    };
                    let component = if axis == Axis::Horizontal {
                        &mut position.0
                    } else {
                        &mut position.1
                    };
                    *component += i32::from(sign) * i32::from(STICK_FULL);
                }
            }
        }
        let clamp = |value: i32| value.clamp(-i32::from(STICK_FULL), i32::from(STICK_FULL)) as i16;
        gamepad.left_stick = (clamp(left.0), clamp(left.1));
        gamepad.right_stick = (clamp(right.0), clamp(right.1));
        gamepad
    }

    /// The state of a physical gamepad. Both report sticks with up positive.
    pub fn from_host(host: &HostGamepad) -> Self {
        let trigger = |button| {
            (host.get(button).unwrap_or(0.0).clamp(0.0, 1.0) * f32::from(TRIGGER_FULL)) as u8
        };
        let stick = |position: bevy::math::Vec2| {
            let axis = |value: f32| (value.clamp(-1.0, 1.0) * f32::from(STICK_FULL)) as i16;
            (axis(position.x), axis(position.y))
        };
        Self {
            buttons: GAMEPAD_BUTTONS
                .iter()
                .filter(|&&(button, _)| host.pressed(button))
                .fold(0, |buttons, &(_, bit)| buttons | bit),
            left_trigger: trigger(GamepadButton::LeftTrigger2),
            right_trigger: trigger(GamepadButton::RightTrigger2),
            left_stick: stick(host.left_stick()),
            right_stick: stick(host.right_stick()),
        }
    }

    /// Both inputs at once: buttons from either, the further-pressed trigger, and stick
    /// positions added together.
    pub fn combine(self, other: Self) -> Self {
        let add = |a: i16, b: i16| a.saturating_add(b);
        Self {
            buttons: self.buttons | other.buttons,
            left_trigger: self.left_trigger.max(other.left_trigger),
            right_trigger: self.right_trigger.max(other.right_trigger),
            left_stick: (
                add(self.left_stick.0, other.left_stick.0),
                add(self.left_stick.1, other.left_stick.1),
            ),
            right_stick: (
                add(self.right_stick.0, other.right_stick.0),
                add(self.right_stick.1, other.right_stick.1),
            ),
        }
    }
}

/// The current gamepad and its XInput packet number, which changes whenever the state does.
struct Controller {
    gamepad: Gamepad,
    packet_number: u32,
}

static CONTROLLER: Mutex<Controller> = Mutex::new(Controller {
    gamepad: Gamepad {
        buttons: 0,
        left_trigger: 0,
        right_trigger: 0,
        left_stick: (0, 0),
        right_stick: (0, 0),
    },
    packet_number: 0,
});

/// Records the latest host input.
pub fn update(gamepad: Gamepad) {
    let mut controller = CONTROLLER.lock().unwrap();
    if controller.gamepad != gamepad {
        debug!(target: "controller", "{gamepad:?}");
        controller.gamepad = gamepad;
        controller.packet_number = controller.packet_number.wrapping_add(1);
    }
}

/// The gamepad and its packet number, as the title reads them.
pub fn state() -> (u32, Gamepad) {
    let controller = CONTROLLER.lock().unwrap();
    (controller.packet_number, controller.gamepad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(keys: &[KeyCode]) -> Gamepad {
        let mut input = ButtonInput::default();
        for &key in keys {
            input.press(key);
        }
        Gamepad::from_keys(&input)
    }

    #[test]
    fn keys_map_to_buttons_triggers_and_sticks() {
        let gamepad = held(&[
            KeyCode::Enter,
            KeyCode::ArrowUp,
            KeyCode::KeyW,
            KeyCode::KeyA,
        ]);
        assert_eq!(gamepad.buttons, A | DPAD_UP);
        assert_eq!(gamepad.right_trigger, TRIGGER_FULL);
        assert_eq!(gamepad.left_trigger, 0);
        assert_eq!(gamepad.left_stick, (-STICK_FULL, 0));
    }

    #[test]
    fn physical_gamepads_map_buttons_triggers_and_sticks() {
        use bevy::input::gamepad::GamepadAxis;
        let mut host = HostGamepad::default();
        host.digital_mut().press(GamepadButton::South);
        host.analog_mut().set(GamepadButton::RightTrigger2, 1.0);
        host.analog_mut().set(GamepadAxis::LeftStickX, -1.0);
        let gamepad = Gamepad::from_host(&host);
        assert_eq!(gamepad.buttons, A);
        assert_eq!(gamepad.right_trigger, TRIGGER_FULL);
        assert_eq!(gamepad.left_stick, (-STICK_FULL, 0));
    }

    #[test]
    fn keyboard_and_gamepad_combine() {
        let keyboard = held(&[KeyCode::KeyD, KeyCode::Enter]);
        let pad = Gamepad {
            buttons: B,
            left_stick: (STICK_FULL, 0),
            ..Gamepad::default()
        };
        let both = keyboard.combine(pad);
        assert_eq!(both.buttons, A | B);
        assert_eq!(both.left_stick, (STICK_FULL, 0));
    }

    #[test]
    fn the_packet_number_changes_only_with_the_state() {
        let pressed = Gamepad {
            buttons: A,
            ..Gamepad::default()
        };
        update(Gamepad::default());
        let (before, _) = state();
        update(pressed);
        update(pressed);
        let (after, gamepad) = state();
        assert_eq!(after, before.wrapping_add(1));
        assert_eq!(gamepad, pressed);
    }

    #[test]
    fn opposite_stick_directions_cancel() {
        assert_eq!(held(&[KeyCode::KeyA, KeyCode::KeyD]).left_stick, (0, 0));
    }
}
