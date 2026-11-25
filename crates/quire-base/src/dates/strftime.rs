// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! strftime patterns, written exactly as chrono 0.4 (without locales) wrote them.
//!
//! People type these patterns into templates (`{{date:%A %-d %B}}`) and
//! periodic-note settings, so the text they give must not change when the
//! date library does. This is a port of chrono's `StrftimeItems` and
//! `DelayedFormat` driven by jiff values; `tests/chrono_oracle.rs` checks it
//! against chrono over a grid of patterns, dates, times and offsets
//! (`docs/design/0.14-jiff.md` §3).

use std::fmt::Write as _;

use jiff::civil::{Date, DateTime, Time, Weekday};

/// What a pattern is applied to: a date, a time, or both, with or without
/// an offset (chrono's `NaiveDate`, `NaiveTime`, `NaiveDateTime`,
/// `DateTime<FixedOffset>`, `DateTime<Utc>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub date: Option<Date>,
    pub time: Option<Time>,
    /// Seconds east of UTC, and whether to name the zone `UTC` (for `%Z`)
    /// rather than by the offset (`+01:00`).
    pub offset: Option<(i32, bool)>,
}

impl Moment {
    /// A date alone (`%H` and the zone can't be written).
    pub const fn date(d: Date) -> Self {
        Self { date: Some(d), time: None, offset: None }
    }

    /// A time alone.
    pub const fn time(t: Time) -> Self {
        Self { date: None, time: Some(t), offset: None }
    }

    /// A date and time with no zone (`%z` can't be written).
    pub fn naive(dt: DateTime) -> Self {
        Self { date: Some(dt.date()), time: Some(dt.time()), offset: None }
    }

    /// A local date and time at `offset_seconds` east of UTC; `%Z` is the
    /// offset (`+01:00`), as chrono wrote it for `Local` and `FixedOffset`.
    pub fn at_offset(dt: DateTime, offset_seconds: i32) -> Self {
        Self { date: Some(dt.date()), time: Some(dt.time()), offset: Some((offset_seconds, false)) }
    }

    /// A date and time in UTC; `%Z` is `UTC`.
    pub fn utc(dt: DateTime) -> Self {
        Self { date: Some(dt.date()), time: Some(dt.time()), offset: Some((0, true)) }
    }
}

/// Why a pattern gave no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// The pattern itself is not one (an unknown `%x`, a `%` at the end).
    BadPattern,
    /// The pattern asks for what the value doesn't have (`%H` of a date,
    /// `%z` without a zone), or can't be written (`%#z`).
    Unwritable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pad {
    None,
    Zero,
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(clippy::enum_variant_names, reason = "the names are chrono's `Numeric`")]
pub(super) enum Num {
    Year,
    YearDiv100,
    YearMod100,
    IsoYear,
    IsoYearMod100,
    Quarter,
    Month,
    Day,
    WeekFromSun,
    WeekFromMon,
    IsoWeek,
    NumDaysFromSun,
    WeekdayFromMon,
    Ordinal,
    Hour,
    Hour12,
    Minute,
    Second,
    Nanosecond,
    Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fixed {
    ShortMonthName,
    LongMonthName,
    ShortWeekdayName,
    LongWeekdayName,
    LowerAmPm,
    UpperAmPm,
    /// `%.f`
    Nanosecond,
    Nanosecond3,
    Nanosecond6,
    Nanosecond9,
    Nanosecond3NoDot,
    Nanosecond6NoDot,
    Nanosecond9NoDot,
    TimezoneName,
    TimezoneOffset,
    TimezoneOffsetColon,
    TimezoneOffsetDoubleColon,
    TimezoneOffsetTripleColon,
    /// `%#z`: chrono parses it but has no way to write it.
    TimezoneOffsetPermissive,
    Rfc3339,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Item<'a> {
    Literal(&'a str),
    Space(&'a str),
    Num(Num, Pad),
    Fixed(Fixed),
}

const fn num0(n: Num) -> Item<'static> {
    Item::Num(n, Pad::Zero)
}

const fn nums(n: Num) -> Item<'static> {
    Item::Num(n, Pad::Space)
}

const fn num(n: Num) -> Item<'static> {
    Item::Num(n, Pad::None)
}

const D_FMT: &[Item<'static>] = &[num0(Num::Month), Item::Literal("/"), num0(Num::Day), Item::Literal("/"), num0(Num::YearMod100)];
const D_T_FMT: &[Item<'static>] = &[
    Item::Fixed(Fixed::ShortWeekdayName),
    Item::Space(" "),
    Item::Fixed(Fixed::ShortMonthName),
    Item::Space(" "),
    nums(Num::Day),
    Item::Space(" "),
    num0(Num::Hour),
    Item::Literal(":"),
    num0(Num::Minute),
    Item::Literal(":"),
    num0(Num::Second),
    Item::Space(" "),
    num0(Num::Year),
];
const T_FMT: &[Item<'static>] = &[num0(Num::Hour), Item::Literal(":"), num0(Num::Minute), Item::Literal(":"), num0(Num::Second)];
const T_FMT_AMPM: &[Item<'static>] = &[
    num0(Num::Hour12),
    Item::Literal(":"),
    num0(Num::Minute),
    Item::Literal(":"),
    num0(Num::Second),
    Item::Space(" "),
    Item::Fixed(Fixed::UpperAmPm),
];

/// The items of `pattern`, as chrono's `StrftimeItems` reads it; `Err` where
/// chrono gives `Item::Error`.
pub(super) fn items(pattern: &str) -> Result<Vec<Item<'_>>, FormatError> {
    let mut out = Vec::new();
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            // A run of spaces: written as it is, and matches any run of
            // spaces (or none) when parsing.
            let end = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
            out.push(Item::Space(&rest[..end]));
            rest = &rest[end..];
            continue;
        }
        if c != '%' {
            let end = rest.find(|c: char| c.is_whitespace() || c == '%').unwrap_or(rest.len());
            out.push(Item::Literal(&rest[..end]));
            rest = &rest[end..];
            continue;
        }
        let mut r = &rest[1..];
        let mut next = || {
            let c = r.chars().next().ok_or(FormatError::BadPattern)?;
            r = &r[c.len_utf8()..];
            Ok::<char, FormatError>(c)
        };
        let spec = next()?;
        let pad = match spec {
            '-' => Some(Pad::None),
            '0' => Some(Pad::Zero),
            '_' => Some(Pad::Space),
            _ => None,
        };
        let alternate = spec == '#';
        let spec = if pad.is_some() || alternate { next()? } else { spec };
        if alternate && spec != 'z' {
            return Err(FormatError::BadPattern);
        }
        let several: Option<&[Item<'static>]> = match spec {
            'D' | 'x' => Some(D_FMT),
            'F' => Some(&[num0(Num::Year), Item::Literal("-"), num0(Num::Month), Item::Literal("-"), num0(Num::Day)]),
            'R' => Some(&[num0(Num::Hour), Item::Literal(":"), num0(Num::Minute)]),
            'T' | 'X' => Some(T_FMT),
            'c' => Some(D_T_FMT),
            'r' => Some(T_FMT_AMPM),
            'v' => Some(&[nums(Num::Day), Item::Literal("-"), Item::Fixed(Fixed::ShortMonthName), Item::Literal("-"), num0(Num::Year)]),
            _ => None,
        };
        if let Some(several) = several {
            // A padding flag is only for one number.
            if pad.is_some() {
                return Err(FormatError::BadPattern);
            }
            out.extend(several.iter().cloned());
            rest = r;
            continue;
        }
        let item = match spec {
            'A' => Item::Fixed(Fixed::LongWeekdayName),
            'B' => Item::Fixed(Fixed::LongMonthName),
            'C' => num0(Num::YearDiv100),
            'G' => num0(Num::IsoYear),
            'H' => num0(Num::Hour),
            'I' => num0(Num::Hour12),
            'M' => num0(Num::Minute),
            'P' => Item::Fixed(Fixed::LowerAmPm),
            'S' => num0(Num::Second),
            'U' => num0(Num::WeekFromSun),
            'V' => num0(Num::IsoWeek),
            'W' => num0(Num::WeekFromMon),
            'Y' => num0(Num::Year),
            'Z' => Item::Fixed(Fixed::TimezoneName),
            'a' => Item::Fixed(Fixed::ShortWeekdayName),
            'b' | 'h' => Item::Fixed(Fixed::ShortMonthName),
            'd' => num0(Num::Day),
            'e' => nums(Num::Day),
            'f' => num0(Num::Nanosecond),
            'g' => num0(Num::IsoYearMod100),
            'j' => num0(Num::Ordinal),
            'k' => nums(Num::Hour),
            'l' => nums(Num::Hour12),
            'm' => num0(Num::Month),
            'n' => Item::Space("\n"),
            'p' => Item::Fixed(Fixed::UpperAmPm),
            'q' => num(Num::Quarter),
            's' => num(Num::Timestamp),
            't' => Item::Space("\t"),
            'u' => num(Num::WeekdayFromMon),
            'w' => num(Num::NumDaysFromSun),
            'y' => num0(Num::YearMod100),
            'z' if alternate => Item::Fixed(Fixed::TimezoneOffsetPermissive),
            'z' => Item::Fixed(Fixed::TimezoneOffset),
            '+' => Item::Fixed(Fixed::Rfc3339),
            ':' => {
                if let Some(t) = r.strip_prefix("::z") {
                    r = t;
                    Item::Fixed(Fixed::TimezoneOffsetTripleColon)
                } else if let Some(t) = r.strip_prefix(":z") {
                    r = t;
                    Item::Fixed(Fixed::TimezoneOffsetDoubleColon)
                } else if let Some(t) = r.strip_prefix('z') {
                    r = t;
                    Item::Fixed(Fixed::TimezoneOffsetColon)
                } else {
                    return Err(FormatError::BadPattern);
                }
            }
            '.' => match next()? {
                'f' => Item::Fixed(Fixed::Nanosecond),
                d @ ('3' | '6' | '9') => match next()? {
                    'f' => Item::Fixed(match d {
                        '3' => Fixed::Nanosecond3,
                        '6' => Fixed::Nanosecond6,
                        _ => Fixed::Nanosecond9,
                    }),
                    _ => return Err(FormatError::BadPattern),
                },
                _ => return Err(FormatError::BadPattern),
            },
            d @ ('3' | '6' | '9') => match next()? {
                'f' => Item::Fixed(match d {
                    '3' => Fixed::Nanosecond3NoDot,
                    '6' => Fixed::Nanosecond6NoDot,
                    _ => Fixed::Nanosecond9NoDot,
                }),
                _ => return Err(FormatError::BadPattern),
            },
            '%' => Item::Literal("%"),
            _ => return Err(FormatError::BadPattern),
        };
        let item = match (pad, item) {
            (None, item) => item,
            (Some(p), Item::Num(n, _)) => Item::Num(n, p),
            (Some(_), _) => return Err(FormatError::BadPattern),
        };
        out.push(item);
        rest = r;
    }
    Ok(out)
}

/// Whether `pattern` is one chrono accepted (templates and periodic-note
/// settings check this before using a pattern).
pub fn is_valid(pattern: &str) -> bool {
    items(pattern).is_ok()
}

/// `m` written with `pattern`, as chrono 0.4 wrote it.
///
/// # Errors
/// [`FormatError::BadPattern`] where chrono's items had an error;
/// [`FormatError::Unwritable`] where chrono's `write_to` failed (its
/// `to_string` panicked there: `%#z`, `%H` of a date, `%z` without a zone).
pub fn format(pattern: &str, m: &Moment) -> Result<String, FormatError> {
    let mut out = String::with_capacity(pattern.len() + 16);
    for item in items(pattern)? {
        let ok = match item {
            Item::Literal(s) | Item::Space(s) => put(&mut out, s),
            Item::Num(n, pad) => numeric(&mut out, m, n, pad),
            Item::Fixed(f) => fixed(&mut out, m, f),
        };
        ok.ok_or(FormatError::Unwritable)?;
    }
    Ok(out)
}

/// [`format()`] for a pattern known to be good and to fit the value (the
/// patterns written in Quire's code): the text, or empty if not.
pub fn fmt(pattern: &str, m: &Moment) -> String {
    format(pattern, m).unwrap_or_default()
}

/// Append `s`; for arms that answer `Option<()>`.
#[expect(clippy::unnecessary_wraps, reason = "every arm of `fixed` answers an Option")]
fn put(out: &mut String, s: &str) -> Option<()> {
    out.push_str(s);
    Some(())
}

fn two(out: &mut String, v: i64, pad: Pad) -> Option<()> {
    // chrono casts to u8 first.
    let v = u8::try_from(v.rem_euclid(256)).ok()?;
    let ones = char::from(b'0' + v % 10);
    match (v / 10, pad) {
        (0, Pad::None) => {}
        (0, Pad::Space) => out.push(' '),
        (tens, _) => out.push(char::from(b'0' + tens)),
    }
    out.push(ones);
    Some(())
}

fn hundreds(out: &mut String, v: i64) -> Option<()> {
    if !(0..100).contains(&v) {
        return None;
    }
    write!(out, "{v:02}").ok()
}

fn write_n(out: &mut String, n: usize, v: i64, pad: Pad, always_sign: bool) -> Option<()> {
    match (always_sign, pad) {
        (true, Pad::None) => write!(out, "{v:+}"),
        (true, Pad::Zero) => write!(out, "{:+01$}", v, n + 1),
        (true, Pad::Space) => write!(out, "{:+1$}", v, n + 1),
        (false, Pad::None) => write!(out, "{v}"),
        (false, Pad::Zero) => write!(out, "{v:0n$}"),
        (false, Pad::Space) => write!(out, "{v:n$}"),
    }
    .ok()
}

fn year(out: &mut String, y: i64, pad: Pad) -> Option<()> {
    if (1000..=9999).contains(&y) {
        hundreds(out, y / 100)?;
        hundreds(out, y % 100)
    } else {
        write_n(out, 4, y, pad, !(0..10_000).contains(&y))
    }
}

/// Monday 0 .. Sunday 6.
fn from_monday(w: Weekday) -> i64 {
    i64::from(w.to_monday_zero_offset())
}

/// Sunday 0 .. Saturday 6.
fn from_sunday(w: Weekday) -> i64 {
    i64::from(w.to_sunday_zero_offset())
}

/// chrono's `weeks_from`: the week of the year, weeks starting on `start`.
fn weeks_from(d: Date, start_monday: bool) -> i64 {
    let days_since = if start_monday { from_monday(d.weekday()) } else { from_sunday(d.weekday()) };
    (i64::from(d.day_of_year()) - days_since + 6) / 7
}

fn hour12(t: Time) -> (bool, i64) {
    let h = i64::from(t.hour());
    let h12 = match h % 12 {
        0 => 12,
        x => x,
    };
    (h >= 12, h12)
}
fn numeric(out: &mut String, m: &Moment, n: Num, pad: Pad) -> Option<()> {
    use Num::{
        Day, Hour, Hour12, IsoWeek, IsoYear, IsoYearMod100, Minute, Month, Nanosecond, NumDaysFromSun, Ordinal, Quarter, Second, Timestamp,
        WeekFromMon, WeekFromSun, WeekdayFromMon, Year, YearDiv100, YearMod100,
    };
    match (n, m.date, m.time) {
        (Year, Some(d), _) => year(out, i64::from(d.year()), pad),
        (YearDiv100, Some(d), _) => two(out, i64::from(d.year()).div_euclid(100), pad),
        (YearMod100, Some(d), _) => two(out, i64::from(d.year()).rem_euclid(100), pad),
        (IsoYear, Some(d), _) => year(out, i64::from(d.iso_week_date().year()), pad),
        (IsoYearMod100, Some(d), _) => two(out, i64::from(d.iso_week_date().year()).rem_euclid(100), pad),
        (Quarter, Some(d), _) => write!(out, "{}", (d.month() - 1) / 3 + 1).ok(),
        (Month, Some(d), _) => two(out, i64::from(d.month()), pad),
        (Day, Some(d), _) => two(out, i64::from(d.day()), pad),
        (WeekFromSun, Some(d), _) => two(out, weeks_from(d, false), pad),
        (WeekFromMon, Some(d), _) => two(out, weeks_from(d, true), pad),
        (IsoWeek, Some(d), _) => two(out, i64::from(d.iso_week_date().week()), pad),
        (NumDaysFromSun, Some(d), _) => write!(out, "{}", from_sunday(d.weekday())).ok(),
        (WeekdayFromMon, Some(d), _) => write!(out, "{}", from_monday(d.weekday()) + 1).ok(),
        (Ordinal, Some(d), _) => write_n(out, 3, i64::from(d.day_of_year()), pad, false),
        (Hour, _, Some(t)) => two(out, i64::from(t.hour()), pad),
        (Hour12, _, Some(t)) => two(out, hour12(t).1, pad),
        (Minute, _, Some(t)) => two(out, i64::from(t.minute()), pad),
        (Second, _, Some(t)) => two(out, i64::from(t.second()), pad),
        (Nanosecond, _, Some(t)) => write_n(out, 9, i64::from(t.subsec_nanosecond()), pad, false),
        (Timestamp, Some(d), Some(t)) => {
            // Whole seconds of the wall clock (a fraction never rounds toward 1970).
            let utc = days_from_civil(d) * 86_400 + i64::from(t.hour()) * 3600 + i64::from(t.minute()) * 60 + i64::from(t.second());
            let off = m.offset.map_or(0, |(o, _)| i64::from(o));
            write_n(out, 9, utc - off, pad, false)
        }
        _ => None,
    }
}

/// Days from 1970-01-01 to `d` (proleptic Gregorian, any year jiff has).
pub(super) fn days_from_civil(d: Date) -> i64 {
    let (m, day) = (i64::from(d.month()), i64::from(d.day()));
    let y = i64::from(d.year()) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const SHORT_MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const LONG_MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const SHORT_WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const LONG_WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Short English month name, January = 1.
pub fn short_month(month: i8) -> &'static str {
    SHORT_MONTHS[usize::try_from(month - 1).unwrap_or(0) % 12]
}

/// Short English weekday name.
pub fn short_weekday(w: Weekday) -> &'static str {
    SHORT_WEEKDAYS[usize::try_from(w.to_sunday_zero_offset()).unwrap_or(0) % 7]
}

fn month_index(d: Date) -> usize {
    usize::try_from(d.month() - 1).unwrap_or(0)
}

fn weekday_index(d: Date) -> usize {
    usize::try_from(d.weekday().to_sunday_zero_offset()).unwrap_or(0) % 7
}

/// chrono's offset writer: `colons`, and the precision (0 hours, 1 minutes, 2 seconds).
fn offset(out: &mut String, off: i32, colons: bool, precision: u8) -> Option<()> {
    let sign = if off < 0 { '-' } else { '+' };
    let off = i64::from(off).abs();
    let (hours, mins, secs) = match precision {
        0 => (off / 3600, 0, 0),
        1 => {
            let minutes = (off + 30) / 60;
            (minutes / 60, minutes % 60, 0)
        }
        _ => (off / 3600, off / 60 % 60, off % 60),
    };
    out.push(sign);
    hundreds(out, hours)?;
    if precision >= 1 {
        if colons {
            out.push(':');
        }
        hundreds(out, mins)?;
    }
    if precision >= 2 {
        if colons {
            out.push(':');
        }
        hundreds(out, secs)?;
    }
    Some(())
}

/// chrono's `Display` of a `FixedOffset`: `+01:00`, `+05:30:15`.
pub fn offset_name(off: i32) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let o = off.unsigned_abs();
    let (h, mi, s) = (o / 3600, o / 60 % 60, o % 60);
    if s == 0 { format!("{sign}{h:02}:{mi:02}") } else { format!("{sign}{h:02}:{mi:02}:{s:02}") }
}
fn fixed(out: &mut String, m: &Moment, f: Fixed) -> Option<()> {
    use Fixed::{
        LongMonthName, LongWeekdayName, LowerAmPm, Nanosecond, Nanosecond3, Nanosecond3NoDot, Nanosecond6, Nanosecond6NoDot, Nanosecond9,
        Nanosecond9NoDot, Rfc3339, ShortMonthName, ShortWeekdayName, TimezoneName, TimezoneOffset, TimezoneOffsetColon,
        TimezoneOffsetDoubleColon, TimezoneOffsetTripleColon, UpperAmPm,
    };
    let nano = m.time.map(|t| i64::from(t.subsec_nanosecond()));
    match (f, m.date, m.time, m.offset) {
        (ShortMonthName, Some(d), _, _) => put(out, SHORT_MONTHS[month_index(d)]),
        (LongMonthName, Some(d), _, _) => put(out, LONG_MONTHS[month_index(d)]),
        (ShortWeekdayName, Some(d), _, _) => put(out, SHORT_WEEKDAYS[weekday_index(d)]),
        (LongWeekdayName, Some(d), _, _) => put(out, LONG_WEEKDAYS[weekday_index(d)]),
        (LowerAmPm, _, Some(t), _) => put(out, if hour12(t).0 { "pm" } else { "am" }),
        (UpperAmPm, _, Some(t), _) => put(out, if hour12(t).0 { "PM" } else { "AM" }),
        (Nanosecond, _, Some(_), _) => {
            let nano = nano?;
            if nano != 0 {
                auto_si(out, nano)?;
            }
            Some(())
        }
        (Nanosecond3, _, Some(_), _) => write!(out, ".{:03}", nano? / 1_000_000 % 1000).ok(),
        (Nanosecond6, _, Some(_), _) => write!(out, ".{:06}", nano? / 1_000 % 1_000_000).ok(),
        (Nanosecond9, _, Some(_), _) => write!(out, ".{:09}", nano?).ok(),
        (Nanosecond3NoDot, _, Some(_), _) => write!(out, "{:03}", nano? / 1_000_000 % 1000).ok(),
        (Nanosecond6NoDot, _, Some(_), _) => write!(out, "{:06}", nano? / 1_000 % 1_000_000).ok(),
        (Nanosecond9NoDot, _, Some(_), _) => write!(out, "{:09}", nano?).ok(),
        (TimezoneName, _, _, Some((_, true))) => put(out, "UTC"),
        (TimezoneName, _, _, Some((o, false))) => put(out, &offset_name(o)),
        (TimezoneOffset, _, _, Some((o, _))) => offset(out, o, false, 1),
        (TimezoneOffsetColon, _, _, Some((o, _))) => offset(out, o, true, 1),
        (TimezoneOffsetDoubleColon, _, _, Some((o, _))) => offset(out, o, true, 2),
        (TimezoneOffsetTripleColon, _, _, Some((o, _))) => offset(out, o, false, 0),
        (Rfc3339, Some(d), Some(t), Some((o, _))) => {
            let y = i64::from(d.year());
            if (0..=9999).contains(&y) {
                hundreds(out, y / 100)?;
                hundreds(out, y % 100)?;
            } else {
                write!(out, "{y:+05}").ok()?;
            }
            write!(out, "-{:02}-{:02}T{:02}:{:02}:{:02}", d.month(), d.day(), t.hour(), t.minute(), t.second()).ok()?;
            if nano? != 0 {
                auto_si(out, nano?)?;
            }
            offset(out, o, true, 1)
        }
        _ => None,
    }
}

/// `.123`, `.123456` or `.123456789`: the shortest of the three that is exact.
fn auto_si(out: &mut String, nano: i64) -> Option<()> {
    if nano % 1_000_000 == 0 {
        write!(out, ".{:03}", nano / 1_000_000)
    } else if nano % 1_000 == 0 {
        write!(out, ".{:06}", nano / 1_000)
    } else {
        write!(out, ".{nano:09}")
    }
    .ok()
}

/// RFC 2822 as chrono's `to_rfc2822` wrote it: `Tue, 1 Jul 2003 10:52:37 +0200`.
pub fn rfc2822(dt: DateTime, offset_seconds: i32) -> Option<String> {
    let d = dt.date();
    let y = i64::from(d.year());
    if !(0..=9999).contains(&y) {
        return None;
    }
    let mut out = format!("{}, {} {} ", SHORT_WEEKDAYS[weekday_index(d)], d.day(), SHORT_MONTHS[month_index(d)]);
    write!(out, "{y:04} {:02}:{:02}:{:02} ", dt.hour(), dt.minute(), dt.second()).ok()?;
    offset(&mut out, offset_seconds, false, 1)?;
    Some(out)
}
