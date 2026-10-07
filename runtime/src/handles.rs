//! The title's handle table: each handle names one kernel object.

use std::collections::BTreeMap;

/// Handles look like the console's: nonzero, 4-byte aligned, well away from guest pointers.
const FIRST_HANDLE: u32 = 0xF800_0004;
const HANDLE_STRIDE: u32 = 4;

pub struct HandleTable<T> {
    next: u32,
    objects: BTreeMap<u32, T>,
}

impl<T: Clone> HandleTable<T> {
    pub const fn new() -> Self {
        Self {
            next: FIRST_HANDLE,
            objects: BTreeMap::new(),
        }
    }

    /// Handles are never reused, so a stale handle cannot alias a newer object.
    pub fn insert(&mut self, object: T) -> Option<u32> {
        let handle = self.next;
        self.next = handle.checked_add(HANDLE_STRIDE)?;
        self.objects.insert(handle, object);
        Some(handle)
    }

    pub fn get(&self, handle: u32) -> Option<T> {
        self.objects.get(&handle).cloned()
    }

    pub fn remove(&mut self, handle: u32) -> Option<T> {
        self.objects.remove(&handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT: u32 = 0x7000_0000;

    #[test]
    fn closed_handles_stop_resolving_and_are_not_reused() {
        let mut table = HandleTable::new();
        let first = table.insert(OBJECT).unwrap();
        assert_eq!(table.get(first), Some(OBJECT));
        assert_eq!(table.remove(first), Some(OBJECT));
        assert_eq!(table.get(first), None);
        assert_ne!(table.insert(OBJECT), Some(first));
    }
}
