//! The guest's 32-bit address space, mapped as one contiguous host region.
//!
//! Generated code addresses guest memory as `base + guest_address` with no bounds checks, so the
//! whole 4 GiB range is mapped up front. Pages are only backed by the OS once touched.

use std::io;
use std::ptr::NonNull;

/// Matches PPC_MEMORY_SIZE in XenonRecomp's ppc_context.h.
pub const GUEST_ADDRESS_SPACE_SIZE: usize = 1 << 32;

pub struct GuestMemory {
    base: NonNull<u8>,
}

// The region is plain memory shared by guest threads, as on the console.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    pub fn reserve() -> io::Result<Self> {
        let base = platform::map(GUEST_ADDRESS_SPACE_SIZE)?;
        Ok(Self { base })
    }

    pub fn base(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Guest memory is big-endian. Panics if the word would extend past the end of guest memory.
    pub fn write_u32(&self, address: u32, value: u32) {
        let bytes = value.to_be_bytes();
        assert!(
            address as usize + bytes.len() <= GUEST_ADDRESS_SPACE_SIZE,
            "guest write of {} bytes at {address:#010x} runs past the end of guest memory",
            bytes.len()
        );
        // SAFETY: the assert keeps the whole write inside the mapping.
        unsafe {
            let destination = self.base().add(address as usize);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
        }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        platform::unmap(self.base, GUEST_ADDRESS_SPACE_SIZE);
    }
}

/// A bump allocator for runtime-owned guest structures. Nothing is ever freed, and the mapping
/// starts zeroed, so allocations are zero-filled.
pub struct GuestHeap {
    next: u32,
    end: u32,
}

impl GuestHeap {
    pub const fn new(base: u32, size: u32) -> Self {
        Self {
            next: base,
            end: base + size,
        }
    }

    pub fn allocate(&mut self, size: u32, alignment: u32) -> Option<u32> {
        let start = self.next.checked_next_multiple_of(alignment)?;
        let end = start.checked_add(size)?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(start)
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::ptr::{self, NonNull};

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

    pub fn map(size: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: an anonymous mapping with no address hint has no preconditions.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                MAP_FLAGS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(NonNull::new(address.cast()).expect("mmap returned null"))
    }

    pub fn unmap(base: NonNull<u8>, size: usize) {
        // SAFETY: base and size describe the mapping created by map.
        unsafe { libc::munmap(base.as_ptr().cast(), size) };
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::ptr::{self, NonNull};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    pub fn map(size: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: a fresh allocation with no address hint has no preconditions.
        let address =
            unsafe { VirtualAlloc(ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        NonNull::new(address.cast()).ok_or_else(io::Error::last_os_error)
    }

    pub fn unmap(base: NonNull<u8>, _size: usize) {
        // SAFETY: base is the start of the allocation created by map; MEM_RELEASE requires size 0.
        unsafe { VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::{GuestHeap, GuestMemory};

    const HEAP_BASE: u32 = 0x7000_0000;
    const HEAP_SIZE: u32 = 0x1000;
    const ALIGNMENT: u32 = 0x100;
    /// The highest address a 4-byte word fits at.
    const LAST_WORD_ADDRESS: u32 = u32::MAX - 3;

    #[test]
    fn allocations_are_aligned_and_do_not_overlap() {
        let mut heap = GuestHeap::new(HEAP_BASE, HEAP_SIZE);
        let first = heap.allocate(1, ALIGNMENT).unwrap();
        let second = heap.allocate(1, ALIGNMENT).unwrap();
        assert_eq!(first, HEAP_BASE);
        assert_eq!(second, HEAP_BASE + ALIGNMENT);
    }

    #[test]
    fn allocation_past_the_end_fails() {
        let mut heap = GuestHeap::new(HEAP_BASE, HEAP_SIZE);
        assert!(heap.allocate(HEAP_SIZE, ALIGNMENT).is_some());
        assert!(heap.allocate(1, ALIGNMENT).is_none());
    }

    #[test]
    #[should_panic(expected = "runs past the end of guest memory")]
    fn word_write_straddling_the_end_of_guest_memory_panics() {
        let memory = GuestMemory::reserve().unwrap();
        memory.write_u32(LAST_WORD_ADDRESS + 1, 0);
    }
}
