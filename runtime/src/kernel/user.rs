//! User profiles (XamUser*). One local profile is signed in as the first player; it is never
//! signed in to Xbox Live, since the runtime has no network.

use bevy::log::info;

use super::{FALSE, ImportCall, overlapped};

/// The user index the profile is signed in at.
const SIGNED_IN_USER: u32 = 0;
/// XUSER_SIGNIN_STATE values.
const NOT_SIGNED_IN: u32 = 0;
const SIGNED_IN_LOCALLY: u32 = 1;

/// The profile's offline XUID: offline XUIDs have 0xE in the top nibble.
pub(super) const LOCAL_PROFILE_XUID: u64 = 0xE000_0000_0000_0001;
/// The profile's gamertag.
const GAMERTAG: &str = "Player";
/// XamUserGetXUID's type mask bit for offline XUIDs.
const OFFLINE_XUID_TYPE: u32 = 0x1;
/// Win32 error codes.
const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_NO_SUCH_USER: u32 = 0x525;

/// XUSER_READ_PROFILE_SETTINGS: the setting count and a pointer to the settings, which follow.
const READ_RESULTS_HEADER_SIZE: u32 = 8;
const READ_RESULTS_COUNT: u32 = 0x0;
const READ_RESULTS_SETTINGS: u32 = 0x4;
/// XUSER_PROFILE_SETTING: the source, the user (an index or XUID), the setting ID, and the
/// value as XUSER_DATA (a type byte, then an 8-byte-aligned value).
const SETTING_SIZE: u32 = 0x28;
const SETTING_USER: u32 = 0x8;
const SETTING_ID: u32 = 0x10;
/// A setting ID packs its data type in the top four bits and, for strings and binary data, its
/// size in bits 16-27.
const SETTING_TYPE_SHIFT: u32 = 28;
const SETTING_SIZE_SHIFT: u32 = 16;
const SETTING_SIZE_MASK: u32 = 0xFFF;
const WIDE_STRING_TYPE: u32 = 4;
const BINARY_TYPE: u32 = 6;
/// How many settings one call may read.
const MAXIMUM_SETTINGS_PER_READ: u32 = 32;
/// XamUserReadProfileSettings' argument positions; the last two arrive on the stack.
const READ_SETTING_COUNT: u32 = 4;
const READ_SETTING_IDS: u32 = 5;
const READ_BUFFER_SIZE: u32 = 6;
const READ_BUFFER: u32 = 7;
const READ_OVERLAPPED: u32 = 8;
const GUEST_WORD_SIZE: u32 = 4;
/// XUSER_SIGNIN_INFO: XUID, info flags, sign-in state, guest number, sponsor, then the name.
const SIGNIN_INFO_SIZE: u32 = 0x28;
const SIGNIN_INFO_XUID: u32 = 0x0;
const SIGNIN_INFO_STATE: u32 = 0xC;
const SIGNIN_INFO_NAME: u32 = 0x18;
const SIGNIN_INFO_NAME_CAPACITY: usize = 16;

/// XamUserGetSigninState(user) -> sign-in state.
pub(super) fn get_signin_state(call: &ImportCall) -> u32 {
    if call.argument(0) == SIGNED_IN_USER {
        SIGNED_IN_LOCALLY
    } else {
        NOT_SIGNED_IN
    }
}

/// XamUserGetXUID(user, type mask, XUID out) -> Win32 error. Only the offline XUID exists.
pub(super) fn get_xuid(call: &ImportCall) -> u32 {
    let (user, types, xuid_out) = (call.argument(0), call.argument(1), call.argument(2));
    if user != SIGNED_IN_USER || types & OFFLINE_XUID_TYPE == 0 {
        return ERROR_NO_SUCH_USER;
    }
    call.memory.write_u64(xuid_out, LOCAL_PROFILE_XUID);
    ERROR_SUCCESS
}

/// XamUserGetName(user, buffer, buffer size) -> Win32 error. Writes the gamertag NUL-terminated,
/// cut to fit the buffer.
pub(super) fn get_name(call: &ImportCall) -> u32 {
    let (user, buffer, size) = (call.argument(0), call.argument(1), call.argument(2));
    if user != SIGNED_IN_USER {
        return ERROR_NO_SUCH_USER;
    }
    let Some(room) = (size as usize).checked_sub(1) else {
        return ERROR_SUCCESS;
    };
    let name = &GAMERTAG.as_bytes()[..GAMERTAG.len().min(room)];
    call.memory.write_bytes(buffer, name);
    call.memory.write_u8(buffer + name.len() as u32, 0);
    ERROR_SUCCESS
}

/// XamUserCheckPrivilege(user, privilege, result out) -> Win32 error. Privileges are Xbox Live
/// permissions, and the profile is never signed in to Live, so none is held.
pub(super) fn check_privilege(call: &ImportCall) -> u32 {
    let (user, result_out) = (call.argument(0), call.argument(2));
    if user != SIGNED_IN_USER {
        return ERROR_NO_SUCH_USER;
    }
    call.memory.write_u32(result_out, FALSE);
    ERROR_SUCCESS
}

/// XamUserReadProfileSettings(title, user, XUID count, XUIDs, setting count, setting IDs,
/// buffer size in/out, buffer, overlapped) -> Win32 error. The profile is new, so no setting has
/// a value: each comes back with source 0 and an empty value, as for an unset setting.
pub(super) fn read_profile_settings(call: &ImportCall) -> u32 {
    let (user, xuid_count, xuids) = (call.argument(1), call.argument(2), call.argument(3));
    let (count, ids) = (
        call.argument(READ_SETTING_COUNT),
        call.argument(READ_SETTING_IDS),
    );
    let (size_pointer, buffer) = (call.argument(READ_BUFFER_SIZE), call.argument(READ_BUFFER));
    let overlapped = call.argument(READ_OVERLAPPED);
    if !(1..=MAXIMUM_SETTINGS_PER_READ).contains(&count) || size_pointer == 0 {
        return ERROR_INVALID_PARAMETER;
    }
    let ids: Vec<u32> = (0..count)
        .map(|index| call.memory.read_u32(ids + index * GUEST_WORD_SIZE))
        .collect();
    info!(target: "user", "profile settings requested: {ids:08x?}");

    // Variable-length values are sized by their IDs even when unset, so the buffer can hold them.
    let data_size: u32 = ids
        .iter()
        .filter(|&&id| matches!(id >> SETTING_TYPE_SHIFT, WIDE_STRING_TYPE | BINARY_TYPE))
        .map(|&id| (id >> SETTING_SIZE_SHIFT) & SETTING_SIZE_MASK)
        .sum();
    let needed = READ_RESULTS_HEADER_SIZE + count * SETTING_SIZE + data_size;
    let buffer_size = call.memory.read_u32(size_pointer);
    if buffer == 0 || buffer_size < needed {
        // As on the console, the needed size is only reported to a caller that asked with 0.
        if buffer_size == 0 {
            call.memory.write_u32(size_pointer, needed);
        }
        return ERROR_INSUFFICIENT_BUFFER;
    }

    let result = if xuids == 0 && user != SIGNED_IN_USER {
        ERROR_NO_SUCH_USER
    } else {
        let settings = buffer + READ_RESULTS_HEADER_SIZE;
        call.memory.write_u32(buffer + READ_RESULTS_COUNT, count);
        call.memory
            .write_u32(buffer + READ_RESULTS_SETTINGS, settings);
        for (index, &id) in (0..).zip(&ids) {
            let setting = settings + index * SETTING_SIZE;
            call.memory.zero(setting, SETTING_SIZE);
            if xuid_count == 0 {
                call.memory.write_u32(setting + SETTING_USER, user);
            } else {
                call.memory
                    .write_u64(setting + SETTING_USER, LOCAL_PROFILE_XUID);
            }
            call.memory.write_u32(setting + SETTING_ID, id);
        }
        ERROR_SUCCESS
    };
    if overlapped == 0 {
        result
    } else {
        overlapped::complete(call, overlapped, result)
    }
}

/// XamUserGetSigninInfo(user, flags, information out) -> Win32 error.
pub(super) fn get_signin_info(call: &ImportCall) -> u32 {
    let (user, info) = (call.argument(0), call.argument(2));
    if user != SIGNED_IN_USER {
        return ERROR_NO_SUCH_USER;
    }
    let memory = call.memory;
    memory.zero(info, SIGNIN_INFO_SIZE);
    memory.write_u64(info + SIGNIN_INFO_XUID, LOCAL_PROFILE_XUID);
    memory.write_u32(info + SIGNIN_INFO_STATE, SIGNED_IN_LOCALLY);
    // The zeroed field keeps the name NUL-terminated.
    let name = &GAMERTAG.as_bytes()[..GAMERTAG.len().min(SIGNIN_INFO_NAME_CAPACITY - 1)];
    memory.write_bytes(info + SIGNIN_INFO_NAME, name);
    ERROR_SUCCESS
}
