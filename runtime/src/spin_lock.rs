//! Kernel spin locks: a word in guest memory that is 0 when free and holds the owning guest
//! thread otherwise, changed only with atomic compare-and-swap.

use std::thread;

use crate::memory::GuestMemory;

const FREE: u32 = 0;
/// Spins this many times before giving the host scheduler a chance to run the owner.
const SPINS_BEFORE_YIELD: u32 = 64;

/// Spins until `owner` holds the lock.
pub fn acquire(memory: &GuestMemory, lock: u32, owner: u32) {
    let mut spins = 0;
    while !try_acquire(memory, lock, owner) {
        spins += 1;
        if spins % SPINS_BEFORE_YIELD == 0 {
            thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

pub fn try_acquire(memory: &GuestMemory, lock: u32, owner: u32) -> bool {
    memory.compare_exchange_u32(lock, FREE, owner).is_ok()
}

pub fn release(memory: &GuestMemory, lock: u32) {
    memory.write_u32(lock, FREE);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const LOCK: u32 = 0x1000;
    const THREAD_A: u32 = 0x7000_0000;
    const THREAD_B: u32 = 0x7000_1000;

    #[test]
    fn a_held_lock_cannot_be_taken_until_released() {
        let memory = GuestMemory::reserve().unwrap();
        acquire(&memory, LOCK, THREAD_A);
        assert!(!try_acquire(&memory, LOCK, THREAD_B));
        release(&memory, LOCK);
        assert!(try_acquire(&memory, LOCK, THREAD_B));
    }

    #[test]
    fn a_spinning_thread_gets_the_lock_once_it_is_released() {
        let memory = Arc::new(GuestMemory::reserve().unwrap());
        acquire(&memory, LOCK, THREAD_A);
        let acquired = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (memory, acquired) = (Arc::clone(&memory), Arc::clone(&acquired));
            thread::spawn(move || {
                acquire(&memory, LOCK, THREAD_B);
                acquired.store(true, Ordering::Release);
            })
        };
        assert!(!acquired.load(Ordering::Acquire));
        release(&memory, LOCK);
        waiter.join().unwrap();
        assert_eq!(memory.read_u32(LOCK), THREAD_B);
    }
}
