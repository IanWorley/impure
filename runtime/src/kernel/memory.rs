//! Memory manager imports: the title's virtual address space and physical memory.

use std::sync::Mutex;

use bevy::log::{debug, warn};

use super::{ImportCall, NO_RETURN_VALUE, NtStatus};
use crate::physical_memory::{self, PhysicalMemory, Protection};
use crate::status;
use crate::virtual_memory::{AllocateError, AllocationType, FreeError, VirtualMemory};

static VIRTUAL_MEMORY: Mutex<VirtualMemory> = Mutex::new(VirtualMemory::new());
static PHYSICAL_MEMORY: Mutex<PhysicalMemory> = Mutex::new(PhysicalMemory::new());
/// MmAllocatePhysicalMemory's result when nothing fits.
const ALLOCATION_FAILED: u32 = 0;
/// MmAllocatePhysicalMemory accepts any physical address and the default alignment.
const ANY_PHYSICAL_ADDRESS: (u32, u32) = (0, u32::MAX);
const DEFAULT_ALIGNMENT: u32 = 0;

/// NtAllocateVirtualMemory(base in/out, size in/out, allocation type, protection, debug memory).
/// Protection is ignored: every guest page is readable and writable.
pub(super) fn nt_allocate_virtual_memory(call: &ImportCall) -> u32 {
    let (base_pointer, size_pointer) = (call.argument(0), call.argument(1));
    let kind = AllocationType(call.argument(2));
    let requested_base = call.memory.read_u32(base_pointer);
    let size = call.memory.read_u32(size_pointer);

    let result = VIRTUAL_MEMORY
        .lock()
        .unwrap()
        .allocate(requested_base, size, kind);
    let status = match result {
        Ok(allocation) => {
            call.memory.write_u32(base_pointer, allocation.base);
            call.memory.write_u32(size_pointer, allocation.size);
            NtStatus::SUCCESS
        }
        Err(AllocateError::InvalidParameter) => NtStatus::INVALID_PARAMETER,
        Err(AllocateError::ConflictingAddresses) => NtStatus::CONFLICTING_ADDRESSES,
        Err(AllocateError::NoMemory) => NtStatus::NO_MEMORY,
        Err(AllocateError::Unsupported(what)) => status::halt_guest_thread(format!(
            "NtAllocateVirtualMemory asked for {what} (type {:#x}), which the runtime does not support",
            kind.0
        )),
    };
    status.0
}

/// NtFreeVirtualMemory free types.
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;

/// NtFreeVirtualMemory(base in/out, size in/out, free type, debug memory) -> NTSTATUS. Freed
/// pages are cleared, since fresh allocations rely on guest memory starting zeroed.
pub(super) fn nt_free_virtual_memory(call: &ImportCall) -> u32 {
    let (base_pointer, size_pointer, free_type) =
        (call.argument(0), call.argument(1), call.argument(2));
    let base = call.memory.read_u32(base_pointer);
    let size = call.memory.read_u32(size_pointer);
    let mut virtual_memory = VIRTUAL_MEMORY.lock().unwrap();
    let result = match free_type {
        MEM_RELEASE if size == 0 => virtual_memory.release(base),
        MEM_DECOMMIT => virtual_memory.decommit(base, size),
        _ => return NtStatus::INVALID_PARAMETER.0,
    };
    drop(virtual_memory);
    match result {
        Ok(freed) => {
            call.memory.zero(freed.base, freed.size);
            call.memory.write_u32(base_pointer, freed.base);
            call.memory.write_u32(size_pointer, freed.size);
            NtStatus::SUCCESS.0
        }
        Err(FreeError::NotAtBase) => NtStatus::FREE_VM_NOT_AT_BASE.0,
        Err(FreeError::NotAllocated) => NtStatus::MEMORY_NOT_ALLOCATED.0,
    }
}

/// MmAllocatePhysicalMemoryEx(flags, size, protection, lowest, highest, alignment) -> address.
pub(super) fn mm_allocate_physical_memory_ex(call: &ImportCall) -> u32 {
    let (size, protection) = (call.argument(1), Protection(call.argument(2)));
    let (lowest, highest, alignment) = (call.argument(3), call.argument(4), call.argument(5));
    allocate_physical(size, alignment, lowest, highest, protection)
}

/// MmAllocatePhysicalMemory(flags, size, protection) -> address.
pub(super) fn mm_allocate_physical_memory(call: &ImportCall) -> u32 {
    let (size, protection) = (call.argument(1), Protection(call.argument(2)));
    let (lowest, highest) = ANY_PHYSICAL_ADDRESS;
    allocate_physical(size, DEFAULT_ALIGNMENT, lowest, highest, protection)
}

pub(super) fn allocate_physical(
    size: u32,
    alignment: u32,
    lowest: u32,
    highest: u32,
    protection: Protection,
) -> u32 {
    PHYSICAL_MEMORY
        .lock()
        .unwrap()
        .allocate(size, alignment, lowest, highest, protection)
        .unwrap_or(ALLOCATION_FAILED)
}

/// MmFreePhysicalMemory(type, address). Freeing an address that was not allocated is the
/// title's bug; it is logged rather than fatal, as the console does not check either.
pub(super) fn mm_free_physical_memory(call: &ImportCall) -> u32 {
    free_physical(call.argument(1));
    NO_RETURN_VALUE
}

pub(super) fn free_physical(address: u32) {
    if !PHYSICAL_MEMORY.lock().unwrap().free(address) {
        warn!(target: "memory", "freeing {address:#010x}, which is not a physical allocation");
    }
}

/// MmGetPhysicalAddress(address) -> physical address.
pub(super) fn mm_get_physical_address(call: &ImportCall) -> u32 {
    physical_memory::physical_address(call.argument(0))
}

/// The address the title uses for a physical address it allocated; see
/// `PhysicalMemory::view_address`.
pub(super) fn view_address(physical: u32) -> Option<u32> {
    PHYSICAL_MEMORY.lock().unwrap().view_address(physical)
}

/// Page protection flags.
const PAGE_READWRITE: u32 = 0x4;

/// MmQueryAddressProtect(address) -> protection flags. Protection is not modelled: every guest
/// page is readable and writable.
pub(super) fn mm_query_address_protect(call: &ImportCall) -> u32 {
    debug!(target: "memory", "protection of {:#010x} queried", call.argument(0));
    PAGE_READWRITE
}
