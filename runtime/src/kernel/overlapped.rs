//! XOVERLAPPED completion for asynchronous Xam calls. The runtime finishes every such call
//! immediately: it records the result in the overlapped block, signals its event, and returns
//! ERROR_IO_PENDING, as the console does for a call that completes on its own.

use super::{ImportCall, objects};
use crate::status;

/// XOVERLAPPED: the result, the transferred length, an internal context, the event to signal,
/// a completion routine and its context, then the extended error.
const RESULT: u32 = 0x0;
const LENGTH: u32 = 0x4;
const EVENT: u32 = 0xC;
const COMPLETION_ROUTINE: u32 = 0x10;
const EXTENDED_ERROR: u32 = 0x18;
/// The Win32 error a call returns when it completes through its overlapped block.
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
    let memory = call.memory;
    if memory.read_u32(overlapped + COMPLETION_ROUTINE) != 0 {
        status::halt_guest_thread(format!(
            "an overlapped call from {:#010x} asked for a completion routine, which is not supported yet",
            call.return_address()
        ));
    }
    memory.write_u32(overlapped + RESULT, result);
    memory.write_u32(overlapped + LENGTH, length);
    memory.write_u32(overlapped + EXTENDED_ERROR, result);
    let event = memory.read_u32(overlapped + EVENT);
    if event != 0 {
        objects::signal_event(call, event);
    }
    ERROR_IO_PENDING
}
