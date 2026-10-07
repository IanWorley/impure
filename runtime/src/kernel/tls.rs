//! Thread-local storage imports.

use std::sync::Mutex;

use super::{FALSE, ImportCall, TRUE};
use crate::guest;
use crate::tls::ThreadLocalStorage;

static SLOTS: Mutex<ThreadLocalStorage> = Mutex::new(ThreadLocalStorage::new());
/// KeTlsAlloc's result when no slot is free (TLS_OUT_OF_INDEXES).
const OUT_OF_SLOTS: u32 = u32::MAX;
/// KeTlsGetValue's result for an unallocated slot.
const NO_VALUE: u32 = 0;

fn boolean(value: bool) -> u32 {
    if value { TRUE } else { FALSE }
}

/// KeTlsAlloc() -> slot.
pub(super) fn alloc(_: &ImportCall) -> u32 {
    SLOTS.lock().unwrap().allocate().unwrap_or(OUT_OF_SLOTS)
}

/// KeTlsFree(slot) -> BOOLEAN.
pub(super) fn free(call: &ImportCall) -> u32 {
    boolean(SLOTS.lock().unwrap().free(call.argument(0)))
}

/// KeTlsGetValue(slot) -> value.
pub(super) fn get_value(call: &ImportCall) -> u32 {
    let thread = guest::current_thread();
    SLOTS
        .lock()
        .unwrap()
        .get(thread, call.argument(0))
        .unwrap_or(NO_VALUE)
}

/// KeTlsSetValue(slot, value) -> BOOLEAN.
pub(super) fn set_value(call: &ImportCall) -> u32 {
    let thread = guest::current_thread();
    boolean(
        SLOTS
            .lock()
            .unwrap()
            .set(thread, call.argument(0), call.argument(1)),
    )
}
