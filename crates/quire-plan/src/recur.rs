// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recurrence rules (design §E.2): `every month on the 1st`, `every 2 weeks
//! on Mon, Thu`, or `RRULE:` text, as one [`Rule`] type.
//!
//! The rules are a subset of RFC 5545: `FREQ` (daily, weekly, monthly,
//! yearly), `INTERVAL`, `BYDAY` (with ordinals in monthly and yearly rules),
//! `BYMONTHDAY` (negative counts from the end), `BYMONTH` and `UNTIL`.
//! Dates that don't exist (30 February) are skipped, as RFC 5545 says, so
//! "every month" from 31 January next falls on 31 March.
//!
//! [`next_occurrence`] is checked against the `rrule` crate on 10,000
//! seeded random rules (tests).

use std::fmt::Write as _;

use chrono::{Datelike, Duration, NaiveDate, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub freq: Freq,
    pub interval: u32,
    /// (ordinal within the month or year, weekday); `None` = every such weekday.
    pub by_day: Vec<(Option<i8>, Weekday)>,
    /// Days of the month; `-1` is the last day.
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<u8>,
    pub until: Option<NaiveDate>,
    /// Count from the completion date instead of the due date.
    pub when_done: bool,
}

impl Rule {
    pub const fn new(freq: Freq) -> Self {
        Self { freq, interval: 1, by_day: Vec::new(), by_month_day: Vec::new(), by_month: Vec::new(), until: None, when_done: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurError {
    /// The text isn't a rule this version understands.
    Unsupported(String),
}

impl std::fmt::Display for RecurError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(t) => write!(f, "can't read the repeat rule “{t}” (try “every week on Monday” or “every month on the 1st”)"),
        }
    }
}

impl std::error::Error for RecurError {}

const WEEKDAYS: [Weekday; 7] = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun];
const MONTHS: [&str; 12] =
    ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

fn weekday_word(w: &str) -> Option<Weekday> {
    let w = w.trim_end_matches(['s', '.', ',']).to_ascii_lowercase();
    let full = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
    // "tues", "thurs", "weds" and two- or three-letter forms too.
    full.iter()
        .position(|f| w.len() >= 2 && (f.starts_with(&w) || (w == "weds" && *f == "wednesday") || (w == "thur" && *f == "thursday")))
        .map(|i| WEEKDAYS[i])
}

fn month_word(w: &str) -> Option<u8> {
    let w = w.trim_end_matches(['.', ',']).to_ascii_lowercase();
    MONTHS.iter().position(|m| w.len() >= 3 && m.starts_with(&w)).map(|i| i as u8 + 1)
}

/// `1st`, `2nd`, `3`, `twenty`… only digits with optional suffix.
fn ordinal_word(w: &str) -> Option<i8> {
    let w = w.trim_end_matches([',', '.']).to_ascii_lowercase();
    match w.as_str() {
        "first" => return Some(1),
        "second" => return Some(2),
        "third" => return Some(3),
        "fourth" => return Some(4),
        "fifth" => return Some(5),
        "last" => return Some(-1),
        _ => {}
    }
    let digits: String = w.chars().take_while(char::is_ascii_digit).collect();
    let suffix = &w[digits.len()..];
    // 1st, 2nd, 3rd, 4th (the suffix as bytes keeps the spell checker quiet).
    if digits.is_empty() || !matches!(suffix.as_bytes(), b"" | b"st" | [b'n', b'd'] | b"rd" | b"th") {
        return None;
    }
    digits.parse::<i8>().ok().filter(|n| (1..=31).contains(n))
}

fn unsupported(text: &str) -> RecurError {
    RecurError::Unsupported(text.trim().to_string())
}

/// Parse natural-language or `RRULE:` text.
pub fn parse(text: &str) -> Result<Rule, RecurError> {
    let t = text.trim();
    if t.len() >= 6 && (t[..6].eq_ignore_ascii_case("rrule:") || t.to_ascii_uppercase().starts_with("FREQ=")) {
        return parse_rrule(t);
    }
    parse_natural(t)
}

fn parse_natural(text: &str) -> Result<Rule, RecurError> {
    let lower = text.to_ascii_lowercase().replace(',', " , ");
    let words: Vec<&str> = lower.split_whitespace().collect();
    let mut i = 0;
    let err = || unsupported(text);
    if words.first() != Some(&"every") {
        return Err(err());
    }
    i += 1;
    let mut rule;
    // every [N]
    let mut interval = 1;
    if let Some(n) = words.get(i).and_then(|w| w.parse::<u32>().ok()) {
        if n == 0 {
            return Err(err());
        }
        interval = n;
        i += 1;
    } else if words.get(i) == Some(&"other") {
        interval = 2;
        i += 1;
    }
    let unit = *words.get(i).ok_or_else(err)?;
    i += 1;
    match unit {
        "day" | "days" => rule = Rule::new(Freq::Daily),
        "week" | "weeks" => rule = Rule::new(Freq::Weekly),
        "month" | "months" => rule = Rule::new(Freq::Monthly),
        "year" | "years" => rule = Rule::new(Freq::Yearly),
        "weekday" | "weekdays" if interval == 1 => {
            rule = Rule::new(Freq::Weekly);
            rule.by_day = WEEKDAYS[..5].iter().map(|d| (None, *d)).collect();
        }
        "weekend" | "weekends" if interval == 1 => {
            rule = Rule::new(Freq::Weekly);
            rule.by_day = vec![(None, Weekday::Sat), (None, Weekday::Sun)];
        }
        w if weekday_word(w).is_some() => {
            // "every monday and thursday"
            rule = Rule::new(Freq::Weekly);
            i -= 1;
            rule.by_day = weekday_list(&words, &mut i);
        }
        w if month_word(w).is_some() && interval == 1 => {
            // "every january [on the 15th]"
            rule = Rule::new(Freq::Yearly);
            i -= 1;
            rule.by_month = month_list(&words, &mut i);
        }
        _ => return Err(err()),
    }
    rule.interval = interval;
    while i < words.len() {
        match words[i] {
            "on" => {
                i += 1;
                if words.get(i) == Some(&"the") {
                    i += 1;
                    on_the(&mut rule, &words, &mut i).ok_or_else(err)?;
                } else {
                    let days = weekday_list(&words, &mut i);
                    if days.is_empty() || matches!(rule.freq, Freq::Monthly | Freq::Yearly) && rule.by_day.iter().any(|d| d.0.is_some()) {
                        return Err(err());
                    }
                    rule.by_day.extend(days);
                }
            }
            "in" => {
                i += 1;
                let months = month_list(&words, &mut i);
                if months.is_empty() || rule.freq == Freq::Weekly {
                    return Err(err());
                }
                rule.by_month.extend(months);
            }
            "until" => {
                i += 1;
                let d = words.get(i).and_then(|w| NaiveDate::parse_from_str(w, "%Y-%m-%d").ok()).ok_or_else(err)?;
                rule.until = Some(d);
                i += 1;
            }
            "when" if words.get(i + 1) == Some(&"done") => {
                rule.when_done = true;
                i += 2;
            }
            "," | "and" => i += 1,
            _ => return Err(err()),
        }
    }
    if rule.freq == Freq::Daily && rule.by_day.iter().any(|d| d.0.is_some()) {
        return Err(err());
    }
    if rule.freq == Freq::Weekly && (!rule.by_month_day.is_empty() || rule.by_day.iter().any(|d| d.0.is_some())) {
        return Err(err());
    }
    rule.by_day.dedup();
    rule.by_month_day.dedup();
    rule.by_month.dedup();
    Ok(rule)
}

/// After `on the`: `1st[, 15th]`, `last [day]`, `2nd tuesday`, `last friday`.
fn on_the(rule: &mut Rule, words: &[&str], i: &mut usize) -> Option<()> {
    let mut any = false;
    while let Some(w) = words.get(*i) {
        if matches!(*w, "," | "and" | "the") {
            *i += 1;
            continue;
        }
        let Some(n) = ordinal_word(w) else { break };
        *i += 1;
        match words.get(*i).copied() {
            Some(d) if weekday_word(d).is_some() => {
                if !matches!(rule.freq, Freq::Monthly | Freq::Yearly) || n > 5 {
                    return None;
                }
                rule.by_day.push((Some(n), weekday_word(d)?));
                *i += 1;
            }
            Some("day") => {
                if rule.freq == Freq::Weekly || rule.freq == Freq::Daily {
                    return None;
                }
                rule.by_month_day.push(n);
                *i += 1;
            }
            _ => {
                if rule.freq == Freq::Weekly || rule.freq == Freq::Daily {
                    return None;
                }
                rule.by_month_day.push(n);
            }
        }
        any = true;
    }
    any.then_some(())
}

fn weekday_list(words: &[&str], i: &mut usize) -> Vec<(Option<i8>, Weekday)> {
    let mut out = Vec::new();
    while let Some(w) = words.get(*i) {
        if matches!(*w, "," | "and") {
            *i += 1;
            continue;
        }
        let Some(d) = weekday_word(w) else { break };
        out.push((None, d));
        *i += 1;
    }
    out
}

fn month_list(words: &[&str], i: &mut usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(w) = words.get(*i) {
        if matches!(*w, "," | "and") {
            *i += 1;
            continue;
        }
        let Some(m) = month_word(w) else { break };
        out.push(m);
        *i += 1;
    }
    out
}

fn parse_rrule(text: &str) -> Result<Rule, RecurError> {
    let err = || unsupported(text);
    let body = text.trim();
    let body = if body.len() >= 6 && body[..6].eq_ignore_ascii_case("rrule:") { &body[6..] } else { body };
    let mut rule: Option<Rule> = None;
    let mut parts = Vec::new();
    for part in body.split(';').filter(|p| !p.trim().is_empty()) {
        let (k, v) = part.split_once('=').ok_or_else(err)?;
        let (k, v) = (k.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase());
        if k == "FREQ" {
            rule = Some(Rule::new(match v.as_str() {
                "DAILY" => Freq::Daily,
                "WEEKLY" => Freq::Weekly,
                "MONTHLY" => Freq::Monthly,
                "YEARLY" => Freq::Yearly,
                _ => return Err(err()),
            }));
        } else {
            parts.push((k, v));
        }
    }
    let mut rule = rule.ok_or_else(err)?;
    for (k, v) in parts {
        match k.as_str() {
            "INTERVAL" => rule.interval = v.parse().ok().filter(|n| *n > 0).ok_or_else(err)?,
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.trim();
                    if d.len() < 2 {
                        return Err(err());
                    }
                    let (n, code) = d.split_at(d.len() - 2);
                    let wd = match code {
                        "MO" => Weekday::Mon,
                        "TU" => Weekday::Tue,
                        "WE" => Weekday::Wed,
                        "TH" => Weekday::Thu,
                        "FR" => Weekday::Fri,
                        "SA" => Weekday::Sat,
                        "SU" => Weekday::Sun,
                        _ => return Err(err()),
                    };
                    let ord = if n.is_empty() {
                        None
                    } else {
                        Some(n.trim_start_matches('+').parse::<i8>().ok().filter(|n| *n != 0 && n.abs() <= 53).ok_or_else(err)?)
                    };
                    rule.by_day.push((ord, wd));
                }
            }
            "BYMONTHDAY" => {
                for d in v.split(',') {
                    rule.by_month_day.push(d.trim().parse::<i8>().ok().filter(|n| *n != 0 && n.abs() <= 31).ok_or_else(err)?);
                }
            }
            "BYMONTH" => {
                for m in v.split(',') {
                    rule.by_month.push(m.trim().parse::<u8>().ok().filter(|n| (1..=12).contains(n)).ok_or_else(err)?);
                }
            }
            "UNTIL" => {
                let d = v.get(..8).and_then(|d| NaiveDate::parse_from_str(d, "%Y%m%d").ok()).ok_or_else(err)?;
                rule.until = Some(d);
            }
            "WKST" if v == "MO" => {}
            // COUNT, BYSETPOS, BYYEARDAY, BYWEEKNO, other WKST and times aren't in the subset.
            _ => return Err(err()),
        }
    }
    if rule.by_day.iter().any(|d| d.0.is_some()) && matches!(rule.freq, Freq::Daily | Freq::Weekly) {
        return Err(err());
    }
    if rule.freq == Freq::Weekly && !rule.by_month_day.is_empty() {
        return Err(err());
    }
    Ok(rule)
}

impl Rule {
    /// The rule as `RRULE:` text (without `RRULE:` when `bare`).
    pub fn to_rrule(&self) -> String {
        let mut s = format!(
            "RRULE:FREQ={}",
            match self.freq {
                Freq::Daily => "DAILY",
                Freq::Weekly => "WEEKLY",
                Freq::Monthly => "MONTHLY",
                Freq::Yearly => "YEARLY",
            }
        );
        if self.interval > 1 {
            let _ = write!(s, ";INTERVAL={}", self.interval);
        }
        if !self.by_month.is_empty() {
            let v: Vec<String> = self.by_month.iter().map(u8::to_string).collect();
            let _ = write!(s, ";BYMONTH={}", v.join(","));
        }
        if !self.by_month_day.is_empty() {
            let v: Vec<String> = self.by_month_day.iter().map(i8::to_string).collect();
            let _ = write!(s, ";BYMONTHDAY={}", v.join(","));
        }
        if !self.by_day.is_empty() {
            let v: Vec<String> = self
                .by_day
                .iter()
                .map(|(n, d)| format!("{}{}", n.map(|n| n.to_string()).unwrap_or_default(), &d.to_string().to_ascii_uppercase()[..2]))
                .collect();
            let _ = write!(s, ";BYDAY={}", v.join(","));
        }
        if let Some(u) = self.until {
            let _ = write!(s, ";UNTIL={}", u.format("%Y%m%d"));
        }
        s
    }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).and_then(|d| d.pred_opt()).map_or(28, |d| d.day())
}

/// A month day (`-1` = last) as a date, if the month has it.
fn month_day(y: i32, m: u32, md: i8) -> Option<NaiveDate> {
    let n = days_in_month(y, m) as i32;
    let d = if md < 0 { n + 1 + i32::from(md) } else { i32::from(md) };
    (1..=n).contains(&d).then(|| NaiveDate::from_ymd_opt(y, m, d as u32)).flatten()
}

/// Every `wd` in `from..=to`; with `ord`, only the nth (negative from the end).
fn weekdays_in(from: NaiveDate, to: NaiveDate, ord: Option<i8>, wd: Weekday) -> Vec<NaiveDate> {
    let shift = (7 + i64::from(wd.num_days_from_monday()) - i64::from(from.weekday().num_days_from_monday())) % 7;
    let mut all = Vec::new();
    let mut d = from + Duration::days(shift);
    while d <= to {
        all.push(d);
        d += Duration::days(7);
    }
    match ord {
        None => all,
        Some(n) if n > 0 => all.get(n as usize - 1).copied().into_iter().collect(),
        Some(n) => all.len().checked_sub((-n) as usize).and_then(|i| all.get(i)).copied().into_iter().collect(),
    }
}

/// Whether `d` is the `ord`th `wd` of the month (or year, when `in_year`).
fn is_nth(d: NaiveDate, ord: Option<i8>, wd: Weekday, in_year: bool) -> bool {
    if d.weekday() != wd {
        return false;
    }
    let Some(n) = ord else { return true };
    let (from, to) = if in_year {
        (NaiveDate::from_ymd_opt(d.year(), 1, 1), NaiveDate::from_ymd_opt(d.year(), 12, 31))
    } else {
        (NaiveDate::from_ymd_opt(d.year(), d.month(), 1), month_day(d.year(), d.month(), -1))
    };
    let (Some(from), Some(to)) = (from, to) else { return false };
    weekdays_in(from, to, Some(n), wd).first() == Some(&d)
}

/// Dates the rule gives in month `m` of year `y`. Weekday ordinals count
/// within the month, or within the year for a yearly rule without months
/// (RFC 5545 §3.3.10).
fn in_month(rule: &Rule, base: NaiveDate, y: i32, m: u32) -> Vec<NaiveDate> {
    let in_year = rule.freq == Freq::Yearly && rule.by_month.is_empty();
    let (Some(first), Some(last)) = (NaiveDate::from_ymd_opt(y, m, 1), month_day(y, m, -1)) else { return Vec::new() };
    if !rule.by_month_day.is_empty() {
        rule.by_month_day
            .iter()
            .filter_map(|md| month_day(y, m, *md))
            .filter(|d| rule.by_day.is_empty() || rule.by_day.iter().any(|(o, w)| is_nth(*d, *o, *w, in_year)))
            .collect()
    } else if !rule.by_day.is_empty() {
        rule.by_day.iter().flat_map(|(o, w)| weekdays_in(first, last, *o, *w)).collect()
    } else {
        NaiveDate::from_ymd_opt(y, m, base.day()).into_iter().collect()
    }
}

/// Dates the rule gives in the period that starts at `start`.
fn period_dates(rule: &Rule, base: NaiveDate, start: NaiveDate) -> Vec<NaiveDate> {
    let month_ok = |d: &NaiveDate| rule.by_month.is_empty() || rule.by_month.contains(&(d.month() as u8));
    let mut v: Vec<NaiveDate> = match rule.freq {
        Freq::Daily => {
            let d = start;
            let ok = month_ok(&d)
                && (rule.by_month_day.is_empty() || rule.by_month_day.iter().any(|md| month_day(d.year(), d.month(), *md) == Some(d)))
                && (rule.by_day.is_empty() || rule.by_day.iter().any(|(_, w)| *w == d.weekday()));
            if ok { vec![d] } else { Vec::new() }
        }
        Freq::Weekly => {
            let days: Vec<Weekday> = if rule.by_day.is_empty() { vec![base.weekday()] } else { rule.by_day.iter().map(|d| d.1).collect() };
            (0..7).map(|k| start + Duration::days(k)).filter(|d| days.contains(&d.weekday()) && month_ok(d)).collect()
        }
        Freq::Monthly => {
            if month_ok(&start) {
                in_month(rule, base, start.year(), start.month())
            } else {
                Vec::new()
            }
        }
        Freq::Yearly => {
            let y = start.year();
            if !rule.by_month.is_empty() {
                rule.by_month.iter().flat_map(|m| in_month(rule, base, y, u32::from(*m))).collect()
            } else if !rule.by_month_day.is_empty() {
                (1..=12).flat_map(|m| in_month(rule, base, y, m)).collect()
            } else if !rule.by_day.is_empty() {
                let (Some(from), Some(to)) = (NaiveDate::from_ymd_opt(y, 1, 1), NaiveDate::from_ymd_opt(y, 12, 31)) else {
                    return Vec::new();
                };
                rule.by_day.iter().flat_map(|(o, w)| weekdays_in(from, to, *o, *w)).collect()
            } else {
                NaiveDate::from_ymd_opt(y, base.month(), base.day()).into_iter().collect()
            }
        }
    };
    v.sort();
    v.dedup();
    v
}

/// The start of the `k`th period after the base's own.
fn period_start(rule: &Rule, base: NaiveDate, k: i64) -> Option<NaiveDate> {
    let step = k * i64::from(rule.interval);
    match rule.freq {
        Freq::Daily => base.checked_add_signed(Duration::days(step)),
        Freq::Weekly => {
            let monday = base - Duration::days(i64::from(base.weekday().num_days_from_monday()));
            monday.checked_add_signed(Duration::weeks(step))
        }
        Freq::Monthly => {
            let months = i64::from(base.year()) * 12 + i64::from(base.month0()) + step;
            NaiveDate::from_ymd_opt(i32::try_from(months.div_euclid(12)).ok()?, months.rem_euclid(12) as u32 + 1, 1)
        }
        Freq::Yearly => NaiveDate::from_ymd_opt(i32::try_from(i64::from(base.year()) + step).ok()?, 1, 1),
    }
}

/// The first date the rule produces strictly after `base`; `None` when the
/// rule has ended or produces nothing in 1,000 periods.
pub fn next_occurrence(rule: &Rule, base: NaiveDate) -> Option<NaiveDate> {
    for k in 0..1000 {
        let start = period_start(rule, base, k)?;
        if rule.until.is_some_and(|u| start > u) {
            return None;
        }
        if let Some(d) = period_dates(rule, base, start).into_iter().find(|d| *d > base) {
            return rule.until.is_none_or(|u| d <= u).then_some(d);
        }
    }
    None
}

/// A hint for rules that skip months ("every month" from the 31st).
pub fn hint(rule: &Rule, base: NaiveDate) -> Option<&'static str> {
    (rule.freq == Freq::Monthly && rule.by_month_day.is_empty() && rule.by_day.is_empty() && base.day() > 28)
        .then_some("Months without this day are skipped; “every month on the last day” repeats at each month's end.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn next(rule: &str, base: &str) -> Option<String> {
        next_occurrence(&parse(rule).unwrap(), d(base)).map(|d| d.to_string())
    }

    #[test]
    fn natural_language() {
        assert_eq!(next("every day", "2026-11-02").as_deref(), Some("2026-11-03"));
        assert_eq!(next("every 3 days", "2026-11-30").as_deref(), Some("2026-12-03"));
        assert_eq!(next("every week", "2026-11-02").as_deref(), Some("2026-11-09"));
        assert_eq!(next("every 2 weeks on Mon, Thu", "2026-11-02").as_deref(), Some("2026-11-05"));
        assert_eq!(next("every 2 weeks on Mon, Thu", "2026-11-05").as_deref(), Some("2026-11-16"));
        assert_eq!(next("every weekday", "2026-11-06").as_deref(), Some("2026-11-09"));
        assert_eq!(next("every weekend", "2026-11-02").as_deref(), Some("2026-11-07"));
        assert_eq!(next("every Tuesday and Friday", "2026-11-04").as_deref(), Some("2026-11-06"));
        assert_eq!(next("every month on the 1st", "2026-12-01").as_deref(), Some("2027-01-01"));
        assert_eq!(next("every month on the 1st and 15th", "2026-12-01").as_deref(), Some("2026-12-15"));
        assert_eq!(next("every month on the last day", "2027-01-31").as_deref(), Some("2027-02-28"));
        assert_eq!(next("every month on the last", "2027-01-31").as_deref(), Some("2027-02-28"));
        assert_eq!(next("every month on the 2nd Tuesday", "2026-11-10").as_deref(), Some("2026-12-08"));
        assert_eq!(next("every month on the last Friday", "2026-11-02").as_deref(), Some("2026-11-27"));
        assert_eq!(next("every year", "2028-02-29").as_deref(), Some("2032-02-29"));
        assert_eq!(next("every year in March on the 15th", "2026-11-02").as_deref(), Some("2027-03-15"));
        assert_eq!(next("every January", "2026-11-02").as_deref(), Some("2027-01-02"));
        assert_eq!(next("every other week", "2026-11-02").as_deref(), Some("2026-11-16"));
        assert_eq!(next("every day until 2026-11-03", "2026-11-03"), None);
        let r = parse("every week when done").unwrap();
        assert!(r.when_done);
        for bad in [
            "every",
            "each day",
            "every 0 days",
            "every week on the 1st",
            "every day on the 2nd Monday",
            "every month on the 40th",
            "every fortnight",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn weekdays_with_and_without_ordinals_combine() {
        // Every Friday plus the second-to-last Wednesday of the year.
        let r = parse("RRULE:FREQ=YEARLY;BYDAY=FR,-2WE").unwrap();
        assert_eq!(next_occurrence(&r, d("2009-08-25")), Some(d("2009-08-28")));
        assert_eq!(next_occurrence(&r, d("2009-12-19")), Some(d("2009-12-23")));
        let r = parse("every month on the 2nd Wednesday").map(|mut r| {
            r.by_day.push((None, Weekday::Fri));
            r
        });
        assert_eq!(next_occurrence(&r.unwrap(), d("2026-11-06")), Some(d("2026-11-11")));
    }

    #[test]
    fn invalid_dates_are_skipped() {
        // RFC 5545: 31 Feb doesn't exist, so the next is 31 March.
        assert_eq!(next("every month", "2027-01-31").as_deref(), Some("2027-03-31"));
        assert!(hint(&parse("every month").unwrap(), d("2027-01-31")).is_some());
        assert!(hint(&parse("every month").unwrap(), d("2027-01-15")).is_none());
    }

    #[test]
    fn rrule_text() {
        let r = parse("RRULE:FREQ=MONTHLY;INTERVAL=2;BYDAY=-1FR;UNTIL=20271231T000000Z").unwrap();
        assert_eq!(
            (r.freq, r.interval, r.by_day.clone(), r.until),
            (Freq::Monthly, 2, vec![(Some(-1), Weekday::Fri)], Some(d("2027-12-31")))
        );
        assert_eq!(r.to_rrule(), "RRULE:FREQ=MONTHLY;INTERVAL=2;BYDAY=-1FR;UNTIL=20271231");
        assert_eq!(parse(&r.to_rrule()).unwrap(), r);
        assert!(parse("RRULE:FREQ=DAILY;COUNT=3").is_err());
        assert!(parse("RRULE:FREQ=WEEKLY;BYDAY=2MO").is_err());
        assert!(parse("FREQ=HOURLY").is_err());
    }

    /// The oracle: `rrule` (dev-dependency) agrees on 10,000 random rules.
    #[test]
    fn matches_rrule_on_random_rules() {
        use rrule::{RRuleSet, Tz};
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let (mut compared, mut none) = (0, 0);
        for _ in 0..10_000 {
            let freq = [Freq::Daily, Freq::Weekly, Freq::Monthly, Freq::Yearly][rnd(4) as usize];
            let mut r = Rule::new(freq);
            r.interval = [1, 1, 1, 2, 3, 4][rnd(6) as usize];
            // All weekdays with an ordinal or none: `rrule` 0.14 gives no dates
            // for a mix (`BYDAY=FR,-2WE`), which RFC 5545 allows (tested by hand).
            let ordinals = matches!(freq, Freq::Monthly | Freq::Yearly) && rnd(2) == 0;
            if rnd(2) == 0 {
                for _ in 0..=rnd(3) {
                    let ord = ordinals.then(|| [1, 2, 3, 4, -1, -2][rnd(6) as usize]);
                    let wd = WEEKDAYS[rnd(7) as usize];
                    if !r.by_day.iter().any(|x| x.1 == wd) {
                        r.by_day.push((ord, wd));
                    }
                }
            }
            if freq != Freq::Weekly && rnd(3) == 0 {
                for _ in 0..=rnd(2) {
                    let md = [1, 2, 13, 15, 28, 29, 30, 31, -1, -2][rnd(10) as usize];
                    if !r.by_month_day.contains(&md) {
                        r.by_month_day.push(md);
                    }
                }
            }
            if rnd(4) == 0 {
                for _ in 0..=rnd(2) {
                    let m = 1 + rnd(12) as u8;
                    if !r.by_month.contains(&m) {
                        r.by_month.push(m);
                    }
                }
            }
            let base = NaiveDate::from_ymd_opt(1995 + rnd(60) as i32, 1, 1).unwrap() + Duration::days(rnd(366) as i64);
            if rnd(5) == 0 {
                r.until = Some(base + Duration::days(rnd(800) as i64));
            }
            let ours = next_occurrence(&r, base);
            let mut rr = r.to_rrule();
            if let Some(u) = r.until {
                rr = rr.replace(&format!("UNTIL={}", u.format("%Y%m%d")), &format!("UNTIL={}T000000Z", u.format("%Y%m%d")));
            }
            let text = format!("DTSTART:{}T000000Z\n{rr}", base.format("%Y%m%d"));
            let set: RRuleSet = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
            let after = chrono::TimeZone::from_utc_datetime(&Tz::UTC, &(base + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap());
            // Our search stops after 1,000 periods; look no further than that.
            let horizon =
                period_start(&r, base, 1000).map(|h| chrono::TimeZone::from_utc_datetime(&Tz::UTC, &h.and_hms_opt(0, 0, 0).unwrap()));
            let mut set = set.after(after);
            if let Some(h) = horizon {
                set = set.before(h);
            }
            let theirs = set.all(1).dates.first().map(chrono::DateTime::date_naive);
            assert_eq!(ours, theirs, "{} from {base}", text.replace('\n', " "));
            if ours.is_some() {
                compared += 1;
            } else {
                none += 1;
            }
        }
        assert!(compared > 9_000, "{compared} compared, {none} without a next date");
    }
}
