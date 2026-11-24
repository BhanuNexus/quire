// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Conversions between chrono and jiff values for the crates not yet moved
//! to `dates` (`docs/design/0.14-jiff.md` §6). Deleted with the last of them.

use chrono::{Datelike, TimeZone as _, Timelike};

use super::{Date, DateTime, Time, TimeZone, Timestamp, Zoned};

/// A chrono date as jiff's.
///
/// # Panics
/// Never for a date in jiff's range (years -9999 to 9999).
pub fn date(d: chrono::NaiveDate) -> Date {
    let y = i16::try_from(d.year()).unwrap_or(9999);
    let m = i8::try_from(d.month()).unwrap_or(1);
    let day = i8::try_from(d.day()).unwrap_or(1);
    Date::new(y, m, day).unwrap_or_default()
}

/// A jiff date as chrono's.
pub fn to_date(d: Date) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(i32::from(d.year()), u32::try_from(d.month()).unwrap_or(1), u32::try_from(d.day()).unwrap_or(1))
        .unwrap_or_default()
}

/// A chrono time as jiff's (a leap second is the last nanosecond of :59).
pub fn time(t: chrono::NaiveTime) -> Time {
    let n = |v: u32| i8::try_from(v).unwrap_or(0);
    Time::new(n(t.hour()), n(t.minute()), n(t.second()), i32::try_from(t.nanosecond().min(999_999_999)).unwrap_or(0)).unwrap_or_default()
}

/// A chrono date and time as jiff's.
pub fn datetime(dt: chrono::NaiveDateTime) -> DateTime {
    date(dt.date()).to_datetime(time(dt.time()))
}

/// A local chrono instant as a jiff one in the system zone.
pub fn local(dt: chrono::DateTime<chrono::Local>) -> Zoned {
    Timestamp::from_millisecond(dt.timestamp_millis()).map_or_else(|_| super::now_local(), |t| t.to_zoned(TimeZone::system()))
}

/// A jiff instant as a local chrono one.
pub fn to_local(z: &Zoned) -> chrono::DateTime<chrono::Local> {
    chrono::Local.timestamp_millis_opt(z.timestamp().as_millisecond()).single().unwrap_or_else(chrono::Local::now)
}
