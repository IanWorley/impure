//! Kernel object imports: creating, signaling and waiting on events, semaphores and mutants,
//! either through handles (Nt*) or through guest pointers to the objects themselves (Ke*).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bevy::log::{debug, info};

use super::{HANDLES, ImportCall, KernelObject, NO_RETURN_VALUE, NtStatus, read_ansi_string};
use crate::dispatcher::{self, Dispatcher, DispatcherError, ObjectType, WaitOutcome};
use crate::{guest, status, time};

pub(super) static DISPATCHER: Dispatcher = Dispatcher::new();
/// Named objects, by lowercase name; titles treat object names case-insensitively.
static NAMES: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

/// Kernel objects are never freed yet; the system heap is large enough for boot.
const OBJECT_ALIGNMENT: u32 = 0x10;
/// OBJECT_ATTRIBUTES: the root directory handle, then a pointer to the object's name.
const OBJECT_ATTRIBUTES_NAME: u32 = 0x4;
/// NtCurrentThread(): a pseudo-handle for the calling thread.
const CURRENT_THREAD_HANDLE: u32 = 0xFFFF_FFFE;
/// The most objects one wait may name (MAXIMUM_WAIT_OBJECTS).
const MAXIMUM_WAIT_OBJECTS: u32 = 64;
/// WAIT_TYPE: wait until every object is signaled, or until any one is.
const WAIT_ALL: u32 = 0;
/// NtCreateEvent's EVENT_TYPE argument.
const NOTIFICATION_EVENT: u32 = 0;
const SYNCHRONIZATION_EVENT: u32 = 1;
const GUEST_POINTER_SIZE: u32 = 4;
/// How long a wait with no timeout may last before it is reported.
const LONG_WAIT: Duration = Duration::from_secs(5);

/// NtCreateEvent(handle out, attributes, event type, initially signaled) -> NTSTATUS.
pub(super) fn nt_create_event(call: &ImportCall) -> u32 {
    let kind = match call.argument(2) {
        NOTIFICATION_EVENT => ObjectType::NotificationEvent,
        SYNCHRONIZATION_EVENT => ObjectType::SynchronizationEvent,
        _ => return NtStatus::INVALID_PARAMETER.0,
    };
    let signaled = call.argument(3) != 0;
    let event_types = [
        ObjectType::NotificationEvent,
        ObjectType::SynchronizationEvent,
    ];
    create(call, dispatcher::HEADER_SIZE, &event_types, |object| {
        DISPATCHER.initialize_event(call.memory, object, kind, signaled);
    })
}

/// NtCreateSemaphore(handle out, attributes, initial count, limit) -> NTSTATUS.
pub(super) fn nt_create_semaphore(call: &ImportCall) -> u32 {
    let (count, limit) = (call.argument(2), call.argument(3));
    if count > limit {
        return NtStatus::INVALID_PARAMETER.0;
    }
    create(
        call,
        dispatcher::SEMAPHORE_SIZE,
        &[ObjectType::Semaphore],
        |object| {
            DISPATCHER.initialize_semaphore(call.memory, object, count, limit);
            debug!(target: "semaphore", "{object:#010x} created ({count} of {limit}) by thread {:#010x} from {:#010x}", guest::current_thread(), call.return_address());
        },
    )
}

/// NtCreateMutant(handle out, attributes, initially owned) -> NTSTATUS.
pub(super) fn nt_create_mutant(call: &ImportCall) -> u32 {
    let owner = (call.argument(2) != 0).then(guest::current_thread);
    create(
        call,
        dispatcher::MUTANT_SIZE,
        &[ObjectType::Mutant],
        |object| {
            DISPATCHER.initialize_mutant(call.memory, object, owner);
        },
    )
}

/// NtSetEvent(handle, previous state out) -> NTSTATUS.
pub(super) fn nt_set_event(call: &ImportCall) -> u32 {
    with_handle(call.argument(0), |object| {
        let previous = DISPATCHER.set_signaled(call.memory, object, true);
        write_optional(call, call.argument(1), previous);
        NtStatus::SUCCESS
    })
}

/// Signals the event a handle names, for imports that report completion through one.
/// Handles that do not name a waitable object are ignored, as the console does.
pub(super) fn signal_event(call: &ImportCall, handle: u32) {
    if let Some(event) = resolve(handle) {
        DISPATCHER.set_signaled(call.memory, event, true);
    }
}

/// NtClearEvent(handle) -> NTSTATUS.
pub(super) fn nt_clear_event(call: &ImportCall) -> u32 {
    with_handle(call.argument(0), |object| {
        DISPATCHER.set_signaled(call.memory, object, false);
        NtStatus::SUCCESS
    })
}

/// NtReleaseSemaphore(handle, increment, previous count out) -> NTSTATUS.
pub(super) fn nt_release_semaphore(call: &ImportCall) -> u32 {
    with_handle(call.argument(0), |object| {
        let released = DISPATCHER.release_semaphore(call.memory, object, call.argument(1));
        debug!(target: "semaphore", "{object:#010x} released by {} by thread {:#010x} from {:#010x}: {released:?}", call.argument(1), guest::current_thread(), call.return_address());
        match released {
            Ok(previous) => {
                write_optional(call, call.argument(2), previous);
                NtStatus::SUCCESS
            }
            Err(error) => dispatcher_status(error),
        }
    })
}

/// NtReleaseMutant(handle, previous state out) -> NTSTATUS.
pub(super) fn nt_release_mutant(call: &ImportCall) -> u32 {
    with_handle(call.argument(0), |object| {
        match DISPATCHER.release_mutant(call.memory, object, guest::current_thread()) {
            Ok(previous) => {
                write_optional(call, call.argument(1), previous);
                NtStatus::SUCCESS
            }
            Err(error) => dispatcher_status(error),
        }
    })
}

/// NtWaitForSingleObjectEx(handle, wait mode, alertable, timeout) -> NTSTATUS.
/// Nothing queues APCs yet, so alertable waits behave like plain ones.
pub(super) fn nt_wait_for_single_object_ex(call: &ImportCall) -> u32 {
    let Some(object) = resolve(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    wait(call, &[object], false, call.argument(3))
}

/// NtWaitForMultipleObjectsEx(count, handles, wait type, wait mode, alertable, timeout).
pub(super) fn nt_wait_for_multiple_objects_ex(call: &ImportCall) -> u32 {
    let Some(handles) = read_pointer_array(call, call.argument(0), call.argument(1)) else {
        return NtStatus::INVALID_PARAMETER.0;
    };
    let Some(objects) = handles.into_iter().map(resolve).collect::<Option<Vec<_>>>() else {
        return NtStatus::INVALID_HANDLE.0;
    };
    wait(
        call,
        &objects,
        call.argument(2) == WAIT_ALL,
        call.argument(5),
    )
}

/// KeInitializeSemaphore(semaphore, count, limit).
pub(super) fn ke_initialize_semaphore(call: &ImportCall) -> u32 {
    let (semaphore, count, limit) = (call.argument(0), call.argument(1), call.argument(2));
    DISPATCHER.initialize_semaphore(call.memory, semaphore, count, limit);
    debug!(target: "semaphore", "{semaphore:#010x} initialized ({count} of {limit}) by thread {:#010x} from {:#010x}", guest::current_thread(), call.return_address());
    NO_RETURN_VALUE
}

/// KeReleaseSemaphore(semaphore, priority increment, adjustment, wait) -> previous count.
pub(super) fn ke_release_semaphore(call: &ImportCall) -> u32 {
    let (semaphore, adjustment) = (call.argument(0), call.argument(2));
    debug!(target: "semaphore", "{semaphore:#010x} released by {adjustment} by thread {:#010x} from {:#010x}", guest::current_thread(), call.return_address());
    DISPATCHER
        .release_semaphore(call.memory, semaphore, adjustment)
        .unwrap_or_else(|error| halt_on(error, "KeReleaseSemaphore"))
}

/// KeSetEvent(event, priority increment, wait) -> previous state.
pub(super) fn ke_set_event(call: &ImportCall) -> u32 {
    DISPATCHER.set_signaled(call.memory, call.argument(0), true)
}

/// KeResetEvent(event) -> previous state.
pub(super) fn ke_reset_event(call: &ImportCall) -> u32 {
    DISPATCHER.set_signaled(call.memory, call.argument(0), false)
}

/// KeWaitForSingleObject(object, reason, mode, alertable, timeout) -> NTSTATUS.
pub(super) fn ke_wait_for_single_object(call: &ImportCall) -> u32 {
    wait(call, &[call.argument(0)], false, call.argument(4))
}

/// KeWaitForMultipleObjects(count, objects, wait type, reason, mode, alertable, timeout,
/// wait blocks) -> NTSTATUS. The runtime keeps its own wait state, so the wait blocks are unused.
pub(super) fn ke_wait_for_multiple_objects(call: &ImportCall) -> u32 {
    let Some(objects) = read_pointer_array(call, call.argument(0), call.argument(1)) else {
        return NtStatus::INVALID_PARAMETER.0;
    };
    wait(
        call,
        &objects,
        call.argument(2) == WAIT_ALL,
        call.argument(6),
    )
}

/// ObReferenceObjectByHandle(handle, object type, object out) -> NTSTATUS.
/// The object type is not checked: the runtime does not model the type objects yet.
pub(super) fn ob_reference_object_by_handle(call: &ImportCall) -> u32 {
    let Some(object) = resolve(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    call.memory.write_u32(call.argument(2), object);
    NtStatus::SUCCESS.0
}

/// ObDereferenceObject(object). Objects are never freed yet, so there is no count to drop.
pub(super) fn ob_dereference_object(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// Allocates and initializes an unnamed object, and writes a new handle to it to argument 0.
/// Creates an object of one of the `types` (allocating and initializing it) and writes a new
/// handle to it to argument 0. A named object that already exists is opened instead, if it is
/// one of the `types`.
pub(super) fn create(
    call: &ImportCall,
    size: u32,
    types: &[ObjectType],
    initialize: impl FnOnce(u32),
) -> u32 {
    let attributes = call.argument(1);
    let name_string = if attributes == 0 {
        0
    } else {
        call.memory.read_u32(attributes + OBJECT_ATTRIBUTES_NAME)
    };
    // Held across creation, so two threads creating the same name get the same object.
    let mut names = NAMES.lock().unwrap();
    let name = (name_string != 0).then(|| read_ansi_string(call, name_string).to_ascii_lowercase());
    let existing = name.as_ref().and_then(|name| names.get(name).copied());
    let (object, status) = match existing {
        Some(object) => {
            let matches = dispatcher::object_type(call.memory, object)
                .is_ok_and(|existing_type| types.contains(&existing_type));
            if !matches {
                return NtStatus::OBJECT_TYPE_MISMATCH.0;
            }
            (object, NtStatus::OBJECT_NAME_EXISTS)
        }
        None => {
            let Some(object) = guest::allocate(size, OBJECT_ALIGNMENT) else {
                return NtStatus::INSUFFICIENT_RESOURCES.0;
            };
            initialize(object);
            if let Some(name) = name {
                names.insert(name, object);
            }
            (object, NtStatus::SUCCESS)
        }
    };
    drop(names);
    let Some(handle) = HANDLES
        .lock()
        .unwrap()
        .insert(KernelObject::Dispatcher(object))
    else {
        return NtStatus::INSUFFICIENT_RESOURCES.0;
    };
    call.memory.write_u32(call.argument(0), handle);
    status.0
}

/// The guest address of the waitable object a handle names. File handles name no guest object.
pub(super) fn resolve(handle: u32) -> Option<u32> {
    if handle == CURRENT_THREAD_HANDLE {
        return Some(guest::current_thread());
    }
    match HANDLES.lock().unwrap().get(handle)? {
        KernelObject::Dispatcher(object) => Some(object),
        KernelObject::File(_) | KernelObject::Listener(_) | KernelObject::Enumerator(_) => None,
    }
}

fn with_handle(handle: u32, action: impl FnOnce(u32) -> NtStatus) -> u32 {
    resolve(handle).map_or(NtStatus::INVALID_HANDLE, action).0
}

/// Many imports report a previous state through an optional out pointer.
fn write_optional(call: &ImportCall, pointer: u32, value: u32) {
    if pointer != 0 {
        call.memory.write_u32(pointer, value);
    }
}

fn read_pointer_array(call: &ImportCall, count: u32, array: u32) -> Option<Vec<u32>> {
    (1..=MAXIMUM_WAIT_OBJECTS).contains(&count).then(|| {
        (0..count)
            .map(|index| call.memory.read_u32(array + index * GUEST_POINTER_SIZE))
            .collect()
    })
}

fn wait(call: &ImportCall, objects: &[u32], wait_all: bool, timeout_pointer: u32) -> u32 {
    let deadline = deadline(call, timeout_pointer);
    let thread = guest::current_thread();
    let wait = |deadline| DISPATCHER.wait(call.memory, objects, wait_all, thread, deadline);
    // A wait with no timeout that lasts a while is reported once, with what it waits for: the
    // first clue when the title's threads wait on each other forever.
    let outcome = match deadline {
        Some(_) => wait(deadline),
        None => match wait(Some(Instant::now() + LONG_WAIT)) {
            Ok(WaitOutcome::TimedOut) => {
                let described: Vec<String> = objects
                    .iter()
                    .map(|&object| dispatcher::describe(call.memory, object))
                    .collect();
                info!(
                    target: "wait",
                    "thread {thread:#010x} has waited {}s from {:#010x} for {} of [{}]",
                    LONG_WAIT.as_secs(),
                    call.return_address(),
                    if wait_all { "all" } else { "any" },
                    described.join(", ")
                );
                wait(None)
            }
            outcome => outcome,
        },
    };
    match outcome {
        // STATUS_WAIT_0 + index.
        Ok(WaitOutcome::Satisfied(index)) => NtStatus::SUCCESS.0 + index as u32,
        Ok(WaitOutcome::TimedOut) => NtStatus::TIMEOUT.0,
        Err(error) => halt_on(error, "a wait"),
    }
}

/// Timeouts are LARGE_INTEGERs in system time units: negative is relative, positive is an
/// absolute system time, and a null pointer means wait forever.
pub(super) fn deadline(call: &ImportCall, timeout_pointer: u32) -> Option<Instant> {
    if timeout_pointer == 0 {
        return None;
    }
    let timeout = call.memory.read_i64(timeout_pointer);
    let units = if timeout <= 0 {
        timeout.unsigned_abs()
    } else {
        (timeout as u64).saturating_sub(time::now())
    };
    Some(Instant::now() + Duration::from_nanos(units.saturating_mul(time::NANOSECONDS_PER_UNIT)))
}

fn dispatcher_status(error: DispatcherError) -> NtStatus {
    match error {
        DispatcherError::SemaphoreLimitExceeded => NtStatus::SEMAPHORE_LIMIT_EXCEEDED,
        DispatcherError::MutantNotOwned => NtStatus::MUTANT_NOT_OWNED,
        DispatcherError::UnknownObjectType { .. } => halt_on(error, "an object import"),
    }
}

/// Errors a Ke* import has no status to report, or that mean the guest touched an object the
/// runtime does not model; continuing would hide the problem.
fn halt_on(error: DispatcherError, context: &str) -> ! {
    status::halt_guest_thread(format!("{context} failed: {error:?}"))
}
