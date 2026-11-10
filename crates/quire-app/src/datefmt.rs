// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dates as shown, apart from dates as stored (0.12).
//!
//! Property values that are dates (`2026-11-05`, or with a time:
//! `2026-11-05T14:30`) always stay ISO in the file. `[editing] date_format`
//! (a strftime format, empty for ISO) only changes what the Properties
//! panel and view tables show and accept when you type. The computer's
//! region settings play no part. Month and day names are English.
//!
//! A time that follows the date is shown after it (`5 Nov 2026 14:30`) and
//! kept as it was when the date is edited.

use std::fmt::Write as _;

use chrono::NaiveDate;
use chrono::format::{Item, StrftimeItems};
use quire_markdown::frontmatter::is_date;

/// The stored form.
pub const ISO: &str = "%Y-%m-%d";

/// Formats offered in Settings: (format, label).
pub const PRESETS: [(&str, &str); 6] = [
    ("", "As stored (ISO)"),
    ("%d.%m.%Y", "Day.Month.Year"),
    ("%d/%m/%Y", "Day/Month/Year"),
    ("%m/%d/%Y", "Month/Day/Year"),
    ("%-d %b %Y", "5 Nov 2026"),
    ("%A, %-d %B %Y", "Thursday, 5 November 2026"),
];

fn date_part(value: &str) -> Option<(NaiveDate, &str)> {
    let v = value.trim();
    if !is_date(v) {
        return None;
    }
    let d = NaiveDate::parse_from_str(&v[..10], ISO).ok()?;
    Some((d, v[10..].trim_start_matches(['T', ' ']).trim()))
}

fn print(d: NaiveDate, format: &str) -> Option<String> {
    if StrftimeItems::new(format).any(|i| matches!(i, Item::Error)) {
        return None;
    }
    let mut out = String::new();
    write!(out, "{}", d.format(format)).ok()?;
    Some(out)
}

/// Whether `format` is a date format chrono can print (no time fields, no
/// unknown `%` codes) that shows the day, the month and the year: changing
/// any one of them changes the text.
///
/// The answer depends only on `format` and the tables draw many dates a
/// frame, so it is remembered per format (a handful at most).
pub fn valid(format: &str) -> bool {
    thread_local! {
        static SEEN: std::cell::RefCell<std::collections::HashMap<String, bool>> = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    if let Some(known) = SEEN.with(|m| m.borrow().get(format).copied()) {
        return known;
    }
    let verdict = compute_valid(format);
    SEEN.with(|m| {
        let mut m = m.borrow_mut();
        // A format being typed in Settings makes many; don't keep them all.
        if m.len() >= 32 {
            m.clear();
        }
        m.insert(format.to_string(), verdict);
    });
    verdict
}

fn compute_valid(format: &str) -> bool {
    let at = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).and_then(|d| print(d, format));
    let base = at(2026, 11, 5);
    base.is_some() && [at(2026, 11, 6), at(2026, 12, 5), at(2027, 11, 5)].iter().all(|o| o.is_some() && *o != base)
}

/// Whether a date shown in `format` can be typed back (`%A` alone can't).
pub fn round_trips(format: &str) -> bool {
    let d = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap_or_default();
    print(d, format).is_some_and(|s| NaiveDate::parse_from_str(&s, format) == Ok(d))
}

/// `value` as shown. A value that isn't a date, and an empty or invalid
/// `format`, give `value` unchanged.
pub fn display(value: &str, format: &str) -> String {
    if format.trim().is_empty() || !valid(format) {
        return value.to_string();
    }
    let Some((date, time)) = date_part(value) else { return value.to_string() };
    let Some(mut shown) = print(date, format) else { return value.to_string() };
    if !time.is_empty() {
        shown.push(' ');
        shown.push_str(time);
    }
    shown
}

/// A time of day as stored after a date: `14:30`, `14:30:05`, `14:30:05.250`,
/// optionally with a zone (`Z`, `+01:00`, `-0500`, `+01`).
fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| b.get(from..from + n).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if !(digits(0, 2) && b.get(2) == Some(&b':') && digits(3, 2)) {
        return false;
    }
    let mut i = 5;
    if b.get(i) == Some(&b':') && digits(i + 1, 2) {
        i += 3;
        if b.get(i) == Some(&b'.') {
            let frac = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if frac == 0 {
                return false;
            }
            i += 1 + frac;
        }
    }
    match b.get(i..) {
        Some([] | [b'Z']) => true,
        Some([b'+' | b'-', rest @ ..]) => {
            matches!(rest.len(), 2 | 4 | 5)
                && rest.iter().enumerate().all(|(k, c)| if rest.len() == 5 && k == 2 { *c == b':' } else { c.is_ascii_digit() })
        }
        _ => false,
    }
}

/// What was typed into a date field, as the ISO text to store.
///
/// `previous` is the stored value: its time is kept unless a time was typed. ISO is
/// always accepted. `None` when the text isn't a date in `format`.
pub fn parse(typed: &str, format: &str, previous: &str) -> Option<String> {
    let t = typed.trim();
    if is_date(t) && date_part(t).is_some() {
        return Some(t.to_string());
    }
    if format.trim().is_empty() || !valid(format) {
        return None;
    }
    // A trailing time (`14:30`, `14:30:05`) is split off.
    let (text, time) = match t.rsplit_once(' ') {
        Some((head, tail)) if is_time(tail) => (head.trim(), Some(tail)),
        _ => (t, None),
    };
    let date = NaiveDate::parse_from_str(text, format).ok()?;
    let mut iso = date.format(ISO).to_string();
    let kept = date_part(previous).map(|(_, t)| t).filter(|t| !t.is_empty());
    if let Some(time) = time.or(kept) {
        // The separator the stored value had (`T` or a space) stays.
        iso.push(if previous.trim().get(10..).is_some_and(|r| r.starts_with(' ')) { ' ' } else { 'T' });
        iso.push_str(time);
    }
    Some(iso)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_in_the_chosen_format_and_keeps_the_time() {
        assert_eq!(display("2026-11-05", ""), "2026-11-05");
        assert_eq!(display("2026-11-05", "%d.%m.%Y"), "05.11.2026");
        assert_eq!(display("2026-11-05", "%-d %b %Y"), "5 Nov 2026");
        assert_eq!(display("2026-11-05", "%A, %-d %B %Y"), "Thursday, 5 November 2026");
        assert_eq!(display("2026-11-05T14:30", "%m/%d/%Y"), "11/05/2026 14:30");
        assert_eq!(display("2026-11-05 09:00:00", "%d/%m/%Y"), "05/11/2026 09:00:00");
        // Not dates, and bad formats, are left alone.
        assert_eq!(display("soon", "%d.%m.%Y"), "soon");
        assert_eq!(display("2026-13-45", "%d.%m.%Y"), "2026-13-45");
        assert_eq!(display("2026-11-05", "%H:%M"), "2026-11-05", "a time format shows no date");
        assert_eq!(display("2026-11-05", "%Q"), "2026-11-05");
        assert_eq!(display("2026-11-05", "%Y"), "2026-11-05", "a format must show day, month and year");
    }

    #[test]
    fn typing_goes_back_to_iso() {
        assert_eq!(parse("05.11.2026", "%d.%m.%Y", "2026-11-01").as_deref(), Some("2026-11-05"));
        assert_eq!(parse("5 Nov 2026", "%-d %b %Y", "").as_deref(), Some("2026-11-05"));
        // The stored time stays; a typed time wins.
        assert_eq!(parse("06.11.2026", "%d.%m.%Y", "2026-11-05T14:30").as_deref(), Some("2026-11-06T14:30"));
        assert_eq!(parse("06.11.2026 08:15", "%d.%m.%Y", "2026-11-05T14:30").as_deref(), Some("2026-11-06T08:15"));
        // ISO is always accepted, whatever the format.
        assert_eq!(parse("2026-11-07", "%d.%m.%Y", "").as_deref(), Some("2026-11-07"));
        assert_eq!(parse("2026-11-07", "", "").as_deref(), Some("2026-11-07"));
        assert_eq!(parse("11/05/2026", "%d.%m.%Y", ""), None);
        assert_eq!(parse("31.02.2026", "%d.%m.%Y", ""), None);
        assert_eq!(parse("tomorrow", "", ""), None);
    }

    #[test]
    fn a_time_zone_offset_can_be_edited_and_is_kept() {
        for stored in ["2026-11-05T14:30+01:00", "2026-11-05T14:30:05Z", "2026-11-05T14:30:05.250-0500", "2026-11-05 14:30+01"] {
            let shown = display(stored, "%d.%m.%Y");
            assert!(shown.starts_with("05.11.2026 "), "{shown}");
            // Typing the shown text back, with the day changed, keeps the time and its zone.
            let typed = shown.replacen("05.11", "06.11", 1);
            let want = stored.replacen("2026-11-05", "2026-11-06", 1);
            assert_eq!(parse(&typed, "%d.%m.%Y", stored), Some(want), "{stored}");
            // Unedited text parses to the same thing it came from.
            assert_eq!(parse(&shown, "%d.%m.%Y", stored).as_deref(), Some(stored));
        }
        assert_eq!(parse("06.11.2026 14:3", "%d.%m.%Y", ""), None);
    }

    #[test]
    fn valid_is_remembered_per_format() {
        for _ in 0..3 {
            assert!(valid("%d.%m.%Y"));
            assert!(!valid("%Y"));
            assert!(!valid("%Q"));
        }
    }

    #[test]
    fn every_preset_is_valid_and_all_but_the_weekday_one_round_trip() {
        for (f, _) in PRESETS.iter().filter(|(f, _)| !f.is_empty()) {
            assert!(valid(f), "{f}");
        }
        for f in ["%d.%m.%Y", "%d/%m/%Y", "%m/%d/%Y", "%-d %b %Y", "%A, %-d %B %Y"] {
            assert!(round_trips(f), "{f}");
        }
        assert!(!round_trips("%a %-d %b"), "no year to read back");
    }
}
