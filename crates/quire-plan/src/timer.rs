// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The running task timer and pomodoro cycles (design 0.11-tasks §1).
//!
//! One timer runs per device. Its state is a small JSON file in the app's
//! config folder (`quire-app::task_timer` reads and writes it), so a timer
//! survives a restart, and the app and `quire-cli tasks start|stop` share
//! it. Nothing reaches the vault until the timer stops (or a pomodoro's work
//! session ends): then one time entry is written to the task.

use chrono::{DateTime, Duration, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::task_note::PomodoroPrefs;
use crate::time::{self, TimeEntry};

/// Which task the timer is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRef {
    /// Vault-relative path of the note.
    pub path: String,
    /// An inline task's description (as the index reads it); `None` for a task note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// What the status bar shows.
    pub title: String,
}

/// A pomodoro phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Work,
    ShortBreak,
    LongBreak,
}

impl Phase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::ShortBreak => "Short break",
            Self::LongBreak => "Long break",
        }
    }

    fn minutes(self, p: &PomodoroPrefs) -> u32 {
        match self {
            Self::Work => p.work,
            Self::ShortBreak => p.short_break,
            Self::LongBreak => p.long_break,
        }
        .max(1)
    }
}

/// Where a pomodoro cycle is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pomodoro {
    pub phase: Phase,
    /// RFC 3339 with offset.
    pub phase_started: String,
    /// Work sessions finished so far.
    pub done: u32,
}

/// The timer file: `<config>/task-timer.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    /// The file's format (1).
    #[serde(default = "one")]
    pub format: u32,
    /// The vault's folder (absolute).
    pub vault: String,
    pub task: TaskRef,
    /// When the current work session started (RFC 3339 with offset).
    pub started: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pomodoro: Option<Pomodoro>,
    /// Fields a newer Quire wrote, kept.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

const fn one() -> u32 {
    1
}

/// What happened when a pomodoro phase ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub finished: Phase,
    pub next: Phase,
    /// The work session to record, when work finished.
    pub record: Option<TimeEntry>,
    /// `next` started, or (work after a break, with `auto_continue` off) the timer stops.
    pub stopped: bool,
}

impl Running {
    pub fn new(vault: String, task: TaskRef, now: DateTime<FixedOffset>, pomodoro: bool) -> Self {
        let now_s = time::format_instant(now);
        Self {
            format: 1,
            other: serde_json::Map::new(),
            vault,
            task,
            started: now_s.clone(),
            pomodoro: pomodoro.then_some(Pomodoro { phase: Phase::Work, phase_started: now_s, done: 0 }),
        }
    }

    pub fn started_at(&self) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_rfc3339(&self.started).ok()
    }

    /// Whether time is being counted now (not a pomodoro break).
    pub fn working(&self) -> bool {
        self.pomodoro.as_ref().is_none_or(|p| p.phase == Phase::Work)
    }

    /// Stop: the session to record (nothing during a pomodoro break).
    pub fn stop(&self, now: DateTime<FixedOffset>) -> Option<TimeEntry> {
        if !self.working() {
            return None;
        }
        let start = self.started_at()?;
        Some(time::session(time::trunc_seconds(start), time::trunc_seconds(now)))
    }

    /// The current phase and when it ends, for a pomodoro.
    pub fn phase_end(&self, prefs: &PomodoroPrefs) -> Option<(Phase, DateTime<FixedOffset>)> {
        let p = self.pomodoro.as_ref()?;
        let start = DateTime::parse_from_rfc3339(&p.phase_started).ok()?;
        Some((p.phase, start + Duration::minutes(i64::from(p.phase.minutes(prefs)))))
    }

    /// Seconds shown on the clock: counting up, or down to the end of a pomodoro phase.
    pub fn clock_seconds(&self, now: DateTime<FixedOffset>, prefs: &PomodoroPrefs) -> i64 {
        match self.phase_end(prefs) {
            Some((_, end)) => (end - now).num_seconds().max(0),
            None => self.started_at().map_or(0, |s| (now - s).num_seconds().max(0)),
        }
    }

    /// Move a pomodoro on if its phase has ended by `now`. One phase per
    /// call: a timer left overnight records only the work session that
    /// ended, and the break after it, already over, starts work again from
    /// `now` (the app calls this every second).
    pub fn advance(&mut self, now: DateTime<FixedOffset>, prefs: &PomodoroPrefs) -> Option<Transition> {
        let (phase, end) = self.phase_end(prefs)?;
        if now < end {
            return None;
        }
        let p = self.pomodoro.as_mut()?;
        let end_s = time::format_instant(end);
        match phase {
            Phase::Work => {
                let start = DateTime::parse_from_rfc3339(&self.started).ok().unwrap_or(end);
                p.done += 1;
                let next =
                    if prefs.long_every > 0 && p.done.is_multiple_of(prefs.long_every) { Phase::LongBreak } else { Phase::ShortBreak };
                p.phase = next;
                p.phase_started = end_s;
                Some(Transition { finished: Phase::Work, next, record: Some(time::session(start, end)), stopped: false })
            }
            Phase::ShortBreak | Phase::LongBreak => {
                // A break that ended long ago (Quire was closed): work starts
                // now, not when nobody was there to do it.
                let start = if now - end > Duration::minutes(1) { time::format_instant(now) } else { end_s };
                p.phase = Phase::Work;
                p.phase_started.clone_from(&start);
                self.started = start;
                Some(Transition { finished: phase, next: Phase::Work, record: None, stopped: !prefs.auto_continue })
            }
        }
    }
}

/// The note's text with `entry` recorded for `task`, or `None` when the
/// task can't be found (an inline task whose line changed).
///
/// * A task note gets the entry appended to its `time_entries` (or
///   `timeEntries`, whichever it has; a new list in `style`'s name). Only
///   the lines of that list change.
/// * An inline task's `[time:: …]` total grows by the entry's minutes; the
///   rest of its line, and every other line, stays as it was.
pub fn record_entry(src: &str, task: &TaskRef, entry: &TimeEntry, style: time::Style) -> Option<String> {
    match task.line.as_deref() {
        None => {
            let props = quire_markdown::frontmatter::parse(src).map(|f| f.fields).unwrap_or_default();
            let has = |k: &str| props.keys().any(|x| x == k);
            let style = if has("timeEntries") && !has("time_entries") {
                time::Style::TaskNotes
            } else if has("time_entries") {
                time::Style::Quire
            } else {
                style
            };
            let edit = quire_markdown::frontmatter::append_list_item(src, style.list_key(), &time::entry_yaml(entry, style))?;
            Some(edit.apply(src))
        }
        Some(text) => {
            let (start, line) = find_inline(src, text)?;
            let marker = quire_markdown::tasks::marker_in_line(line)?;
            let old = quire_markdown::tasks::inline_field(line, marker, "time");
            let now = entry.end.unwrap_or(entry.start);
            let total = time::add_to_total(old.as_deref(), entry.minutes(now));
            let new_line = quire_markdown::tasks::set_inline_field(line, marker, "time", Some(&total));
            Some(format!("{}{new_line}{}", &src[..start], &src[start + line.len()..]))
        }
    }
}

/// The byte offset and text (no newline) of the open inline task whose
/// description is `text` (a done one if no open one matches).
pub fn find_inline<'a>(src: &'a str, text: &str) -> Option<(usize, &'a str)> {
    let mut done = None;
    let mut at = 0;
    for raw in src.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        if let Some((_, t)) = crate::tasks::parse_line(line)
            && t.text == text
        {
            if !t.done {
                return Some((at, line));
            }
            done.get_or_insert((at, line));
        }
        at += raw.len();
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn task() -> TaskRef {
        TaskRef { path: "Tasks/Write.md".into(), line: None, title: "Write".into() }
    }

    #[test]
    fn plain_timer_records_one_session() {
        let r = Running::new("/v".into(), task(), at("2026-11-06T10:00:00.250+01:00"), false);
        let json = serde_json::to_string(&r).unwrap();
        let back: Running = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r, "survives a restart");
        let e = back.stop(at("2026-11-06T10:25:09.900+01:00")).unwrap();
        assert_eq!(time::format_instant(e.start), "2026-11-06T10:00:00+01:00");
        assert_eq!(e.minutes(at("2027-01-01T00:00:00Z")), 25);
        assert_eq!(back.clock_seconds(at("2026-11-06T10:01:05+01:00"), &PomodoroPrefs::default()), 65, "whole seconds are kept");
    }

    #[test]
    fn pomodoro_cycles() {
        let prefs = PomodoroPrefs { long_every: 2, ..PomodoroPrefs::default() };
        let mut r = Running::new("/v".into(), task(), at("2026-11-06T10:00:00+01:00"), true);
        assert!(r.advance(at("2026-11-06T10:24:59+01:00"), &prefs).is_none());
        assert_eq!(r.clock_seconds(at("2026-11-06T10:24:00+01:00"), &prefs), 60);
        let t = r.advance(at("2026-11-06T10:25:01+01:00"), &prefs).unwrap();
        assert_eq!((t.finished, t.next, t.stopped), (Phase::Work, Phase::ShortBreak, false));
        assert_eq!(t.record.unwrap().minutes(at("2027-01-01T00:00:00Z")), 25);
        assert!(!r.working() && r.stop(at("2026-11-06T10:26:00+01:00")).is_none(), "breaks aren't recorded");
        let t = r.advance(at("2026-11-06T10:30:00+01:00"), &prefs).unwrap();
        assert_eq!((t.finished, t.next, t.record), (Phase::ShortBreak, Phase::Work, None));
        assert_eq!(r.started, "2026-11-06T10:30:00+01:00");
        let t = r.advance(at("2026-11-06T10:55:00+01:00"), &prefs).unwrap();
        assert_eq!(t.next, Phase::LongBreak, "every second session with long_every = 2");
        // After a restart long past the break: one phase per call.
        let t = r.advance(at("2026-11-06T18:00:00+01:00"), &prefs).unwrap();
        assert_eq!(t.next, Phase::Work);
        assert_eq!(r.started, "2026-11-06T18:00:00+01:00", "work resumes now, not at 11:10");
        assert!(r.advance(at("2026-11-06T18:00:01+01:00"), &prefs).is_none());
        let off = PomodoroPrefs { auto_continue: false, ..prefs };
        let mut r = Running::new("/v".into(), task(), at("2026-11-06T10:00:00+01:00"), true);
        r.advance(at("2026-11-06T10:25:00+01:00"), &off);
        assert!(r.advance(at("2026-11-06T10:30:00+01:00"), &off).unwrap().stopped);
    }

    #[test]
    fn records_to_notes_and_lines() {
        let e = time::session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:00+01:00"));
        let note = "---\ntype: task\nstatus: doing   # kept\n---\n# Write\n";
        let out = record_entry(note, &task(), &e, time::Style::Quire).unwrap();
        assert_eq!(
            out,
            "---\ntype: task\nstatus: doing   # kept\ntime_entries:\n  - start: 2026-11-06T10:00:00+01:00\n    end: 2026-11-06T10:25:00+01:00\n---\n# Write\n"
        );
        // A note in TaskNotes' names keeps them, whatever the vault's style.
        let tn = "---\ntags: [task]\ntimeEntries:\n  - startTime: \"2026-11-05T09:00:00.000Z\"\n    endTime: \"2026-11-05T09:30:00.000Z\"\n---\n";
        let out = record_entry(tn, &task(), &e, time::Style::Quire).unwrap();
        assert!(out.ends_with("    endTime: \"2026-11-05T09:30:00.000Z\"\n  - startTime: 2026-11-06T10:00:00+01:00\n    endTime: 2026-11-06T10:25:00+01:00\n---\n"), "{out}");
        let props = quire_markdown::frontmatter::parse(&out).unwrap().fields;
        let n =
            crate::task_note::read(&props, "x", &crate::task_note::TaskPrefs::default(), at("2026-01-01T00:00:00Z").timezone()).unwrap();
        assert_eq!(n.spent(at("2027-01-01T00:00:00Z")), 55);
        // Inline: the total grows; other lines and fields stay.
        let src = "# Day\n- [x] Call Bo [time:: 5m]\n- [ ] Call Bo 📅 2026-11-07 [time:: 1h]\n- [ ] Other\n";
        let inline = TaskRef { path: "Day.md".into(), line: Some("Call Bo".into()), title: "Call Bo".into() };
        assert_eq!(record_entry(src, &inline, &e, time::Style::Quire).unwrap(), src.replace("[time:: 1h]", "[time:: 1h25m]"));
        let other = TaskRef { line: Some("Other".into()), ..inline.clone() };
        assert_eq!(record_entry(src, &other, &e, time::Style::Quire).unwrap(), src.replace("- [ ] Other", "- [ ] Other [time:: 25m]"));
        let gone = TaskRef { line: Some("Nope".into()), ..inline };
        assert!(record_entry(src, &gone, &e, time::Style::Quire).is_none());
    }
}
