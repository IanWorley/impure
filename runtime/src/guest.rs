//! Boots the recompiled game: verifies and maps your own default.xex, prepares the main thread's
//! guest structures, and runs the entry point on a dedicated host thread.

use std::cell::Cell;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::{fs, thread};

use bevy::log::info;
use sha2::{Digest, Sha256};

use crate::memory::{GuestHeap, GuestMemory};
use crate::xex_header;
use crate::{game_module, physical_memory};

/// SHA-256 of the base World disc default.xex (no title update) the generated code came from.
pub const EXPECTED_XEX_SHA256: &str =
    "83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c";

/// Runtime-owned guest structures live below the executable image (based at 0x82000000), and
/// end where the console's memory-mapped GPU registers (0x7FC80000) begin their reserved range.
const SYSTEM_HEAP_BASE: u32 = 0x7000_0000;
const SYSTEM_HEAP_SIZE: u32 = 0x0F00_0000;
const GUEST_PAGE_SIZE: u32 = 0x1000;

/// Space left above the initial stack pointer for the caller-frame slots a callee may write.
const STACK_TOP_RESERVE: u32 = 0x100;

/// The processor control region that r13 points to. Field offsets follow Xenia's X_KPCR.
const PCR_SIZE: u32 = GUEST_PAGE_SIZE;
const PCR_TLS_POINTER: u32 = 0x0;
const PCR_SELF_POINTER: u32 = 0x30;
const PCR_STACK_BASE: u32 = 0x70;
const PCR_STACK_LIMIT: u32 = 0x74;
const PCR_CURRENT_THREAD: u32 = 0x100;
/// The number of the hardware thread (0-5) the guest thread is running on.
const PCR_CURRENT_CPU: u32 = 0x10C;
const TLS_SIZE: u32 = GUEST_PAGE_SIZE;
const THREAD_OBJECT_SIZE: u32 = GUEST_PAGE_SIZE;
/// KTHREAD's thread ID (Xenia's X_KTHREAD layout). Titles read it inline: Direct3D compares
/// it with the device owner's to decide whether a thread may write commands itself.
const THREAD_OBJECT_THREAD_ID: u32 = 0x14C;

/// Every guest call is a host call, so deep guest recursion needs a deep host stack.
const HOST_STACK_SIZE: usize = 64 << 20;

struct Guest {
    memory: GuestMemory,
    heap: Mutex<GuestHeap>,
}

static GUEST: OnceLock<Guest> = OnceLock::new();
/// Guest address of the copy of the executable's XEX header.
static XEX_HEADER: OnceLock<u32> = OnceLock::new();

thread_local! {
    /// The guest thread object (KTHREAD) of the guest thread running on this host thread.
    static CURRENT_THREAD: Cell<u32> = const { Cell::new(0) };
}

/// The guest address space. Only available once the guest has started.
pub fn memory() -> &'static GuestMemory {
    &GUEST.get().expect("the guest has not started").memory
}

/// The guest address of the calling guest thread's thread object; kernel objects record it as
/// their owner.
pub fn current_thread() -> u32 {
    CURRENT_THREAD.get()
}

/// The guest address of the executable's XEX header. Only available once the guest has started.
pub fn xex_header() -> u32 {
    *XEX_HEADER.get().expect("the guest has not started")
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

    let disc_root = xex_path.parent().unwrap_or(Path::new("."));
    crate::kernel::mount_disc(disc_root);

    let (memory, unaliased) = GuestMemory::reserve_with_aliases(
        physical_memory::PHYSICAL_MEMORY_SIZE as usize,
        &physical_memory::aliases(),
    )
    .map_err(|error| format!("cannot map guest memory: {error}"))?;
    // The GPU reads every allocation through one view (see `physical_memory::guest_address`),
    // so the views must share their memory.
    if let Some(alias) = unaliased.first() {
        return Err(format!(
            "physical memory view at {:#010x} cannot be shared: this host cannot map memory at its offset",
            alias.guest_address
        ));
    }
    let heap = Mutex::new(GuestHeap::new(SYSTEM_HEAP_BASE, SYSTEM_HEAP_SIZE));
    GUEST
        .set(Guest { memory, heap })
        .map_err(|_| "the guest is already running".to_string())?;
    let guest = GUEST.get().unwrap();
    #[cfg(unix)]
    crate::crash::install(guest.memory.base());

    let module = game_module::get();
    // SAFETY: xex is the whole file, and base maps the entire guest address space.
    let entry = unsafe { (module.guest_load)(xex.as_ptr(), xex.len(), guest.memory.base()) };
    if entry == 0 {
        return Err("the executable does not match the generated code's image layout".to_string());
    }

    // SAFETY: a plain function with no arguments.
    let title_fixes = unsafe { (module.title_fix_count)() };
    info!("{title_fixes} title fixes in place (cpp/patches.cpp)");

    load_xex_header(guest, &xex)?;
    let main_thread = crate::kernel::start_main_thread(entry)?;
    info!(
        "entry point {entry:#010x}, stack top {:#010x}, PCR {:#010x}",
        main_thread.stack_top, main_thread.pcr
    );
    Ok(())
}

/// Copies the header into guest memory, where the console keeps it for the loader's lifetime.
fn load_xex_header(guest: &Guest, xex: &[u8]) -> Result<(), String> {
    let header = xex_header::header_bytes(xex)
        .ok_or("the executable is too short for the size its XEX header records")?;
    let size = u32::try_from(header.len()).map_err(|_| "the XEX header is too large")?;
    let address = guest
        .heap
        .lock()
        .unwrap()
        .allocate(size, GUEST_PAGE_SIZE)
        .ok_or("system heap exhausted")?;
    guest.memory.write_bytes(address, header);
    XEX_HEADER
        .set(address)
        .map_err(|_| "the XEX header is already loaded".to_string())
}

/// A guest thread's structures in guest memory.
#[derive(Clone, Copy, Debug)]
pub struct ThreadEnvironment {
    pub stack_top: u32,
    pub pcr: u32,
    /// The thread object (KTHREAD); the kernel initializes its dispatcher header.
    pub thread_object: u32,
}

/// Allocates a stack of `stack_size` bytes (rounded up to whole pages) and the structures r13
/// exposes to the C runtime.
pub fn prepare_thread(stack_size: u32) -> Result<ThreadEnvironment, String> {
    let guest = GUEST.get().ok_or("the guest has not started")?;
    let stack_size = stack_size
        .checked_next_multiple_of(GUEST_PAGE_SIZE)
        .ok_or("stack size too large")?;
    let mut heap = guest.heap.lock().unwrap();
    let mut allocate = |size| {
        heap.allocate(size, GUEST_PAGE_SIZE)
            .ok_or("system heap exhausted")
    };
    let stack_limit = allocate(stack_size)?;
    let stack_base = stack_limit + stack_size;
    let pcr = allocate(PCR_SIZE)?;
    let tls = allocate(TLS_SIZE)?;
    let thread_object = allocate(THREAD_OBJECT_SIZE)?;

    let memory = &guest.memory;
    memory.write_u32(pcr + PCR_TLS_POINTER, tls);
    memory.write_u32(pcr + PCR_SELF_POINTER, pcr);
    memory.write_u32(pcr + PCR_STACK_BASE, stack_base);
    memory.write_u32(pcr + PCR_STACK_LIMIT, stack_limit);
    memory.write_u32(pcr + PCR_CURRENT_THREAD, thread_object);
    // The thread object's address doubles as the thread ID, as ExCreateThread reports it.
    memory.write_u32(thread_object + THREAD_OBJECT_THREAD_ID, thread_object);
    Ok(ThreadEnvironment {
        stack_top: stack_base - STACK_TOP_RESERVE,
        pcr,
        thread_object,
    })
}

/// Records which of the console's six hardware threads `environment` is running on; code
/// that runs per CPU, such as interrupt handlers, reads it from the PCR.
pub fn set_current_cpu(environment: ThreadEnvironment, cpu: u8) {
    memory().write_u8(environment.pcr + PCR_CURRENT_CPU, cpu);
}

/// Starts a host thread with a stack deep enough to run guest code on.
pub fn spawn(name: String, body: impl FnOnce() + Send + 'static) -> Result<(), String> {
    thread::Builder::new()
        .name(name)
        .stack_size(HOST_STACK_SIZE)
        .spawn(body)
        .map(drop)
        .map_err(|error| format!("cannot start a guest thread: {error}"))
}

/// Runs `function(arguments)` to completion on the calling host thread (which must come from
/// `spawn`), as the guest thread `environment`, and returns its result.
pub fn call(environment: ThreadEnvironment, function: u32, arguments: [u32; 2]) -> u32 {
    CURRENT_THREAD.set(environment.thread_object);
    let [argument0, argument1] = arguments;
    // SAFETY: the image is loaded and the function table filled by pure_guest_load, and the
    // mapping lives for the rest of the process.
    unsafe {
        (game_module::get().guest_run)(
            memory().base(),
            function,
            environment.stack_top,
            environment.pcr,
            argument0,
            argument1,
        )
    }
}
