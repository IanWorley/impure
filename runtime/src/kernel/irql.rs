//! Interrupt request levels and spin locks. Host threads are scheduled by the host, so the IRQL
//! does not mask anything; it is tracked per guest thread so the values the title saves and
//! restores round-trip as on the console.

use std::cell::Cell;

use super::{FALSE, ImportCall, NO_RETURN_VALUE, TRUE};
use crate::{guest, spin_lock};

/// The level code runs at normally, and the level spin locks raise it to.
const PASSIVE_LEVEL: u32 = 0;
const DISPATCH_LEVEL: u32 = 2;

thread_local! {
    static IRQL: Cell<u32> = const { Cell::new(PASSIVE_LEVEL) };
}

/// Sets the calling thread's IRQL, returning the previous one.
fn set_irql(level: u32) -> u32 {
    IRQL.replace(level)
}

/// KfAcquireSpinLock(lock) -> previous IRQL.
pub(super) fn kf_acquire_spin_lock(call: &ImportCall) -> u32 {
    let previous = set_irql(DISPATCH_LEVEL);
    spin_lock::acquire(call.memory, call.argument(0), guest::current_thread());
    previous
}

/// KfReleaseSpinLock(lock, previous IRQL).
pub(super) fn kf_release_spin_lock(call: &ImportCall) -> u32 {
    spin_lock::release(call.memory, call.argument(0));
    set_irql(call.argument(1));
    NO_RETURN_VALUE
}

/// KeAcquireSpinLockAtRaisedIrql(lock).
pub(super) fn ke_acquire_spin_lock_at_raised_irql(call: &ImportCall) -> u32 {
    spin_lock::acquire(call.memory, call.argument(0), guest::current_thread());
    NO_RETURN_VALUE
}

/// KeTryToAcquireSpinLockAtRaisedIrql(lock) -> BOOLEAN.
pub(super) fn ke_try_to_acquire_spin_lock_at_raised_irql(call: &ImportCall) -> u32 {
    if spin_lock::try_acquire(call.memory, call.argument(0), guest::current_thread()) {
        TRUE
    } else {
        FALSE
    }
}

/// KeReleaseSpinLockFromRaisedIrql(lock).
pub(super) fn ke_release_spin_lock_from_raised_irql(call: &ImportCall) -> u32 {
    spin_lock::release(call.memory, call.argument(0));
    NO_RETURN_VALUE
}

/// KeRaiseIrqlToDpcLevel() -> previous IRQL.
pub(super) fn ke_raise_irql_to_dpc_level(_: &ImportCall) -> u32 {
    set_irql(DISPATCH_LEVEL)
}

/// KfLowerIrql(level).
pub(super) fn kf_lower_irql(call: &ImportCall) -> u32 {
    set_irql(call.argument(0));
    NO_RETURN_VALUE
}
