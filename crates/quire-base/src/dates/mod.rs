// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dates and times: the one parser and the one formatter every crate uses
//! (`docs/design/0.14-jiff.md`), and the local clock.
//!
//! Values are jiff's: [`Date`], [`DateTime`] (a date and time with no zone),
//! [`Zoned`] (with the system zone) and [`Timestamp`]. Patterns are
//! strftime as chrono 0.4 read and wrote them, because people type them into
//! templates, periodic-note settings and the date format setting:
//! [`format()`] writes, [`parse_date`] and its siblings read.

mod strftime;
mod strptime;

pub use jiff::civil::{Date, DateTime, Time, Weekday};
pub use jiff::tz::TimeZone;
pub use jiff::{Span, Timestamp, Zoned};
pub use strftime::{FormatError, Moment, fmt, format, is_valid, offset_name, rfc2822, short_month, short_weekday};
pub use strptime::{ParseError, parse_date, parse_datetime, parse_rfc2822, parse_rfc3339, parse_time_of_day, parse_with_offset};

/// The current date and time in the system's time zone.
pub fn now_local() -> Zoned {
    Zoned::now()
}

/// Today's date in the system's time zone.
pub fn today() -> Date {
    now_local().date()
}

/// A moment in the system zone, for [`format()`] (`%Z` is the offset, `+01:00`).
pub fn local_moment(z: &Zoned) -> Moment {
    Moment::at_offset(z.datetime(), z.offset().seconds())
}

/// `now_local()` written with `pattern` (a pattern from Quire's own code).
pub fn now_fmt(pattern: &str) -> String {
    fmt(pattern, &local_moment(&now_local()))
}

/// `YYYY-MM-DD` of `d`.
pub fn ymd(d: Date) -> String {
    fmt("%Y-%m-%d", &Moment::date(d))
}

/// Today as `YYYY-MM-DD`.
pub fn today_ymd() -> String {
    ymd(today())
}

/// The instant `ms` milliseconds after the epoch, as a UTC date and time.
pub fn utc_from_ms(ms: i64) -> Option<DateTime> {
    Some(Timestamp::from_millisecond(ms).ok()?.to_zoned(TimeZone::UTC).datetime())
}

/// The instant `ms` milliseconds after the epoch, in the system zone.
pub fn local_from_ms(ms: i64) -> Option<Zoned> {
    Some(Timestamp::from_millisecond(ms).ok()?.to_zoned(TimeZone::system()))
}

/// The instant `ms` milliseconds after the epoch written with `pattern` in UTC (`%Z` is `UTC`).
pub fn fmt_utc_ms(ms: i64, pattern: &str) -> Option<String> {
    utc_from_ms(ms).map(|dt| fmt(pattern, &Moment::utc(dt)))
}

/// The instant `ms` milliseconds after the epoch written with `pattern` in the system zone.
pub fn fmt_local_ms(ms: i64, pattern: &str) -> Option<String> {
    local_from_ms(ms).map(|z| fmt(pattern, &local_moment(&z)))
}

/// Milliseconds since the epoch of `dt` read at `offset_seconds` east of UTC.
pub fn ms_at_offset(dt: DateTime, offset_seconds: i32) -> Option<i64> {
    let utc = dt.to_zoned(TimeZone::UTC).ok()?.timestamp().as_millisecond();
    utc.checked_sub(i64::from(offset_seconds) * 1000)
}

/// A wall-clock time in the system zone, or none when the clocks skip it
/// (a DST gap) or show it twice (a fold): chrono's `LocalResult::single`.
pub fn local_single(dt: DateTime) -> Option<Zoned> {
    single_in(&TimeZone::system(), dt)
}

/// [`local_single`] in `tz`.
pub fn single_in(tz: &TimeZone, dt: DateTime) -> Option<Zoned> {
    tz.to_ambiguous_zoned(dt).unambiguous().ok()
}

/// A wall-clock time in the system zone; in a fold the earlier of the two
/// instants, in a gap none: chrono's `LocalResult::earliest`.
pub fn local_earliest(dt: DateTime) -> Option<Zoned> {
    earliest_in(&TimeZone::system(), dt)
}

/// [`local_earliest`] in `tz`.
pub fn earliest_in(tz: &TimeZone, dt: DateTime) -> Option<Zoned> {
    let amb = tz.to_ambiguous_zoned(dt);
    if matches!(amb.offset(), jiff::tz::AmbiguousOffset::Gap { .. }) { None } else { amb.earlier().ok() }
}

/// `s` as a `YYYY-MM-DD` date, the whole of it (`2026-11-20T10:00` is not
/// one). For a date a person typed into a date field.
pub fn parse_ymd(s: &str) -> Option<Date> {
    parse_date(s, "%Y-%m-%d").ok()
}

/// The date at the start of `s`: its first ten bytes as `YYYY-MM-DD`, so
/// `2026-11-20T10:00` and `2026-11-20 (Fri)` are 2026-11-20. For dates in
/// front matter and task fields, which may carry a time.
pub fn parse_ymd_prefix(s: &str) -> Option<Date> {
    parse_ymd(s.get(..10)?)
}

/// `d` moved by `days` (none past the calendar's ends).
pub fn add_days(d: Date, days: i64) -> Option<Date> {
    d.checked_add(Span::new().try_days(days).ok()?).ok()
}

/// Whole days from `a` to `b` (negative when `b` is earlier).
pub fn days_between(a: Date, b: Date) -> i64 {
    i64::from((b - a).get_days())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_string_or_its_first_ten_bytes() {
        let d = Date::new(2026, 11, 20).ok();
        assert_eq!(parse_ymd("2026-11-20"), d);
        assert_eq!(parse_ymd("2026-11-20T10:00"), None);
        assert_eq!(parse_ymd_prefix("2026-11-20T10:00"), d);
        assert_eq!(parse_ymd_prefix("2026-11-20"), d);
        assert_eq!(parse_ymd_prefix("2026-11-2"), None);
        // No trimming: the first ten bytes, " 2026-11-2", read as the 2nd
        // (as every 0.13 copy did). Callers that trim do it first.
        assert_eq!(parse_ymd_prefix(" 2026-11-20"), Date::new(2026, 11, 2).ok());
        assert_eq!(parse_ymd_prefix("2026-11-2é"), None, "not a char boundary");
        assert_eq!(parse_ymd("2026-02-30"), None);
    }

    #[test]
    fn days() {
        let d = Date::new(2026, 12, 31).unwrap_or_default();
        assert_eq!(add_days(d, 1), Date::new(2027, 1, 1).ok());
        assert_eq!(days_between(d, Date::new(2026, 12, 1).unwrap_or_default()), -30);
        assert_eq!(ymd(d), "2026-12-31");
    }

    /// A DST gap and fold in a fixed zone, resolved as chrono's
    /// `LocalResult::single` and `earliest` were.
    #[test]
    fn gaps_and_folds() {
        let tz = TimeZone::get("Europe/Berlin").or_else(|_| TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3"));
        let Ok(tz) = tz else { return };
        let at = |d: i8, h: i8, m: i8| Date::new(2026, 3, d).unwrap_or_default().at(h, m, 0, 0);
        // 2026-03-29 02:30 doesn't exist in Berlin.
        assert_eq!(single_in(&tz, at(29, 2, 30)), None);
        assert_eq!(earliest_in(&tz, at(29, 2, 30)), None);
        // 2026-10-25 02:30 happens twice: single gives none, earliest the summer-time one.
        let fold = Date::new(2026, 10, 25).unwrap_or_default().at(2, 30, 0, 0);
        assert_eq!(single_in(&tz, fold), None);
        assert_eq!(earliest_in(&tz, fold).map(|z| z.offset().seconds()), Some(7200));
        assert_eq!(single_in(&tz, at(29, 12, 0)).map(|z| z.offset().seconds()), Some(7200));
    }
}
