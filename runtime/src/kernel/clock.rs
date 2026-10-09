//! KeTimeStampBundle: kernel data the console's clock interrupt keeps current, which titles and
//! the libraries linked into them (the SDK's GetTickCount among them) read directly instead of
//! calling the kernel. A zero-filled block made time stand still for every timer built on it.

use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bevy::log::info;

use crate::{guest, time};

/// The console's clock interrupt runs every millisecond.
const UPDATE_PERIOD: Duration = Duration::from_millis(1);
const THREAD_NAME: &str = "time-stamp-bundle";

/// Keeps the bundle at `address` current from now on.
pub fn start(address: u32) {
    let boot = Instant::now();
    let memory = guest::memory();
    time::write_time_stamp_bundle(memory, address, boot.elapsed(), SystemTime::now());
    let started = thread::Builder::new()
        .name(THREAD_NAME.into())
        .spawn(move || {
            loop {
                thread::sleep(UPDATE_PERIOD);
                time::write_time_stamp_bundle(memory, address, boot.elapsed(), SystemTime::now());
            }
        });
    match started {
        Ok(_) => {
            info!(target: "import", "KeTimeStampBundle at {address:#010x} is kept current every millisecond")
        }
        Err(error) => info!(target: "import", "cannot keep KeTimeStampBundle current: {error}"),
    }
}
