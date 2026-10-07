//! Imports that describe the console and the title's process, answered with fixed values.

use super::{ImportCall, NO_RETURN_VALUE, NtStatus};
use crate::{status, time};

/// KeGetCurrentProcessType: titles run as the user process (0 is idle, 2 is system).
const USER_PROCESS_TYPE: u32 = 1;
/// XGetAVPack: the video cable. HDMI, as Xenia reports by default.
const HDMI_AV_PACK: u32 = 8;

/// XCONFIG categories and settings, by (category, setting).
const USER_CATEGORY: u16 = 3;
/// The time zone settings: bias in minutes, standard and daylight names, change dates and
/// biases. The runtime reports UTC with no daylight saving, matching the UTC system time.
const USER_TIME_ZONE_BIAS: u16 = 0x1;
const USER_TIME_ZONE_STANDARD_NAME: u16 = 0x2;
const USER_TIME_ZONE_DAYLIGHT_NAME: u16 = 0x3;
const USER_TIME_ZONE_STANDARD_DATE: u16 = 0x4;
const USER_TIME_ZONE_DAYLIGHT_DATE: u16 = 0x5;
const USER_TIME_ZONE_STANDARD_BIAS: u16 = 0x6;
const USER_TIME_ZONE_DAYLIGHT_BIAS: u16 = 0x7;
const UTC_NAME: [u8; 4] = *b"UTC\0";
const NO_CHANGE_DATE: [u8; 4] = [0; 4];
const NO_BIAS: u32 = 0;
const USER_LANGUAGE: u16 = 0x9;
const USER_VIDEO_FLAGS: u16 = 0xA;
const USER_COUNTRY: u16 = 0xE;
/// The user's video flags, as Xenia reports them; consistent with the widescreen 720p mode the
/// video imports report.
const USER_VIDEO_FLAGS_VALUE: u32 = 0x0004_0000;
/// The user's retail flags: none set (as Xenia reports).
const USER_RETAIL_FLAGS: u16 = 0xC;
const NO_RETAIL_FLAGS: u32 = 0;
/// The console's country, a one-byte code: the United States (Xenia's default).
const UNITED_STATES: u8 = 103;
/// The console's language: English.
const ENGLISH: u32 = 1;

/// The console settings the runtime answers, as big-endian bytes.
fn config_setting(category: u16, setting: u16) -> Option<Vec<u8>> {
    match (category, setting) {
        (USER_CATEGORY, USER_VIDEO_FLAGS) => Some(USER_VIDEO_FLAGS_VALUE.to_be_bytes().to_vec()),
        (USER_CATEGORY, USER_RETAIL_FLAGS) => Some(NO_RETAIL_FLAGS.to_be_bytes().to_vec()),
        (USER_CATEGORY, USER_COUNTRY) => Some(vec![UNITED_STATES]),
        (
            USER_CATEGORY,
            USER_TIME_ZONE_BIAS | USER_TIME_ZONE_STANDARD_BIAS | USER_TIME_ZONE_DAYLIGHT_BIAS,
        ) => Some(NO_BIAS.to_be_bytes().to_vec()),
        (USER_CATEGORY, USER_TIME_ZONE_STANDARD_NAME | USER_TIME_ZONE_DAYLIGHT_NAME) => {
            Some(UTC_NAME.to_vec())
        }
        (USER_CATEGORY, USER_TIME_ZONE_STANDARD_DATE | USER_TIME_ZONE_DAYLIGHT_DATE) => {
            Some(NO_CHANGE_DATE.to_vec())
        }
        (USER_CATEGORY, USER_LANGUAGE) => Some(ENGLISH.to_be_bytes().to_vec()),
        _ => None,
    }
}

/// ExGetXConfigSetting(category, setting, buffer, buffer size, required size out) -> NTSTATUS.
/// A setting the runtime does not know stops the guest: a guessed value could silently
/// misconfigure the title.
pub(super) fn get_xconfig_setting(call: &ImportCall) -> u32 {
    let (category, setting) = (call.argument(0) as u16, call.argument(1) as u16);
    let (buffer, buffer_size, required_size) =
        (call.argument(2), call.argument(3) as u16, call.argument(4));
    let Some(value) = config_setting(category, setting) else {
        status::halt_guest_thread(format!(
            "ExGetXConfigSetting asked for unknown setting {setting:#x} in category {category:#x}"
        ));
    };
    let size = value.len() as u16;
    if required_size != 0 {
        call.memory.write_u16(required_size, size);
    }
    if buffer != 0 {
        if buffer_size < size {
            return NtStatus::BUFFER_TOO_SMALL.0;
        }
        call.memory.write_bytes(buffer, &value);
    }
    NtStatus::SUCCESS.0
}

/// The system version the runtime reports: the console's final system update, 2.0.17559.0,
/// packed as major (4 bits), minor (4 bits), build (16 bits) and QFE (8 bits).
const SYSTEM_VERSION: (u32, u32, u32, u32) = (2, 0, 17559, 0);

/// XamGetSystemVersion() -> packed system version.
pub(super) fn system_version(_: &ImportCall) -> u32 {
    let (major, minor, build, qfe) = SYSTEM_VERSION;
    (major << 28) | (minor << 24) | (build << 8) | qfe
}

/// KeGetCurrentProcessType() -> process type.
pub(super) fn current_process_type(_: &ImportCall) -> u32 {
    USER_PROCESS_TYPE
}

/// XGetGameRegion: the console's region, North America, consistent with its country.
const NORTH_AMERICA_REGION: u32 = 0x00FF;

/// XGetGameRegion() -> region.
pub(super) fn game_region(_: &ImportCall) -> u32 {
    NORTH_AMERICA_REGION
}

/// XGetAVPack() -> cable type.
pub(super) fn av_pack(_: &ImportCall) -> u32 {
    HDMI_AV_PACK
}

/// ExRegisterTitleTerminateNotification(registration, register or unregister). The runtime never
/// ends the title gracefully, so there is no notification to deliver.
pub(super) fn register_title_terminate_notification(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// KiApcNormalRoutineNop(arguments...). The kernel's do-nothing APC routine.
pub(super) fn apc_normal_routine_nop(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// KeQuerySystemTime(time out): the current time in 100 ns units since 1601.
pub(super) fn query_system_time(call: &ImportCall) -> u32 {
    call.memory.write_u64(call.argument(0), time::now());
    NO_RETURN_VALUE
}
