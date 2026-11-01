// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Natural-language dates: `@today`, `@tomorrow`, `@next friday`, `@in 3 days`,
//! `@2 weeks ago`, `@dec 25`, `@2026-12-25`.

use chrono::{Datelike, Duration, Months, NaiveDate, Weekday};

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month(s: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let s = s.get(..3)?;
    M.iter().position(|m| *m == s).map(|i| i as u32 + 1)
}

fn add_units(today: NaiveDate, n: i64, unit: &str) -> Option<NaiveDate> {
    let unit = unit.trim_end_matches('s');
    match unit {
        "day" => today.checked_add_signed(Duration::days(n)),
        "week" => today.checked_add_signed(Duration::weeks(n)),
        "month" if n >= 0 => today.checked_add_months(Months::new(n as u32)),
        "month" => today.checked_sub_months(Months::new(n.unsigned_abs() as u32)),
        "year" if n >= 0 => today.checked_add_months(Months::new(n as u32 * 12)),
        "year" => today.checked_sub_months(Months::new(n.unsigned_abs() as u32 * 12)),
        _ => None,
    }
}

fn number(s: &str) -> Option<i64> {
    s.parse().ok().or(match s {
        "a" | "an" | "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "ten" => Some(10),
        _ => None,
    })
}

/// Parse a phrase (without the `@`) relative to `today`.
pub fn parse_natural_date(phrase: &str, today: NaiveDate) -> Option<NaiveDate> {
    let p = phrase.trim().to_lowercase();
    let words: Vec<&str> = p.split_whitespace().collect();
    match words.as_slice() {
        ["today" | "now"] => Some(today),
        ["tomorrow" | "tmr" | "tmrw"] => today.succ_opt(),
        ["yesterday"] => today.pred_opt(),
        [w] if weekday(w).is_some() => {
            let target = weekday(w)?;
            let delta = (7 + target.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64) % 7;
            today.checked_add_signed(Duration::days(if delta == 0 { 7 } else { delta }))
        }
        ["next", w] if weekday(w).is_some() => {
            // "next friday" = the friday of next week.
            let target = weekday(w)?;
            let monday_next = today + Duration::days(7 - today.weekday().num_days_from_monday() as i64);
            Some(monday_next + Duration::days(target.num_days_from_monday() as i64))
        }
        ["next", "week"] => Some(today + Duration::weeks(1)),
        ["next", "month"] => today.checked_add_months(Months::new(1)),
        ["next", "year"] => today.checked_add_months(Months::new(12)),
        ["in", n, unit] => add_units(today, number(n)?, unit),
        [n, unit, "ago"] => add_units(today, -number(n)?, unit),
        [m, d] if month(m).is_some() => {
            let (m, d) = (month(m)?, d.trim_end_matches(|c: char| c.is_alphabetic()).parse().ok()?);
            let this_year = NaiveDate::from_ymd_opt(today.year(), m, d)?;
            if this_year < today { NaiveDate::from_ymd_opt(today.year() + 1, m, d) } else { Some(this_year) }
        }
        [iso] => NaiveDate::parse_from_str(iso, "%Y-%m-%d").ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateMatch {
    /// Char offset of the `@` within the line.
    pub start_char: usize,
    pub date: NaiveDate,
}

/// Find the longest `@phrase` (up to three words) that ends exactly at the end
/// of `before_caret` and parses as a date. Call this when the user types a
/// space or presses Enter.
pub fn trailing_date_phrase(before_caret: &str, today: NaiveDate) -> Option<DateMatch> {
    let at = before_caret.rfind('@')?;
    let phrase = &before_caret[at + 1..];
    let preceded_ok = before_caret[..at].chars().next_back().is_none_or(char::is_whitespace);
    if !preceded_ok || phrase.is_empty() || phrase.split_whitespace().count() > 3 || phrase.starts_with(' ') {
        return None;
    }
    let date = parse_natural_date(phrase, today)?;
    Some(DateMatch { start_char: before_caret[..at].chars().count(), date })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn parses_phrases() {
        let sun = d(2026, 11, 1); // a Sunday
        assert_eq!(parse_natural_date("today", sun), Some(sun));
        assert_eq!(parse_natural_date("Tomorrow", sun), Some(d(2026, 11, 2)));
        assert_eq!(parse_natural_date("friday", sun), Some(d(2026, 11, 6)));
        assert_eq!(parse_natural_date("sunday", sun), Some(d(2026, 11, 8)));
        assert_eq!(parse_natural_date("next friday", sun), Some(d(2026, 11, 6)));
        assert_eq!(parse_natural_date("next friday", d(2026, 11, 4)), Some(d(2026, 11, 13)));
        assert_eq!(parse_natural_date("in 3 days", sun), Some(d(2026, 11, 4)));
        assert_eq!(parse_natural_date("2 weeks ago", sun), Some(d(2026, 10, 18)));
        assert_eq!(parse_natural_date("in a month", sun), Some(d(2026, 12, 1)));
        assert_eq!(parse_natural_date("dec 25", sun), Some(d(2026, 12, 25)));
        assert_eq!(parse_natural_date("jan 5th", sun), Some(d(2027, 1, 5)));
        assert_eq!(parse_natural_date("2027-02-03", sun), Some(d(2027, 2, 3)));
        assert_eq!(parse_natural_date("someday", sun), None);
    }

    #[test]
    fn finds_trailing_phrase() {
        let sun = d(2026, 11, 1);
        let m = trailing_date_phrase("Call Asha @next friday", sun).unwrap();
        assert_eq!(m.start_char, 10);
        assert_eq!(m.date, d(2026, 11, 6));
        assert!(trailing_date_phrase("email me@today", sun).is_none());
        assert!(trailing_date_phrase("@ today", sun).is_none());
    }
}
