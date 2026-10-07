//! Guest threads: the main thread, ExCreateThread, and suspension. Each guest thread runs on
//! its own host thread; its thread object is a dispatcher object that is signaled on exit, so
//! waits on thread handles go through the same dispatcher as every other wait.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Instant;

use bevy::log::info;

use super::objects::{DISPATCHER, deadline, resolve};
use super::{HANDLES, ImportCall, KernelObject, NO_RETURN_VALUE, NtStatus};
use crate::guest::{self, ThreadEnvironment};
use crate::{status, xex_header};

/// Suspend counts of threads that have not been resumed yet, by thread object.
static SUSPEND_COUNTS: Mutex<BTreeMap<u32, u32>> = Mutex::new(BTreeMap::new());
static RESUMED: Condvar = Condvar::new();

/// What the title set about how each thread is scheduled. Host threads are scheduled by the
/// host, so these are only recorded for the title to read back.
#[derive(Clone, Copy)]
struct Scheduling {
    base_priority: i32,
    affinity: u32,
    boost_disabled: bool,
}

const DEFAULT_SCHEDULING: Scheduling = Scheduling {
    base_priority: 0,
    // All six hardware threads.
    affinity: 0x3F,
    boost_disabled: false,
};
static SCHEDULING: Mutex<BTreeMap<u32, Scheduling>> = Mutex::new(BTreeMap::new());

/// ExCreateThread's creation flag for a thread that waits to be resumed.
const CREATE_SUSPENDED: u32 = 0x1;
/// Used when the XEX header has no default stack size; the console's own default.
const FALLBACK_STACK_SIZE: u32 = 0x4_0000;
const MINIMUM_STACK_SIZE: u32 = 0x4000;
const MAIN_THREAD_NAME: &str = "guest-main";
/// KeResumeThread's result for a thread that was not suspended.
const NOT_SUSPENDED: u32 = 0;

/// Starts the title's entry point on the main guest thread.
pub fn start_main_thread(entry: u32) -> Result<ThreadEnvironment, String> {
    let environment = guest::prepare_thread(default_stack_size())?;
    DISPATCHER.initialize_thread(guest::memory(), environment.thread_object);
    let thread = environment.thread_object;
    guest::spawn(MAIN_THREAD_NAME.to_string(), move || {
        guest::call(environment, entry, [0, 0]);
        exit(thread);
        status::report_stop(format!("entry point {entry:#010x} returned"));
    })?;
    Ok(environment)
}

/// ExCreateThread(handle out, stack size, thread ID out, XAPI startup routine, start address,
/// start context, creation flags) -> NTSTATUS. With a startup routine, the thread runs
/// `startup(start address, context)`, which calls the start address itself.
pub(super) fn ex_create_thread(call: &ImportCall) -> u32 {
    let (handle_out, stack_size, thread_id_out) =
        (call.argument(0), call.argument(1), call.argument(2));
    let (startup, start, context, flags) = (
        call.argument(3),
        call.argument(4),
        call.argument(5),
        call.argument(6),
    );
    let stack_size = if stack_size == 0 {
        default_stack_size()
    } else {
        stack_size
    };
    let Ok(environment) = guest::prepare_thread(stack_size.max(MINIMUM_STACK_SIZE)) else {
        return NtStatus::NO_MEMORY.0;
    };
    let thread = environment.thread_object;
    DISPATCHER.initialize_thread(call.memory, thread);
    let suspended = flags & CREATE_SUSPENDED != 0;
    if suspended {
        SUSPEND_COUNTS.lock().unwrap().insert(thread, 1);
    }
    let Some(handle) = HANDLES
        .lock()
        .unwrap()
        .insert(KernelObject::Dispatcher(thread))
    else {
        return NtStatus::INSUFFICIENT_RESOURCES.0;
    };
    call.memory.write_u32(handle_out, handle);
    if thread_id_out != 0 {
        // The thread object's address is unique and stable, so it doubles as the thread ID.
        call.memory.write_u32(thread_id_out, thread);
    }

    let (function, arguments) = if startup != 0 {
        (startup, [start, context])
    } else {
        (start, [context, 0])
    };
    info!(target: "thread", "created {thread:#010x}: {function:#010x}({:#x}, {:#x}){}",
        arguments[0], arguments[1], if suspended { ", suspended" } else { "" });
    let spawned = guest::spawn(format!("guest-{thread:08x}"), move || {
        wait_while_suspended(thread);
        let exit_code = guest::call(environment, function, arguments);
        info!(target: "thread", "{thread:#010x} exited with {exit_code:#x}");
        exit(thread);
    });
    if let Err(error) = spawned {
        status::halt_guest_thread(error);
    }
    NtStatus::SUCCESS.0
}

/// NtResumeThread(handle, previous suspend count out) -> NTSTATUS.
pub(super) fn nt_resume_thread(call: &ImportCall) -> u32 {
    let Some(thread) = resolve(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let previous = resume(thread);
    let previous_out = call.argument(1);
    if previous_out != 0 {
        call.memory.write_u32(previous_out, previous);
    }
    NtStatus::SUCCESS.0
}

/// KeResumeThread(thread) -> previous suspend count.
pub(super) fn ke_resume_thread(call: &ImportCall) -> u32 {
    resume(call.argument(0))
}

fn resume(thread: u32) -> u32 {
    let mut counts = SUSPEND_COUNTS.lock().unwrap();
    let Some(count) = counts.get_mut(&thread) else {
        return NOT_SUSPENDED;
    };
    let previous = *count;
    *count -= 1;
    if *count == 0 {
        counts.remove(&thread);
        RESUMED.notify_all();
    }
    previous
}

fn wait_while_suspended(thread: u32) {
    let counts = SUSPEND_COUNTS.lock().unwrap();
    let _resumed = RESUMED
        .wait_while(counts, |counts| counts.contains_key(&thread))
        .unwrap();
}

/// Prepares a guest thread for runtime work that runs guest code, such as interrupt callbacks.
pub(super) fn prepare_system_thread(stack_size: u32) -> Result<ThreadEnvironment, String> {
    let environment = guest::prepare_thread(stack_size)?;
    DISPATCHER.initialize_thread(guest::memory(), environment.thread_object);
    Ok(environment)
}

fn exit(thread: u32) {
    DISPATCHER.set_signaled(guest::memory(), thread, true);
}

fn default_stack_size() -> u32 {
    xex_header::field(
        guest::memory(),
        guest::xex_header(),
        xex_header::DEFAULT_STACK_SIZE_KEY,
    )
    .unwrap_or(FALLBACK_STACK_SIZE)
}

/// Applies `change` to a thread's recorded scheduling, returning the settings before it.
fn update_scheduling(thread: u32, change: impl FnOnce(&mut Scheduling)) -> Scheduling {
    let mut scheduling = SCHEDULING.lock().unwrap();
    let settings = scheduling.entry(thread).or_insert(DEFAULT_SCHEDULING);
    let previous = *settings;
    change(settings);
    previous
}

/// KeSetBasePriorityThread(thread, priority) -> previous priority.
pub(super) fn ke_set_base_priority_thread(call: &ImportCall) -> u32 {
    let priority = call.argument(1) as i32;
    update_scheduling(call.argument(0), |settings| {
        settings.base_priority = priority
    })
    .base_priority as u32
}

/// KeQueryBasePriorityThread(thread) -> priority.
pub(super) fn ke_query_base_priority_thread(call: &ImportCall) -> u32 {
    update_scheduling(call.argument(0), |_| {}).base_priority as u32
}

/// KeSetAffinityThread(thread, affinity, previous affinity out) -> NTSTATUS. Unlike Windows,
/// the console returns a status and reports the previous affinity through the pointer.
pub(super) fn ke_set_affinity_thread(call: &ImportCall) -> u32 {
    let (thread, affinity, previous_out) = (call.argument(0), call.argument(1), call.argument(2));
    if affinity == 0 {
        return NtStatus::INVALID_PARAMETER.0;
    }
    let previous = update_scheduling(thread, |settings| settings.affinity = affinity).affinity;
    if previous_out != 0 {
        call.memory.write_u32(previous_out, previous);
    }
    NtStatus::SUCCESS.0
}

/// KeSetDisableBoostThread(thread, disable) -> whether boosting was disabled before.
pub(super) fn ke_set_disable_boost_thread(call: &ImportCall) -> u32 {
    let disable = call.argument(1) != 0;
    u32::from(
        update_scheduling(call.argument(0), |settings| {
            settings.boost_disabled = disable
        })
        .boost_disabled,
    )
}

/// KeEnterCriticalRegion() and KeLeaveCriticalRegion(): hold off kernel APCs for the calling
/// thread. Nothing queues APCs yet, so there is nothing to hold off.
pub(super) fn ke_critical_region(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// KeDelayExecutionThread(mode, alertable, interval) -> NTSTATUS. The interval uses the wait
/// timeout encoding; nothing queues APCs yet, so an alertable delay is never cut short.
pub(super) fn ke_delay_execution_thread(call: &ImportCall) -> u32 {
    match deadline(call, call.argument(2)) {
        Some(due) => {
            let remaining = due.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                thread::yield_now();
            } else {
                thread::sleep(remaining);
            }
        }
        None => status::halt_guest_thread("KeDelayExecutionThread with no interval".to_string()),
    }
    NtStatus::SUCCESS.0
}

/// NtYieldExecution() -> NTSTATUS.
pub(super) fn nt_yield_execution(_: &ImportCall) -> u32 {
    thread::yield_now();
    NtStatus::SUCCESS.0
}

/// EXCEPTION_RECORD: the code, then (after flags, a chained record and the address) the
/// parameter count and parameters.
const EXCEPTION_CODE: u32 = 0x0;
const EXCEPTION_PARAMETERS: u32 = 0x14;
const EXCEPTION_PARAMETER_SIZE: u32 = 4;
/// The MSVC convention for naming a thread: the parameters are a type tag, a pointer to the
/// name, the thread ID (-1 for the caller) and flags. A debugger catches it; titles swallow it.
const SET_THREAD_NAME_EXCEPTION: u32 = 0x406D_1388;
const THREAD_NAME_PARAMETER: u32 = 1;
const THREAD_ID_PARAMETER: u32 = 2;
/// The thread ID parameter that means the calling thread.
const CALLING_THREAD_ID: u32 = u32::MAX;
const MAXIMUM_THREAD_NAME_LENGTH: u32 = 256;

/// RtlRaiseException(record). Only the thread-naming exception is handled; dispatching other
/// exceptions to the title's handlers is not modelled yet.
pub(super) fn rtl_raise_exception(call: &ImportCall) -> u32 {
    let record = call.argument(0);
    let code = call.memory.read_u32(record + EXCEPTION_CODE);
    if code != SET_THREAD_NAME_EXCEPTION {
        status::halt_guest_thread(format!(
            "exception {code:#010x} raised from {:#010x}; exception dispatch is not supported yet",
            call.return_address()
        ));
    }
    let parameter = |index: u32| {
        call.memory
            .read_u32(record + EXCEPTION_PARAMETERS + index * EXCEPTION_PARAMETER_SIZE)
    };
    let name_pointer = parameter(THREAD_NAME_PARAMETER);
    // Thread IDs are thread object addresses (see ex_create_thread).
    let thread = match parameter(THREAD_ID_PARAMETER) {
        CALLING_THREAD_ID => guest::current_thread(),
        thread => thread,
    };
    let name: Vec<u8> = (0..MAXIMUM_THREAD_NAME_LENGTH)
        .map(|offset| call.memory.read_u8(name_pointer + offset))
        .take_while(|&byte| byte != 0)
        .collect();
    info!(target: "thread", "{thread:#010x} is named {:?}", String::from_utf8_lossy(&name));
    NO_RETURN_VALUE
}

/// KeEnableFpuExceptions(enabled). The generated code never traps on floating-point
/// conditions, so there is nothing to enable.
pub(super) fn ke_enable_fpu_exceptions(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// ExTerminateThread(exit code). Ends the calling guest thread: its thread object is signaled
/// for anyone waiting on it, and the host thread parks for good, since it cannot unwind through
/// the generated code's frames.
pub(super) fn ex_terminate_thread(call: &ImportCall) -> u32 {
    let thread = guest::current_thread();
    info!(target: "thread", "{thread:#010x} exited with {:#x}", call.argument(0));
    exit(thread);
    loop {
        thread::park();
    }
}
