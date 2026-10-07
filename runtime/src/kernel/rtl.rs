//! Run-time library imports.

use super::{FALSE, ImportCall, NO_RETURN_VALUE, NtStatus, TRUE};
use crate::critical_section::CriticalSections;
use crate::guest;
use crate::time;

static CRITICAL_SECTIONS: CriticalSections = CriticalSections::new();
/// ANSI_STRING and UNICODE_STRING: length and capacity in bytes, then the buffer pointer.
const STRING_LENGTH: u32 = 0x0;
const STRING_MAXIMUM_LENGTH: u32 = 0x2;
const STRING_BUFFER: u32 = 0x4;

/// RtlInitializeCriticalSection takes no spin count; the guest's waits do not spin anyway.
const DEFAULT_SPIN_COUNT: u32 = 0;

/// RtlInitializeCriticalSection(section).
pub(super) fn initialize_critical_section(call: &ImportCall) -> u32 {
    CRITICAL_SECTIONS.initialize(call.memory, call.argument(0), DEFAULT_SPIN_COUNT);
    NO_RETURN_VALUE
}

/// RtlInitializeCriticalSectionAndSpinCount(section, spin count) -> NTSTATUS.
pub(super) fn initialize_critical_section_and_spin_count(call: &ImportCall) -> u32 {
    CRITICAL_SECTIONS.initialize(call.memory, call.argument(0), call.argument(1));
    NtStatus::SUCCESS.0
}

/// RtlEnterCriticalSection(section).
pub(super) fn enter_critical_section(call: &ImportCall) -> u32 {
    CRITICAL_SECTIONS.enter(call.memory, call.argument(0), guest::current_thread());
    NO_RETURN_VALUE
}

/// RtlTryEnterCriticalSection(section) -> BOOLEAN.
pub(super) fn try_enter_critical_section(call: &ImportCall) -> u32 {
    if CRITICAL_SECTIONS.try_enter(call.memory, call.argument(0), guest::current_thread()) {
        TRUE
    } else {
        FALSE
    }
}

/// RtlLeaveCriticalSection(section).
pub(super) fn leave_critical_section(call: &ImportCall) -> u32 {
    CRITICAL_SECTIONS.leave(call.memory, call.argument(0));
    NO_RETURN_VALUE
}

/// RtlInitAnsiString(string out, NUL-terminated source).
pub(super) fn init_ansi_string(call: &ImportCall) -> u32 {
    init_counted_string(call, size_of::<u8>() as u32, |address| {
        call.memory.read_u8(address) == 0
    })
}

/// RtlInitUnicodeString(string out, NUL-terminated UTF-16 source).
pub(super) fn init_unicode_string(call: &ImportCall) -> u32 {
    init_counted_string(call, size_of::<u16>() as u32, |address| {
        call.memory.read_u16(address) == 0
    })
}

/// Points a counted string at a NUL-terminated source of `unit`-byte characters. The length
/// excludes the terminator and the maximum length includes it; a null source gives an empty
/// string. Lengths are 16-bit, so longer sources are cut off, as on the console.
fn init_counted_string(call: &ImportCall, unit: u32, is_terminator: impl Fn(u32) -> bool) -> u32 {
    let (string, source) = (call.argument(0), call.argument(1));
    let (length, maximum_length) = if source == 0 {
        (0, 0)
    } else {
        let units = (0..)
            .find(|&index| is_terminator(source + index * unit))
            .unwrap();
        let bytes = units * unit;
        let limit = u32::from(u16::MAX) - unit;
        (bytes.min(limit) as u16, (bytes.min(limit) + unit) as u16)
    };
    call.memory.write_u16(string + STRING_LENGTH, length);
    call.memory
        .write_u16(string + STRING_MAXIMUM_LENGTH, maximum_length);
    call.memory.write_u32(string + STRING_BUFFER, source);
    NO_RETURN_VALUE
}

const WORD_SIZE: u32 = 4;

/// RtlFillMemoryUlong(destination, length in bytes, pattern). Fills whole words only.
pub(super) fn fill_memory_ulong(call: &ImportCall) -> u32 {
    let (destination, length, pattern) = (call.argument(0), call.argument(1), call.argument(2));
    for index in 0..length / WORD_SIZE {
        call.memory
            .write_u32(destination + index * WORD_SIZE, pattern);
    }
    NO_RETURN_VALUE
}

/// RtlCompareMemory(first, second, length) -> how many leading bytes match.
pub(super) fn compare_memory(call: &ImportCall) -> u32 {
    let (first, second, length) = (call.argument(0), call.argument(1), call.argument(2));
    (0..length)
        .take_while(|&offset| {
            call.memory.read_u8(first + offset) == call.memory.read_u8(second + offset)
        })
        .count() as u32
}

/// RtlCompareMemoryUlong(source, length in bytes, pattern) -> how many leading bytes are whole
/// words equal to the pattern.
pub(super) fn compare_memory_ulong(call: &ImportCall) -> u32 {
    let (source, length, pattern) = (call.argument(0), call.argument(1), call.argument(2));
    let matching_words = (0..length / WORD_SIZE)
        .take_while(|&index| call.memory.read_u32(source + index * WORD_SIZE) == pattern)
        .count() as u32;
    matching_words * WORD_SIZE
}

/// Win32 error codes, for RtlNtStatusToDosError.
const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_FUNCTION: u32 = 1;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_WRITE_PROTECT: u32 = 19;
const ERROR_BAD_LENGTH: u32 = 24;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_MORE_DATA: u32 = 234;
const ERROR_DIRECTORY: u32 = 267;
/// What Windows returns for a status it has no mapping for.
const ERROR_MR_MID_NOT_FOUND: u32 = 317;

/// RtlNtStatusToDosError(status) -> Win32 error. Covers every status the runtime returns.
pub(super) fn nt_status_to_dos_error(call: &ImportCall) -> u32 {
    let status = call.argument(0);
    [
        (NtStatus::SUCCESS, ERROR_SUCCESS),
        (NtStatus::INVALID_DEVICE_REQUEST, ERROR_INVALID_FUNCTION),
        (NtStatus::OBJECT_NAME_NOT_FOUND, ERROR_FILE_NOT_FOUND),
        (NtStatus::NO_SUCH_FILE, ERROR_FILE_NOT_FOUND),
        (NtStatus::OBJECT_PATH_NOT_FOUND, ERROR_PATH_NOT_FOUND),
        (NtStatus::ACCESS_DENIED, ERROR_ACCESS_DENIED),
        (NtStatus::FILE_IS_A_DIRECTORY, ERROR_ACCESS_DENIED),
        (NtStatus::INVALID_HANDLE, ERROR_INVALID_HANDLE),
        (NtStatus::NO_MEMORY, ERROR_NOT_ENOUGH_MEMORY),
        (NtStatus::INSUFFICIENT_RESOURCES, ERROR_NOT_ENOUGH_MEMORY),
        (NtStatus::NO_MORE_FILES, ERROR_NO_MORE_FILES),
        (NtStatus::MEDIA_WRITE_PROTECTED, ERROR_WRITE_PROTECT),
        (NtStatus::INFO_LENGTH_MISMATCH, ERROR_BAD_LENGTH),
        (NtStatus::END_OF_FILE, ERROR_HANDLE_EOF),
        (NtStatus::INVALID_PARAMETER, ERROR_INVALID_PARAMETER),
        (NtStatus::BUFFER_TOO_SMALL, ERROR_INSUFFICIENT_BUFFER),
        (NtStatus::OBJECT_NAME_COLLISION, ERROR_ALREADY_EXISTS),
        (NtStatus::BUFFER_OVERFLOW, ERROR_MORE_DATA),
        (NtStatus::NOT_A_DIRECTORY, ERROR_DIRECTORY),
    ]
    .into_iter()
    .find(|(nt_status, _)| nt_status.0 == status)
    .map_or(ERROR_MR_MID_NOT_FOUND, |(_, error)| error)
}

/// TIME_FIELDS: year, month, day, hour, minute, second, milliseconds and weekday, 16 bits each.
const TIME_FIELD_SIZE: u32 = 2;

/// RtlTimeToTimeFields(time, fields out).
pub(super) fn time_to_time_fields(call: &ImportCall) -> u32 {
    let time = call.memory.read_i64(call.argument(0)) as u64;
    let fields = time::to_fields(time);
    let values = [
        fields.year,
        fields.month,
        fields.day,
        fields.hour,
        fields.minute,
        fields.second,
        fields.milliseconds,
        fields.weekday,
    ];
    for (index, value) in (0..).zip(values) {
        call.memory
            .write_u16(call.argument(1) + index * TIME_FIELD_SIZE, value);
    }
    NO_RETURN_VALUE
}

/// RtlTimeFieldsToTime(fields, time out) -> BOOLEAN: whether the fields were a valid date.
pub(super) fn time_fields_to_time(call: &ImportCall) -> u32 {
    let field = |index: u32| {
        call.memory
            .read_u16(call.argument(0) + index * TIME_FIELD_SIZE)
    };
    let fields = time::TimeFields {
        year: field(0),
        month: field(1),
        day: field(2),
        hour: field(3),
        minute: field(4),
        second: field(5),
        milliseconds: field(6),
        weekday: field(7),
    };
    match time::from_fields(fields) {
        Some(time) => {
            call.memory.write_u64(call.argument(1), time);
            TRUE
        }
        None => FALSE,
    }
}
