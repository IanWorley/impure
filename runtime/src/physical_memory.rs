//! The console's 512 MiB of physical memory, as MmAllocatePhysicalMemoryEx hands it out.
//!
//! The console exposes physical memory through three views chosen by page size: 64 KiB pages at
//! 0xA0000000, 16 MiB pages at 0xC0000000 and 4 KiB pages at 0xE0000000. The 4 KiB view is
//! offset by one page: its address 0xE0000000 is physical 0x1000, and titles rely on that when
//! they compute physical addresses themselves. All three views show the same memory, so titles
//! may read through one view what they wrote through another: the host maps the first two onto
//! one block (see `aliases`), and accesses to the third are redirected into the first (see
//! `memory::host_address`). One allocator assigns each physical range once, and the caller gets
//! the address in its page size's view. The GPU names physical addresses, which the runtime reads
//! through the 64 KiB-page view (see `guest_address`).

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

/// Where guest memory shows physical memory: the views the host maps, each starting at its
/// physical offset. The small-page view's one-page offset is finer than many hosts can map, so it
/// is redirected rather than mapped.
pub fn aliases() -> [Alias; 2] {
    [LARGE_PAGE_VIEW, HUGE_PAGE_VIEW].map(|view| Alias {
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

/// The guest address showing physical address `physical`. Every view shows the same bytes, so
/// one serves for any allocation, and for memory the title has freed while the GPU still reads
/// it, as the console's GPU does. None past the end of physical memory.
pub fn guest_address(physical: u32) -> Option<u32> {
    (physical < PHYSICAL_MEMORY_SIZE).then(|| LARGE_PAGE_VIEW.address(physical))
}

pub struct PhysicalMemory {
    allocated: AddressRanges,
}

impl PhysicalMemory {
    pub const fn new() -> Self {
        Self {
            allocated: AddressRanges::new(),
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
        Some(view.address(physical))
    }

    /// Frees the allocation that starts at `address` (MmFreePhysicalMemory). Returns whether
    /// there was one.
    pub fn free(&mut self, address: u32) -> bool {
        self.allocated.remove(physical_address(address)).is_some()
    }

    /// The size of the allocation `address` falls in, in any view, rounded up to whole pages
    /// as it was allocated.
    pub fn allocation_size(&self, address: u32) -> Option<u32> {
        let (_, size) = self.allocated.containing(physical_address(address))?;
        Some(size)
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
    fn the_small_page_view_is_redirected_to_the_same_physical_bytes() {
        const OFFSET: u32 = 0x3C;
        let address = SMALL_PAGE_VIEW.base + OFFSET;
        assert_eq!(
            crate::memory::host_address(address),
            LARGE_PAGE_VIEW.address(physical_address(address))
        );
    }

    #[test]
    fn allocation_size_is_the_whole_pages_allocated() {
        const OFFSET: u32 = 0x3C;
        let mut memory = PhysicalMemory::new();
        let small = allocate(&mut memory, ODD_SIZE, 0).unwrap();
        let rounded = ODD_SIZE.next_multiple_of(SMALL_PAGE_VIEW.page_size);
        assert_eq!(memory.allocation_size(small + OFFSET), Some(rounded));
        assert_eq!(memory.allocation_size(small + rounded), None);
    }

    #[test]
    fn guest_address_shows_any_view_through_the_large_page_view() {
        const OFFSET: u32 = 0x3C;
        let mut memory = PhysicalMemory::new();
        let small = allocate(&mut memory, ODD_SIZE, 0).unwrap() + OFFSET;
        let shown = guest_address(physical_address(small)).unwrap();
        assert_eq!(
            crate::memory::host_address(shown),
            crate::memory::host_address(small)
        );
        assert_eq!(guest_address(PHYSICAL_MEMORY_SIZE), None);
    }

    #[test]
    fn freed_ranges_are_reused() {
        let mut memory = PhysicalMemory::new();
        let first = allocate(&mut memory, ODD_SIZE, 0).unwrap();
        assert!(memory.free(first));
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
