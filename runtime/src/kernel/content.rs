//! Storage devices and content packages (XamShowDeviceSelectorUI, XamContent*). The runtime
//! offers one device, the hard drive, for the title's saves. Each content package is a host
//! directory under the runtime's data directory:
//! `content/<XUID>/<title ID>/<content type>/<file name>/`, with its display name in a sidecar
//! file beside it.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bevy::log::info;

use super::{HANDLES, ImportCall, KernelObject, files, overlapped};
use crate::{guest, xex_header};

/// The device ID the runtime gives the hard drive.
pub(super) const HARD_DRIVE_DEVICE_ID: u32 = 1;
/// XamShowDeviceSelectorUI's arguments: user, content type, content flags, space requested (a
/// 64-bit value in one register), device ID out, overlapped.
const SELECTOR_DEVICE_ID_OUT: u32 = 4;
const SELECTOR_OVERLAPPED: u32 = 5;
/// XamContentCreateEnumerator's buffer size out and handle out arguments.
const ENUMERATOR_BUFFER_SIZE_OUT: u32 = 5;
const ENUMERATOR_HANDLE_OUT: u32 = 6;
/// Any device, when a title enumerates content.
const ANY_DEVICE: u32 = 0;
/// Win32 error codes.
const ERROR_SUCCESS: u32 = 0;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// XCONTENT_DATA: device, content type, a 128-character UTF-16 display name and a 42-byte
/// file name.
const CONTENT_DATA_SIZE: usize = 0x134;
const CONTENT_DEVICE_ID: usize = 0x0;
const CONTENT_TYPE: usize = 0x4;
const CONTENT_DISPLAY_NAME: usize = 0x8;
const DISPLAY_NAME_CHARACTERS: usize = 128;
const CONTENT_FILE_NAME: usize = 0x108;
const FILE_NAME_BYTES: usize = 42;
/// The sidecar beside a package directory that holds its display name.
const DISPLAY_NAME_SUFFIX: &str = ".display_name";

/// XEX_HEADER_EXECUTION_INFO, whose title ID the content directories are named after.
const EXECUTION_INFO_KEY: u32 = 0x0004_0006;
const EXECUTION_INFO_TITLE_ID: u32 = 0xC;
/// Where the runtime keeps saves, unless PURE_DATA_DIR says otherwise.
const DATA_DIRECTORY_ENVIRONMENT: &str = "PURE_DATA_DIR";
const DATA_DIRECTORY_NAME: &str = "pure-runtime";
const CONTENT_DIRECTORY: &str = "content";

/// XCONTENT_DEVICE_DATA: device ID, device type, total and free bytes, then a 28-character
/// UTF-16 name.
const DEVICE_DATA_SIZE: u32 = 0x50;
const DEVICE_DATA_ID: u32 = 0x0;
const DEVICE_DATA_TYPE: u32 = 0x4;
const DEVICE_DATA_TOTAL_BYTES: u32 = 0x8;
const DEVICE_DATA_FREE_BYTES: u32 = 0x10;
const DEVICE_DATA_NAME: u32 = 0x18;
const DEVICE_NAME_CHARACTERS: usize = 28;
const HARD_DRIVE_DEVICE_TYPE: u32 = 1;
/// The hard drive the runtime reports: a launch-era 20 GiB drive with plenty free, the same on
/// every host.
const HARD_DRIVE_TOTAL_BYTES: u64 = 20 << 30;
const HARD_DRIVE_FREE_BYTES: u64 = 16 << 30;
const HARD_DRIVE_NAME: &str = "Hard Drive";
const ERROR_DEVICE_NOT_CONNECTED: u32 = 0x48F;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_ALREADY_EXISTS: u32 = 183;

/// XamContentCreateEx's arguments: user, root name, content data, flags, disposition out,
/// license mask out, cache size, content size (64 bits in one register), overlapped.
const CREATE_ROOT_NAME: u32 = 1;
const CREATE_CONTENT_DATA: u32 = 2;
const CREATE_FLAGS: u32 = 3;
const CREATE_DISPOSITION_OUT: u32 = 4;
const CREATE_LICENSE_MASK_OUT: u32 = 5;
const CREATE_OVERLAPPED: u32 = 8;
/// The flags' low nibble says what to do when the package does or does not exist.
const CREATE_MODE_MASK: u32 = 0xF;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;
/// The disposition reported back: the package was created, or an existing one opened.
const CREATED: u32 = 1;
const OPENED: u32 = 2;
/// Content is not licensed per user or console in the runtime.
const NO_LICENSE_MASK: u32 = 0;
/// Root names are mounted as `<name>:`, like the disc's `game:`.
const DEVICE_SUFFIX: char = ':';
const MAXIMUM_ROOT_NAME_LENGTH: u32 = 64;

/// An enumeration in progress: fixed-size items, handed out a few per XamEnumerate call.
pub(super) struct Enumerator {
    items: Vec<Vec<u8>>,
    items_per_call: usize,
    next: usize,
}

/// The directory the runtime keeps the title's saves under.
fn data_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os(DATA_DIRECTORY_ENVIRONMENT) {
        return PathBuf::from(directory);
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local").join("share"))
    };
    base.join(DATA_DIRECTORY_NAME)
}

fn title_id() -> u32 {
    let memory = guest::memory();
    xex_header::field(memory, guest::xex_header(), EXECUTION_INFO_KEY)
        .map_or(0, |info| memory.read_u32(info + EXECUTION_INFO_TITLE_ID))
}

/// The directory holding the profile's packages of `content_type`.
pub(super) fn content_type_directory(content_type: u32) -> PathBuf {
    data_directory()
        .join(CONTENT_DIRECTORY)
        .join(format!("{:016X}", super::user::LOCAL_PROFILE_XUID))
        .join(format!("{:08X}", title_id()))
        .join(format!("{content_type:08X}"))
}

/// XamShowDeviceSelectorUI(...) -> Win32 error. With one device there is nothing to choose, so
/// the "selector" answers at once with the hard drive.
pub(super) fn show_device_selector_ui(call: &ImportCall) -> u32 {
    call.memory
        .write_u32(call.argument(SELECTOR_DEVICE_ID_OUT), HARD_DRIVE_DEVICE_ID);
    match call.argument(SELECTOR_OVERLAPPED) {
        0 => ERROR_SUCCESS,
        overlapped => overlapped::complete(call, overlapped, ERROR_SUCCESS),
    }
}

/// XamContentCreateEnumerator(user, device, content type, flags, items per call, buffer size
/// out, handle out) -> Win32 error. Lists the profile's packages of that type on the hard drive.
pub(super) fn create_enumerator(call: &ImportCall) -> u32 {
    let (device, content_type, items_per_call) = (
        call.argument(1),
        call.argument(2),
        call.argument(4) as usize,
    );
    let (size_out, handle_out) = (
        call.argument(ENUMERATOR_BUFFER_SIZE_OUT),
        call.argument(ENUMERATOR_HANDLE_OUT),
    );
    if !matches!(device, ANY_DEVICE | HARD_DRIVE_DEVICE_ID) || handle_out == 0 {
        return ERROR_INVALID_PARAMETER;
    }
    if size_out != 0 {
        call.memory
            .write_u32(size_out, (CONTENT_DATA_SIZE * items_per_call) as u32);
    }
    let directory = content_type_directory(content_type);
    let mut names: Vec<String> = fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    info!(target: "content", "{} packages of type {content_type:#x} in {}", names.len(), directory.display());
    let items = names
        .iter()
        .map(|name| {
            let display_name =
                fs::read_to_string(directory.join(format!("{name}{DISPLAY_NAME_SUFFIX}")))
                    .unwrap_or_else(|_| name.clone());
            content_data(content_type, &display_name, name)
        })
        .collect();
    let enumerator = Enumerator {
        items,
        items_per_call: items_per_call.max(1),
        next: 0,
    };
    let Some(handle) = HANDLES
        .lock()
        .unwrap()
        .insert(KernelObject::Enumerator(Arc::new(Mutex::new(enumerator))))
    else {
        return ERROR_INVALID_PARAMETER;
    };
    call.memory.write_u32(handle_out, handle);
    ERROR_SUCCESS
}

/// An XCONTENT_DATA for a package on the hard drive.
pub(super) fn content_data(content_type: u32, display_name: &str, file_name: &str) -> Vec<u8> {
    let mut data = vec![0; CONTENT_DATA_SIZE];
    data[CONTENT_DEVICE_ID..CONTENT_DEVICE_ID + 4]
        .copy_from_slice(&HARD_DRIVE_DEVICE_ID.to_be_bytes());
    data[CONTENT_TYPE..CONTENT_TYPE + 4].copy_from_slice(&content_type.to_be_bytes());
    // The terminating NUL comes from the zeroed buffer, so leave room for it.
    for (index, unit) in display_name
        .encode_utf16()
        .take(DISPLAY_NAME_CHARACTERS - 1)
        .enumerate()
    {
        let offset = CONTENT_DISPLAY_NAME + index * 2;
        data[offset..offset + 2].copy_from_slice(&unit.to_be_bytes());
    }
    let name = &file_name.as_bytes()[..file_name.len().min(FILE_NAME_BYTES - 1)];
    data[CONTENT_FILE_NAME..CONTENT_FILE_NAME + name.len()].copy_from_slice(name);
    data
}

/// XamEnumerate(handle, flags, buffer, buffer size, items returned out, overlapped) -> Win32
/// error. Writes as many items as fit, up to the enumerator's per-call count.
pub(super) fn enumerate(call: &ImportCall) -> u32 {
    let (handle, buffer, buffer_size) = (call.argument(0), call.argument(2), call.argument(3));
    let (returned_out, overlapped) = (call.argument(4), call.argument(5));
    let Some(KernelObject::Enumerator(enumerator)) = HANDLES.lock().unwrap().get(handle) else {
        return ERROR_INVALID_PARAMETER;
    };
    let mut enumerator = enumerator.lock().unwrap();
    let mut written = 0u32;
    let mut offset = 0usize;
    while (written as usize) < enumerator.items_per_call && enumerator.next < enumerator.items.len()
    {
        let item = &enumerator.items[enumerator.next];
        if offset + item.len() > buffer_size as usize {
            break;
        }
        call.memory.write_bytes(buffer + offset as u32, item);
        offset += item.len();
        written += 1;
        enumerator.next += 1;
    }
    let result = if written == 0 {
        ERROR_NO_MORE_FILES
    } else {
        ERROR_SUCCESS
    };
    if returned_out != 0 {
        call.memory.write_u32(returned_out, written);
        return result;
    }
    if overlapped == 0 {
        return result;
    }
    overlapped::complete_with_length(call, overlapped, result, written)
}

/// XamContentGetDeviceData(device, data out) -> Win32 error.
pub(super) fn get_device_data(call: &ImportCall) -> u32 {
    let (device, data) = (call.argument(0), call.argument(1));
    if device != HARD_DRIVE_DEVICE_ID {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let memory = call.memory;
    memory.zero(data, DEVICE_DATA_SIZE);
    memory.write_u32(data + DEVICE_DATA_ID, HARD_DRIVE_DEVICE_ID);
    memory.write_u32(data + DEVICE_DATA_TYPE, HARD_DRIVE_DEVICE_TYPE);
    memory.write_u64(data + DEVICE_DATA_TOTAL_BYTES, HARD_DRIVE_TOTAL_BYTES);
    memory.write_u64(data + DEVICE_DATA_FREE_BYTES, HARD_DRIVE_FREE_BYTES);
    // The zeroed field keeps the name NUL-terminated.
    for (index, unit) in (0..).zip(
        HARD_DRIVE_NAME
            .encode_utf16()
            .take(DEVICE_NAME_CHARACTERS - 1),
    ) {
        memory.write_u16(data + DEVICE_DATA_NAME + index * 2, unit);
    }
    ERROR_SUCCESS
}

/// XamContentCreateEx(user, root name, content data, flags, disposition out, license mask out,
/// cache size, content size, overlapped) -> Win32 error. Creates or opens the package and
/// mounts it writable at `<root name>:`.
pub(super) fn create_ex(call: &ImportCall) -> u32 {
    let memory = call.memory;
    let root_name = read_c_string(call, call.argument(CREATE_ROOT_NAME));
    let data = call.argument(CREATE_CONTENT_DATA);
    let content_type = memory.read_u32(data + CONTENT_TYPE as u32);
    let file_name = read_c_string(call, data + CONTENT_FILE_NAME as u32);
    let display_name: String = char::decode_utf16(
        (0..DISPLAY_NAME_CHARACTERS as u32)
            .map(|index| memory.read_u16(data + CONTENT_DISPLAY_NAME as u32 + index * 2))
            .take_while(|&unit| unit != 0),
    )
    .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect();

    let directory = content_type_directory(content_type);
    let package = directory.join(&file_name);
    let exists = package.is_dir();
    let mode = call.argument(CREATE_FLAGS) & CREATE_MODE_MASK;
    let outcome = match mode {
        CREATE_NEW if exists => Err(ERROR_ALREADY_EXISTS),
        OPEN_EXISTING | TRUNCATE_EXISTING if !exists => Err(ERROR_PATH_NOT_FOUND),
        OPEN_EXISTING | OPEN_ALWAYS if exists => Ok(OPENED),
        CREATE_ALWAYS | TRUNCATE_EXISTING if exists => fs::remove_dir_all(&package)
            .map(|_| CREATED)
            .map_err(|_| ERROR_ACCESS_DENIED),
        CREATE_NEW | CREATE_ALWAYS | OPEN_ALWAYS => Ok(CREATED),
        _ => Err(ERROR_INVALID_PARAMETER),
    };
    let outcome = outcome.and_then(|disposition| {
        if disposition == CREATED {
            fs::create_dir_all(&package).map_err(|_| ERROR_ACCESS_DENIED)?;
            let sidecar = directory.join(format!("{file_name}{DISPLAY_NAME_SUFFIX}"));
            fs::write(sidecar, &display_name).map_err(|_| ERROR_ACCESS_DENIED)?;
        }
        Ok(disposition)
    });
    info!(target: "content", "{root_name}: {file_name:?} ({display_name:?}) mode {mode} -> {outcome:?}");

    let (result, disposition) = match outcome {
        Ok(disposition) => {
            files::mount_writable(&format!("{root_name}{DEVICE_SUFFIX}"), package);
            let license_out = call.argument(CREATE_LICENSE_MASK_OUT);
            if license_out != 0 {
                memory.write_u32(license_out, NO_LICENSE_MASK);
            }
            (ERROR_SUCCESS, disposition)
        }
        Err(error) => (error, 0),
    };
    let disposition_out = call.argument(CREATE_DISPOSITION_OUT);
    if disposition_out != 0 {
        memory.write_u32(disposition_out, disposition);
    }
    match call.argument(CREATE_OVERLAPPED) {
        0 => result,
        overlapped => overlapped::complete_with_length(call, overlapped, result, disposition),
    }
}

/// XamContentClose(root name, overlapped) -> Win32 error. Unmounts the package.
pub(super) fn close(call: &ImportCall) -> u32 {
    let root_name = read_c_string(call, call.argument(0));
    let result = if files::unmount(&format!("{root_name}{DEVICE_SUFFIX}")) {
        ERROR_SUCCESS
    } else {
        ERROR_PATH_NOT_FOUND
    };
    info!(target: "content", "{root_name}: closed -> {result}");
    match call.argument(1) {
        0 => result,
        overlapped => overlapped::complete(call, overlapped, result),
    }
}

fn read_c_string(call: &ImportCall, address: u32) -> String {
    let bytes: Vec<u8> = (0..MAXIMUM_ROOT_NAME_LENGTH)
        .map(|offset| call.memory.read_u8(address + offset))
        .take_while(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
