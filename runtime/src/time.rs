//! The console's time format: 100 ns units since 1601-01-01 (a Windows FILETIME).

use std::time::{SystemTime, UNIX_EPOCH};

pub const NANOSECONDS_PER_UNIT: u64 = 100;
const UNITS_PER_SECOND: u64 = 1_000_000_000 / NANOSECONDS_PER_UNIT;
/// Seconds from 1601-01-01 to the Unix epoch (1970-01-01).
const SECONDS_FROM_1601_TO_1970: u64 = 11_644_473_600;

/// Host times before 1970 (which no real file has) clamp to the Unix epoch.
pub fn filetime(time: SystemTime) -> u64 {
    let since_unix = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since_unix.as_secs() + SECONDS_FROM_1601_TO_1970) * UNITS_PER_SECOND
        + u64::from(since_unix.subsec_nanos()) / NANOSECONDS_PER_UNIT
}

/// A calendar date and time (TIME_FIELDS). The weekday counts from Sunday = 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeFields {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
    pub weekday: u16,
}

const UNITS_PER_MILLISECOND: u64 = 1_000_000 / NANOSECONDS_PER_UNIT;
const MILLISECONDS_PER_SECOND: u64 = 1000;
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;
const HOURS_PER_DAY: u64 = 24;
const SECONDS_PER_DAY: u64 = SECONDS_PER_MINUTE * MINUTES_PER_HOUR * HOURS_PER_DAY;
const DAYS_PER_WEEK: u64 = 7;
/// 1601-01-01 was a Monday.
const FIRST_DAY_WEEKDAY: u64 = 1;
/// Days from 0000-03-01 (where the civil-date algorithm counts from) to 1601-01-01.
const DAYS_TO_1601: i64 = 584_694;
/// The civil-date algorithm works in 400-year eras of 146,097 days, with years that start in
/// March so the leap day falls last.
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;

/// RtlTimeToTimeFields: a system time as calendar fields.
pub fn to_fields(time: u64) -> TimeFields {
    let milliseconds = time / UNITS_PER_MILLISECOND;
    let seconds = milliseconds / MILLISECONDS_PER_SECOND;
    let days = seconds / SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days as i64 + DAYS_TO_1601);
    let second_of_day = seconds % SECONDS_PER_DAY;
    TimeFields {
        year: year as u16,
        month: month as u16,
        day: day as u16,
        hour: (second_of_day / (SECONDS_PER_MINUTE * MINUTES_PER_HOUR)) as u16,
        minute: (second_of_day / SECONDS_PER_MINUTE % MINUTES_PER_HOUR) as u16,
        second: (second_of_day % SECONDS_PER_MINUTE) as u16,
        milliseconds: (milliseconds % MILLISECONDS_PER_SECOND) as u16,
        weekday: ((days + FIRST_DAY_WEEKDAY) % DAYS_PER_WEEK) as u16,
    }
}

/// RtlTimeFieldsToTime: calendar fields as a system time, or None if they are out of range or
/// before 1601. The weekday is ignored.
pub fn from_fields(fields: TimeFields) -> Option<u64> {
    let valid = (1..=12).contains(&fields.month)
        && fields.day >= 1
        && fields.day <= days_in_month(i64::from(fields.year), i64::from(fields.month))
        && fields.hour < HOURS_PER_DAY as u16
        && fields.minute < MINUTES_PER_HOUR as u16
        && fields.second < SECONDS_PER_MINUTE as u16
        && fields.milliseconds < MILLISECONDS_PER_SECOND as u16;
    if !valid {
        return None;
    }
    let days = days_from_civil(
        i64::from(fields.year),
        i64::from(fields.month),
        i64::from(fields.day),
    ) - DAYS_TO_1601;
    let days = u64::try_from(days).ok()?;
    let seconds = days * SECONDS_PER_DAY
        + u64::from(fields.hour) * SECONDS_PER_MINUTE * MINUTES_PER_HOUR
        + u64::from(fields.minute) * SECONDS_PER_MINUTE
        + u64::from(fields.second);
    Some(
        (seconds * MILLISECONDS_PER_SECOND + u64::from(fields.milliseconds))
            * UNITS_PER_MILLISECOND,
    )
}

fn days_in_month(year: i64, month: i64) -> u16 {
    let next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    (next - days_from_civil(year, month, 1)) as u16
}

/// Days since 0000-03-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(YEARS_PER_ERA);
    let year_of_era = year - era * YEARS_PER_ERA;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era
}

/// The inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let era = days.div_euclid(DAYS_PER_ERA);
    let day_of_era = days - era * DAYS_PER_ERA;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * YEARS_PER_ERA + i64::from(month <= 2);
    (year, month, day)
}

/// KeTimeStampBundle's fields, big-endian: the time since boot and the system time, both in
/// 100 ns units, then the milliseconds since boot.
const BUNDLE_INTERRUPT_TIME: u32 = 0x0;
const BUNDLE_SYSTEM_TIME: u32 = 0x8;
const BUNDLE_TICK_COUNT: u32 = 0x10;

/// Writes KeTimeStampBundle at `address` as the console's clock interrupt keeps it.
pub fn write_time_stamp_bundle(
    memory: &crate::memory::GuestMemory,
    address: u32,
    since_boot: std::time::Duration,
    now: SystemTime,
) {
    let units = (since_boot.as_nanos() / u128::from(NANOSECONDS_PER_UNIT)) as u64;
    memory.write_u64(address + BUNDLE_INTERRUPT_TIME, units);
    memory.write_u64(address + BUNDLE_SYSTEM_TIME, filetime(now));
    memory.write_u32(address + BUNDLE_TICK_COUNT, since_boot.as_millis() as u32);
}

/// The current time, as KeQuerySystemTime reports it.
pub fn now() -> u64 {
    filetime(SystemTime::now())
}

#[cfg(test)]
mod time_stamp_bundle_tests {
    use super::*;
    use crate::memory::GuestMemory;
    use std::time::Duration;

    #[test]
    fn the_bundle_holds_interrupt_time_system_time_and_ticks() {
        const BUNDLE: u32 = 0x1000;
        let memory = GuestMemory::reserve().unwrap();
        let now = SystemTime::now();
        write_time_stamp_bundle(&memory, BUNDLE, Duration::from_millis(1500), now);
        assert_eq!(memory.read_i64(BUNDLE + BUNDLE_INTERRUPT_TIME), 15_000_000);
        assert_eq!(
            memory.read_i64(BUNDLE + BUNDLE_SYSTEM_TIME) as u64,
            filetime(now)
        );
        assert_eq!(memory.read_u32(BUNDLE + BUNDLE_TICK_COUNT), 1500);
    }
}

#[cfg(test)]
mod calendar_tests {
    use super::*;

    /// 2008-09-23 12:34:56.789, Pure's North American release day, a Tuesday.
    const RELEASE: TimeFields = TimeFields {
        year: 2008,
        month: 9,
        day: 23,
        hour: 12,
        minute: 34,
        second: 56,
        milliseconds: 789,
        weekday: 2,
    };

    #[test]
    fn the_epoch_is_monday_1601_01_01() {
        let fields = to_fields(0);
        assert_eq!(
            (fields.year, fields.month, fields.day, fields.weekday),
            (1601, 1, 1, 1)
        );
    }

    #[test]
    fn fields_round_trip_and_reject_impossible_dates() {
        let time = from_fields(RELEASE).unwrap();
        assert_eq!(to_fields(time), RELEASE);
        let february_30 = TimeFields {
            month: 2,
            day: 30,
            ..RELEASE
        };
        assert_eq!(from_fields(february_30), None);
        let leap_day = TimeFields {
            year: 2008,
            month: 2,
            day: 29,
            ..RELEASE
        };
        assert!(from_fields(leap_day).is_some());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_unix_epoch_is_the_well_known_filetime() {
        /// 1970-01-01 as a FILETIME, as documented for Windows.
        const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;
        assert_eq!(filetime(UNIX_EPOCH), UNIX_EPOCH_FILETIME);
        assert_eq!(
            filetime(UNIX_EPOCH + Duration::from_nanos(NANOSECONDS_PER_UNIT)),
            UNIX_EPOCH_FILETIME + 1
        );
    }
}
