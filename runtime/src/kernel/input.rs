//! Controller input (XamInput*). Controller 0 is connected and reports the host keyboard's
//! state through the bindings in controller.rs.

use super::ImportCall;
use crate::controller;

/// Only the first controller is connected.
const CONNECTED_USER: u32 = 0;
/// Win32 error codes these imports return.
const ERROR_SUCCESS: u32 = 0;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 0x48F;
/// XINPUT_STATE: a packet number, then XINPUT_GAMEPAD's buttons, triggers and sticks.
const PACKET_NUMBER: u32 = 0x0;
const BUTTONS: u32 = 0x4;
const LEFT_TRIGGER: u32 = 0x6;
const RIGHT_TRIGGER: u32 = 0x7;
const LEFT_STICK: u32 = 0x8;
const RIGHT_STICK: u32 = 0xC;
const STICK_AXIS_SIZE: u32 = 2;

/// XamInputGetState(user, flags, state out) -> Win32 error.
pub(super) fn get_state(call: &ImportCall) -> u32 {
    if call.argument(0) != CONNECTED_USER {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let state = call.argument(2);
    let (packet_number, gamepad) = controller::state();
    let memory = call.memory;
    memory.write_u32(state + PACKET_NUMBER, packet_number);
    memory.write_u16(state + BUTTONS, gamepad.buttons);
    memory.write_u8(state + LEFT_TRIGGER, gamepad.left_trigger);
    memory.write_u8(state + RIGHT_TRIGGER, gamepad.right_trigger);
    for (offset, (x, y)) in [
        (LEFT_STICK, gamepad.left_stick),
        (RIGHT_STICK, gamepad.right_stick),
    ] {
        memory.write_u16(state + offset, x as u16);
        memory.write_u16(state + offset + STICK_AXIS_SIZE, y as u16);
    }
    ERROR_SUCCESS
}

/// XamInputSetState(user, flags, vibration) -> Win32 error. Rumble is not wired to host input
/// yet, so the connected controller accepts and ignores it.
pub(super) fn set_state(call: &ImportCall) -> u32 {
    if call.argument(0) == CONNECTED_USER {
        ERROR_SUCCESS
    } else {
        ERROR_DEVICE_NOT_CONNECTED
    }
}
