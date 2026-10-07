//! The title's virtual address space, as NtAllocateVirtualMemory hands it out.
//!
//! Guest memory is mapped and zeroed up front (see memory.rs), so reserving and committing are
//! bookkeeping only: the runtime tracks which ranges are allocated so it never hands out the same
//! range twice. Page protection is not enforced; every guest page is readable and writable.

use crate::address_ranges::AddressRanges;

const SMALL_PAGE_SIZE: u32 = 0x1000;
const LARGE_PAGE_SIZE: u32 = 0x1_0000;

/// The console splits user virtual memory by page size: 4 KiB pages below 0x40000000 and 64 KiB
/// pages above. The bottom 64 KiB stays unmapped to catch null pointers, and the 64 KiB range
/// ends where the runtime's own system heap begins (see guest.rs).
const SMALL_PAGE_RANGE: (u32, u32) = (0x0001_0000, 0x4000_0000);
const LARGE_PAGE_RANGE: (u32, u32) = (0x4000_0000, 0x7000_0000);

/// The `AllocationType` argument of NtAllocateVirtualMemory (Xbox 360 flag values).
#[derive(Clone, Copy, Debug)]
pub struct AllocationType(pub u32);

impl AllocationType {
    pub const COMMIT: u32 = 0x1000;
    pub const RESERVE: u32 = 0x2000;
    pub const LARGE_PAGES: u32 = 0x2000_0000;
    pub const SIXTEEN_MB_PAGES: u32 = 0x8000_0000;

    fn contains(self, flag: u32) -> bool {
        self.0 & flag != 0
    }
}

/// A range of guest addresses handed to the title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    pub base: u32,
    pub size: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AllocateError {
    InvalidParameter,
    /// The requested range overlaps an allocation, or a commit names an unreserved range.
    ConflictingAddresses,
    NoMemory,
    /// A request the runtime does not model yet; stopping beats guessing.
    Unsupported(&'static str),
}

struct VirtualHeap {
    start: u32,
    end: u32,
    page_size: u32,
    reserved: AddressRanges,
}

impl VirtualHeap {
    const fn new((start, end): (u32, u32), page_size: u32) -> Self {
        Self {
            start,
            end,
            page_size,
            reserved: AddressRanges::new(),
        }
    }

    fn contains_address(&self, address: u32) -> bool {
        (self.start..self.end).contains(&address)
    }

    /// Lowest free range that fits `size` bytes (already a multiple of the page size).
    fn reserve_anywhere(&mut self, size: u32) -> Option<u32> {
        let base =
            self.reserved
                .first_fit(self.start, u64::from(self.end), size, self.page_size)?;
        self.reserved.insert(base, size);
        Some(base)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The address is not in an allocated range.
    NotAllocated,
    /// A release named an address inside a reservation rather than its base.
    NotAtBase,
}

pub struct VirtualMemory {
    small_pages: VirtualHeap,
    large_pages: VirtualHeap,
}

impl VirtualMemory {
    pub const fn new() -> Self {
        Self {
            small_pages: VirtualHeap::new(SMALL_PAGE_RANGE, SMALL_PAGE_SIZE),
            large_pages: VirtualHeap::new(LARGE_PAGE_RANGE, LARGE_PAGE_SIZE),
        }
    }

    /// NtAllocateVirtualMemory: with no base, reserves a fresh range sized up to whole pages;
    /// with a base, reserves exactly there, or commits pages inside an existing reservation.
    pub fn allocate(
        &mut self,
        requested_base: u32,
        size: u32,
        kind: AllocationType,
    ) -> Result<Allocation, AllocateError> {
        if kind.contains(AllocationType::SIXTEEN_MB_PAGES) {
            return Err(AllocateError::Unsupported("16 MiB pages"));
        }
        if size == 0 || !kind.contains(AllocationType::RESERVE | AllocationType::COMMIT) {
            return Err(AllocateError::InvalidParameter);
        }

        if requested_base == 0 {
            let heap = if kind.contains(AllocationType::LARGE_PAGES) {
                &mut self.large_pages
            } else {
                &mut self.small_pages
            };
            let size = size
                .checked_next_multiple_of(heap.page_size)
                .ok_or(AllocateError::NoMemory)?;
            let base = heap.reserve_anywhere(size).ok_or(AllocateError::NoMemory)?;
            return Ok(Allocation { base, size });
        }

        let heap = [&mut self.small_pages, &mut self.large_pages]
            .into_iter()
            .find(|heap| heap.contains_address(requested_base))
            .ok_or(AllocateError::InvalidParameter)?;
        let page_size = u64::from(heap.page_size);
        let base = requested_base - requested_base % heap.page_size;
        let end = (u64::from(requested_base) + u64::from(size)).next_multiple_of(page_size);
        if end > u64::from(heap.end) {
            return Err(AllocateError::InvalidParameter);
        }
        // Checked against the heap's end just above.
        let end = end as u32;
        let allocation = Allocation {
            base,
            size: end - base,
        };

        if kind.contains(AllocationType::RESERVE) {
            if heap.reserved.overlaps(base, u64::from(end)) {
                return Err(AllocateError::ConflictingAddresses);
            }
            heap.reserved.insert(base, allocation.size);
            return Ok(allocation);
        }
        if heap.reserved.covers(base, u64::from(end)) {
            Ok(allocation)
        } else {
            Err(AllocateError::ConflictingAddresses)
        }
    }
}

impl VirtualMemory {
    fn heap_containing(&mut self, address: u32) -> Option<&mut VirtualHeap> {
        [&mut self.small_pages, &mut self.large_pages]
            .into_iter()
            .find(|heap| heap.contains_address(address))
    }

    /// NtFreeVirtualMemory with MEM_RELEASE: frees the whole reservation that starts at `base`.
    /// Returns the freed range, whose contents the caller must clear for the next reservation.
    pub fn release(&mut self, base: u32) -> Result<Allocation, FreeError> {
        let heap = self.heap_containing(base).ok_or(FreeError::NotAllocated)?;
        let (start, size) = heap
            .reserved
            .containing(base)
            .ok_or(FreeError::NotAllocated)?;
        if start != base {
            return Err(FreeError::NotAtBase);
        }
        heap.reserved.remove(start);
        Ok(Allocation { base, size })
    }

    /// NtFreeVirtualMemory with MEM_DECOMMIT: the whole pages of `base..base + size` inside one
    /// reservation, which stays reserved. A size of 0 means the rest of the reservation.
    pub fn decommit(&mut self, base: u32, size: u32) -> Result<Allocation, FreeError> {
        let heap = self.heap_containing(base).ok_or(FreeError::NotAllocated)?;
        let (start, reserved_size) = heap
            .reserved
            .containing(base)
            .ok_or(FreeError::NotAllocated)?;
        let page_size = heap.page_size;
        let first = base - base % page_size;
        let reservation_end = u64::from(start) + u64::from(reserved_size);
        let end = if size == 0 {
            reservation_end
        } else {
            (u64::from(base) + u64::from(size)).next_multiple_of(u64::from(page_size))
        };
        if end > reservation_end {
            return Err(FreeError::NotAllocated);
        }
        Ok(Allocation {
            base: first,
            size: (end - u64::from(first)) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESERVE_LARGE: AllocationType =
        AllocationType(AllocationType::RESERVE | AllocationType::LARGE_PAGES);
    const COMMIT: AllocationType = AllocationType(AllocationType::COMMIT);
    const RESERVE: AllocationType = AllocationType(AllocationType::RESERVE);
    /// Any size that is not a whole number of pages.
    const ODD_SIZE: u32 = 0x1234;

    #[test]
    fn fresh_reservations_are_page_rounded_and_do_not_overlap() {
        let mut memory = VirtualMemory::new();
        let first = memory.allocate(0, ODD_SIZE, RESERVE_LARGE).unwrap();
        let second = memory.allocate(0, ODD_SIZE, RESERVE_LARGE).unwrap();
        assert_eq!(
            first,
            Allocation {
                base: LARGE_PAGE_RANGE.0,
                size: LARGE_PAGE_SIZE
            }
        );
        assert_eq!(second.base, first.base + LARGE_PAGE_SIZE);
    }

    #[test]
    fn page_size_selects_the_heap() {
        let mut memory = VirtualMemory::new();
        let small = memory.allocate(0, ODD_SIZE, RESERVE).unwrap();
        assert_eq!(small.base, SMALL_PAGE_RANGE.0);
        assert_eq!(small.size, ODD_SIZE.next_multiple_of(SMALL_PAGE_SIZE));
    }

    #[test]
    fn commit_inside_a_reservation_succeeds_and_outside_conflicts() {
        let mut memory = VirtualMemory::new();
        let reservation = memory
            .allocate(0, 2 * LARGE_PAGE_SIZE, RESERVE_LARGE)
            .unwrap();
        let inner = reservation.base + LARGE_PAGE_SIZE + ODD_SIZE;
        assert_eq!(
            memory.allocate(inner, ODD_SIZE, COMMIT),
            Ok(Allocation {
                base: reservation.base + LARGE_PAGE_SIZE,
                size: LARGE_PAGE_SIZE
            })
        );
        let past_end = reservation.base + reservation.size;
        assert_eq!(
            memory.allocate(past_end, ODD_SIZE, COMMIT),
            Err(AllocateError::ConflictingAddresses)
        );
    }

    #[test]
    fn reserving_an_overlapping_range_conflicts() {
        let mut memory = VirtualMemory::new();
        let reservation = memory.allocate(0, LARGE_PAGE_SIZE, RESERVE_LARGE).unwrap();
        assert_eq!(
            memory.allocate(reservation.base, LARGE_PAGE_SIZE, RESERVE_LARGE),
            Err(AllocateError::ConflictingAddresses)
        );
    }

    #[test]
    fn fresh_reservations_fill_gaps_left_by_fixed_ones() {
        let mut memory = VirtualMemory::new();
        let fixed_base = LARGE_PAGE_RANGE.0 + LARGE_PAGE_SIZE;
        memory
            .allocate(fixed_base, LARGE_PAGE_SIZE, RESERVE_LARGE)
            .unwrap();
        let gap = memory.allocate(0, LARGE_PAGE_SIZE, RESERVE_LARGE).unwrap();
        let after = memory.allocate(0, LARGE_PAGE_SIZE, RESERVE_LARGE).unwrap();
        assert_eq!(gap.base, LARGE_PAGE_RANGE.0);
        assert_eq!(after.base, fixed_base + LARGE_PAGE_SIZE);
    }

    #[test]
    fn released_reservations_can_be_reserved_again() {
        let mut memory = VirtualMemory::new();
        let reservation = memory.allocate(0, ODD_SIZE, RESERVE_LARGE).unwrap();
        assert_eq!(
            memory.release(reservation.base + 1),
            Err(FreeError::NotAtBase)
        );
        assert_eq!(memory.release(reservation.base), Ok(reservation));
        assert_eq!(
            memory.release(reservation.base),
            Err(FreeError::NotAllocated)
        );
        assert_eq!(memory.allocate(0, ODD_SIZE, RESERVE_LARGE), Ok(reservation));
    }

    #[test]
    fn decommit_covers_whole_pages_and_keeps_the_reservation() {
        let mut memory = VirtualMemory::new();
        let reservation = memory
            .allocate(0, 2 * LARGE_PAGE_SIZE, RESERVE_LARGE)
            .unwrap();
        assert_eq!(
            memory.decommit(reservation.base + ODD_SIZE, ODD_SIZE),
            Ok(Allocation {
                base: reservation.base,
                size: LARGE_PAGE_SIZE
            })
        );
        assert_eq!(memory.decommit(reservation.base, 0), Ok(reservation));
        assert_eq!(memory.release(reservation.base), Ok(reservation));
    }

    #[test]
    fn requests_without_reserve_or_commit_are_rejected() {
        let mut memory = VirtualMemory::new();
        let neither = AllocationType(AllocationType::LARGE_PAGES);
        assert_eq!(
            memory.allocate(0, LARGE_PAGE_SIZE, neither),
            Err(AllocateError::InvalidParameter)
        );
    }
}
