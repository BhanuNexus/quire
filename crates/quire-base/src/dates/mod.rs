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
pub use jiff::tz::{Offset, TimeZone};
pub use jiff::{Span, Timestamp, Zoned};
pub use strftime::{FormatError, Moment, fmt, format, is_valid, offset_name, rfc2822, short_month, short_weekday};
pub use strptime::{ParseError, parse_date, parse_datetime, parse_rfc2822, parse_rfc3339, parse_time_of_day, parse_with_offset};

/// 1970-01-01 00:00, what chrono's `DateTime::default()` was.
pub const EPOCH: DateTime = DateTime::constant(1970, 1, 1, 0, 0, 0, 0);

/// `.render(pattern)` on the values Quire formats with its own patterns.
///
/// It is what chrono's `.format(pattern).to_string()` was. A date has no
/// time and a date or date-time no zone; asking for one gives empty text.
pub trait Strftime {
    /// `self` written with `pattern`.
    fn render(&self, pattern: &str) -> String;
}

impl Strftime for Date {
    fn render(&self, pattern: &str) -> String {
        fmt(pattern, &Moment::date(*self))
    }
}

impl Strftime for Time {
    fn render(&self, pattern: &str) -> String {
        fmt(pattern, &Moment::time(*self))
    }
}

impl Strftime for DateTime {
    fn render(&self, pattern: &str) -> String {
        fmt(pattern, &Moment::naive(*self))
    }
}

impl Strftime for Zoned {
    fn render(&self, pattern: &str) -> String {
        fmt(pattern, &local_moment(self))
    }
}

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

/// `dt` at the fixed offset `offset_seconds` east of UTC (chrono's
/// `DateTime<FixedOffset>`): `%Z` and `%:z` both write the offset.
pub fn at_offset(dt: DateTime, offset_seconds: i32) -> Option<Zoned> {
    let off = Offset::from_seconds(offset_seconds).ok()?;
    dt.to_zoned(TimeZone::fixed(off)).ok()
}

/// RFC 3339 text as an instant at its own offset (chrono's `DateTime::parse_from_rfc3339`).
pub fn zoned_rfc3339(s: &str) -> Option<Zoned> {
    let (dt, off) = parse_rfc3339(s).ok()?;
    at_offset(dt, off)
}

/// `z` written with `pattern`, its offset as the zone (chrono's
/// `DateTime<FixedOffset>` and `DateTime<Local>`).
pub fn fmt_zoned(z: &Zoned, pattern: &str) -> String {
    fmt(pattern, &local_moment(z))
}

/// Whole seconds from `a` to `b`, toward zero (chrono's `(b - a).num_seconds()`).
pub fn seconds_between(a: &Zoned, b: &Zoned) -> i64 {
    let ns = b.timestamp().as_nanosecond() - a.timestamp().as_nanosecond();
    i64::try_from(ns / 1_000_000_000).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX })
}

/// `z` moved by `seconds` (an exact duration, as chrono's `+ Duration`), in its own zone.
pub fn plus_seconds(z: &Zoned, seconds: i64) -> Option<Zoned> {
    let t = z.timestamp().checked_add(Span::new().try_seconds(seconds).ok()?).ok()?;
    Some(t.to_zoned(z.time_zone().clone()))
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

/// `d` moved by `days`, or `d` itself past the calendar's ends (years
/// -9999 and 9999; chrono panicked there).
pub fn plus_days(d: Date, days: i64) -> Date {
    add_days(d, days).unwrap_or(d)
}

/// `dt` moved by `days` days of 24 hours, or `dt` itself past the calendar's ends.
pub fn plus_days_dt(dt: DateTime, days: i64) -> DateTime {
    Span::new().try_days(days).ok().and_then(|s| dt.checked_add(s).ok()).unwrap_or(dt)
}

/// `dt` moved by `seconds`, or `dt` itself past the calendar's ends.
pub fn plus_seconds_dt(dt: DateTime, seconds: i64) -> DateTime {
    Span::new().try_seconds(seconds).ok().and_then(|s| dt.checked_add(s).ok()).unwrap_or(dt)
}

/// `d` written with `pattern` (a pattern from Quire's own code).
pub fn fmt_date(d: Date, pattern: &str) -> String {
    fmt(pattern, &Moment::date(d))
}

/// `t` written with `pattern` (a pattern from Quire's own code).
pub fn fmt_time(t: Time, pattern: &str) -> String {
    fmt(pattern, &Moment::time(t))
}

/// `dt` written with `pattern` (a pattern from Quire's own code).
pub fn fmt_naive(dt: DateTime, pattern: &str) -> String {
    fmt(pattern, &Moment::naive(dt))
}

/// The date `y-m-d`, if there is one (chrono's `from_ymd_opt`, with its types).
pub fn ymd_opt(y: i32, m: u32, d: u32) -> Option<Date> {
    Date::new(i16::try_from(y).ok()?, i8::try_from(m).ok()?, i8::try_from(d).ok()?).ok()
}

/// `d`'s year and month as chrono gave them (`i32`, `u32`).
pub fn year_month(d: Date) -> (i32, u32) {
    (i32::from(d.year()), u32::try_from(d.month()).unwrap_or(1))
}

/// Days of `w` since Monday: Monday 0 .. Sunday 6.
pub fn from_monday(w: Weekday) -> i64 {
    i64::from(w.to_monday_zero_offset())
}

/// Whole days from `a` to `b` (negative when `b` is earlier).
pub fn days_between(a: Date, b: Date) -> i64 {
    strftime::days_from_civil(b) - strftime::days_from_civil(a)
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

    /// jiff's calendar runs from year -9999 to 9999 (chrono's to about
    /// ±262,000): a date beyond it is refused or left alone, never a panic.
    #[test]
    fn years_beyond_9999_are_refused() {
        // chrono read a signed five-digit year; it is out of range now.
        assert_eq!(parse_date("+10000-01-01", "%Y-%m-%d"), Err(ParseError::Impossible));
        assert_eq!(parse_ymd("10000-01-01"), None, "as with chrono: `%Y` reads four digits");
        assert_eq!(ymd_opt(10_000, 1, 1), None);
        let last = Date::new(9999, 12, 31).unwrap_or_default();
        assert_eq!(add_days(last, 1), None);
        assert_eq!(plus_days(last, 1), last, "stays put where chrono panicked");
        assert_eq!(plus_days_dt(last.at(0, 0, 0, 0), 1), last.at(0, 0, 0, 0));
        assert_eq!(utc_from_ms(i64::MAX), None);
        assert_eq!(fmt_utc_ms(i64::MAX, "%Y"), None);
    }

    /// What Quire writes to files that persist or sync reads back the same
    /// and is written the same (strings as 0.13 wrote them).
    #[test]
    fn persisted_formats_round_trip() {
        let dt = Date::new(2026, 11, 6).unwrap_or_default().at(10, 0, 0, 0);
        for (pattern, text) in [
            ("%Y-%m-%dT%H:%M:%S%:z", "2026-11-06T10:00:00+01:00"),
            ("%Y-%m-%d %H:%M", "2026-11-06 10:00"),
            ("%Y%m%d-%H%M%S", "20261106-100000"),
            ("%Y-%m", "2026-11"),
            ("%Y%m%d", "20261106"),
            ("%Y-%m-%dT%H:%M:%SZ", "2026-11-06T10:00:00Z"),
            ("%Y%m%dT%H%M%SZ", "20261106T100000Z"),
            ("%a, %d %b %Y %H:%M:%S GMT", "Fri, 06 Nov 2026 10:00:00 GMT"),
        ] {
            assert_eq!(fmt(pattern, &Moment::at_offset(dt, 3600)), text, "{pattern}");
            if pattern != "%Y-%m" {
                let back = parse_with_offset(text, pattern).map(|(d, _)| d).or_else(|_| parse_datetime(text, pattern));
                let back = back.or_else(|_| parse_date(text, pattern).map(|d| d.to_datetime(Time::midnight())));
                assert_eq!(back.map(DateTime::date), Ok(dt.date()), "{pattern}");
            }
        }
        assert_eq!(
            zoned_rfc3339("2026-11-06T10:00:00+01:00").map(|z| fmt_zoned(&z, "%Y-%m-%dT%H:%M:%S%:z")).as_deref(),
            Some("2026-11-06T10:00:00+01:00")
        );
        assert_eq!(fmt_utc_ms(1_793_577_600_000, "%Y-%m").as_deref(), Some("2026-11"));
        assert_eq!(rfc2822(dt, 7200).as_deref(), Some("Fri, 6 Nov 2026 10:00:00 +0200"));
    }

    /// With no time-zone database (a minimal container, a sandbox) and a
    /// zone named that can't be found, local time is UTC, as chrono's
    /// `Local` was: jiff's `TimeZone::system()` never fails. Checked in a
    /// child process, since the system zone is process-wide state.
    #[test]
    fn no_zone_database_means_utc() {
        if std::env::var_os("QUIRE_TZ_CHILD").is_some() {
            let now = now_local();
            assert_eq!(now.offset().seconds(), 0, "{now}");
            let utc = utc_from_ms(now.timestamp().as_millisecond()).unwrap_or(EPOCH);
            assert_eq!(fmt_zoned(&now, "%F %T"), fmt_utc_ms(now.timestamp().as_millisecond(), "%F %T").unwrap_or_default());
            assert_eq!(local_single(utc).map(|z| z.offset().seconds()), Some(0));
            assert_eq!(fmt_local_ms(0, "%Y-%m-%d %H:%M %z").as_deref(), Some("1970-01-01 00:00 +0000"));
            return;
        }
        let Ok(me) = std::env::current_exe() else { return };
        let empty = std::env::temp_dir().join("quire-no-zoneinfo-for-this-test");
        let out = std::process::Command::new(me)
            .args(["--exact", "dates::tests::no_zone_database_means_utc", "--test-threads=1"])
            .env("QUIRE_TZ_CHILD", "1")
            // A zone name no database has: what any name is when there is no database.
            .env("TZ", "Nowhere/Missing")
            .env("TZDIR", &empty)
            .output();
        let Ok(out) = out else { return };
        assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the child ran the test");
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
