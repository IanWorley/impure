//! System notifications (XamNotifyCreateListener, XNotifyGetNext). Each listener is a queue of
//! (notification ID, parameter) pairs. Nothing in the runtime raises notifications yet, apart
//! from the ones titles expect at startup.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{FALSE, HANDLES, ImportCall, KernelObject, TRUE};

/// A listener's mask has one bit per notification area; bit 0 is the system area.
const SYSTEM_AREA_MASK: u32 = 1;
/// System notifications, and what titles expect at startup (as Xenia found): the system UI
/// opening and closing, and two sign-in changes.
const SYSTEM_UI: u32 = 0x9;
const SYSTEM_SIGN_IN_CHANGED: u32 = 0xA;
const STARTUP_NOTIFICATIONS: [(u32, u32); 4] = [
    (SYSTEM_UI, 1),
    (SYSTEM_UI, 0),
    (SYSTEM_SIGN_IN_CHANGED, 1),
    (SYSTEM_SIGN_IN_CHANGED, 1),
];
/// XamNotifyCreateListener's result when no handle is available.
const NO_HANDLE: u32 = 0;
/// XNotifyGetNext's match ID that accepts any notification.
const ANY_NOTIFICATION: u32 = 0;

static STARTUP_DELIVERED: AtomicBool = AtomicBool::new(false);

/// Pending notifications, as (ID, parameter), oldest first.
pub(super) type Listener = Mutex<VecDeque<(u32, u32)>>;

/// XamNotifyCreateListener(area mask, version) -> handle, or 0.
pub(super) fn create_listener(call: &ImportCall) -> u32 {
    let mask = call.argument(0);
    let mut queue = VecDeque::new();
    if mask & SYSTEM_AREA_MASK != 0 && !STARTUP_DELIVERED.swap(true, Ordering::Relaxed) {
        queue.extend(STARTUP_NOTIFICATIONS);
    }
    let listener: Listener = Mutex::new(queue);
    HANDLES
        .lock()
        .unwrap()
        .insert(KernelObject::Listener(Arc::new(listener)))
        .unwrap_or(NO_HANDLE)
}

/// XNotifyGetNext(handle, match ID, ID out, parameter out) -> BOOL: whether a notification
/// (with the match ID, unless it is 0) was waiting.
pub(super) fn get_next(call: &ImportCall) -> u32 {
    let (handle, wanted, id_out, parameter_out) = (
        call.argument(0),
        call.argument(1),
        call.argument(2),
        call.argument(3),
    );
    let Some(KernelObject::Listener(listener)) = HANDLES.lock().unwrap().get(handle) else {
        return FALSE;
    };
    let mut queue = listener.lock().unwrap();
    let position = queue
        .iter()
        .position(|&(id, _)| wanted == ANY_NOTIFICATION || id == wanted);
    let Some((id, parameter)) = position.and_then(|position| queue.remove(position)) else {
        return FALSE;
    };
    call.memory.write_u32(id_out, id);
    call.memory.write_u32(parameter_out, parameter);
    TRUE
}
