//! RTL_CRITICAL_SECTION: a recursive lock whose state lives in guest memory, in the console's
//! layout, so guest code that inspects it sees what it expects.
//!
//! Every critical section shares one host mutex and condition variable. Contention wakes all
//! waiters, which is simple and correct; finer-grained waiting can come if profiling asks for it.

use std::sync::{Condvar, Mutex};

use crate::memory::GuestMemory;

/// Field offsets in RTL_CRITICAL_SECTION. It starts with a 16-byte DISPATCHER_HEADER.
const HEADER_TYPE: u32 = 0x0;
const HEADER_ABSOLUTE: u32 = 0x1;
const HEADER_SIGNAL_STATE: u32 = 0x4;
const HEADER_WAIT_LIST: u32 = 0x8;
const LOCK_COUNT: u32 = 0x10;
const RECURSION_COUNT: u32 = 0x14;
const OWNING_THREAD: u32 = 0x18;
/// The header's object type: a synchronization (auto-reset) event.
const SYNCHRONIZATION_EVENT_TYPE: u8 = 1;
/// The header's `absolute` byte holds the spin count in units of this many spins.
const SPIN_COUNT_UNIT: u32 = 256;
/// LockCount is -1 when the section is free and counts holders and waiters above that.
const FREE_LOCK_COUNT: u32 = u32::MAX;
const NO_OWNER: u32 = 0;
/// An empty wait list's links point back at the list head.
const WAIT_LIST_LINK_SIZE: u32 = 4;

pub struct CriticalSections {
    lock: Mutex<()>,
    released: Condvar,
}

impl CriticalSections {
    pub const fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            released: Condvar::new(),
        }
    }

    pub fn initialize(&self, memory: &GuestMemory, section: u32, spin_count: u32) {
        let _guard = self.lock.lock().unwrap();
        memory.write_u8(section + HEADER_TYPE, SYNCHRONIZATION_EVENT_TYPE);
        let spin_units = (spin_count / SPIN_COUNT_UNIT).min(u32::from(u8::MAX)) as u8;
        memory.write_u8(section + HEADER_ABSOLUTE, spin_units);
        memory.write_u32(section + HEADER_SIGNAL_STATE, 0);
        let wait_list = section + HEADER_WAIT_LIST;
        memory.write_u32(wait_list, wait_list);
        memory.write_u32(wait_list + WAIT_LIST_LINK_SIZE, wait_list);
        memory.write_u32(section + LOCK_COUNT, FREE_LOCK_COUNT);
        memory.write_u32(section + RECURSION_COUNT, 0);
        memory.write_u32(section + OWNING_THREAD, NO_OWNER);
    }

    /// Blocks until `thread` owns the section; re-entering a section it owns just counts.
    pub fn enter(&self, memory: &GuestMemory, section: u32, thread: u32) {
        let mut guard = self.lock.lock().unwrap();
        add(memory, section + LOCK_COUNT, 1);
        loop {
            if take(memory, section, thread) {
                return;
            }
            guard = self.released.wait(guard).unwrap();
        }
    }

    pub fn try_enter(&self, memory: &GuestMemory, section: u32, thread: u32) -> bool {
        let _guard = self.lock.lock().unwrap();
        let entered = take(memory, section, thread);
        if entered {
            add(memory, section + LOCK_COUNT, 1);
        }
        entered
    }

    pub fn leave(&self, memory: &GuestMemory, section: u32) {
        let _guard = self.lock.lock().unwrap();
        add(memory, section + LOCK_COUNT, u32::MAX);
        add(memory, section + RECURSION_COUNT, u32::MAX);
        if memory.read_u32(section + RECURSION_COUNT) == 0 {
            memory.write_u32(section + OWNING_THREAD, NO_OWNER);
            self.released.notify_all();
        }
    }
}

/// Makes `thread` the owner, or deepens its recursion, if the section is free or already its own.
fn take(memory: &GuestMemory, section: u32, thread: u32) -> bool {
    let owner = memory.read_u32(section + OWNING_THREAD);
    if owner == NO_OWNER {
        memory.write_u32(section + OWNING_THREAD, thread);
        memory.write_u32(section + RECURSION_COUNT, 1);
        true
    } else if owner == thread {
        add(memory, section + RECURSION_COUNT, 1);
        true
    } else {
        false
    }
}

/// Two's-complement add, so `u32::MAX` decrements.
fn add(memory: &GuestMemory, address: u32, amount: u32) {
    memory.write_u32(address, memory.read_u32(address).wrapping_add(amount));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    const SECTION: u32 = 0x1000;
    const THREAD_A: u32 = 0x7000_0000;
    const THREAD_B: u32 = 0x7000_1000;
    const SPIN_COUNT: u32 = 0x400;

    fn initialized() -> (GuestMemory, CriticalSections) {
        let memory = GuestMemory::reserve().unwrap();
        let sections = CriticalSections::new();
        sections.initialize(&memory, SECTION, SPIN_COUNT);
        (memory, sections)
    }

    #[test]
    fn recursive_entry_counts_and_full_release_frees() {
        let (memory, sections) = initialized();
        sections.enter(&memory, SECTION, THREAD_A);
        sections.enter(&memory, SECTION, THREAD_A);
        assert_eq!(memory.read_u32(SECTION + RECURSION_COUNT), 2);
        assert!(!sections.try_enter(&memory, SECTION, THREAD_B));

        sections.leave(&memory, SECTION);
        assert_eq!(memory.read_u32(SECTION + OWNING_THREAD), THREAD_A);
        sections.leave(&memory, SECTION);
        assert_eq!(memory.read_u32(SECTION + OWNING_THREAD), NO_OWNER);
        assert_eq!(memory.read_u32(SECTION + LOCK_COUNT), FREE_LOCK_COUNT);
        assert!(sections.try_enter(&memory, SECTION, THREAD_B));
    }

    #[test]
    fn another_thread_enters_once_the_owner_leaves() {
        let (memory, sections) = initialized();
        let shared = Arc::new((memory, sections));
        shared.1.enter(&shared.0, SECTION, THREAD_A);

        let waiter = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                shared.1.enter(&shared.0, SECTION, THREAD_B);
                shared.0.read_u32(SECTION + OWNING_THREAD)
            })
        };
        shared.1.leave(&shared.0, SECTION);
        assert_eq!(waiter.join().unwrap(), THREAD_B);
    }
}
