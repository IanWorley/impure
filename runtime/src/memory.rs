//! The guest's 32-bit address space, mapped as one contiguous host region.
//!
//! Generated code addresses guest memory as `base + guest_address` with no bounds checks, so the
//! whole 4 GiB range is mapped up front. Pages are only backed by the OS once touched.
//!
//! Some ranges can show one shared block of memory, so that several guest addresses read and
//! write the same bytes, as the console's physical memory views do. A range is only shared if
//! the host can map memory at its address and offset; the host maps shared memory in pages (4 KiB
//! on most Linux and Intel hosts, 16 KiB on Apple silicon) or, on Windows, 64 KiB units.
//!
//! One view can never be shared that way: the console's 4 KiB-page view of physical memory at
//! 0xE0000000 starts one 4 KiB page into it. So accesses there are redirected instead, to the
//! 64 KiB-page view's address for the same physical byte, by the generated code and by this
//! module alike (see `host_address`).

use std::io;

/// Matches PPC_MEMORY_SIZE in XenonRecomp's ppc_context.h.
pub const GUEST_ADDRESS_SPACE_SIZE: usize = 1 << 32;

/// Guest addresses from here up are the 4 KiB-page view of physical memory...
const REDIRECTED_VIEW_BASE: u32 = 0xE000_0000;
/// ...whose first byte is physical address 0x1000, at this address in the 64 KiB-page view.
const REDIRECTED_VIEW_TARGET: u32 = 0xA000_1000;

/// Where an access to `address` lands in the host mapping. Matches PPC_MEMORY_ADDRESS in
/// XenonRecomp's ppc_context.h (as tools/patches/xenonrecomp.patch defines it).
pub const fn host_address(address: u32) -> u32 {
    if address >= REDIRECTED_VIEW_BASE {
        address - (REDIRECTED_VIEW_BASE - REDIRECTED_VIEW_TARGET)
    } else {
        address
    }
}

/// A guest range that shows the shared block, starting `shared_offset` bytes into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alias {
    pub guest_address: u32,
    pub shared_offset: usize,
    pub length: usize,
}

pub struct GuestMemory {
    mapping: platform::Mapping,
}

// The region is plain memory shared by guest threads, as on the console.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Guest memory with no shared ranges.
    #[cfg(test)]
    pub fn reserve() -> io::Result<Self> {
        Ok(Self {
            mapping: platform::map(GUEST_ADDRESS_SPACE_SIZE, 0, &[])?,
        })
    }

    /// Reserves guest memory in which each of `aliases` shows a block of `shared_size` bytes.
    /// Returns the memory and the aliases the host could not map at their address and offset;
    /// those ranges are ordinary private memory.
    pub fn reserve_with_aliases(
        shared_size: usize,
        aliases: &[Alias],
    ) -> io::Result<(Self, Vec<Alias>)> {
        let granularity = platform::granularity();
        let (mappable, unmappable): (Vec<Alias>, Vec<Alias>) = aliases.iter().partition(|alias| {
            (alias.guest_address as usize).is_multiple_of(granularity)
                && alias.shared_offset.is_multiple_of(granularity)
                && alias.length.is_multiple_of(granularity)
        });
        let mapping = platform::map(GUEST_ADDRESS_SPACE_SIZE, shared_size, &mappable)?;
        Ok((Self { mapping }, unmappable))
    }

    pub fn base(&self) -> *mut u8 {
        self.mapping.base.as_ptr()
    }

    /// Panics if the address is past the end of guest memory.
    pub fn read_u8(&self, address: u32) -> u8 {
        let source = self.host_pointer(address, size_of::<u8>());
        // SAFETY: host_pointer checked that the byte is inside the mapping.
        unsafe { source.read() }
    }

    /// Guest memory is big-endian. Panics if the word would extend past the end of guest memory.
    pub fn read_u32(&self, address: u32) -> u32 {
        let mut bytes = [0; size_of::<u32>()];
        let source = self.host_pointer(address, bytes.len());
        // SAFETY: host_pointer checked that the whole read is inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(source, bytes.as_mut_ptr(), bytes.len()) };
        u32::from_be_bytes(bytes)
    }

    /// A big-endian LARGE_INTEGER. Panics if it would extend past the end of guest memory.
    pub fn read_i64(&self, address: u32) -> i64 {
        let high = u64::from(self.read_u32(address));
        let low = u64::from(self.read_u32(address + size_of::<u32>() as u32));
        ((high << u32::BITS) | low) as i64
    }

    /// Guest memory is big-endian. Panics if the value would extend past the end of guest memory.
    pub fn write_u64(&self, address: u32, value: u64) {
        self.write_bytes(address, &value.to_be_bytes());
    }

    /// Guest memory is big-endian. Panics if the word would extend past the end of guest memory.
    pub fn write_u32(&self, address: u32, value: u32) {
        self.write_bytes(address, &value.to_be_bytes());
    }

    /// Panics if the bytes would extend past the end of guest memory.
    pub fn read_bytes(&self, address: u32, destination: &mut [u8]) {
        let source = self.host_pointer(address, destination.len());
        // SAFETY: host_pointer checked that the whole read is inside the mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(source, destination.as_mut_ptr(), destination.len())
        };
    }

    /// Panics if the bytes would extend past the end of guest memory.
    pub fn write_bytes(&self, address: u32, bytes: &[u8]) {
        let destination = self.host_pointer(address, bytes.len());
        // SAFETY: host_pointer checked that the whole write is inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };
    }

    /// Guest memory is big-endian. Panics if the value would extend past the end of guest memory.
    pub fn read_u16(&self, address: u32) -> u16 {
        let mut bytes = [0; size_of::<u16>()];
        let source = self.host_pointer(address, bytes.len());
        // SAFETY: host_pointer checked that the whole read is inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(source, bytes.as_mut_ptr(), bytes.len()) };
        u16::from_be_bytes(bytes)
    }

    /// Guest memory is big-endian. Panics if the value would extend past the end of guest memory.
    pub fn write_u16(&self, address: u32, value: u16) {
        self.write_bytes(address, &value.to_be_bytes());
    }

    /// Clears `length` bytes. Panics if the range would extend past the end of guest memory.
    pub fn zero(&self, address: u32, length: u32) {
        let destination = self.host_pointer(address, length as usize);
        // SAFETY: host_pointer checked that the whole range is inside the mapping.
        unsafe { destination.write_bytes(0, length as usize) };
    }

    /// Panics if the address is past the end of guest memory.
    pub fn write_u8(&self, address: u32, value: u8) {
        let destination = self.host_pointer(address, size_of::<u8>());
        // SAFETY: host_pointer checked that the byte is inside the mapping.
        unsafe { destination.write(value) };
    }

    /// Atomically replaces the big-endian word at `address` with `new` if it holds `current`;
    /// returns the word it held. Panics if the address is not 4-byte aligned.
    pub fn compare_exchange_u32(&self, address: u32, current: u32, new: u32) -> Result<u32, u32> {
        assert!(
            address.is_multiple_of(size_of::<u32>() as u32),
            "atomic guest access at unaligned address {address:#010x}"
        );
        let pointer = self.host_pointer(address, size_of::<u32>()).cast::<u32>();
        // SAFETY: the word is inside the mapping and aligned (the mapping is page-aligned), and
        // every concurrent runtime access to it goes through this atomic.
        let word = unsafe { std::sync::atomic::AtomicU32::from_ptr(pointer) };
        word.compare_exchange(
            current.to_be(),
            new.to_be(),
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .map(u32::from_be)
        .map_err(u32::from_be)
    }

    /// Panics if the range would extend past the end of guest memory.
    fn host_pointer(&self, address: u32, length: usize) -> *mut u8 {
        assert!(
            address as usize + length <= GUEST_ADDRESS_SPACE_SIZE,
            "guest access of {length} bytes at {address:#010x} runs past the end of guest memory"
        );
        // SAFETY: the range is inside the mapping, and redirecting moves it lower.
        unsafe { self.base().add(host_address(address) as usize) }
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
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr::{self, NonNull};

    use super::Alias;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    const READ_WRITE: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;

    pub struct Mapping {
        pub base: NonNull<u8>,
        size: usize,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: base and size describe the mapping created by map; the shared ranges inside
            // it go with it.
            unsafe { libc::munmap(self.base.as_ptr().cast(), self.size) };
        }
    }

    pub fn granularity() -> usize {
        // SAFETY: sysconf has no preconditions.
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    pub fn map(size: usize, shared_size: usize, aliases: &[Alias]) -> io::Result<Mapping> {
        // SAFETY: an anonymous mapping with no address hint has no preconditions.
        let address = unsafe { libc::mmap(ptr::null_mut(), size, READ_WRITE, MAP_FLAGS, -1, 0) };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mapping = Mapping {
            base: NonNull::new(address.cast()).expect("mmap returned null"),
            size,
        };
        if aliases.is_empty() {
            return Ok(mapping);
        }
        // The descriptor can close once the shared ranges are mapped; they keep the memory alive.
        let shared = shared_memory(shared_size)?;
        for alias in aliases {
            // SAFETY: the alias lies inside the mapping (the caller checked its alignment), and
            // MAP_FIXED replaces that part of it.
            let mapped = unsafe {
                libc::mmap(
                    mapping
                        .base
                        .as_ptr()
                        .add(alias.guest_address as usize)
                        .cast(),
                    alias.length,
                    READ_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    shared.as_raw_fd(),
                    alias.shared_offset as libc::off_t,
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(mapping)
    }

    /// A zero-filled anonymous shared memory object of `size` bytes.
    fn shared_memory(size: usize) -> io::Result<OwnedFd> {
        let descriptor = create_shared_memory()?;
        // SAFETY: the descriptor is valid and owned.
        if unsafe { libc::ftruncate(descriptor.as_raw_fd(), size as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn create_shared_memory() -> io::Result<OwnedFd> {
        // SAFETY: the name is a valid C string.
        let descriptor =
            unsafe { libc::memfd_create(c"pure-physical".as_ptr(), libc::MFD_CLOEXEC) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned a new descriptor this function owns.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    /// Without memfd, a POSIX shared memory object that is unlinked as soon as it is opened.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn create_shared_memory() -> io::Result<OwnedFd> {
        const OWNER_READ_WRITE: libc::c_uint = 0o600;
        let name = std::ffi::CString::new(format!("/pure-physical-{}", std::process::id()))
            .expect("the name has no NUL");
        // SAFETY: the name is a valid C string; O_EXCL keeps another process's object out.
        let descriptor = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                OWNER_READ_WRITE,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; unlinking leaves the open object alive.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        // SAFETY: shm_open returned a new descriptor this function owns.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::ptr::{self, NonNull};

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
        MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc,
        VirtualAlloc2, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::Alias;

    /// Views of a section and placeholder splits both work in allocation-granularity units.
    const ALLOCATION_GRANULARITY: usize = 0x1_0000;

    pub struct Mapping {
        pub base: NonNull<u8>,
        /// Each piece's offset into the mapping, and whether it is a view of the shared section.
        pieces: Vec<(usize, bool)>,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            for &(offset, is_view) in &self.pieces {
                // SAFETY: each piece was mapped or allocated by map at this address.
                unsafe {
                    let address = self.base.as_ptr().add(offset).cast();
                    if is_view {
                        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: address });
                    } else {
                        VirtualFree(address, 0, MEM_RELEASE);
                    }
                }
            }
        }
    }

    pub fn granularity() -> usize {
        ALLOCATION_GRANULARITY
    }

    pub fn map(size: usize, shared_size: usize, aliases: &[Alias]) -> io::Result<Mapping> {
        if aliases.is_empty() {
            // SAFETY: a fresh allocation with no address hint has no preconditions.
            let address = unsafe {
                VirtualAlloc(ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
            };
            let base = NonNull::new(address.cast()).ok_or_else(io::Error::last_os_error)?;
            return Ok(Mapping {
                base,
                pieces: vec![(0, false)],
            });
        }

        // Reserve the whole range as a placeholder, split it at every alias boundary, then
        // replace each piece with a view of the shared section or with committed private memory.
        // SAFETY: a placeholder reservation with no address hint has no preconditions.
        let process = unsafe { GetCurrentProcess() };
        let address = unsafe {
            VirtualAlloc2(
                process,
                ptr::null(),
                size,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                ptr::null_mut(),
                0,
            )
        };
        let base = NonNull::new(address.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;

        let mut sorted = aliases.to_vec();
        sorted.sort_by_key(|alias| alias.guest_address);
        let mut pieces: Vec<(usize, usize, Option<Alias>)> = Vec::new();
        let mut cursor = 0;
        for alias in sorted {
            let start = alias.guest_address as usize;
            if start > cursor {
                pieces.push((cursor, start - cursor, None));
            }
            pieces.push((start, alias.length, Some(alias)));
            cursor = start + alias.length;
        }
        if cursor < size {
            pieces.push((cursor, size - cursor, None));
        }
        // Splitting carves each piece off the front of the remaining placeholder.
        for &(offset, length, _) in &pieces[..pieces.len() - 1] {
            // SAFETY: the piece lies inside the placeholder reserved above.
            let split = unsafe {
                VirtualFree(
                    base.as_ptr().add(offset).cast(),
                    length,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )
            };
            if split == 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // SAFETY: a pagefile-backed section has no preconditions.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                (shared_size as u64 >> u32::BITS) as u32,
                shared_size as u32,
                ptr::null(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut mapping = Mapping {
            base,
            pieces: Vec::new(),
        };
        for (offset, length, alias) in pieces {
            // SAFETY: each piece is a placeholder of exactly this size.
            let replaced = unsafe {
                let address = base.as_ptr().add(offset).cast();
                match alias {
                    Some(alias) => {
                        MapViewOfFile3(
                            section,
                            process,
                            address,
                            alias.shared_offset as u64,
                            length,
                            MEM_REPLACE_PLACEHOLDER,
                            PAGE_READWRITE,
                            ptr::null_mut(),
                            0,
                        )
                        .Value
                    }
                    None => VirtualAlloc2(
                        process,
                        address,
                        length,
                        MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        ptr::null_mut(),
                        0,
                    ),
                }
            };
            if replaced.is_null() {
                // SAFETY: the section handle is valid.
                unsafe { CloseHandle(section) };
                return Err(io::Error::last_os_error());
            }
            mapping.pieces.push((offset, alias.is_some()));
        }
        // The views keep the section alive.
        // SAFETY: the section handle is valid.
        unsafe { CloseHandle(section) };
        Ok(mapping)
    }
}

#[cfg(test)]
mod tests {
    use super::{Alias, GuestHeap, GuestMemory, REDIRECTED_VIEW_BASE, REDIRECTED_VIEW_TARGET};

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

    /// Guest memory is big-endian: the most significant byte is at the lowest address.
    #[test]
    fn words_round_trip_big_endian() {
        const ADDRESS: u32 = 0x1000;
        const VALUE: u32 = 0x1122_3344;
        let memory = GuestMemory::reserve().unwrap();
        memory.write_u32(ADDRESS, VALUE);
        assert_eq!(memory.read_u32(ADDRESS), VALUE);
        // SAFETY: ADDRESS is inside the mapping.
        assert_eq!(
            unsafe { *memory.base().add(ADDRESS as usize) },
            VALUE.to_be_bytes()[0]
        );
    }

    #[test]
    fn the_redirected_view_shows_the_bytes_at_its_target() {
        const OFFSET: u32 = 0x40;
        const VALUE: u32 = 0x1234_5678;
        let memory = GuestMemory::reserve().unwrap();
        memory.write_u32(REDIRECTED_VIEW_BASE + OFFSET, VALUE);
        assert_eq!(memory.read_u32(REDIRECTED_VIEW_TARGET + OFFSET), VALUE);
    }

    #[test]
    fn aliased_ranges_show_the_same_bytes() {
        const SHARED_SIZE: usize = 0x10_0000;
        const FIRST: u32 = 0xA000_0000;
        const SECOND: u32 = 0xC000_0000;
        const OFFSET: u32 = 0x40;
        const VALUE: u32 = 0x1234_5678;
        let alias = |guest_address| Alias {
            guest_address,
            shared_offset: 0,
            length: SHARED_SIZE,
        };
        let (memory, unmapped) =
            GuestMemory::reserve_with_aliases(SHARED_SIZE, &[alias(FIRST), alias(SECOND)]).unwrap();
        assert!(unmapped.is_empty());
        memory.write_u32(FIRST + OFFSET, VALUE);
        assert_eq!(memory.read_u32(SECOND + OFFSET), VALUE);
    }

    #[test]
    #[should_panic(expected = "runs past the end of guest memory")]
    fn word_write_straddling_the_end_of_guest_memory_panics() {
        let memory = GuestMemory::reserve().unwrap();
        memory.write_u32(LAST_WORD_ADDRESS + 1, 0);
    }
}
