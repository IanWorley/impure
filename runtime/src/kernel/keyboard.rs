//! The on-screen keyboard (XamShowKeyboardUI), which titles show to have the player type a name.
//! The import records the request and leaves its overlapped call pending; the window shows the
//! dialog (src/keyboard_dialog.rs) and finishes the call with the player's text, or cancels it.
//! The title polls the overlapped block meanwhile, so its threads keep running.

use std::sync::Mutex;

use bevy::log::info;

use super::{ImportCall, overlapped};
use crate::guest;
use crate::memory::GuestMemory;

/// XamShowKeyboardUI's arguments: user, flags, default text, title, description, the buffer
/// for the text, its length in characters (counting the terminating NUL), and the overlapped.
const DEFAULT_TEXT: u32 = 2;
const TITLE: u32 = 3;
const DESCRIPTION: u32 = 4;
const BUFFER: u32 = 5;
const BUFFER_LENGTH: u32 = 6;
const OVERLAPPED: u32 = 7;
/// Win32 error codes.
const ERROR_SUCCESS: u32 = 0;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_CANCELLED: u32 = 1223;
/// The title's strings are NUL-terminated UTF-16; longer prompts are cut.
const MAXIMUM_PROMPT_UNITS: u32 = 1024;
const UTF16_UNIT_SIZE: u32 = 2;
const NUL: u16 = 0;

/// What the dialog shows, and how much text the title can take back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardPrompt {
    pub title: String,
    pub description: String,
    pub default_text: String,
    /// The most UTF-16 units the title's buffer holds before its terminating NUL.
    pub maximum_length: usize,
}

/// The request the player is answering, and where its answer goes.
struct Request {
    prompt: KeyboardPrompt,
    buffer: u32,
    overlapped: u32,
}

/// The console shows one keyboard at a time.
static OPEN: Mutex<Option<Request>> = Mutex::new(None);

/// XamShowKeyboardUI(user, flags, default text, title, description, buffer, buffer length,
/// overlapped) -> Win32 error. The keyboard is a system dialog that titles poll, so the call
/// needs an overlapped block (Pure always passes one) and completes when the player is done.
pub(super) fn show_keyboard_ui(call: &ImportCall) -> u32 {
    let (buffer, length, overlapped) = (
        call.argument(BUFFER),
        call.argument(BUFFER_LENGTH),
        call.argument(OVERLAPPED),
    );
    if buffer == 0 || length == 0 || overlapped == 0 {
        return ERROR_INVALID_PARAMETER;
    }
    if OPEN.lock().unwrap().is_some() {
        return ERROR_ACCESS_DENIED;
    }
    let memory = call.memory;
    let prompt = KeyboardPrompt {
        title: read_wide_string(memory, call.argument(TITLE)),
        description: read_wide_string(memory, call.argument(DESCRIPTION)),
        default_text: read_wide_string(memory, call.argument(DEFAULT_TEXT)),
        maximum_length: (length - 1) as usize,
    };
    info!(target: "keyboard", "shown: {:?} {:?} (default {:?}, up to {} characters)", prompt.title, prompt.description, prompt.default_text, prompt.maximum_length);
    // Started before the request is recorded: a call the runtime cannot finish stops here.
    let result = overlapped::start(call, overlapped);
    *OPEN.lock().unwrap() = Some(Request {
        prompt,
        buffer,
        overlapped,
    });
    result
}

/// The keyboard the title is waiting on, if any.
pub fn keyboard_prompt() -> Option<KeyboardPrompt> {
    OPEN.lock()
        .unwrap()
        .as_ref()
        .map(|request| request.prompt.clone())
}

/// Finishes the open keyboard with the player's text, or cancels it with `None`, leaving the
/// title's buffer as it was.
pub fn finish_keyboard(text: Option<&str>) {
    let Some(request) = OPEN.lock().unwrap().take() else {
        return;
    };
    let memory = guest::memory();
    let result = match text {
        Some(text) => {
            write_wide_string(memory, request.buffer, text, request.prompt.maximum_length);
            info!(target: "keyboard", "accepted {text:?}");
            ERROR_SUCCESS
        }
        None => {
            info!(target: "keyboard", "cancelled");
            ERROR_CANCELLED
        }
    };
    overlapped::finish(memory, request.overlapped, result, 0);
}

fn read_wide_string(memory: &GuestMemory, address: u32) -> String {
    if address == 0 {
        return String::new();
    }
    let units = (0..MAXIMUM_PROMPT_UNITS)
        .map(|index| memory.read_u16(address + index * UTF16_UNIT_SIZE))
        .take_while(|&unit| unit != NUL);
    char::decode_utf16(units)
        .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Writes `text` NUL-terminated, cut to `maximum_length` units.
fn write_wide_string(memory: &GuestMemory, address: u32, text: &str, maximum_length: usize) {
    let mut end = address;
    for unit in text.encode_utf16().take(maximum_length) {
        memory.write_u16(end, unit);
        end += UTF16_UNIT_SIZE;
    }
    memory.write_u16(end, NUL);
}
