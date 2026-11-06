// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Completing a task (design §E.2): the line gets `[x]` and `✅ today`; a
//! recurring task also gets its next occurrence, a copy with every date
//! moved by the same number of days.

use chrono::{Duration, NaiveDate, NaiveDateTime};
use quire_markdown::Task;
use quire_markdown::tasks::{self, Field, Format};

use crate::recur;

/// What completing a task line gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The completed line.
    pub done: String,
    /// The next occurrence (an open copy), for a recurring task that hasn't ended.
    pub next: Option<String>,
    /// The date the next occurrence is for.
    pub next_date: Option<NaiveDate>,
    /// Why there's no next occurrence when the task has a rule.
    pub note: Option<String>,
}

/// The task on `line`, parsed as the index parses it.
pub fn parse_line(line: &str) -> Option<(usize, Task)> {
    let marker = tasks::marker_in_line(line)?;
    let text = line[marker + 3..].trim().to_string();
    let mut t = Task { done: matches!(line.as_bytes().get(marker + 1), Some(b'x' | b'X')), text, offset: marker, ..Task::default() };
    tasks::parse_meta(line, marker, &mut t);
    Some((marker, t))
}

fn date(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?.get(..10)?, "%Y-%m-%d").ok()
}

/// Complete the task on `line` (no newline) on `today`. `None` when the line
/// isn't an open task.
pub fn complete(line: &str, today: NaiveDate, format: Format) -> Option<Completion> {
    let (marker, task) = parse_line(line)?;
    if task.done {
        return None;
    }
    let today_s = today.format("%Y-%m-%d").to_string();
    let done = tasks::set_field(&tasks::set_done(line, marker, true), marker, Field::Done, Some(&today_s), format);
    let mut out = Completion { done, next: None, next_date: None, note: None };
    let Some(rule_text) = task.recurrence.as_deref() else { return Some(out) };
    let rule = match recur::parse(rule_text) {
        Ok(r) => r,
        Err(e) => {
            out.note = Some(e.to_string());
            return Some(out);
        }
    };
    // The task's own date: due, else scheduled, else start. The rule counts
    // from it, or from today for "when done"; every date moves as it does.
    let anchor = [date(task.due.as_deref()), date(task.scheduled.as_deref()), date(task.start.as_deref())].into_iter().flatten().next();
    let base = if rule.when_done { Some(today) } else { anchor };
    let Some(base) = base else {
        // No dates to move: the next one is a fresh copy, as Obsidian Tasks does.
        out.next = Some(next_line(line, marker, &task, 0, today, format));
        return Some(out);
    };
    let Some(next) = recur::next_occurrence(&rule, base) else {
        out.note = Some("The repeat rule has ended.".into());
        return Some(out);
    };
    let delta = (next - anchor.unwrap_or(base)).num_days();
    out.next = Some(next_line(line, marker, &task, delta, today, format));
    out.next_date = Some(next);
    Some(out)
}

/// The open copy with dates moved by `delta` days.
fn next_line(line: &str, marker: usize, task: &Task, delta: i64, today: NaiveDate, format: Format) -> String {
    let shift = |s: &str| date(Some(s)).map(|d| (d + Duration::days(delta)).format("%Y-%m-%d").to_string());
    let mut l = tasks::set_field(line, marker, Field::Done, None, format);
    for (field, value) in [(Field::Due, &task.due), (Field::Scheduled, &task.scheduled), (Field::Start, &task.start)] {
        if let Some(v) = value.as_deref().and_then(shift) {
            l = tasks::set_field(&l, marker, field, Some(&v), format);
        }
    }
    if let Some(r) = task.reminder.as_deref() {
        let moved = NaiveDateTime::parse_from_str(r, "%Y-%m-%d %H:%M")
            .map(|t| (t + Duration::days(delta)).format("%Y-%m-%d %H:%M").to_string())
            .ok()
            .or_else(|| shift(r));
        if let Some(m) = moved {
            l = tasks::set_field(&l, marker, Field::Reminder, Some(&m), format);
        }
    }
    if task.created.is_some() {
        l = tasks::set_field(&l, marker, Field::Created, Some(&today.format("%Y-%m-%d").to_string()), format);
    }
    // A block id must stay unique: the copy doesn't get one.
    let trimmed = l.trim_end();
    if let Some(caret) = trimmed.rfind(" ^")
        && trimmed[caret + 2..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && caret + 2 < trimmed.len()
    {
        l = format!("{}{}", &trimmed[..caret], &l[trimmed.len()..]);
    }
    tasks::set_done(&l, marker, false)
}

/// A note that is a task (`type: task`, or the `task` tag), as a [`Task`]
/// for the agenda and views. See [`crate::task_note`].
pub fn note_task(props: &std::collections::BTreeMap<String, quire_markdown::Value>, title: &str) -> Option<Task> {
    crate::task_note::note_task(props, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn completing_a_recurring_task_makes_the_next_one() {
        let line = "- [ ] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-11-28 📅 2026-12-01 ⏰ 2026-11-30 09:00 ^rent";
        let c = complete(line, d("2026-12-02"), Format::Emoji).unwrap();
        assert_eq!(
            c.done,
            "- [x] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-11-28 📅 2026-12-01 ⏰ 2026-11-30 09:00 ✅ 2026-12-02 ^rent"
        );
        assert_eq!(c.next.as_deref(), Some("- [ ] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-12-29 📅 2027-01-01 ⏰ 2026-12-31 09:00"));
        assert_eq!(c.next_date, Some(d("2027-01-01")));
    }

    #[test]
    fn when_done_counts_from_today() {
        let c = complete("- [ ] Water plants 🔁 every 3 days when done 📅 2026-11-01", d("2026-11-05"), Format::Emoji).unwrap();
        assert_eq!(c.next.as_deref(), Some("- [ ] Water plants 🔁 every 3 days when done 📅 2026-11-08"));
        assert_eq!(c.next_date, Some(d("2026-11-08")));
    }

    #[test]
    fn plain_ended_and_unreadable() {
        let c = complete("- [ ] Call Bo", d("2026-11-02"), Format::Dataview).unwrap();
        assert_eq!((c.done.as_str(), c.next), ("- [x] Call Bo [completion:: 2026-11-02]", None));
        assert!(complete("- [x] Done already", d("2026-11-02"), Format::Emoji).is_none());
        assert!(complete("not a task", d("2026-11-02"), Format::Emoji).is_none());
        let c = complete("- [ ] Last 🔁 every day until 2026-11-01 📅 2026-11-01", d("2026-11-02"), Format::Emoji).unwrap();
        assert!(c.next.is_none() && c.note.is_some());
        let c = complete("- [ ] Odd 🔁 every blue moon 📅 2026-11-01", d("2026-11-02"), Format::Emoji).unwrap();
        assert!(c.next.is_none() && c.note.unwrap().contains("blue moon"));
        let c = complete("- [ ] Undated 🔁 every week", d("2026-11-02"), Format::Emoji).unwrap();
        assert_eq!(c.next.as_deref(), Some("- [ ] Undated 🔁 every week"));
    }

    #[test]
    fn task_notes() {
        let props = std::collections::BTreeMap::from([
            ("type".to_string(), quire_markdown::Value::Str("task".into())),
            ("status".to_string(), quire_markdown::Value::Str("todo".into())),
            ("due".to_string(), quire_markdown::Value::Str("2026-11-20".into())),
            ("priority".to_string(), quire_markdown::Value::Str("high".into())),
        ]);
        let t = note_task(&props, "Write report").unwrap();
        assert_eq!((t.done, t.due.as_deref(), t.priority, t.text.as_str()), (false, Some("2026-11-20"), Some(1), "Write report"));
        assert!(note_task(&std::collections::BTreeMap::new(), "x").is_none());
    }
}
