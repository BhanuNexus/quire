// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Natural-language dates: `@today`, `@tomorrow`, `@next friday`, `@in 3 days`,
//! `@2 weeks ago`, `@dec 25`, `@2026-12-25`.

use quire_base::dates::{self, Date, Span, Weekday, from_monday};

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Monday,
        "tue" | "tues" | "tuesday" => Weekday::Tuesday,
        "wed" | "wednesday" => Weekday::Wednesday,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thursday,
        "fri" | "friday" => Weekday::Friday,
        "sat" | "saturday" => Weekday::Saturday,
        "sun" | "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

fn month(s: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let s = s.get(..3)?;
    M.iter().position(|m| *m == s).map(|i| i as u32 + 1)
}

fn add_units(today: Date, n: i64, unit: &str) -> Option<Date> {
    let unit = unit.trim_end_matches('s');
    match unit {
        "day" => dates::add_days(today, n),
        "week" => dates::add_days(today, n.checked_mul(7)?),
        "month" => today.checked_add(Span::new().try_months(n).ok()?).ok(),
        "year" => today.checked_add(Span::new().try_months(n.checked_mul(12)?).ok()?).ok(),
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
pub fn parse_natural_date(phrase: &str, today: Date) -> Option<Date> {
    let p = phrase.trim().to_lowercase();
    let words: Vec<&str> = p.split_whitespace().collect();
    match words.as_slice() {
        ["today" | "now"] => Some(today),
        ["tomorrow" | "tmr" | "tmrw"] => today.tomorrow().ok(),
        ["yesterday"] => today.yesterday().ok(),
        [w] if weekday(w).is_some() => {
            let target = weekday(w)?;
            let delta = (7 + from_monday(target) - from_monday(today.weekday())) % 7;
            dates::add_days(today, if delta == 0 { 7 } else { delta })
        }
        ["next", w] if weekday(w).is_some() => {
            // "next friday" = the friday of next week.
            let target = weekday(w)?;
            let monday_next = dates::add_days(today, 7 - from_monday(today.weekday()))?;
            dates::add_days(monday_next, from_monday(target))
        }
        ["next", "week"] => dates::add_days(today, 7),
        ["next", "month"] => today.checked_add(Span::new().months(1)).ok(),
        ["next", "year"] => today.checked_add(Span::new().months(12)).ok(),
        ["in", n, unit] => add_units(today, number(n)?, unit),
        [n, unit, "ago"] => add_units(today, -number(n)?, unit),
        [m, d] if month(m).is_some() => {
            let (m, d) = (month(m)?, d.trim_end_matches(|c: char| c.is_alphabetic()).parse().ok()?);
            let this_year = dates::ymd_opt(i32::from(today.year()), m, d)?;
            if this_year < today { dates::ymd_opt(i32::from(today.year()) + 1, m, d) } else { Some(this_year) }
        }
        [iso] => dates::parse_ymd(iso),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateMatch {
    /// Char offset of the `@` within the line.
    pub start_char: usize,
    pub date: Date,
}

/// Find the longest `@phrase` (up to three words) that ends exactly at the end
/// of `before_caret` and parses as a date. Call this when the user types a
/// space or presses Enter.
pub fn trailing_date_phrase(before_caret: &str, today: Date) -> Option<DateMatch> {
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

    fn d(y: i32, m: u32, day: u32) -> Date {
        dates::ymd_opt(y, m, day).unwrap()
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
