// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Time spent on tasks (design 0.11-tasks §2): time entries in a task
//! note's front matter, durations on inline tasks, and the weekly report.
//!
//! A task note keeps one entry per work session:
//!
//! ```yaml
//! time_entries:
//!   - start: 2026-11-06T10:00:00+01:00
//!     end: 2026-11-06T10:25:00+01:00
//! ```
//!
//! TaskNotes writes the same list as `timeEntries` with `startTime` and
//! `endTime`; both are read. Instants are ISO 8601 with their offset, so a
//! session keeps the local time it was recorded in. An inline task keeps
//! only a total: `[time:: 1h25m]`.

use std::fmt::Write as _;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};

/// One work session. `end` is `None` while it runs (TaskNotes writes an
/// entry without `endTime` for a running timer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeEntry {
    pub start: DateTime<FixedOffset>,
    pub end: Option<DateTime<FixedOffset>>,
    pub description: Option<String>,
}

impl TimeEntry {
    /// Whole minutes in the session (an open one counts up to `now`, a
    /// backwards one counts nothing).
    pub fn minutes(&self, now: DateTime<FixedOffset>) -> u32 {
        let end = self.end.unwrap_or(now);
        u32::try_from((end - self.start).num_seconds().max(0) / 60).unwrap_or(u32::MAX)
    }

    /// Seconds in the session, as [`Self::minutes`].
    pub fn seconds(&self, now: DateTime<FixedOffset>) -> i64 {
        (self.end.unwrap_or(now) - self.start).num_seconds().max(0)
    }
}

/// Which property names new entries are written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// `time_entries` with `start` and `end`.
    #[default]
    Quire,
    /// `timeEntries` with `startTime` and `endTime` (the TaskNotes plugin).
    TaskNotes,
}

impl Style {
    pub const fn list_key(self) -> &'static str {
        match self {
            Self::Quire => "time_entries",
            Self::TaskNotes => "timeEntries",
        }
    }

    const fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Quire => ("start", "end"),
            Self::TaskNotes => ("startTime", "endTime"),
        }
    }
}

/// `2026-11-06T10:00:00+01:00`.
pub fn format_instant(t: DateTime<FixedOffset>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// An instant: RFC 3339 (`2026-11-06T10:00:00+01:00`, `…Z`), or a local
/// date and time without an offset (`2026-11-06 10:00`), read in `offset`.
pub fn parse_instant(s: &str, offset: FixedOffset) -> Option<DateTime<FixedOffset>> {
    let s = s.trim().trim_matches(['"', '\'']);
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t);
    }
    // Minutes only: `2026-11-06T10:00+01:00`.
    if let Ok(t) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M%:z") {
        return Some(t);
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return offset.from_local_datetime(&n).single();
        }
    }
    None
}

/// The YAML of one entry, without the list dash: `start: …` / `end: …`.
pub fn entry_yaml(e: &TimeEntry, style: Style) -> String {
    let (sk, ek) = style.keys();
    let mut s = format!("{sk}: {}", format_instant(e.start));
    if let Some(end) = e.end {
        let _ = write!(s, "\n{ek}: {}", format_instant(end));
    }
    if let Some(d) = e.description.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(s, "\ndescription: {}", quire_markdown::frontmatter::yaml_scalar(d));
    }
    s
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// `key: value` pairs of one item, from its block lines or a flow map.
fn item_pairs(lines: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for l in lines {
        let t = l.trim();
        let flow = t.strip_prefix('{').and_then(|t| t.strip_suffix('}'));
        // A flow map splits at commas (instants have none).
        let parts: Vec<&str> = flow.map_or_else(|| vec![t], |f| f.split(',').collect());
        for part in parts {
            let part = part.trim();
            let at = part.find(": ").or_else(|| part.ends_with(':').then(|| part.len() - 1));
            if let Some(at) = at {
                let v = part[at + 1..].trim();
                // `# comment` after the value.
                let v = v.find(" #").map_or(v, |c| v[..c].trim_end());
                out.push((unquote(&part[..at]), unquote(v)));
            }
        }
    }
    out
}

/// The entries in a list of maps, as text.
///
/// That's the raw block the front-matter reader keeps, or the whole field as
/// [`field_text`](quire_markdown::frontmatter::field_text) gives it. Items
/// without a readable start are skipped. `offset` is used for instants
/// written without one.
pub fn parse_entries(raw: &str, offset: FixedOffset) -> Vec<TimeEntry> {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect();
    // The dash column of the items: the least-indented `- ` line.
    let Some(dash) = lines.iter().filter(|l| l.trim_start().starts_with('-')).map(|l| l.len() - l.trim_start().len()).min() else {
        return Vec::new();
    };
    let mut items: Vec<Vec<&str>> = Vec::new();
    for l in &lines {
        let indent = l.len() - l.trim_start().len();
        if indent == dash && l.trim_start().starts_with('-') {
            items.push(vec![l.trim_start()[1..].trim_start()]);
        } else if indent > dash
            && let Some(last) = items.last_mut()
        {
            last.push(l);
        }
    }
    items
        .iter()
        .filter_map(|it| {
            let pairs = item_pairs(it);
            let get = |keys: &[&str]| pairs.iter().find(|(k, _)| keys.iter().any(|w| k.eq_ignore_ascii_case(w))).map(|(_, v)| v.as_str());
            let start = parse_instant(get(&["start", "startTime"])?, offset)?;
            let end = get(&["end", "endTime"]).filter(|v| !v.is_empty() && *v != "null").and_then(|v| parse_instant(v, offset));
            let description = get(&["description", "note"]).filter(|d| !d.is_empty()).map(str::to_string);
            Some(TimeEntry { start, end, description })
        })
        .collect()
}

/// Minutes from a duration: `1h25m`, `1h 25m`, `25m`, `2h`, `1.5h`,
/// `1:25`, `PT1H25M` (ISO 8601), or a bare number of minutes (`90`, as
/// TaskNotes writes `timeEstimate`).
pub fn parse_duration(s: &str) -> Option<u32> {
    let t = s.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<u32>() {
        return Some(n);
    }
    if let Some((h, m)) = t.split_once(':')
        && let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>())
        && m < 60
    {
        return h.checked_mul(60)?.checked_add(m);
    }
    let body = t.strip_prefix("pt").or_else(|| t.strip_prefix('p')).unwrap_or(&t);
    let (mut total, mut num, mut any) = (0.0f64, String::new(), false);
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        // A unit: h, hr(s), hour(s), m, min(s), minute(s), s, sec(s), d.
        let mut unit = String::from(c);
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphabetic() {
                unit.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        let mins = match unit.as_str() {
            "d" | "day" | "days" => n * 1440.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => n * 60.0,
            "m" | "min" | "mins" | "minute" | "minutes" => n,
            "s" | "sec" | "secs" | "second" | "seconds" => n / 60.0,
            _ => return None,
        };
        total += mins;
        any = true;
    }
    if !num.is_empty() || !any || !total.is_finite() || total > f64::from(u32::MAX) {
        return None;
    }
    Some(total.round() as u32)
}

/// `1h25m`, `25m`, `2h`, `0m`.
pub fn format_duration(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

/// `1:05:09` or `25:09` for a running timer.
pub fn format_clock(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// Minutes in all of `entries`, open ones counting up to `now`.
pub fn total_minutes(entries: &[TimeEntry], now: DateTime<FixedOffset>) -> u32 {
    let secs: i64 = entries.iter().map(|e| e.seconds(now)).sum();
    u32::try_from(secs / 60).unwrap_or(u32::MAX)
}

/// Seconds of an entry on each local day it touches (in the entry's own
/// offset), so a session past midnight counts on both days.
pub fn split_by_day(e: &TimeEntry, now: DateTime<FixedOffset>) -> Vec<(NaiveDate, i64)> {
    let end = e.end.unwrap_or(now).with_timezone(&e.start.timezone());
    let mut out = Vec::new();
    let mut at = e.start;
    while at < end {
        let day = at.date_naive();
        let next_midnight =
            day.succ_opt().and_then(|d| d.and_hms_opt(0, 0, 0)).and_then(|n| e.start.timezone().from_local_datetime(&n).single());
        let stop = next_midnight.map_or(end, |m| m.min(end));
        out.push((day, (stop - at).num_seconds()));
        if stop <= at {
            break;
        }
        at = stop;
    }
    out
}

/// One task in the weekly report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    pub path: String,
    pub title: String,
    /// Minutes Monday … Sunday.
    pub days: [u32; 7],
    pub total: u32,
}

/// Time spent per task and day in the week starting `monday`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WeekReport {
    pub monday: Option<NaiveDate>,
    /// Most time first.
    pub rows: Vec<ReportRow>,
    /// Minutes per day across all tasks.
    pub days: [u32; 7],
    pub total: u32,
}

/// The weekly report over `(path, title, entries)`. Seconds are summed per
/// day before rounding down to minutes.
pub fn week_report<'a>(
    tasks: impl IntoIterator<Item = (&'a str, &'a str, &'a [TimeEntry])>,
    monday: NaiveDate,
    now: DateTime<FixedOffset>,
) -> WeekReport {
    let mut rows = Vec::new();
    let mut day_secs = [0i64; 7];
    for (path, title, entries) in tasks {
        let mut secs = [0i64; 7];
        for e in entries {
            for (d, s) in split_by_day(e, now) {
                let i = (d - monday).num_days();
                if (0..7).contains(&i) {
                    secs[i as usize] += s;
                }
            }
        }
        if secs.iter().all(|s| *s == 0) {
            continue;
        }
        let days = secs.map(|s| u32::try_from(s / 60).unwrap_or(u32::MAX));
        for (t, s) in day_secs.iter_mut().zip(secs) {
            *t += s;
        }
        let total = u32::try_from(secs.iter().sum::<i64>() / 60).unwrap_or(u32::MAX);
        rows.push(ReportRow { path: path.to_string(), title: title.to_string(), days, total });
    }
    rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())));
    let days = day_secs.map(|s| u32::try_from(s / 60).unwrap_or(u32::MAX));
    let total = u32::try_from(day_secs.iter().sum::<i64>() / 60).unwrap_or(u32::MAX);
    WeekReport { monday: Some(monday), rows, days, total }
}

/// An entry from `start` to `end`, the length of a stopped timer.
pub fn session(start: DateTime<FixedOffset>, end: DateTime<FixedOffset>) -> TimeEntry {
    TimeEntry { start, end: Some(end.max(start)), description: None }
}

/// Add `minutes` to an inline total (`[time:: 1h25m]`); an unreadable old
/// value counts as nothing.
pub fn add_to_total(old: Option<&str>, minutes: u32) -> String {
    format_duration(old.and_then(parse_duration).unwrap_or(0).saturating_add(minutes))
}

/// Round a session down to whole seconds (front matter keeps seconds).
pub fn trunc_seconds(t: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    t - Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(h: i32) -> FixedOffset {
        FixedOffset::east_opt(h * 3600).unwrap()
    }

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    #[test]
    fn reads_both_styles_and_flow_items() {
        let raw = "  - start: 2026-11-06T10:00:00+01:00\n    end: \"2026-11-06T10:25:00+01:00\"\n  - {startTime: 2026-11-06T09:00:00Z, endTime: 2026-11-06T09:30:00Z, description: standup}\n  - startTime: \"2026-11-06T12:00:00Z\"\n  - end: 2026-11-06T13:00:00Z\n";
        let e = parse_entries(raw, off(1));
        assert_eq!(e.len(), 3, "the item without a start is skipped");
        assert_eq!(e[0].minutes(at("2026-11-07T00:00:00Z")), 25);
        assert_eq!(e[1].description.as_deref(), Some("standup"));
        assert_eq!(e[1].minutes(at("2026-11-07T00:00:00Z")), 30);
        assert!(e[2].end.is_none(), "a running TaskNotes entry");
        assert_eq!(e[2].minutes(at("2026-11-06T12:10:00Z")), 10);
        // The whole field's text works too (as `field_text` gives it).
        let field = "time_entries:\n  - start: 2026-11-06 10:00\n    end: 2026-11-06 10:45 # meeting\n";
        let e = parse_entries(field, off(2));
        assert_eq!(e[0].start, at("2026-11-06T10:00:00+02:00"));
        assert_eq!(e[0].minutes(at("2027-01-01T00:00:00Z")), 45);
    }

    #[test]
    fn writes_entries() {
        let e = session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:30+01:00"));
        assert_eq!(entry_yaml(&e, Style::Quire), "start: 2026-11-06T10:00:00+01:00\nend: 2026-11-06T10:25:30+01:00");
        assert_eq!(entry_yaml(&e, Style::TaskNotes), "startTime: 2026-11-06T10:00:00+01:00\nendTime: 2026-11-06T10:25:30+01:00");
        let back = parse_entries(&format!("- {}", entry_yaml(&e, Style::TaskNotes).replace('\n', "\n  ")), off(0));
        assert_eq!(back, vec![e]);
    }

    #[test]
    fn durations() {
        for (s, m) in [
            ("1h25m", 85),
            ("1h 25m", 85),
            ("25m", 25),
            ("2h", 120),
            ("1.5h", 90),
            ("90", 90),
            ("1:05", 65),
            ("PT1H25M", 85),
            ("pt45m", 45),
            ("2 hours 5 minutes", 125),
            ("30s", 1),
            ("1d", 1440),
        ] {
            assert_eq!(parse_duration(s), Some(m), "{s}");
        }
        for bad in ["", "soon", "1x", "h", "5h3", "1:75"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(format_duration(85), "1h25m");
        assert_eq!(format_duration(120), "2h");
        assert_eq!(format_duration(7), "7m");
        assert_eq!(format_clock(65), "01:05");
        assert_eq!(format_clock(3725), "1:02:05");
        assert_eq!(add_to_total(Some("1h"), 25), "1h25m");
        assert_eq!(add_to_total(Some("junk"), 5), "5m");
        assert_eq!(add_to_total(None, 0), "0m");
    }

    #[test]
    fn weekly_report_splits_at_midnight() {
        let monday = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let a = [
            session(at("2026-11-02T23:30:00+01:00"), at("2026-11-03T00:45:00+01:00")),
            session(at("2026-11-08T10:00:00+01:00"), at("2026-11-08T10:10:00+01:00")),
            session(at("2026-11-09T10:00:00+01:00"), at("2026-11-09T11:00:00+01:00")),
        ];
        let b = [session(at("2026-11-04T09:00:00+01:00"), at("2026-11-04T11:00:00+01:00"))];
        let none: [TimeEntry; 0] = [];
        let r = week_report([("A.md", "A", &a[..]), ("B.md", "B", &b[..]), ("C.md", "C", &none[..])], monday, at("2026-11-10T00:00:00Z"));
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0].title, "B");
        assert_eq!(r.rows[1].days, [30, 45, 0, 0, 0, 0, 10]);
        assert_eq!(r.rows[1].total, 85);
        assert_eq!(r.days, [30, 45, 120, 0, 0, 0, 10]);
        assert_eq!(r.total, 205);
    }

    /// Seeded property tests: writing and reading entries, totals and the
    /// day split agree for random sessions in random offsets.
    #[test]
    fn time_entry_properties() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..5_000 {
            let offset = FixedOffset::east_opt((rnd(27 * 4) as i32 - 12 * 4) * 900).unwrap();
            let n = 1 + rnd(6) as usize;
            let mut entries = Vec::new();
            let base = at("2026-01-01T00:00:00Z").with_timezone(&offset);
            for _ in 0..n {
                let start = base + Duration::seconds(rnd(400 * 86_400) as i64);
                let end = start + Duration::seconds(rnd(3 * 86_400) as i64);
                entries.push(session(start, end));
            }
            let style = if rnd(2) == 0 { Style::Quire } else { Style::TaskNotes };
            // Written as a block list, read back the same.
            let mut raw = String::new();
            for e in &entries {
                let _ = writeln!(raw, "  - {}", entry_yaml(e, style).replace('\n', "\n    "));
            }
            let back = parse_entries(&raw, FixedOffset::east_opt(0).unwrap());
            assert_eq!(back, entries, "{raw}");
            // A session's days add up to its length, and none is negative.
            let now = base + Duration::days(500);
            for e in &entries {
                let parts = split_by_day(e, now);
                assert_eq!(parts.iter().map(|p| p.1).sum::<i64>(), e.seconds(now));
                assert!(parts.iter().all(|p| p.1 >= 0));
                assert!(parts.windows(2).all(|w| w[0].0 < w[1].0), "days in order");
            }
            // The total is the sum of the parts (within the rounding of each).
            let total = total_minutes(&entries, now);
            let sum: u32 = entries.iter().map(|e| e.minutes(now)).sum();
            assert!(total >= sum && total <= sum + n as u32, "{total} vs {sum}");
            // A week's report never counts more than the sessions hold.
            let monday = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap() + Duration::weeks(rnd(55) as i64);
            let r = week_report([("t.md", "t", &entries[..])], monday, now);
            assert!(r.total <= total + 7, "{} > {total}", r.total);
            // Durations round-trip.
            let m = rnd(100_000) as u32;
            assert_eq!(parse_duration(&format_duration(m)), Some(m));
        }
    }
}
