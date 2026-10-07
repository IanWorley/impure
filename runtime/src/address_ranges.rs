//! Bookkeeping for allocated address ranges, shared by the virtual and physical allocators.
//! Ranges are tracked in 64-bit arithmetic so a range may end exactly at 4 GiB.

use std::collections::BTreeMap;

pub struct AddressRanges {
    /// Allocated ranges, keyed by base address, valued by size.
    ranges: BTreeMap<u32, u32>,
}

impl AddressRanges {
    pub const fn new() -> Self {
        Self {
            ranges: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, base: u32, size: u32) {
        self.ranges.insert(base, size);
    }

    /// Frees the range starting at `base`, returning its size.
    pub fn remove(&mut self, base: u32) -> Option<u32> {
        self.ranges.remove(&base)
    }

    /// The allocated range containing `address`, as (base, size).
    pub fn containing(&self, address: u32) -> Option<(u32, u32)> {
        let (&base, &size) = self.ranges.range(..=address).next_back()?;
        (u64::from(address) < range_end(base, size)).then_some((base, size))
    }

    /// Whether one allocated range covers all of `start..end`.
    pub fn covers(&self, start: u32, end: u64) -> bool {
        self.ranges
            .range(..=start)
            .next_back()
            .is_some_and(|(&base, &size)| end <= range_end(base, size))
    }

    /// Allocated ranges never overlap each other, so only the last one starting before `end`
    /// can reach into `start..end`.
    pub fn overlaps(&self, start: u32, end: u64) -> bool {
        let Some(last) = end.checked_sub(1) else {
            return false;
        };
        self.ranges
            .range(..=last.min(u64::from(u32::MAX)) as u32)
            .next_back()
            .is_some_and(|(&base, &size)| range_end(base, size) > u64::from(start))
    }

    /// The lowest `alignment`-aligned base in `low..high` with `size` free bytes after it.
    pub fn first_fit(&self, low: u32, high: u64, size: u32, alignment: u32) -> Option<u32> {
        let align = |address: u64| address.next_multiple_of(u64::from(alignment));
        let mut candidate = align(u64::from(low));
        for (&base, &range_size) in self.ranges.range(..) {
            if candidate + u64::from(size) <= u64::from(base) {
                break;
            }
            candidate = candidate.max(align(range_end(base, range_size)));
        }
        (candidate + u64::from(size) <= high).then_some(candidate as u32)
    }
}

fn range_end(base: u32, size: u32) -> u64 {
    u64::from(base) + u64::from(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: u32 = 0x1000;
    const HIGH: u64 = 0x10_0000;
    const SIZE: u32 = 0x1000;
    const ALIGNMENT: u32 = 0x4000;

    #[test]
    fn first_fit_skips_allocated_ranges_and_honors_alignment() {
        let mut ranges = AddressRanges::new();
        let first = ranges.first_fit(LOW, HIGH, SIZE, ALIGNMENT).unwrap();
        assert_eq!(first, ALIGNMENT);
        ranges.insert(first, SIZE);
        assert_eq!(
            ranges.first_fit(LOW, HIGH, SIZE, ALIGNMENT),
            Some(2 * ALIGNMENT)
        );
        assert_eq!(
            ranges.first_fit(LOW, u64::from(first) + u64::from(SIZE), SIZE, ALIGNMENT),
            None
        );
    }

    #[test]
    fn covers_and_overlaps_compare_whole_ranges() {
        let mut ranges = AddressRanges::new();
        ranges.insert(LOW, SIZE);
        let end = u64::from(LOW + SIZE);
        assert!(ranges.covers(LOW, end));
        assert!(!ranges.covers(LOW, end + 1));
        assert!(ranges.overlaps(LOW + SIZE - 1, end + 1));
        assert!(!ranges.overlaps(LOW + SIZE, end + 1));
    }
}
