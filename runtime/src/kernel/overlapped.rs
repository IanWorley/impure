//! XOVERLAPPED completion for asynchronous Xam calls. Most such calls finish immediately: they
//! record the result in the overlapped block, signal its event, and return ERROR_IO_PENDING, as
//! the console does for a call that completes on its own. A call that waits for the player
//! (the keyboard) starts pending and is finished later, from the window's thread.

use std::sync::atomic::{Ordering, fence};

use super::{ImportCall, objects};
use crate::memory::GuestMemory;
use crate::status;

/// XOVERLAPPED: the result, the transferred length, an internal context, the event to signal,
/// a completion routine and its context, then the extended error.
const RESULT: u32 = 0x0;
const LENGTH: u32 = 0x4;
const EVENT: u32 = 0xC;
const COMPLETION_ROUTINE: u32 = 0x10;
const EXTENDED_ERROR: u32 = 0x18;
/// The Win32 error a call returns when it completes through its overlapped block, and the
/// result its block holds until then.
pub(super) const ERROR_IO_PENDING: u32 = 997;

/// Completes `overlapped` with `result` and returns what the import should return.
pub(super) fn complete(call: &ImportCall, overlapped: u32, result: u32) -> u32 {
    complete_with_length(call, overlapped, result, 0)
}

/// As `complete`, also reporting a call-specific count (bytes or items) as its length.
pub(super) fn complete_with_length(
    call: &ImportCall,
    overlapped: u32,
    result: u32,
    length: u32,
) -> u32 {
    refuse_completion_routine(call, overlapped);
    finish(call.memory, overlapped, result, length);
    ERROR_IO_PENDING
}

/// Leaves `overlapped` pending for a call that `finish` completes later, and returns what the
/// import should return.
pub(super) fn start(call: &ImportCall, overlapped: u32) -> u32 {
    refuse_completion_routine(call, overlapped);
    call.memory.write_u32(overlapped + RESULT, ERROR_IO_PENDING);
    ERROR_IO_PENDING
}

/// Records `result` in `overlapped` and signals its event. Safe on any thread.
pub(super) fn finish(memory: &GuestMemory, overlapped: u32, result: u32, length: u32) {
    memory.write_u32(overlapped + LENGTH, length);
    memory.write_u32(overlapped + EXTENDED_ERROR, result);
    // Titles poll the result until it is no longer ERROR_IO_PENDING and then read the rest, so
    // it is written last, after the others are visible.
    fence(Ordering::Release);
    memory.write_u32(overlapped + RESULT, result);
    let event = memory.read_u32(overlapped + EVENT);
    if event != 0 {
        objects::signal_event(memory, event);
    }
}

fn refuse_completion_routine(call: &ImportCall, overlapped: u32) {
    if call.memory.read_u32(overlapped + COMPLETION_ROUTINE) != 0 {
        status::halt_guest_thread(format!(
            "an overlapped call from {:#010x} asked for a completion routine, which is not supported yet",
            call.return_address()
        ));
    }
}
