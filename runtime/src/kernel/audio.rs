//! Audio driver (XAudio*) imports. The console's audio driver asks each registered client for
//! another frame of samples by calling its callback; the client answers with
//! XAudioSubmitRenderDriverFrame. The runtime paces those callbacks at the console's frame rate
//! from its own guest thread. Samples are not played yet; submitted frames are only counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use bevy::log::info;

use super::{ImportCall, NO_RETURN_VALUE, threads};
use crate::guest::{self, ThreadEnvironment};
use crate::status;

/// The console mixes 256 samples per frame at 48 kHz.
const SAMPLES_PER_FRAME: u64 = 256;
const SAMPLE_RATE_HZ: u64 = 48_000;
const FRAME_PERIOD: Duration =
    Duration::from_nanos(SAMPLES_PER_FRAME * 1_000_000_000 / SAMPLE_RATE_HZ);
/// The driver supports this many clients; a client's driver handle is this tag plus its index.
const MAXIMUM_CLIENTS: usize = 8;
const DRIVER_HANDLE_TAG: u32 = 0x4155_0000;
const DRIVER_INDEX_MASK: u32 = 0xFFFF;
/// The registration the title passes: its callback, then the callback's argument.
const REGISTRATION_ARGUMENT: u32 = 0x4;
/// Win32 error codes.
const ERROR_SUCCESS: u32 = 0;
const ERROR_ACCESS_DENIED: u32 = 5;
const AUDIO_THREAD_NAME: &str = "audio-driver";
const AUDIO_STACK_SIZE: u32 = 0x1_0000;

/// Each registered client's callback and argument, by index.
static CLIENTS: Mutex<[Option<(u32, u32)>; MAXIMUM_CLIENTS]> = Mutex::new([None; MAXIMUM_CLIENTS]);
static DRIVER_STARTED: Once = Once::new();
static FRAMES_SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// The speaker configuration the runtime reports (Xenia's default).
const SPEAKER_CONFIGURATION: u32 = 0x0001_0001;
/// No voice category's volume has changed, and every category plays at full volume.
const NO_CATEGORY_CHANGES: u32 = 0;
const FULL_VOLUME: f32 = 1.0;

/// XAudioGetSpeakerConfig(configuration out) -> Win32 error.
pub(super) fn get_speaker_config(call: &ImportCall) -> u32 {
    call.memory
        .write_u32(call.argument(0), SPEAKER_CONFIGURATION);
    ERROR_SUCCESS
}

/// XAudioGetVoiceCategoryVolumeChangeMask(driver, mask out) -> Win32 error.
pub(super) fn get_voice_category_volume_change_mask(call: &ImportCall) -> u32 {
    call.memory.write_u32(call.argument(1), NO_CATEGORY_CHANGES);
    ERROR_SUCCESS
}

/// XAudioGetVoiceCategoryVolume(category, volume out) -> Win32 error.
pub(super) fn get_voice_category_volume(call: &ImportCall) -> u32 {
    call.memory
        .write_u32(call.argument(1), FULL_VOLUME.to_bits());
    ERROR_SUCCESS
}

/// XAudioRegisterRenderDriverClient(registration, driver handle out) -> Win32 error.
pub(super) fn register_render_driver_client(call: &ImportCall) -> u32 {
    let (registration, handle_out) = (call.argument(0), call.argument(1));
    let callback = call.memory.read_u32(registration);
    let argument = call.memory.read_u32(registration + REGISTRATION_ARGUMENT);
    let mut clients = CLIENTS.lock().unwrap();
    let Some(index) = clients.iter().position(Option::is_none) else {
        return ERROR_ACCESS_DENIED;
    };
    clients[index] = Some((callback, argument));
    drop(clients);
    call.memory
        .write_u32(handle_out, DRIVER_HANDLE_TAG | index as u32);
    info!(target: "audio", "client {index}: callback {callback:#010x}({argument:#x})");

    DRIVER_STARTED.call_once(|| {
        let started = threads::prepare_system_thread(AUDIO_STACK_SIZE).and_then(|environment| {
            guest::spawn(AUDIO_THREAD_NAME.to_string(), move || {
                driver_loop(environment)
            })
        });
        if let Err(error) = started {
            status::halt_guest_thread(format!("cannot start the audio driver: {error}"));
        }
    });
    ERROR_SUCCESS
}

/// XAudioUnregisterRenderDriverClient(driver handle).
pub(super) fn unregister_render_driver_client(call: &ImportCall) -> u32 {
    let index = (call.argument(0) & DRIVER_INDEX_MASK) as usize;
    if let Some(client) = CLIENTS.lock().unwrap().get_mut(index) {
        *client = None;
    }
    NO_RETURN_VALUE
}

/// XAudioSubmitRenderDriverFrame(driver handle, samples) -> Win32 error. Not played yet.
pub(super) fn submit_render_driver_frame(_: &ImportCall) -> u32 {
    if FRAMES_SUBMITTED.fetch_add(1, Ordering::Relaxed) == 0 {
        info!(target: "audio", "first audio frame submitted");
    }
    ERROR_SUCCESS
}

/// Asks every client for a frame each frame period, as the console's driver does.
fn driver_loop(environment: ThreadEnvironment) -> ! {
    // Advancing the deadline by whole periods keeps the rate exact however long callbacks take.
    let mut due = Instant::now();
    loop {
        due += FRAME_PERIOD;
        thread::sleep(due.saturating_duration_since(Instant::now()));
        let clients = *CLIENTS.lock().unwrap();
        for (callback, argument) in clients.into_iter().flatten() {
            guest::call(environment, callback, [argument, 0]);
        }
    }
}
