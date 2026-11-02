// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! When task reminders (`⏰ 2026-11-30 09:00`) fire (design §E.2).
//!
//! The schedule is rebuilt from the index; firing and snoozing live in the
//! app. Reminders fire only while Quire runs; ones missed in the last week
//! are listed once at start.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};

/// A reminder with only a date fires at this time.
pub const DEFAULT_TIME: (u32, u32) = (9, 0);
/// Missed reminders older than this aren't shown.
pub const MISSED_DAYS: i64 = 7;

/// One reminder: when, and which task line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reminder {
    pub at: NaiveDateTime,
    pub path: String,
    /// Byte offset of the task's checkbox.
    pub offset: usize,
    pub text: String,
}

impl Reminder {
    /// Stays the same while the task's text, note and time do (offsets move with edits).
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}\u{1f}{}", self.path, self.text, self.at.format("%Y-%m-%d %H:%M"))
    }
}

/// `2026-11-30 09:00`, `2026-11-30T09:00` or `2026-11-30` (at 09:00).
pub fn parse_at(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if let Ok(t) = NaiveDateTime::parse_from_str(&s.replacen('T', " ", 1), "%Y-%m-%d %H:%M") {
        return Some(t);
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(d.and_time(NaiveTime::from_hms_opt(DEFAULT_TIME.0, DEFAULT_TIME.1, 0)?))
}

/// Reminders of open tasks, soonest first.
pub fn schedule<'a>(tasks: impl IntoIterator<Item = (&'a str, &'a quire_markdown::Task)>) -> Vec<Reminder> {
    let mut v: Vec<Reminder> = tasks
        .into_iter()
        .filter(|(_, t)| !t.done)
        .filter_map(|(path, t)| {
            let at = parse_at(t.reminder.as_deref()?)?;
            Some(Reminder { at, path: path.to_string(), offset: t.offset, text: t.text.clone() })
        })
        .collect();
    v.sort();
    v
}

/// Reminders that came due in `(after, until]`.
pub fn due_in(list: &[Reminder], after: NaiveDateTime, until: NaiveDateTime) -> Vec<&Reminder> {
    list.iter().filter(|r| r.at > after && r.at <= until).collect()
}

/// Reminders missed while Quire wasn't running: due before `now`, after
/// `last_run` (if known) and within the last week.
pub fn missed(list: &[Reminder], now: NaiveDateTime, last_run: Option<NaiveDateTime>) -> Vec<&Reminder> {
    let floor = now - Duration::days(MISSED_DAYS);
    let after = last_run.map_or(floor, |l| l.max(floor));
    list.iter().filter(|r| r.at > after && r.at <= now).collect()
}

/// The next reminder strictly after `now`.
pub fn next_after(list: &[Reminder], now: NaiveDateTime) -> Option<&Reminder> {
    list.iter().find(|r| r.at > now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soonest_first() {
        let t = |r: &str| quire_markdown::Task { reminder: Some(r.into()), ..Default::default() };
        let (a, b, c) = (t("2026-11-30 09:00"), t("2026-11-02 18:30"), t("2026-11-02"));
        let done = quire_markdown::Task { done: true, ..t("2026-11-01 08:00") };
        let s = schedule([("A.md", &a), ("B.md", &b), ("C.md", &quire_markdown::Task::default()), ("D.md", &c), ("E.md", &done)]);
        assert_eq!(s.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["D.md", "B.md", "A.md"]);
        assert_eq!(s[0].at, parse_at("2026-11-02 09:00").unwrap());
        let now = parse_at("2026-11-02 12:00").unwrap();
        assert_eq!(next_after(&s, now).map(|r| r.path.as_str()), Some("B.md"));
        assert_eq!(due_in(&s, parse_at("2026-11-02 08:00").unwrap(), now).len(), 1);
        assert_eq!(missed(&s, now, None).len(), 1);
        assert_eq!(missed(&s, now, Some(parse_at("2026-11-02 10:00").unwrap())).len(), 0);
        assert_eq!(missed(&s, parse_at("2026-11-20 12:00").unwrap(), None).len(), 0, "older than a week");
        assert_ne!(s[0].key(), s[1].key());
    }
}
