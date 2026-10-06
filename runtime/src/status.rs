//! Why guest execution stopped, shared between guest threads and the Bevy app.

use std::sync::OnceLock;

static STOP_REASON: OnceLock<String> = OnceLock::new();

/// Records the first reason guest execution stopped; later reasons are logged but not kept.
pub fn report_stop(reason: String) {
    bevy::log::error!("guest stopped: {reason}");
    let _ = STOP_REASON.set(reason);
}

/// Stops the calling guest thread for good. Unwinding through generated C++ frames is not
/// possible, so the thread parks instead of returning.
#[cfg(feature = "recompiled")]
pub fn halt_guest_thread(reason: String) -> ! {
    report_stop(reason);
    loop {
        std::thread::park();
    }
}

pub fn stop_reason() -> Option<&'static str> {
    STOP_REASON.get().map(String::as_str)
}
