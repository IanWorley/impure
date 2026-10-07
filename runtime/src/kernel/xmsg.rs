//! Xam messages (XMsg*): requests titles send to the console's built-in apps. The music player
//! is idle with nothing playing; the game interface accepts presence and achievements, which
//! would only matter to Xbox Live. A message the runtime does not know stops the guest, since a
//! failure code could quietly send the title down an error path.

use std::sync::atomic::{AtomicU32, Ordering};

use bevy::log::info;

use super::{ImportCall, overlapped};
use crate::status;

/// The music player app and its messages, whose argument blocks all start with the client.
const MUSIC_PLAYER_APP: u32 = 0xFA;
const GET_STATUS: u32 = 0x0007_0009;
const GET_VOLUME: u32 = 0x0007_000B;
const SET_VOLUME: u32 = 0x0007_000C;
const SET_PLAYBACK_CONTROLLER: u32 = 0x0007_001A;
const GET_PLAYBACK_CONTROLLER: u32 = 0x0007_001B;
/// The game interface app: presence contexts and properties, and achievements.
const GAME_INTERFACE_APP: u32 = 0xFB;
const SET_CONTEXT: u32 = 0x000B_0006;
const SET_PROPERTY: u32 = 0x000B_0007;
const WRITE_ACHIEVEMENTS: u32 = 0x000B_0008;
/// WriteAchievements: the count, then a pointer to XUSER_ACHIEVEMENT entries (a user index and
/// an achievement ID each).
const ACHIEVEMENT_COUNT: u32 = 0x0;
const ACHIEVEMENTS: u32 = 0x4;
const ACHIEVEMENT_SIZE: u32 = 8;
const ACHIEVEMENT_ID: u32 = 4;
/// Each argument block's second and third words.
const FIRST_FIELD: u32 = 0x4;
const SECOND_FIELD: u32 = 0x8;
/// The player's state: idle, as nothing is ever played.
const IDLE: u32 = 0;
/// GetPlaybackController: the title controls playback, and nothing has locked it.
const TITLE_CONTROLLER: u32 = 0;
const NOT_LOCKED: u32 = 0;
/// The HRESULT for a handled message.
const S_OK: u32 = 0;

/// The player's volume, as f32 bits; full volume until the title sets it.
const FULL_VOLUME: f32 = 1.0;
static VOLUME: AtomicU32 = AtomicU32::new(FULL_VOLUME.to_bits());

/// XMsgStartIORequestEx(app, message, overlapped, arguments, size, unknown) -> result.
pub(super) fn start_io_request_ex(call: &ImportCall) -> u32 {
    start_io_request(call)
}

/// XMsgStartIORequest(app, message, overlapped, arguments, size) -> result.
pub(super) fn start_io_request(call: &ImportCall) -> u32 {
    let (app, message, overlapped, arguments) = (
        call.argument(0),
        call.argument(1),
        call.argument(2),
        call.argument(3),
    );
    let result = dispatch(call, app, message, arguments);
    if overlapped == 0 {
        result
    } else {
        overlapped::complete(call, overlapped, result)
    }
}

/// XMsgInProcessCall(app, message, arguments, size) -> result.
pub(super) fn in_process_call(call: &ImportCall) -> u32 {
    dispatch(call, call.argument(0), call.argument(1), call.argument(2))
}

fn dispatch(call: &ImportCall, app: u32, message: u32, arguments: u32) -> u32 {
    let memory = call.memory;
    let field = |offset| memory.read_u32(arguments + offset);
    match (app, message) {
        (MUSIC_PLAYER_APP, GET_STATUS) => memory.write_u32(field(FIRST_FIELD), IDLE),
        (MUSIC_PLAYER_APP, GET_VOLUME) => {
            memory.write_u32(field(FIRST_FIELD), VOLUME.load(Ordering::Relaxed));
        }
        (MUSIC_PLAYER_APP, SET_VOLUME) => VOLUME.store(field(FIRST_FIELD), Ordering::Relaxed),
        (MUSIC_PLAYER_APP, SET_PLAYBACK_CONTROLLER) => {}
        (MUSIC_PLAYER_APP, GET_PLAYBACK_CONTROLLER) => {
            memory.write_u32(field(FIRST_FIELD), TITLE_CONTROLLER);
            memory.write_u32(field(SECOND_FIELD), NOT_LOCKED);
        }
        (GAME_INTERFACE_APP, SET_CONTEXT | SET_PROPERTY) => {}
        (GAME_INTERFACE_APP, WRITE_ACHIEVEMENTS) => {
            let entries = field(ACHIEVEMENTS);
            for index in 0..field(ACHIEVEMENT_COUNT) {
                let id = memory.read_u32(entries + index * ACHIEVEMENT_SIZE + ACHIEVEMENT_ID);
                info!(target: "user", "achievement {id} unlocked");
            }
        }
        _ => status::halt_guest_thread(format!(
            "Xam message {message:#010x} to app {app:#x} from {:#010x} is not supported yet",
            call.return_address()
        )),
    }
    S_OK
}
