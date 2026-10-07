//! The console's 512 MiB of physical memory, as MmAllocatePhysicalMemoryEx hands it out.
//!
//! The console exposes physical memory through three views chosen by page size: 64 KiB pages at
//! 0xA0000000, 16 MiB pages at 0xC0000000 and 4 KiB pages at 0xE0000000. The 4 KiB view is
//! offset by one page: its address 0xE0000000 is physical 0x1000, and titles rely on that when
//! they compute physical addresses themselves. All three views show the same memory (see
//! `aliases`), so titles may read through one view what they wrote through another. One
//! allocator assigns each physical range once, and the caller gets the address in its page
//! size's view.

use std::collections::BTreeMap;

use crate::address_ranges::AddressRanges;
use crate::memory::Alias;

pub const PHYSICAL_MEMORY_SIZE: u32 = 0x2000_0000;

/// One view of physical memory: where it starts, its page size, and the physical address its
/// first byte maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
    base: u32,
    page_size: u32,
    physical_offset: u32,
}

const LARGE_PAGE_VIEW: View = View {
    base: 0xA000_0000,
    page_size: 0x1_0000,
    physical_offset: 0,
};
const HUGE_PAGE_VIEW: View = View {
    base: 0xC000_0000,
    page_size: 0x100_0000,
    physical_offset: 0,
};
const SMALL_PAGE_VIEW: View = View {
    base: 0xE000_0000,
    page_size: 0x1000,
    physical_offset: 0x1000,
};

impl View {
    /// The view an address falls in; addresses below every view map without an offset.
    fn containing(address: u32) -> Option<Self> {
        [SMALL_PAGE_VIEW, HUGE_PAGE_VIEW, LARGE_PAGE_VIEW]
            .into_iter()
            .find(|view| address >= view.base)
    }

    fn address(self, physical: u32) -> u32 {
        self.base + (physical - self.physical_offset)
    }
}

/// Where guest memory shows physical memory: each view, starting at its physical offset.
pub fn aliases() -> [Alias; 3] {
    [LARGE_PAGE_VIEW, HUGE_PAGE_VIEW, SMALL_PAGE_VIEW].map(|view| Alias {
        guest_address: view.base,
        shared_offset: view.physical_offset as usize,
        length: (PHYSICAL_MEMORY_SIZE - view.physical_offset) as usize,
    })
}

/// The protection argument's page-size flags (the same values as NtAllocateVirtualMemory's).
#[derive(Clone, Copy, Debug)]
pub struct Protection(pub u32);

impl Protection {
    const LARGE_PAGES: u32 = 0x2000_0000;
    const SIXTEEN_MB_PAGES: u32 = 0x8000_0000;

    fn view(self) -> View {
        if self.0 & Self::SIXTEEN_MB_PAGES != 0 {
            HUGE_PAGE_VIEW
        } else if self.0 & Self::LARGE_PAGES != 0 {
            LARGE_PAGE_VIEW
        } else {
            SMALL_PAGE_VIEW
        }
    }
}

/// The physical address behind an address in any view (MmGetPhysicalAddress).
pub fn physical_address(address: u32) -> u32 {
    let offset = View::containing(address).map_or(0, |view| view.physical_offset);
    address % PHYSICAL_MEMORY_SIZE + offset
}

pub struct PhysicalMemory {
    allocated: AddressRanges,
    /// The view each allocation was handed out in, keyed by its physical base.
    views: BTreeMap<u32, View>,
}

impl PhysicalMemory {
    pub const fn new() -> Self {
        Self {
            allocated: AddressRanges::new(),
            views: BTreeMap::new(),
        }
    }

    /// Allocates `size` bytes at a physical address in `lowest..=highest` aligned to
    /// `alignment` (both rounded up to whole pages), returning its address in the right view.
    pub fn allocate(
        &mut self,
        size: u32,
        alignment: u32,
        lowest: u32,
        highest: u32,
        protection: Protection,
    ) -> Option<u32> {
        let view = protection.view();
        let size = size.max(1).checked_next_multiple_of(view.page_size)?;
        let alignment = alignment
            .max(view.page_size)
            .checked_next_multiple_of(view.page_size)?;
        let low = lowest.max(view.physical_offset);
        let high = (u64::from(highest) + 1).min(u64::from(PHYSICAL_MEMORY_SIZE));
        let physical = self.allocated.first_fit(low, high, size, alignment)?;
        self.allocated.insert(physical, size);
        self.views.insert(physical, view);
        Some(view.address(physical))
    }

    /// Frees the allocation that starts at `address` (MmFreePhysicalMemory). Returns whether
    /// there was one.
    pub fn free(&mut self, address: u32) -> bool {
        let physical = physical_address(address);
        self.views.remove(&physical);
        self.allocated.remove(physical).is_some()
    }

    /// The address of a physical address in the view its allocation was handed out in. Writing
    /// there also reaches a view the host could not alias (see `memory::GuestMemory`).
    pub fn view_address(&self, physical: u32) -> Option<u32> {
        if !self.allocated.covers(physical, u64::from(physical) + 1) {
            return None;
        }
        let (_, &view) = self.views.range(..=physical).next_back()?;
        Some(view.address(physical))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY_ADDRESS: (u32, u32) = (0, u32::MAX);
    const ODD_SIZE: u32 = 0x12C0;

    fn allocate(memory: &mut PhysicalMemory, size: u32, protection: u32) -> Option<u32> {
        memory.allocate(
            size,
            0,
            ANY_ADDRESS.0,
            ANY_ADDRESS.1,
            Protection(protection),
        )
    }

    #[test]
    fn page_size_picks_the_view_and_physical_ranges_are_never_shared() {
        let mut memory = PhysicalMemory::new();
        let large = allocate(&mut memory, ODD_SIZE, Protection::LARGE_PAGES).unwrap();
        let small = allocate(&mut memory, ODD_SIZE, 0).unwrap();
        assert_eq!(large, LARGE_PAGE_VIEW.base);
        assert_eq!(physical_address(small), LARGE_PAGE_VIEW.page_size);
    }

    #[test]
    fn the_small_page_view_is_one_page_behind_physical_memory() {
        /// An address inside a small-page block and the physical address the console gives it.
        const ADDRESS: u32 = 0xE000_A03C;
        const PHYSICAL: u32 = 0xB03C;
        let mut memory = PhysicalMemory::new();
        assert_eq!(
            allocate(&mut memory, ODD_SIZE, 0),
            Some(SMALL_PAGE_VIEW.base)
        );
        assert_eq!(
            physical_address(SMALL_PAGE_VIEW.base),
            SMALL_PAGE_VIEW.physical_offset
        );
        assert_eq!(physical_address(ADDRESS), PHYSICAL);
    }

    #[test]
    fn view_address_inverts_physical_address_for_each_allocation() {
        const OFFSET: u32 = 0x3C;
        let mut memory = PhysicalMemory::new();
        let small = allocate(&mut memory, ODD_SIZE, 0).unwrap();
        let large = allocate(&mut memory, ODD_SIZE, Protection::LARGE_PAGES).unwrap();
        for address in [small + OFFSET, large + OFFSET] {
            assert_eq!(
                memory.view_address(physical_address(address)),
                Some(address)
            );
        }
        assert_eq!(memory.view_address(PHYSICAL_MEMORY_SIZE - 1), None);
    }

    #[test]
    fn freed_ranges_are_reused() {
        let mut memory = PhysicalMemory::new();
        let first = allocate(&mut memory, ODD_SIZE, 0).unwrap();
        assert!(memory.free(first));
        assert_eq!(memory.view_address(physical_address(first)), None);
        assert!(!memory.free(first));
        assert_eq!(allocate(&mut memory, ODD_SIZE, 0), Some(first));
    }

    #[test]
    fn allocations_respect_the_physical_address_window() {
        const LOWEST: u32 = 0x10_0000;
        let mut memory = PhysicalMemory::new();
        let address = memory
            .allocate(
                ODD_SIZE,
                0,
                LOWEST,
                u32::MAX,
                Protection(Protection::LARGE_PAGES),
            )
            .unwrap();
        assert_eq!(physical_address(address), LOWEST);
        assert_eq!(
            memory.allocate(PHYSICAL_MEMORY_SIZE, 0, 0, u32::MAX, Protection(0)),
            None
        );
    }
}
