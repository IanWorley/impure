//! Kernel timers (NtCreateTimer, NtSetTimerEx, NtCancelTimer). A host thread keeps the armed
//! timers in due-time order, signals each when it falls due, and re-arms periodic ones.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use super::objects::{DISPATCHER, deadline, resolve};
use super::{ImportCall, NtStatus, objects};
use crate::dispatcher::{self, ObjectType};
use crate::{guest, status};

/// NtCreateTimer's TIMER_TYPE argument.
const NOTIFICATION_TIMER: u32 = 0;
const SYNCHRONIZATION_TIMER: u32 = 1;
/// NtSetTimerEx's arguments: handle, due time, APC routine, APC mode, APC argument, resume,
/// period in milliseconds, and the previous state out.
const SET_APC_ROUTINE: u32 = 2;
const SET_PERIOD: u32 = 6;
const SET_PREVIOUS_STATE: u32 = 7;
const TIMER_THREAD_NAME: &str = "kernel-timers";

/// Armed timers, by when they fall due (ties broken by arming order), with their periods.
struct Schedule {
    due: BTreeMap<(Instant, u64), (u32, Option<Duration>)>,
    next_sequence: u64,
}

impl Schedule {
    fn disarm(&mut self, timer: u32) {
        self.due.retain(|_, (armed, _)| *armed != timer);
    }

    fn arm(&mut self, timer: u32, due: Instant, period: Option<Duration>) {
        self.disarm(timer);
        self.due.insert((due, self.next_sequence), (timer, period));
        self.next_sequence += 1;
    }
}

static SCHEDULE: Mutex<Schedule> = Mutex::new(Schedule {
    due: BTreeMap::new(),
    next_sequence: 0,
});
static RESCHEDULED: Condvar = Condvar::new();
static TIMER_THREAD_STARTED: Once = Once::new();

/// NtCreateTimer(handle out, attributes, timer type) -> NTSTATUS.
pub(super) fn nt_create_timer(call: &ImportCall) -> u32 {
    let kind = match call.argument(2) {
        NOTIFICATION_TIMER => ObjectType::NotificationTimer,
        SYNCHRONIZATION_TIMER => ObjectType::SynchronizationTimer,
        _ => return NtStatus::INVALID_PARAMETER.0,
    };
    let timer_types = [
        ObjectType::NotificationTimer,
        ObjectType::SynchronizationTimer,
    ];
    objects::create(call, dispatcher::TIMER_SIZE, &timer_types, |timer| {
        DISPATCHER.initialize_timer(call.memory, timer, kind);
    })
}

/// NtSetTimerEx(handle, due time, APC routine, APC mode, APC argument, resume, period, previous
/// state out) -> NTSTATUS. Arming resets the timer; APC routines are not supported yet.
pub(super) fn nt_set_timer_ex(call: &ImportCall) -> u32 {
    let Some(timer) = resolve(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    let apc_routine = call.argument(SET_APC_ROUTINE);
    if apc_routine != 0 {
        status::halt_guest_thread(format!(
            "NtSetTimerEx with an APC routine ({apc_routine:#010x}) is not supported yet"
        ));
    }
    let Some(due) = deadline(call, call.argument(1)) else {
        return NtStatus::INVALID_PARAMETER.0;
    };
    let period = match call.argument(SET_PERIOD) {
        0 => None,
        milliseconds => Some(Duration::from_millis(u64::from(milliseconds))),
    };
    let previous = DISPATCHER.set_signaled(call.memory, timer, false);
    write_previous_state(call, call.argument(SET_PREVIOUS_STATE), previous);
    start_timer_thread();
    SCHEDULE.lock().unwrap().arm(timer, due, period);
    RESCHEDULED.notify_all();
    NtStatus::SUCCESS.0
}

/// NtCancelTimer(handle, current state out) -> NTSTATUS.
pub(super) fn nt_cancel_timer(call: &ImportCall) -> u32 {
    let Some(timer) = resolve(call.argument(0)) else {
        return NtStatus::INVALID_HANDLE.0;
    };
    SCHEDULE.lock().unwrap().disarm(timer);
    let current = DISPATCHER.signal_state(call.memory, timer);
    write_previous_state(call, call.argument(1), current);
    NtStatus::SUCCESS.0
}

fn write_previous_state(call: &ImportCall, pointer: u32, state: u32) {
    if pointer != 0 {
        call.memory.write_u32(pointer, state);
    }
}

fn start_timer_thread() {
    TIMER_THREAD_STARTED.call_once(|| {
        thread::Builder::new()
            .name(TIMER_THREAD_NAME.to_string())
            .spawn(timer_loop)
            .expect("cannot start the kernel timer thread");
    });
}

fn timer_loop() -> ! {
    let mut schedule = SCHEDULE.lock().unwrap();
    loop {
        let now = Instant::now();
        while let Some(entry) = schedule.due.first_entry() {
            let (due, _) = *entry.key();
            if due > now {
                break;
            }
            let (timer, period) = entry.remove();
            DISPATCHER.set_signaled(guest::memory(), timer, true);
            if let Some(period) = period {
                schedule.arm(timer, due + period, Some(period));
            }
        }
        schedule = match schedule.due.first_key_value() {
            Some((&(due, _), _)) => {
                let wait = due.saturating_duration_since(Instant::now());
                RESCHEDULED.wait_timeout(schedule, wait).unwrap().0
            }
            None => RESCHEDULED.wait(schedule).unwrap(),
        };
    }
}
