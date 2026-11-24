// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reading text back with a strftime pattern, as chrono 0.4's
//! `parse_from_str` did: padding-agnostic numbers (`2026-1-5` reads with
//! `%Y-%m-%d`), spaces in the pattern match any run of spaces or none,
//! names in any letter case, and every field given must agree with the
//! date found. A port of chrono's `parse_internal` and `Parsed`, checked
//! against chrono by `tests/chrono_oracle.rs`.

use jiff::civil::{Date, DateTime, Time, Weekday};

use super::strftime::{Fixed, FormatError, Item, Num, items};

/// Why text didn't read as a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    BadPattern,
    /// The text doesn't match the pattern, or has text left over.
    Invalid,
    /// A field is out of range, or two fields disagree.
    Impossible,
    /// The pattern doesn't give enough to make the value (no day, no minute).
    NotEnough,
}

impl From<FormatError> for ParseError {
    fn from(_: FormatError) -> Self {
        Self::BadPattern
    }
}

type R<T> = Result<T, ParseError>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Parsed {
    year: Option<i64>,
    year_div_100: Option<i64>,
    year_mod_100: Option<i64>,
    isoyear: Option<i64>,
    isoyear_mod_100: Option<i64>,
    quarter: Option<i64>,
    month: Option<i64>,
    week_from_sun: Option<i64>,
    week_from_mon: Option<i64>,
    isoweek: Option<i64>,
    weekday: Option<Weekday>,
    ordinal: Option<i64>,
    day: Option<i64>,
    hour_div_12: Option<i64>,
    hour_mod_12: Option<i64>,
    minute: Option<i64>,
    second: Option<i64>,
    nanosecond: Option<i64>,
    timestamp: Option<i64>,
    offset: Option<i64>,
}

fn set<T: PartialEq>(old: &mut Option<T>, new: T) -> R<()> {
    match old {
        Some(o) if *o != new => Err(ParseError::Impossible),
        _ => {
            *old = Some(new);
            Ok(())
        }
    }
}

fn ranged(old: &mut Option<i64>, v: i64, lo: i64, hi: i64) -> R<()> {
    if !(lo..=hi).contains(&v) {
        return Err(ParseError::Impossible);
    }
    set(old, v)
}

impl Parsed {
    fn set_num(&mut self, n: Num, v: i64) -> R<()> {
        use Num::{
            Day, Hour, Hour12, IsoWeek, IsoYear, IsoYearMod100, Minute, Month, Nanosecond, NumDaysFromSun, Ordinal, Quarter, Second,
            Timestamp, WeekFromMon, WeekFromSun, WeekdayFromMon, Year, YearDiv100, YearMod100,
        };
        let i32_range = (i64::from(i32::MIN), i64::from(i32::MAX));
        match n {
            Year => ranged(&mut self.year, v, i32_range.0, i32_range.1),
            YearDiv100 => ranged(&mut self.year_div_100, v, 0, i32_range.1),
            YearMod100 => ranged(&mut self.year_mod_100, v, 0, 99),
            IsoYear => ranged(&mut self.isoyear, v, i32_range.0, i32_range.1),
            IsoYearMod100 => ranged(&mut self.isoyear_mod_100, v, 0, 99),
            Quarter => ranged(&mut self.quarter, v, 1, 4),
            Month => ranged(&mut self.month, v, 1, 12),
            Day => ranged(&mut self.day, v, 1, 31),
            WeekFromSun => ranged(&mut self.week_from_sun, v, 0, 53),
            WeekFromMon => ranged(&mut self.week_from_mon, v, 0, 53),
            IsoWeek => ranged(&mut self.isoweek, v, 1, 53),
            NumDaysFromSun => self.set_weekday(match v {
                0 => Weekday::Sunday,
                1 => Weekday::Monday,
                2 => Weekday::Tuesday,
                3 => Weekday::Wednesday,
                4 => Weekday::Thursday,
                5 => Weekday::Friday,
                6 => Weekday::Saturday,
                _ => return Err(ParseError::Impossible),
            }),
            WeekdayFromMon => self.set_weekday(match v {
                1 => Weekday::Monday,
                2 => Weekday::Tuesday,
                3 => Weekday::Wednesday,
                4 => Weekday::Thursday,
                5 => Weekday::Friday,
                6 => Weekday::Saturday,
                7 => Weekday::Sunday,
                _ => return Err(ParseError::Impossible),
            }),
            Ordinal => ranged(&mut self.ordinal, v, 1, 366),
            Hour => {
                let (div, m) = match v {
                    0..=11 => (0, v),
                    12..=23 => (1, v - 12),
                    _ => return Err(ParseError::Impossible),
                };
                set(&mut self.hour_div_12, div)?;
                set(&mut self.hour_mod_12, m)
            }
            Hour12 => {
                if !(1..=12).contains(&v) {
                    return Err(ParseError::Impossible);
                }
                set(&mut self.hour_mod_12, if v == 12 { 0 } else { v })
            }
            Minute => ranged(&mut self.minute, v, 0, 59),
            Second => ranged(&mut self.second, v, 0, 60),
            Nanosecond => ranged(&mut self.nanosecond, v, 0, 999_999_999),
            Timestamp => set(&mut self.timestamp, v),
        }
    }

    fn set_weekday(&mut self, w: Weekday) -> R<()> {
        set(&mut self.weekday, w)
    }

    fn set_offset(&mut self, v: i64) -> R<()> {
        set(&mut self.offset, v)
    }
}

/// Up to `max` digits (at least `min`): chrono's `scan::number`.
fn number(s: &str, min: usize, max: usize) -> R<(&str, i64)> {
    let b = s.as_bytes();
    if b.len() < min {
        return Err(ParseError::Invalid);
    }
    let mut n = 0i64;
    for (i, c) in b.iter().take(max).enumerate() {
        if !c.is_ascii_digit() {
            return if i < min { Err(ParseError::Invalid) } else { Ok((&s[i..], n)) };
        }
        n = n.checked_mul(10).and_then(|n| n.checked_add(i64::from(c - b'0'))).ok_or(ParseError::Impossible)?;
    }
    Ok((&s[max.min(b.len())..], n))
}

const SCALE: [i64; 10] = [0, 100_000_000, 10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];

fn nanosecond(s: &str) -> R<(&str, i64)> {
    let len = s.len();
    let (s, v) = number(s, 1, 9)?;
    let v = v * SCALE[len - s.len()];
    Ok((s.trim_start_matches(|c: char| c.is_ascii_digit()), v))
}

fn nanosecond_fixed(s: &str, digits: usize) -> R<(&str, i64)> {
    let (s, v) = number(s, digits, digits)?;
    Ok((s, v * SCALE[digits]))
}

const MONTHS: [&[u8; 3]; 12] = [b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec"];
const MONTH_SUFFIXES: [&[u8]; 12] = [b"uary", b"ruary", b"ch", b"il", b"", b"e", b"y", b"ust", b"tember", b"ober", b"ember", b"ember"];
const WEEKDAYS: [(&[u8; 3], Weekday, &[u8]); 7] = [
    (b"mon", Weekday::Monday, b"day"),
    (b"tue", Weekday::Tuesday, b"sday"),
    (b"wed", Weekday::Wednesday, b"nesday"),
    (b"thu", Weekday::Thursday, b"rsday"),
    (b"fri", Weekday::Friday, b"day"),
    (b"sat", Weekday::Saturday, b"urday"),
    (b"sun", Weekday::Sunday, b"day"),
];

fn lower3(s: &str) -> R<[u8; 3]> {
    let b = s.as_bytes();
    if b.len() < 3 {
        return Err(ParseError::Invalid);
    }
    Ok([b[0] | 32, b[1] | 32, b[2] | 32])
}

fn month(s: &str, long: bool) -> R<(&str, i64)> {
    let l = lower3(s)?;
    let i = MONTHS.iter().position(|m| **m == l).ok_or(ParseError::Invalid)?;
    let mut s = &s[3..];
    let suffix = MONTH_SUFFIXES[i];
    if long && s.len() >= suffix.len() && s.as_bytes()[..suffix.len()].eq_ignore_ascii_case(suffix) {
        s = &s[suffix.len()..];
    }
    Ok((s, i64::try_from(i).unwrap_or(0) + 1))
}

fn weekday(s: &str, long: bool) -> R<(&str, Weekday)> {
    let l = lower3(s)?;
    let (_, w, suffix) = WEEKDAYS.iter().find(|(n, ..)| **n == l).ok_or(ParseError::Invalid)?;
    let mut s = &s[3..];
    if long && s.len() >= suffix.len() && s.as_bytes()[..suffix.len()].eq_ignore_ascii_case(suffix) {
        s = &s[suffix.len()..];
    }
    Ok((s, *w))
}

/// chrono's `scan::timezone_offset` with `colon_or_space` between hours
/// and minutes: `+01:00`, `+0100`, `-05 30`, `Z` when `zulu`.
fn offset(s: &str, zulu: bool, missing_minutes: bool) -> R<(&str, i64)> {
    if zulu && matches!(s.as_bytes().first(), Some(b'Z' | b'z')) {
        return Ok((&s[1..], 0));
    }
    let (negative, mut s) = if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else if let Some(r) = s.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = s.strip_prefix('\u{2212}') {
        (true, r)
    } else {
        return Err(ParseError::Invalid);
    };
    let digits = |s: &str| {
        let b = s.as_bytes();
        (b.len() >= 2).then(|| (b[0], b[1]))
    };
    let hours = match digits(s).ok_or(ParseError::Invalid)? {
        (a @ b'0'..=b'9', b @ b'0'..=b'9') => i64::from((a - b'0') * 10 + (b - b'0')),
        _ => return Err(ParseError::Invalid),
    };
    s = s[2..].trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    let minutes = match digits(s) {
        Some((a @ b'0'..=b'5', b @ b'0'..=b'9')) => i64::from((a - b'0') * 10 + (b - b'0')),
        Some((b'6'..=b'9', b'0'..=b'9')) => return Err(ParseError::Impossible),
        None if missing_minutes => 0,
        Some(_) | None => return Err(ParseError::Invalid),
    };
    s = match s.len() {
        l if l >= 2 => &s[2..],
        0 => s,
        _ => return Err(ParseError::Invalid),
    };
    let secs = hours * 3600 + minutes * 60;
    Ok((s, if negative { -secs } else { secs }))
}
fn parse_into(p: &mut Parsed, mut s: &str, pattern: &str) -> R<()> {
    for item in items(pattern)? {
        match item {
            Item::Literal(lit) => s = s.strip_prefix(lit).ok_or(ParseError::Invalid)?,
            Item::Space(_) => s = s.trim_start(),
            Item::Num(n, _) => {
                use Num::{IsoYear, Nanosecond, NumDaysFromSun, Ordinal, Quarter, Timestamp, WeekdayFromMon, Year};
                let (width, signed) = match n {
                    Year | IsoYear => (4, true),
                    Quarter | NumDaysFromSun | WeekdayFromMon => (1, false),
                    Ordinal => (3, false),
                    Nanosecond => (9, false),
                    Timestamp => (usize::MAX, false),
                    _ => (2, false),
                };
                s = s.trim_start();
                let (rest, v) = if signed && let Some(r) = s.strip_prefix('-') {
                    let (r, v) = number(r, 1, usize::MAX)?;
                    (r, -v)
                } else if signed && let Some(r) = s.strip_prefix('+') {
                    number(r, 1, usize::MAX)?
                } else {
                    number(s, 1, width)?
                };
                s = rest;
                p.set_num(n, v)?;
            }
            Item::Fixed(f) => {
                use Fixed::{
                    LongMonthName, LongWeekdayName, LowerAmPm, Nanosecond, Nanosecond3, Nanosecond3NoDot, Nanosecond6, Nanosecond6NoDot,
                    Nanosecond9, Nanosecond9NoDot, Rfc3339, ShortMonthName, ShortWeekdayName, TimezoneName, TimezoneOffset,
                    TimezoneOffsetColon, TimezoneOffsetDoubleColon, TimezoneOffsetPermissive, TimezoneOffsetTripleColon, UpperAmPm,
                };
                match f {
                    ShortMonthName | LongMonthName => {
                        let (r, m) = month(s, f == LongMonthName)?;
                        s = r;
                        p.set_num(Num::Month, m)?;
                    }
                    ShortWeekdayName | LongWeekdayName => {
                        let (r, w) = weekday(s, f == LongWeekdayName)?;
                        s = r;
                        p.set_weekday(w)?;
                    }
                    LowerAmPm | UpperAmPm => {
                        let b = s.as_bytes();
                        if b.len() < 2 {
                            return Err(ParseError::Invalid);
                        }
                        let pm = match (b[0] | 32, b[1] | 32) {
                            (b'a', b'm') => 0,
                            (b'p', b'm') => 1,
                            _ => return Err(ParseError::Invalid),
                        };
                        set(&mut p.hour_div_12, pm)?;
                        s = &s[2..];
                    }
                    Nanosecond => {
                        if let Some(r) = s.strip_prefix('.') {
                            let (r, n) = nanosecond(r)?;
                            s = r;
                            p.set_num(Num::Nanosecond, n)?;
                        }
                    }
                    Nanosecond3 | Nanosecond6 | Nanosecond9 => {
                        if let Some(r) = s.strip_prefix('.') {
                            let d = match f {
                                Nanosecond3 => 3,
                                Nanosecond6 => 6,
                                _ => 9,
                            };
                            let (r, n) = nanosecond_fixed(r, d)?;
                            s = r;
                            p.set_num(Num::Nanosecond, n)?;
                        }
                    }
                    Nanosecond3NoDot | Nanosecond6NoDot | Nanosecond9NoDot => {
                        let d = match f {
                            Nanosecond3NoDot => 3,
                            Nanosecond6NoDot => 6,
                            _ => 9,
                        };
                        let (r, n) = nanosecond_fixed(s, d)?;
                        s = r;
                        p.set_num(Num::Nanosecond, n)?;
                    }
                    TimezoneName => s = s.trim_start_matches(|c: char| !c.is_whitespace()),
                    TimezoneOffset | TimezoneOffsetColon | TimezoneOffsetDoubleColon | TimezoneOffsetTripleColon => {
                        let (r, o) = offset(s.trim_start(), false, true)?;
                        s = r;
                        p.set_offset(o)?;
                    }
                    TimezoneOffsetPermissive => {
                        let (r, o) = offset(s.trim_start(), true, true)?;
                        s = r;
                        p.set_offset(o)?;
                    }
                    Rfc3339 => return Err(ParseError::BadPattern),
                }
            }
        }
    }
    if s.is_empty() { Ok(()) } else { Err(ParseError::Invalid) }
}

fn resolve_year(y: Option<i64>, q: Option<i64>, r: Option<i64>) -> R<Option<i64>> {
    match (y, q, r) {
        (y, None, None) => Ok(y),
        (Some(y), q, r @ (Some(0..=99) | None)) => {
            if y < 0 {
                return Err(ParseError::Impossible);
            }
            if q.unwrap_or(y / 100) == y / 100 && r.unwrap_or(y % 100) == y % 100 { Ok(Some(y)) } else { Err(ParseError::Impossible) }
        }
        (None, Some(q), Some(r @ 0..=99)) => {
            if q < 0 {
                return Err(ParseError::Impossible);
            }
            Ok(Some(q * 100 + r))
        }
        (None, None, Some(r @ 0..=99)) => Ok(Some(r + if r < 70 { 2000 } else { 1900 })),
        (None, Some(_), None) => Err(ParseError::NotEnough),
        (_, _, Some(_)) => Err(ParseError::Impossible),
    }
}

fn ymd(y: i64, m: i64, d: i64) -> R<Date> {
    let y = i16::try_from(y).map_err(|_| ParseError::Impossible)?;
    let m = i8::try_from(m).map_err(|_| ParseError::Impossible)?;
    let d = i8::try_from(d).map_err(|_| ParseError::Impossible)?;
    Date::new(y, m, d).map_err(|_| ParseError::Impossible)
}

fn yo(y: i64, o: i64) -> R<Date> {
    let first = ymd(y, 1, 1)?;
    let o = i16::try_from(o).map_err(|_| ParseError::Impossible)?;
    first.with().day_of_year(o).build().map_err(|_| ParseError::Impossible)
}

fn days_since(w: Weekday, start: Weekday) -> i64 {
    i64::from((w.to_monday_zero_offset() + 7 - start.to_monday_zero_offset()) % 7)
}

fn weeks_from(d: Date, start: Weekday) -> i64 {
    (i64::from(d.day_of_year()) - days_since(d.weekday(), start) + 6) / 7
}

fn week_date(year: i64, week: i64, w: Weekday, start: Weekday) -> R<Date> {
    let first = yo(year, 1)?;
    let first_week_start = 1 + days_since(start, first.weekday());
    let ordinal = first_week_start + (week - 1) * 7 + days_since(w, start);
    if ordinal <= 0 {
        return Err(ParseError::Impossible);
    }
    yo(year, ordinal)
}

impl Parsed {
    fn to_date(&self) -> R<Date> {
        let year = resolve_year(self.year, self.year_div_100, self.year_mod_100)?;
        let isoyear = resolve_year(self.isoyear, None, self.isoyear_mod_100)?;
        let check_ymd = |d: Date| {
            let y = i64::from(d.year());
            let (div, m) = if y >= 0 { (Some(y / 100), Some(y % 100)) } else { (None, None) };
            self.year.unwrap_or(y) == y
                && self.year_div_100.or(div) == div
                && self.year_mod_100.or(m) == m
                && self.month.unwrap_or_else(|| i64::from(d.month())) == i64::from(d.month())
                && self.day.unwrap_or_else(|| i64::from(d.day())) == i64::from(d.day())
        };
        let check_iso = |d: Date| {
            let w = d.iso_week_date();
            let iso_year = i64::from(w.year());
            let m = (iso_year >= 0).then_some(iso_year % 100);
            self.isoyear.unwrap_or(iso_year) == iso_year
                && self.isoyear_mod_100.or(m) == m
                && self.isoweek.unwrap_or_else(|| i64::from(w.week())) == i64::from(w.week())
                && self.weekday.unwrap_or_else(|| d.weekday()) == d.weekday()
        };
        let check_ordinal = |d: Date| {
            self.ordinal.unwrap_or_else(|| i64::from(d.day_of_year())) == i64::from(d.day_of_year())
                && self.week_from_sun.unwrap_or_else(|| weeks_from(d, Weekday::Sunday)) == weeks_from(d, Weekday::Sunday)
                && self.week_from_mon.unwrap_or_else(|| weeks_from(d, Weekday::Monday)) == weeks_from(d, Weekday::Monday)
        };
        let (ok, d) = match (year, isoyear) {
            (Some(y), _) if self.month.is_some() && self.day.is_some() => {
                let d = ymd(y, self.month.unwrap_or(1), self.day.unwrap_or(1))?;
                (check_iso(d) && check_ordinal(d), d)
            }
            (Some(y), _) if self.ordinal.is_some() => {
                let d = yo(y, self.ordinal.unwrap_or(1))?;
                (check_ymd(d) && check_iso(d) && check_ordinal(d), d)
            }
            (Some(y), _) if self.week_from_sun.is_some() && self.weekday.is_some() => {
                let d = week_date(y, self.week_from_sun.unwrap_or(0), self.weekday.unwrap_or(Weekday::Sunday), Weekday::Sunday)?;
                (check_ymd(d) && check_iso(d) && check_ordinal(d), d)
            }
            (Some(y), _) if self.week_from_mon.is_some() && self.weekday.is_some() => {
                let d = week_date(y, self.week_from_mon.unwrap_or(0), self.weekday.unwrap_or(Weekday::Monday), Weekday::Monday)?;
                (check_ymd(d) && check_iso(d) && check_ordinal(d), d)
            }
            (_, Some(iso_year)) if self.isoweek.is_some() && self.weekday.is_some() => {
                let iso_year = i16::try_from(iso_year).map_err(|_| ParseError::Impossible)?;
                let wk = i8::try_from(self.isoweek.unwrap_or(1)).map_err(|_| ParseError::Impossible)?;
                let d = jiff::civil::ISOWeekDate::new(iso_year, wk, self.weekday.unwrap_or(Weekday::Monday))
                    .map_err(|_| ParseError::Impossible)?
                    .date();
                (check_ymd(d) && check_ordinal(d), d)
            }
            _ => return Err(ParseError::NotEnough),
        };
        if !ok || self.quarter.is_some_and(|q| q != i64::from((d.month() - 1) / 3 + 1)) {
            return Err(ParseError::Impossible);
        }
        Ok(d)
    }

    /// The time; a leap second (`:60`) reads as the last nanosecond of
    /// second 59 (jiff has no leap seconds; chrono kept it).
    fn to_time(&self) -> R<Time> {
        let div = self.hour_div_12.ok_or(ParseError::NotEnough)?;
        let m = self.hour_mod_12.ok_or(ParseError::NotEnough)?;
        let minute = self.minute.ok_or(ParseError::NotEnough)?;
        if self.nanosecond.is_some() && self.second.is_none() {
            return Err(ParseError::NotEnough);
        }
        let (second, nano) = match self.second.unwrap_or(0) {
            60 => (59, 999_999_999),
            s => (s, self.nanosecond.unwrap_or(0)),
        };
        let n = |v: i64| i8::try_from(v).map_err(|_| ParseError::Impossible);
        Time::new(n(div * 12 + m)?, n(minute)?, n(second)?, i32::try_from(nano).map_err(|_| ParseError::Impossible)?)
            .map_err(|_| ParseError::Impossible)
    }

    fn to_datetime(&self, offset: i64) -> R<DateTime> {
        match (self.to_date(), self.to_time()) {
            (Ok(d), Ok(t)) => {
                let dt = d.to_datetime(t);
                if let Some(ts) = self.timestamp
                    && ts != utc_seconds(dt)? - offset
                {
                    return Err(ParseError::Impossible);
                }
                Ok(dt)
            }
            (date, time) => {
                let Some(ts) = self.timestamp else {
                    date?;
                    time?;
                    return Err(ParseError::NotEnough);
                };
                for e in [date.err(), time.err()].into_iter().flatten() {
                    if e == ParseError::Impossible {
                        return Err(e);
                    }
                }
                let at = jiff::Timestamp::from_second(ts.checked_add(offset).ok_or(ParseError::Impossible)?)
                    .map_err(|_| ParseError::Impossible)?
                    .to_zoned(jiff::tz::TimeZone::UTC)
                    .datetime();
                let mut p = self.clone();
                if p.second == Some(60) {
                    if at.second() != 59 {
                        return Err(ParseError::Impossible);
                    }
                } else {
                    p.set_num(Num::Second, i64::from(at.second()))?;
                }
                p.set_num(Num::Year, i64::from(at.year()))?;
                p.set_num(Num::Ordinal, i64::from(at.day_of_year()))?;
                p.set_num(Num::Hour, i64::from(at.hour()))?;
                p.set_num(Num::Minute, i64::from(at.minute()))?;
                Ok(p.to_date()?.to_datetime(p.to_time()?))
            }
        }
    }
}

fn utc_seconds(dt: DateTime) -> R<i64> {
    Ok(dt.to_zoned(jiff::tz::TimeZone::UTC).map_err(|_| ParseError::Impossible)?.timestamp().as_second())
}

/// `s` read with `pattern` as a date (chrono's `NaiveDate::parse_from_str`).
///
/// # Errors
/// When `s` doesn't match `pattern` or gives no valid date.
pub fn parse_date(s: &str, pattern: &str) -> R<Date> {
    let mut p = Parsed::default();
    parse_into(&mut p, s, pattern)?;
    p.to_date()
}

/// `s` read with `pattern` as a time of day (chrono's `NaiveTime::parse_from_str`).
///
/// # Errors
/// When `s` doesn't match `pattern` or gives no valid time.
pub fn parse_time_of_day(s: &str, pattern: &str) -> R<Time> {
    let mut p = Parsed::default();
    parse_into(&mut p, s, pattern)?;
    p.to_time()
}

/// `s` read with `pattern` as a date and time with no zone (chrono's
/// `NaiveDateTime::parse_from_str`).
///
/// # Errors
/// When `s` doesn't match `pattern` or gives no valid date and time.
pub fn parse_datetime(s: &str, pattern: &str) -> R<DateTime> {
    let mut p = Parsed::default();
    parse_into(&mut p, s, pattern)?;
    p.to_datetime(0)
}

/// `s` read with `pattern` as a date and time with its offset in seconds
/// east of UTC (chrono's `DateTime::<FixedOffset>::parse_from_str`; a Unix
/// timestamp alone means UTC).
///
/// # Errors
/// When `s` doesn't match `pattern`, gives no valid date and time, or no offset.
pub fn parse_with_offset(s: &str, pattern: &str) -> R<(DateTime, i32)> {
    let mut p = Parsed::default();
    parse_into(&mut p, s, pattern)?;
    let off = match (p.offset, p.timestamp) {
        (Some(o), _) => o,
        (None, Some(_)) => 0,
        (None, None) => return Err(ParseError::NotEnough),
    };
    let off32 = i32::try_from(off).map_err(|_| ParseError::Impossible)?;
    if off32.abs() >= 86_400 {
        return Err(ParseError::Impossible);
    }
    Ok((p.to_datetime(off)?, off32))
}

/// RFC 3339 as chrono's `parse_from_rfc3339` read it.
///
/// `T`, `t` or a space between date and time, any number of fraction
/// digits, `Z`/`z` or `±hh:mm`. The date and time as written, and the
/// offset in seconds east of UTC.
///
/// # Errors
/// When `s` is not RFC 3339.
pub fn parse_rfc3339(s: &str) -> R<(DateTime, i32)> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return Err(ParseError::Invalid);
    }
    let digit = |i: usize| if b[i].is_ascii_digit() { Ok(i64::from(b[i] - b'0')) } else { Err(ParseError::Invalid) };
    let year = digit(0)? * 1000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?;
    let sep = |i: usize, c: u8| if b[i] == c { Ok(()) } else { Err(ParseError::Invalid) };
    sep(4, b'-')?;
    let month = digit(5)? * 10 + digit(6)?;
    sep(7, b'-')?;
    let day = digit(8)? * 10 + digit(9)?;
    let date = ymd(year, month, day)?;
    if !matches!(b[10], b't' | b'T' | b' ') {
        return Err(ParseError::Invalid);
    }
    let hour = digit(11)? * 10 + digit(12)?;
    sep(13, b':')?;
    let min = digit(14)? * 10 + digit(15)?;
    sep(16, b':')?;
    let sec = digit(17)? * 10 + digit(18)?;
    let (sec, leap) = if sec == 60 { (59, true) } else { (sec, false) };
    let (mut rest, nano) = match s[19..].strip_prefix('.') {
        Some(r) => nanosecond(r)?,
        None => (&s[19..], 0),
    };
    let nano = if leap { 999_999_999 } else { nano };
    let n = |v: i64| i8::try_from(v).map_err(|_| ParseError::Impossible);
    let time = Time::new(n(hour)?, n(min)?, n(sec)?, i32::try_from(nano).map_err(|_| ParseError::Impossible)?)
        .map_err(|_| ParseError::Impossible)?;
    // The offset: `Z`, or `±hh:mm` with exactly one colon.
    let off = if matches!(rest.as_bytes().first(), Some(b'Z' | b'z')) {
        rest = &rest[1..];
        0
    } else {
        let (neg, r) = match rest.chars().next() {
            Some('+') => (false, &rest[1..]),
            Some('-') => (true, &rest[1..]),
            Some('\u{2212}') => (true, &rest['\u{2212}'.len_utf8()..]),
            _ => return Err(ParseError::Invalid),
        };
        let rb = r.as_bytes();
        if rb.len() < 5 || rb[2] != b':' || !rb[..2].iter().chain(&rb[3..5]).all(u8::is_ascii_digit) {
            return Err(ParseError::Invalid);
        }
        let h = i64::from((rb[0] - b'0') * 10 + rb[1] - b'0');
        let m = i64::from((rb[3] - b'0') * 10 + rb[4] - b'0');
        if m > 59 {
            return Err(ParseError::Impossible);
        }
        rest = &r[5..];
        let o = h * 3600 + m * 60;
        if neg { -o } else { o }
    };
    if !rest.is_empty() {
        return Err(ParseError::Invalid);
    }
    let off = i32::try_from(off).map_err(|_| ParseError::Impossible)?;
    if off.abs() >= 86_400 {
        return Err(ParseError::Impossible);
    }
    Ok((date.to_datetime(time), off))
}

/// At least one space, then the rest.
fn space(s: &str) -> R<&str> {
    let t = s.trim_start();
    if t.len() < s.len() { Ok(t) } else { Err(ParseError::Invalid) }
}

/// An HTTP `Date` header (RFC 2822 / RFC 7231), as a UTC date and time.
///
/// `Tue, 1 Jul 2003 10:52:37 +0200` or `Sun, 06 Nov 1994 08:49:37 GMT`, as
/// chrono's `parse_from_rfc2822` read it (no comments, two-digit years
/// 2000-2049 and 1950-1999).
///
/// # Errors
/// When `s` is not such a date.
pub fn parse_rfc2822(s: &str) -> R<DateTime> {
    let mut s = s.trim_start();
    let weekday_given = if let Ok((r, w)) = weekday(s, false) {
        s = r.strip_prefix(',').ok_or(ParseError::Invalid)?;
        Some(w)
    } else {
        None
    };
    s = s.trim_start();
    let (r, day) = number(s, 1, 2)?;
    s = space(r)?;
    let (r, month) = month(s, false)?;
    s = space(r)?;
    let len = s.len();
    let (r, mut year) = number(s, 2, usize::MAX)?;
    match (len - r.len(), year) {
        (2, 0..=49) => year += 2000,
        (2, 50..=99) | (3, _) => year += 1900,
        _ => {}
    }
    s = space(r)?;
    let (r, hour) = number(s, 2, 2)?;
    let r = r.trim_start().strip_prefix(':').ok_or(ParseError::Invalid)?.trim_start();
    let (mut r, minute) = number(r, 2, 2)?;
    let mut second = 0;
    if let Some(t) = r.trim_start().strip_prefix(':') {
        let (t, sec) = number(t, 2, 2)?;
        second = sec;
        r = t;
    }
    let r = space(r)?;
    // The zone: an offset or a name.
    let upto = r.bytes().position(|c| !c.is_ascii_alphabetic()).unwrap_or(r.len());
    let (r, off) = if upto > 0 {
        let name = r[..upto].to_ascii_lowercase();
        let h = match name.as_str() {
            "gmt" | "ut" | "z" => 0,
            "edt" => -4,
            "est" | "cdt" => -5,
            "cst" | "mdt" => -6,
            "mst" | "pdt" => -7,
            "pst" => -8,
            n if n.len() == 1 && n != "j" => 0,
            _ => return Err(ParseError::Invalid),
        };
        (&r[upto..], h * 3600)
    } else {
        let (neg, t) = match r.as_bytes().first() {
            Some(b'+') => (false, &r[1..]),
            Some(b'-') => (true, &r[1..]),
            _ => return Err(ParseError::Invalid),
        };
        let tb = t.as_bytes();
        if tb.len() < 4 || !tb[..4].iter().all(u8::is_ascii_digit) {
            return Err(ParseError::Invalid);
        }
        let h = i64::from((tb[0] - b'0') * 10 + tb[1] - b'0');
        let m = i64::from((tb[2] - b'0') * 10 + tb[3] - b'0');
        if m > 59 {
            return Err(ParseError::Impossible);
        }
        let o = h * 3600 + m * 60;
        (&t[4..], if neg { -o } else { o })
    };
    if !r.is_empty() {
        return Err(ParseError::Invalid);
    }
    let date = ymd(year, month, day)?;
    if weekday_given.is_some_and(|w| w != date.weekday()) {
        return Err(ParseError::Impossible);
    }
    let n = |v: i64| i8::try_from(v).map_err(|_| ParseError::Impossible);
    let (second, nano) = if second == 60 { (59, 999_999_999) } else { (second, 0) };
    let local = date.to_datetime(Time::new(n(hour)?, n(minute)?, n(second)?, nano).map_err(|_| ParseError::Impossible)?);
    let utc = utc_seconds(local)? - off;
    Ok(jiff::Timestamp::from_second(utc).map_err(|_| ParseError::Impossible)?.to_zoned(jiff::tz::TimeZone::UTC).datetime())
}
