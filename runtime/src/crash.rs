//! Reports a host crash (a segmentation fault or bus error) before the process dies: the faulting
//! address, as a guest address when it falls in guest memory, and a host backtrace, whose frames
//! name the generated guest functions. Debuggers change thread timing enough to hide these
//! crashes, so the runtime reports them itself.
//!
//! Building a backtrace in a signal handler is not async-signal-safe; the process is dying
//! anyway, and a report that usually works beats none. After reporting, the default action is
//! restored and the signal raised again, so the exit status is unchanged.

use std::backtrace::Backtrace;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::memory::GUEST_ADDRESS_SPACE_SIZE;

/// The guest mapping's host base, recorded when the guest starts.
static GUEST_BASE: AtomicUsize = AtomicUsize::new(0);
const REPORTED_SIGNALS: [libc::c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];
/// The lookup table holds an 8-byte host function pointer per 4-byte guest instruction.
const LOOKUP_BYTES_PER_GUEST_BYTE: usize = 2;

unsafe extern "C" {
    // The lookup table's layout, from cpp/glue.cpp.
    static pure_lookup_table_offset: u64;
    static pure_code_base: u32;
    static pure_code_size: u32;
}

/// Installs the handler; `guest_base` lets it translate faulting addresses.
pub fn install(guest_base: *mut u8) {
    GUEST_BASE.store(guest_base as usize, Ordering::Relaxed);
    for signal in REPORTED_SIGNALS {
        // SAFETY: a zeroed sigaction with a valid handler and flags is a valid argument.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = report as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// The faulting instruction's address, from the interrupted context.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn instruction_pointer(context: *mut libc::c_void) -> usize {
    // SAFETY: SA_SIGINFO handlers receive the interrupted ucontext.
    unsafe {
        (*context.cast::<libc::ucontext_t>()).uc_mcontext.gregs[libc::REG_RIP as usize] as usize
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn instruction_pointer(_: *mut libc::c_void) -> usize {
    0
}

/// `symbol+offset` for a code address, so the instruction can be found in the binary.
fn symbolize(address: usize) -> String {
    // SAFETY: dladdr only reads the dynamic loader's tables; info is a plain out-parameter.
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if address == 0
            || libc::dladdr(address as *const libc::c_void, &mut info) == 0
            || info.dli_sname.is_null()
        {
            return format!("{address:#x}");
        }
        let name = std::ffi::CStr::from_ptr(info.dli_sname).to_string_lossy();
        format!("{name}+{:#x}", address - info.dli_saddr as usize)
    }
}

/// The guest address an indirect call was making, when `address` is where its unchecked lookup
/// lands outside the table: generated code reads the host function for `target` at
/// `table + (target - code base) * 2`, with 32-bit wrapping of the subtraction.
fn indirect_call_target(address: usize, base: usize) -> Option<u32> {
    // SAFETY: constants defined in cpp/glue.cpp and never written.
    let (table_offset, code_base, code_size) =
        unsafe { (pure_lookup_table_offset, pure_code_base, pure_code_size) };
    let offset = address.checked_sub(base + table_offset as usize)?;
    let table_size = code_size as usize * LOOKUP_BYTES_PER_GUEST_BYTE;
    let reach = GUEST_ADDRESS_SPACE_SIZE * LOOKUP_BYTES_PER_GUEST_BYTE;
    (table_size..reach)
        .contains(&offset)
        .then(|| code_base.wrapping_add((offset / LOOKUP_BYTES_PER_GUEST_BYTE) as u32))
}

extern "C" fn report(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for SA_SIGINFO handlers.
    let address = unsafe { (*info).si_addr() } as usize;
    let base = GUEST_BASE.load(Ordering::Relaxed);
    let guest = address
        .checked_sub(base)
        .filter(|offset| *offset < GUEST_ADDRESS_SPACE_SIZE)
        .map_or_else(
            || "outside guest memory".to_string(),
            |offset| format!("guest {offset:#010x}"),
        );
    let call = indirect_call_target(address, base).map_or_else(String::new, |target| {
        format!(", past the lookup table: an indirect call to {target:#010x}?")
    });
    let thread = std::thread::current();
    eprintln!(
        "host crash: signal {signal} on thread {:?} at host {address:#x} ({guest}{call}), instruction {}\n{}",
        thread.name().unwrap_or("unnamed"),
        symbolize(instruction_pointer(context)),
        Backtrace::force_capture()
    );
    // SAFETY: restoring the default action and re-raising ends the process as the signal would.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}
