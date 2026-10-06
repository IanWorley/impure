//! Boots the recompiled game: verifies and maps your own default.xex, prepares the main thread's
//! guest structures, and runs the entry point on a dedicated host thread.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::{fs, thread};

use bevy::log::info;
use sha2::{Digest, Sha256};

use crate::memory::{GuestHeap, GuestMemory};
use crate::status;

/// SHA-256 of the base World disc default.xex (no title update) the generated code came from.
const EXPECTED_XEX_SHA256: &str =
    "83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c";

/// Runtime-owned guest structures live below the executable image (based at 0x82000000).
const SYSTEM_HEAP_BASE: u32 = 0x7000_0000;
const SYSTEM_HEAP_SIZE: u32 = 0x1000_0000;
const GUEST_PAGE_SIZE: u32 = 0x1000;

const MAIN_THREAD_STACK_SIZE: u32 = 0x10_0000;
/// Space left above the initial stack pointer for the caller-frame slots a callee may write.
const STACK_TOP_RESERVE: u32 = 0x100;

/// The processor control region that r13 points to. Field offsets follow Xenia's X_KPCR.
const PCR_SIZE: u32 = GUEST_PAGE_SIZE;
const PCR_TLS_POINTER: u32 = 0x0;
const PCR_SELF_POINTER: u32 = 0x30;
const PCR_STACK_BASE: u32 = 0x70;
const PCR_STACK_LIMIT: u32 = 0x74;
const PCR_CURRENT_THREAD: u32 = 0x100;
const TLS_SIZE: u32 = GUEST_PAGE_SIZE;
const THREAD_OBJECT_SIZE: u32 = GUEST_PAGE_SIZE;

/// Every guest call is a host call, so deep guest recursion needs a deep host stack.
const HOST_STACK_SIZE: usize = 64 << 20;
const MAIN_THREAD_NAME: &str = "guest-main";

struct Guest {
    memory: GuestMemory,
    heap: Mutex<GuestHeap>,
}

static GUEST: OnceLock<Guest> = OnceLock::new();

unsafe extern "C" {
    fn pure_guest_load(xex: *const u8, xex_size: usize, base: *mut u8) -> u32;
    fn pure_guest_run(base: *mut u8, function: u32, stack_top: u32, pcr: u32);
}

/// Allocates zero-filled guest memory owned by the runtime.
pub fn allocate(size: u32, alignment: u32) -> Option<u32> {
    GUEST.get()?.heap.lock().unwrap().allocate(size, alignment)
}

pub fn start(xex_path: &Path) -> Result<(), String> {
    let xex = fs::read(xex_path)
        .map_err(|error| format!("cannot read {}: {error}", xex_path.display()))?;
    let hash = format!("{:x}", Sha256::digest(&xex));
    if hash != EXPECTED_XEX_SHA256 {
        return Err(format!(
            "{} has SHA-256 {hash}; the generated code was produced from {EXPECTED_XEX_SHA256}",
            xex_path.display()
        ));
    }

    let memory =
        GuestMemory::reserve().map_err(|error| format!("cannot map guest memory: {error}"))?;
    let heap = Mutex::new(GuestHeap::new(SYSTEM_HEAP_BASE, SYSTEM_HEAP_SIZE));
    GUEST
        .set(Guest { memory, heap })
        .map_err(|_| "the guest is already running".to_string())?;
    let guest = GUEST.get().unwrap();

    // SAFETY: xex is the whole file, and base maps the entire guest address space.
    let entry = unsafe { pure_guest_load(xex.as_ptr(), xex.len(), guest.memory.base()) };
    if entry == 0 {
        return Err("the executable does not match the generated code's image layout".to_string());
    }

    let (stack_top, pcr) = prepare_main_thread(guest)?;
    info!("entry point {entry:#010x}, stack top {stack_top:#010x}, PCR {pcr:#010x}");

    // Raw pointers are not Send; the mapping lives for the rest of the process.
    let base = guest.memory.base() as usize;
    thread::Builder::new()
        .name(MAIN_THREAD_NAME.to_string())
        .stack_size(HOST_STACK_SIZE)
        .spawn(move || {
            // SAFETY: the image is loaded and the function table filled by pure_guest_load.
            unsafe { pure_guest_run(base as *mut u8, entry, stack_top, pcr) };
            status::report_stop(format!("entry point {entry:#010x} returned"));
        })
        .map_err(|error| format!("cannot start the guest thread: {error}"))?;
    Ok(())
}

/// Allocates the main thread's stack and the structures r13 exposes to the C runtime.
fn prepare_main_thread(guest: &Guest) -> Result<(u32, u32), String> {
    let mut heap = guest.heap.lock().unwrap();
    let mut allocate = |size| {
        heap.allocate(size, GUEST_PAGE_SIZE)
            .ok_or("system heap exhausted")
    };
    let stack_limit = allocate(MAIN_THREAD_STACK_SIZE)?;
    let stack_base = stack_limit + MAIN_THREAD_STACK_SIZE;
    let pcr = allocate(PCR_SIZE)?;
    let tls = allocate(TLS_SIZE)?;
    let thread_object = allocate(THREAD_OBJECT_SIZE)?;

    let memory = &guest.memory;
    memory.write_u32(pcr + PCR_TLS_POINTER, tls);
    memory.write_u32(pcr + PCR_SELF_POINTER, pcr);
    memory.write_u32(pcr + PCR_STACK_BASE, stack_base);
    memory.write_u32(pcr + PCR_STACK_LIMIT, stack_limit);
    memory.write_u32(pcr + PCR_CURRENT_THREAD, thread_object);
    Ok((stack_base - STACK_TOP_RESERVE, pcr))
}
