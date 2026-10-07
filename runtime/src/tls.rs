//! Thread-local storage slots (KeTlsAlloc and friends).
//!
//! Values are keyed by guest thread and slot in one table, so allocating a slot can clear its
//! value in every thread, as the console guarantees.

use std::collections::BTreeMap;

/// How many slots a title may allocate; Windows' TLS_MINIMUM_AVAILABLE.
const SLOT_COUNT: u32 = u64::BITS;

pub struct ThreadLocalStorage {
    /// Bit n is set while slot n is allocated.
    allocated: u64,
    values: BTreeMap<(u32, u32), u32>,
}

impl ThreadLocalStorage {
    pub const fn new() -> Self {
        Self {
            allocated: 0,
            values: BTreeMap::new(),
        }
    }

    /// The lowest free slot, whose value starts at 0 in every thread.
    pub fn allocate(&mut self) -> Option<u32> {
        let slot = (!self.allocated).trailing_zeros();
        if slot >= SLOT_COUNT {
            return None;
        }
        self.allocated |= 1 << slot;
        self.values.retain(|&(_, value_slot), _| value_slot != slot);
        Some(slot)
    }

    pub fn free(&mut self, slot: u32) -> bool {
        let allocated = self.is_allocated(slot);
        if allocated {
            self.allocated &= !(1 << slot);
        }
        allocated
    }

    pub fn get(&self, thread: u32, slot: u32) -> Option<u32> {
        self.is_allocated(slot)
            .then(|| self.values.get(&(thread, slot)).copied().unwrap_or(0))
    }

    pub fn set(&mut self, thread: u32, slot: u32, value: u32) -> bool {
        let allocated = self.is_allocated(slot);
        if allocated {
            self.values.insert((thread, slot), value);
        }
        allocated
    }

    fn is_allocated(&self, slot: u32) -> bool {
        slot < SLOT_COUNT && self.allocated & (1 << slot) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD_A: u32 = 0x7000_0000;
    const THREAD_B: u32 = 0x7000_1000;
    const VALUE: u32 = 0x1234;

    #[test]
    fn values_are_per_thread() {
        let mut tls = ThreadLocalStorage::new();
        let slot = tls.allocate().unwrap();
        assert!(tls.set(THREAD_A, slot, VALUE));
        assert_eq!(tls.get(THREAD_A, slot), Some(VALUE));
        assert_eq!(tls.get(THREAD_B, slot), Some(0));
    }

    #[test]
    fn a_reallocated_slot_starts_cleared() {
        let mut tls = ThreadLocalStorage::new();
        let slot = tls.allocate().unwrap();
        tls.set(THREAD_A, slot, VALUE);
        assert!(tls.free(slot));
        assert_eq!(tls.get(THREAD_A, slot), None);
        assert_eq!(tls.allocate(), Some(slot));
        assert_eq!(tls.get(THREAD_A, slot), Some(0));
    }

    #[test]
    fn allocation_fails_once_every_slot_is_taken() {
        let mut tls = ThreadLocalStorage::new();
        for _ in 0..SLOT_COUNT {
            assert!(tls.allocate().is_some());
        }
        assert_eq!(tls.allocate(), None);
    }
}
