//! Waitable kernel objects (events, semaphores, mutants, threads, timers) and waits on them.
//!
//! Each object lives in guest memory in the console's layout, starting with a DISPATCHER_HEADER,
//! so Ke* imports can take guest pointers and guest code that initializes an object inline still
//! works. All objects share one lock and condition variable, like the console's dispatcher lock:
//! any signal wakes every waiter to recheck, which is simple and correct.

use std::sync::{Condvar, Mutex};
use std::time::Instant;

use crate::memory::GuestMemory;

/// DISPATCHER_HEADER fields.
const HEADER_TYPE: u32 = 0x0;
const HEADER_SIGNAL_STATE: u32 = 0x4;
const HEADER_WAIT_LIST: u32 = 0x8;
/// An empty wait list's links point back at the list head.
const WAIT_LIST_LINK_SIZE: u32 = 4;
pub const HEADER_SIZE: u32 = 0x10;
/// KSEMAPHORE: the header, then the limit.
const SEMAPHORE_LIMIT: u32 = 0x10;
pub const SEMAPHORE_SIZE: u32 = 0x14;
/// KMUTANT: the header, a list entry, the owning thread, and the abandoned flag.
const MUTANT_OWNER: u32 = 0x18;
pub const MUTANT_SIZE: u32 = 0x20;
/// KTIMER: the header, due time, list entry, DPC and period.
pub const TIMER_SIZE: u32 = 0x28;
const NO_OWNER: u32 = 0;
/// A free mutant's signal state; each acquisition by its owner takes one away.
const MUTANT_FREE: u32 = 1;

/// The header's type byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    /// Stays signaled until reset (manual reset).
    NotificationEvent = 0,
    /// A satisfied wait resets it (auto reset).
    SynchronizationEvent = 1,
    Mutant = 2,
    Semaphore = 5,
    /// Signaled once the thread exits, and stays signaled.
    Thread = 6,
    /// Timers signal when due: a notification timer stays signaled, a synchronization timer
    /// resets when a wait takes it.
    NotificationTimer = 8,
    SynchronizationTimer = 9,
}

impl ObjectType {
    fn from_byte(byte: u8) -> Option<Self> {
        [
            Self::NotificationEvent,
            Self::SynchronizationEvent,
            Self::Mutant,
            Self::Semaphore,
            Self::Thread,
            Self::NotificationTimer,
            Self::SynchronizationTimer,
        ]
        .into_iter()
        .find(|kind| *kind as u8 == byte)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The index of the object that satisfied a wait-any, or 0 for a wait-all.
    Satisfied(usize),
    TimedOut,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DispatcherError {
    /// The object's type byte is not one the runtime models.
    UnknownObjectType {
        object: u32,
        type_byte: u8,
    },
    SemaphoreLimitExceeded,
    MutantNotOwned,
}

pub struct Dispatcher {
    lock: Mutex<()>,
    changed: Condvar,
}

impl Dispatcher {
    pub const fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    pub fn initialize_event(
        &self,
        memory: &GuestMemory,
        event: u32,
        kind: ObjectType,
        signaled: bool,
    ) {
        let _guard = self.lock.lock().unwrap();
        write_header(memory, event, kind, u32::from(signaled));
        self.changed.notify_all();
    }

    pub fn initialize_semaphore(
        &self,
        memory: &GuestMemory,
        semaphore: u32,
        count: u32,
        limit: u32,
    ) {
        let _guard = self.lock.lock().unwrap();
        write_header(memory, semaphore, ObjectType::Semaphore, count);
        memory.write_u32(semaphore + SEMAPHORE_LIMIT, limit);
        self.changed.notify_all();
    }

    /// A mutant created owned starts acquired once by `owner`.
    pub fn initialize_mutant(&self, memory: &GuestMemory, mutant: u32, owner: Option<u32>) {
        let _guard = self.lock.lock().unwrap();
        let state = if owner.is_some() {
            MUTANT_FREE - 1
        } else {
            MUTANT_FREE
        };
        write_header(memory, mutant, ObjectType::Mutant, state);
        memory.write_u32(mutant + MUTANT_OWNER, owner.unwrap_or(NO_OWNER));
        self.changed.notify_all();
    }

    /// An unsignaled timer.
    pub fn initialize_timer(&self, memory: &GuestMemory, timer: u32, kind: ObjectType) {
        let _guard = self.lock.lock().unwrap();
        write_header(memory, timer, kind, 0);
    }

    /// A running thread: unsignaled until it exits.
    pub fn initialize_thread(&self, memory: &GuestMemory, thread: u32) {
        let _guard = self.lock.lock().unwrap();
        write_header(memory, thread, ObjectType::Thread, 0);
    }

    /// An object's signal state.
    pub fn signal_state(&self, memory: &GuestMemory, object: u32) -> u32 {
        let _guard = self.lock.lock().unwrap();
        memory.read_u32(object + HEADER_SIGNAL_STATE)
    }

    /// Sets an event's (or an exiting thread's) signal state, returning the previous one.
    pub fn set_signaled(&self, memory: &GuestMemory, object: u32, signaled: bool) -> u32 {
        let _guard = self.lock.lock().unwrap();
        let previous = memory.read_u32(object + HEADER_SIGNAL_STATE);
        memory.write_u32(object + HEADER_SIGNAL_STATE, u32::from(signaled));
        if signaled {
            self.changed.notify_all();
        }
        previous
    }

    /// Adds `increment` to a semaphore's count, returning the previous count.
    pub fn release_semaphore(
        &self,
        memory: &GuestMemory,
        semaphore: u32,
        increment: u32,
    ) -> Result<u32, DispatcherError> {
        let _guard = self.lock.lock().unwrap();
        let previous = memory.read_u32(semaphore + HEADER_SIGNAL_STATE);
        let limit = memory.read_u32(semaphore + SEMAPHORE_LIMIT);
        let count = previous
            .checked_add(increment)
            .filter(|&count| count <= limit)
            .ok_or(DispatcherError::SemaphoreLimitExceeded)?;
        memory.write_u32(semaphore + HEADER_SIGNAL_STATE, count);
        self.changed.notify_all();
        Ok(previous)
    }

    /// Releases one acquisition of a mutant `thread` owns, returning the previous signal state.
    pub fn release_mutant(
        &self,
        memory: &GuestMemory,
        mutant: u32,
        thread: u32,
    ) -> Result<u32, DispatcherError> {
        let _guard = self.lock.lock().unwrap();
        if memory.read_u32(mutant + MUTANT_OWNER) != thread {
            return Err(DispatcherError::MutantNotOwned);
        }
        let previous = memory.read_u32(mutant + HEADER_SIGNAL_STATE);
        let state = previous.wrapping_add(1);
        memory.write_u32(mutant + HEADER_SIGNAL_STATE, state);
        if state == MUTANT_FREE {
            memory.write_u32(mutant + MUTANT_OWNER, NO_OWNER);
            self.changed.notify_all();
        }
        Ok(previous)
    }

    /// Blocks until one object (or, with `wait_all`, every object) is signaled for `thread`, then
    /// applies each satisfied object's side effect. With no deadline, waits forever.
    pub fn wait(
        &self,
        memory: &GuestMemory,
        objects: &[u32],
        wait_all: bool,
        thread: u32,
        deadline: Option<Instant>,
    ) -> Result<WaitOutcome, DispatcherError> {
        let mut guard = self.lock.lock().unwrap();
        loop {
            let signaled = objects
                .iter()
                .map(|&object| is_signaled(memory, object, thread))
                .collect::<Result<Vec<_>, _>>()?;
            let satisfied = if wait_all {
                signaled.iter().all(|&ready| ready).then_some(0)
            } else {
                signaled.iter().position(|&ready| ready)
            };
            if let Some(index) = satisfied {
                let taken = if wait_all {
                    objects
                } else {
                    &objects[index..=index]
                };
                for &object in taken {
                    satisfy(memory, object, thread)?;
                }
                return Ok(WaitOutcome::Satisfied(index));
            }

            guard = match deadline {
                None => self.changed.wait(guard).unwrap(),
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(WaitOutcome::TimedOut);
                    }
                    self.changed.wait_timeout(guard, remaining).unwrap().0
                }
            };
        }
    }
}

fn write_header(memory: &GuestMemory, object: u32, kind: ObjectType, signal_state: u32) {
    memory.write_u8(object + HEADER_TYPE, kind as u8);
    memory.write_u32(object + HEADER_SIGNAL_STATE, signal_state);
    let wait_list = object + HEADER_WAIT_LIST;
    memory.write_u32(wait_list, wait_list);
    memory.write_u32(wait_list + WAIT_LIST_LINK_SIZE, wait_list);
}

/// An object's type and raw signal state, for diagnostics.
pub fn describe(memory: &GuestMemory, object: u32) -> String {
    let state = memory.read_u32(object + HEADER_SIGNAL_STATE);
    match object_type(memory, object) {
        Ok(kind) => format!("{object:#010x} {kind:?} state {state}"),
        Err(_) => format!(
            "{object:#010x} type {} state {state}",
            memory.read_u8(object + HEADER_TYPE)
        ),
    }
}

pub fn object_type(memory: &GuestMemory, object: u32) -> Result<ObjectType, DispatcherError> {
    let type_byte = memory.read_u8(object + HEADER_TYPE);
    ObjectType::from_byte(type_byte).ok_or(DispatcherError::UnknownObjectType { object, type_byte })
}

fn is_signaled(memory: &GuestMemory, object: u32, thread: u32) -> Result<bool, DispatcherError> {
    let state = memory.read_u32(object + HEADER_SIGNAL_STATE);
    Ok(match object_type(memory, object)? {
        ObjectType::Mutant => {
            state == MUTANT_FREE || memory.read_u32(object + MUTANT_OWNER) == thread
        }
        _ => state != 0,
    })
}

/// What a satisfied wait does to the object.
fn satisfy(memory: &GuestMemory, object: u32, thread: u32) -> Result<(), DispatcherError> {
    let state_address = object + HEADER_SIGNAL_STATE;
    let take_one = || {
        memory.write_u32(
            state_address,
            memory.read_u32(state_address).wrapping_sub(1),
        )
    };
    match object_type(memory, object)? {
        ObjectType::NotificationEvent | ObjectType::Thread | ObjectType::NotificationTimer => {}
        ObjectType::SynchronizationEvent | ObjectType::SynchronizationTimer => {
            memory.write_u32(state_address, 0)
        }
        ObjectType::Semaphore => take_one(),
        ObjectType::Mutant => {
            take_one();
            memory.write_u32(object + MUTANT_OWNER, thread);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    const FIRST: u32 = 0x1000;
    const SECOND: u32 = 0x1100;
    const THREAD_A: u32 = 0x7000_0000;
    const THREAD_B: u32 = 0x7000_1000;
    const SEMAPHORE_LIMIT_VALUE: u32 = 2;

    fn setup() -> (GuestMemory, Dispatcher) {
        (GuestMemory::reserve().unwrap(), Dispatcher::new())
    }

    fn poll(
        dispatcher: &Dispatcher,
        memory: &GuestMemory,
        objects: &[u32],
        all: bool,
        thread: u32,
    ) -> WaitOutcome {
        dispatcher
            .wait(memory, objects, all, thread, Some(Instant::now()))
            .unwrap()
    }

    #[test]
    fn synchronization_events_reset_on_wait_and_notification_events_do_not() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_event(&memory, FIRST, ObjectType::SynchronizationEvent, true);
        dispatcher.initialize_event(&memory, SECOND, ObjectType::NotificationEvent, true);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[SECOND], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[SECOND], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
    }

    #[test]
    fn semaphores_count_down_and_enforce_their_limit() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_semaphore(&memory, FIRST, 1, SEMAPHORE_LIMIT_VALUE);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            dispatcher.release_semaphore(&memory, FIRST, SEMAPHORE_LIMIT_VALUE),
            Ok(0)
        );
        assert_eq!(
            dispatcher.release_semaphore(&memory, FIRST, 1),
            Err(DispatcherError::SemaphoreLimitExceeded)
        );
    }

    #[test]
    fn mutants_are_recursive_for_their_owner_and_exclusive_otherwise() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_mutant(&memory, FIRST, None);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_B),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            dispatcher.release_mutant(&memory, FIRST, THREAD_B),
            Err(DispatcherError::MutantNotOwned)
        );
        dispatcher.release_mutant(&memory, FIRST, THREAD_A).unwrap();
        dispatcher.release_mutant(&memory, FIRST, THREAD_A).unwrap();
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_B),
            WaitOutcome::Satisfied(0)
        );
    }

    #[test]
    fn wait_all_takes_nothing_until_every_object_is_signaled() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_event(&memory, FIRST, ObjectType::SynchronizationEvent, true);
        dispatcher.initialize_event(&memory, SECOND, ObjectType::SynchronizationEvent, false);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST, SECOND], true, THREAD_A),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[SECOND, FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(1)
        );
    }

    #[test]
    fn a_blocked_waiter_wakes_when_another_thread_signals() {
        const WAIT_LIMIT: Duration = Duration::from_secs(10);
        let (memory, dispatcher) = setup();
        dispatcher.initialize_event(&memory, FIRST, ObjectType::SynchronizationEvent, false);
        let shared = Arc::new((memory, dispatcher));
        let waiter = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let deadline = Instant::now() + WAIT_LIMIT;
                shared
                    .1
                    .wait(&shared.0, &[FIRST], false, THREAD_B, Some(deadline))
            })
        };
        shared.1.set_signaled(&shared.0, FIRST, true);
        assert_eq!(waiter.join().unwrap(), Ok(WaitOutcome::Satisfied(0)));
    }

    #[test]
    fn threads_signal_on_exit_and_stay_signaled() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_thread(&memory, FIRST);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::TimedOut
        );
        dispatcher.set_signaled(&memory, FIRST, true);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_B),
            WaitOutcome::Satisfied(0)
        );
    }

    #[test]
    fn synchronization_timers_reset_on_wait() {
        let (memory, dispatcher) = setup();
        dispatcher.initialize_timer(&memory, FIRST, ObjectType::SynchronizationTimer);
        dispatcher.initialize_timer(&memory, SECOND, ObjectType::NotificationTimer);
        dispatcher.set_signaled(&memory, FIRST, true);
        dispatcher.set_signaled(&memory, SECOND, true);
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[FIRST], false, THREAD_A),
            WaitOutcome::TimedOut
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[SECOND], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
        assert_eq!(
            poll(&dispatcher, &memory, &[SECOND], false, THREAD_A),
            WaitOutcome::Satisfied(0)
        );
    }

    #[test]
    fn unknown_object_types_are_reported() {
        const UNMODELED_TYPE: u8 = 0x7F;
        let (memory, dispatcher) = setup();
        memory.write_u8(FIRST, UNMODELED_TYPE);
        assert_eq!(
            dispatcher.wait(&memory, &[FIRST], false, THREAD_A, Some(Instant::now())),
            Err(DispatcherError::UnknownObjectType {
                object: FIRST,
                type_byte: UNMODELED_TYPE
            })
        );
    }
}
