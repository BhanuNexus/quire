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
    /// An inline task's description (as the index reads it); `None` for a
    /// task note, and for an inline task in a locked folder (its text
    /// mustn't reach the plain timer file; [`Self::at`] finds the line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// What the status bar shows (for a locked note, only its name).
    pub title: String,
    /// Where an inline task's line was when the timer started, so the time
    /// goes to that line and not to another with the same words (0.11
    /// review L1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<LineId>,
}

/// An inline task line's identity: its line number and hashes (no text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineId {
    /// Zero-based line number when the timer started.
    pub line: usize,
    /// Hash of the whole line.
    pub full: String,
    /// Hash of the description and its dates (not the box or done date):
    /// a completed recurring task keeps it, its next occurrence doesn't.
    pub key: String,
    /// Hash of the description.
    pub desc: String,
    /// The line's `^block-id`, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
}

/// FNV-1a, 64 bits, as hex: a stable hash (not a secret).
fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A line's `^block-id` (at its end), if any.
fn block_id(line: &str) -> Option<&str> {
    let last = line.trim_end().rsplit(char::is_whitespace).next()?;
    let id = last.strip_prefix('^')?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(id)
}

impl LineId {
    /// The identity of task line `line` (number `line_no`), or `None` when
    /// it isn't a task.
    pub fn of(line_no: usize, line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\n', '\r']);
        let (_, t) = crate::tasks::parse_line(line)?;
        Some(Self {
            line: line_no,
            full: hash(line),
            key: hash(&task_key(&t)),
            desc: hash(&t.text),
            block: block_id(line).map(str::to_string),
        })
    }
}

fn task_key(t: &quire_markdown::Task) -> String {
    format!("{}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}", t.text, t.due, t.scheduled, t.start)
}

impl TaskRef {
    /// A task note.
    pub fn note(path: &str, title: &str) -> Self {
        Self { path: path.to_string(), line: None, title: title.to_string(), at: None }
    }

    /// The inline task on line `line_no` (its text `line`). In a locked
    /// note (`locked`), neither the task's words nor its title are kept:
    /// only the line's identity, and "a task in `Note`".
    pub fn inline(path: &str, line_no: usize, line: &str, locked: bool) -> Option<Self> {
        let at = LineId::of(line_no, line)?;
        let (_, t) = crate::tasks::parse_line(line.trim_end_matches(['\n', '\r']))?;
        if locked {
            return Some(Self { path: path.to_string(), line: None, title: locked_title(path), at: Some(at) });
        }
        Some(Self { path: path.to_string(), line: Some(t.text.clone()), title: t.text, at: Some(at) })
    }

    /// Whether this inline task's words are `text` (by hash when the
    /// words weren't kept).
    pub fn matches_text(&self, text: &str) -> bool {
        self.line.as_deref() == Some(text) || (self.line.is_none() && self.at.as_ref().is_some_and(|a| a.desc == hash(text)))
    }

    /// An inline task (not a task note).
    pub const fn is_inline(&self) -> bool {
        self.line.is_some() || self.at.is_some()
    }

    /// Whether `other` means the same task.
    pub fn same_task(&self, other: &Self) -> bool {
        if self.path != other.path || self.is_inline() != other.is_inline() {
            return false;
        }
        match (&self.at, &other.at) {
            (Some(a), Some(b)) => a.full == b.full || (a.block.is_some() && a.block == b.block),
            _ => self.line == other.line,
        }
    }
}

/// What a locked note's task is called in the timer file, the status bar
/// and notifications: never its words, only the note's name.
pub fn locked_title(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.strip_suffix(".md").unwrap_or(name);
    format!("a task in {stem}")
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
    /// The timer stopped at this instant but its session couldn't be
    /// written to the task (the note moved, is locked, its line changed):
    /// it is kept here, not counting, until it is logged somewhere or
    /// discarded (0.11 review M1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlogged: Option<String>,
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
            unlogged: None,
            pomodoro: pomodoro.then_some(Pomodoro { phase: Phase::Work, phase_started: now_s, done: 0 }),
        }
    }

    pub fn started_at(&self) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_rfc3339(&self.started).ok()
    }

    /// Whether time is being counted now (not a pomodoro break, nor a
    /// stopped session waiting to be logged).
    pub fn working(&self) -> bool {
        self.unlogged.is_none() && self.pomodoro.as_ref().is_none_or(|p| p.phase == Phase::Work)
    }

    /// When an unlogged session ended.
    pub fn unlogged_at(&self) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_rfc3339(self.unlogged.as_deref()?).ok()
    }

    /// Stop: the session to record (nothing during a pomodoro break). An
    /// unlogged session ends when it stopped, not `now`.
    pub fn stop(&self, now: DateTime<FixedOffset>) -> Option<TimeEntry> {
        let start = self.started_at()?;
        if let Some(end) = self.unlogged_at() {
            return Some(time::session(time::trunc_seconds(start), time::trunc_seconds(end)));
        }
        if !self.working() {
            return None;
        }
        Some(time::session(time::trunc_seconds(start), time::trunc_seconds(now)))
    }

    /// Keep `entry` as an unlogged session: no pomodoro, not counting.
    pub fn keep_unlogged(&mut self, entry: &TimeEntry) {
        self.started = time::format_instant(entry.start);
        self.unlogged = Some(time::format_instant(entry.end.unwrap_or(entry.start)));
        self.pomodoro = None;
    }

    /// Whether this is the same timer as `other` (same task, same start):
    /// a compare-and-swap check before the timer file is changed.
    pub fn same_timer(&self, other: &Self) -> bool {
        self.vault == other.vault && self.started == other.started && self.task.same_task(&other.task) && self.unlogged == other.unlogged
    }

    /// The current phase and when it ends, for a pomodoro.
    pub fn phase_end(&self, prefs: &PomodoroPrefs) -> Option<(Phase, DateTime<FixedOffset>)> {
        let p = self.pomodoro.as_ref()?;
        let start = DateTime::parse_from_rfc3339(&p.phase_started).ok()?;
        Some((p.phase, start + Duration::minutes(i64::from(p.phase.minutes(prefs)))))
    }

    /// Seconds shown on the clock: counting up, or down to the end of a pomodoro phase.
    pub fn clock_seconds(&self, now: DateTime<FixedOffset>, prefs: &PomodoroPrefs) -> i64 {
        if let (Some(s), Some(e)) = (self.started_at(), self.unlogged_at()) {
            return (e - s).num_seconds().max(0);
        }
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
        if self.unlogged.is_some() {
            return None;
        }
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
/// task can't be found (an inline task whose line changed) or its front
/// matter has a shape Quire won't append to.
///
/// * A task note gets the entry appended to its `time_entries` (or
///   `timeEntries`, whichever it has, in any case; a new list in `style`'s
///   name). Only the lines of that list change.
/// * An inline task's `[time:: …]` total grows by the entry's minutes; the
///   rest of its line, and every other line, stays as it was.
pub fn record_entry(src: &str, task: &TaskRef, entry: &TimeEntry, style: time::Style) -> Option<String> {
    if !task.is_inline() {
        let props = quire_markdown::frontmatter::parse(src).map(|f| f.fields).unwrap_or_default();
        // The reader matches names in any case (0.11 review L7): append to
        // the list the note already has, under its own spelling.
        let named = |n: &str| props.keys().find(|k| k.eq_ignore_ascii_case(n)).cloned();
        let existing = named("time_entries").or_else(|| named("timeEntries"));
        let style = match existing.as_deref() {
            Some(k) if k.eq_ignore_ascii_case("timeEntries") => time::Style::TaskNotes,
            Some(_) => time::Style::Quire,
            None => style,
        };
        let key = existing.unwrap_or_else(|| style.list_key().to_string());
        let edit = quire_markdown::frontmatter::append_list_item(src, &key, &time::entry_yaml(entry, style))?;
        return Some(edit.apply(src));
    }
    let (start, line) = find_task_line(src, task)?;
    let marker = quire_markdown::tasks::marker_in_line(line)?;
    let old = quire_markdown::tasks::inline_field(line, marker, "time");
    let now = entry.end.unwrap_or(entry.start);
    let total = time::add_to_total(old.as_deref(), entry.minutes(now));
    let new_line = quire_markdown::tasks::set_inline_field(line, marker, "time", Some(&total));
    Some(format!("{}{new_line}{}", &src[..start], &src[start + line.len()..]))
}

/// A task line: (line number, byte offset, text, the task).
type TaskLine<'a> = (usize, usize, &'a str, quire_markdown::Task);

/// The byte offset and text (no newline) of `task`'s line in `src`.
///
/// By its identity when the timer stored one: its `^block-id`, else the
/// same line (nearest the line number it had), else the same task and
/// dates (a completed recurring task, not its next occurrence), else the
/// same words, open before done, nearest first. Without one (older timer
/// files, `quire-cli`): the first open line with the words.
pub fn find_task_line<'a>(src: &'a str, task: &TaskRef) -> Option<(usize, &'a str)> {
    let mut lines = Vec::new();
    let mut at = 0;
    for (n, raw) in src.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        if let Some((_, t)) = crate::tasks::parse_line(line) {
            lines.push((n, at, line, t));
        }
        at += raw.len();
    }
    let Some(id) = &task.at else {
        let text = task.line.as_deref()?;
        let hit = |done: bool| lines.iter().find(|(_, _, _, t)| t.text == text && t.done == done);
        return hit(false).or_else(|| hit(true)).map(|(_, at, line, _)| (*at, *line));
    };
    let nearest = |pred: &dyn Fn(&TaskLine<'_>) -> bool| {
        lines.iter().filter(|x| pred(x)).min_by_key(|(n, ..)| n.abs_diff(id.line)).map(|(_, at, line, _)| (*at, *line))
    };
    if let Some(b) = &id.block
        && let Some(hit) = nearest(&|(_, _, line, _)| block_id(line) == Some(b.as_str()))
    {
        return Some(hit);
    }
    nearest(&|(_, _, line, _)| hash(line) == id.full)
        .or_else(|| nearest(&|(_, _, _, t)| hash(&task_key(t)) == id.key))
        .or_else(|| nearest(&|(_, _, _, t)| !t.done && hash(&t.text) == id.desc))
        .or_else(|| nearest(&|(_, _, _, t)| hash(&t.text) == id.desc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn task() -> TaskRef {
        TaskRef::note("Tasks/Write.md", "Write")
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
        let inline = TaskRef { path: "Day.md".into(), line: Some("Call Bo".into()), title: "Call Bo".into(), at: None };
        assert_eq!(record_entry(src, &inline, &e, time::Style::Quire).unwrap(), src.replace("[time:: 1h]", "[time:: 1h25m]"));
        let other = TaskRef { line: Some("Other".into()), ..inline.clone() };
        assert_eq!(record_entry(src, &other, &e, time::Style::Quire).unwrap(), src.replace("- [ ] Other", "- [ ] Other [time:: 25m]"));
        let gone = TaskRef { line: Some("Nope".into()), ..inline };
        assert!(record_entry(src, &gone, &e, time::Style::Quire).is_none());
    }

    /// 0.11 review L1: the time goes to the line the timer was started on,
    /// not the first open line with the same words.
    #[test]
    fn inline_time_goes_to_its_own_line() {
        let e = time::session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:00+01:00"));
        // Two identical lines in a daily note: the second one was timed.
        let src = "- [ ] Call Bo\n- [ ] Call Bo\n";
        let t = TaskRef::inline("Day.md", 1, "- [ ] Call Bo", false).unwrap();
        assert_eq!(record_entry(src, &t, &e, time::Style::Quire).unwrap(), "- [ ] Call Bo\n- [ ] Call Bo [time:: 25m]\n");
        // A recurring task completed while timed: its next occurrence is
        // inserted above, and the time goes to the completed one.
        let started = "- [ ] Gym 🔁 every week 📅 2026-11-02\n";
        let t = TaskRef::inline("Day.md", 0, started.trim_end(), false).unwrap();
        let now = "- [ ] Gym 🔁 every week 📅 2026-11-09\n- [x] Gym 🔁 every week 📅 2026-11-02 ✅ 2026-11-02\n";
        let out = record_entry(now, &t, &e, time::Style::Quire).unwrap();
        assert_eq!(out, "- [ ] Gym 🔁 every week 📅 2026-11-09\n- [x] Gym 🔁 every week 📅 2026-11-02 ✅ 2026-11-02 [time:: 25m]\n");
        // A block id wins over everything.
        let t = TaskRef::inline("Day.md", 0, "- [ ] Call ^b1", false).unwrap();
        let src = "- [ ] Call\n- [ ] Call, then email ^b1\n";
        assert_eq!(record_entry(src, &t, &e, time::Style::Quire).unwrap(), "- [ ] Call\n- [ ] Call, then email [time:: 25m] ^b1\n");
        // Lines moved down: the nearest same line.
        let t = TaskRef::inline("Day.md", 3, "- [ ] A", false).unwrap();
        let src = "- [ ] A\nx\ny\nz\n- [ ] A\n";
        assert_eq!(record_entry(src, &t, &e, time::Style::Quire).unwrap(), "- [ ] A\nx\ny\nz\n- [ ] A [time:: 25m]\n");
    }

    /// 0.11 review M7 and S-M5: a locked note's task keeps no words in the
    /// timer file, and is still found by its line.
    #[test]
    fn a_locked_task_keeps_no_words() {
        let t = TaskRef::inline("Journal/Day.md", 2, "- [ ] Tell Sam about the diagnosis", true).unwrap();
        let json = serde_json::to_string(&Running::new("/v".into(), t.clone(), at("2026-11-06T10:00:00+01:00"), false)).unwrap();
        assert!(!json.contains("Sam") && !json.contains("diagnosis"), "{json}");
        assert_eq!(t.title, "a task in Day");
        assert!(t.is_inline() && t.matches_text("Tell Sam about the diagnosis"));
        let e = time::session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:00+01:00"));
        let src = "# Day\n\n- [ ] Tell Sam about the diagnosis\n";
        assert!(record_entry(src, &t, &e, time::Style::Quire).unwrap().contains("diagnosis [time:: 25m]"));
        assert_eq!(locked_title("Journal/2026/Day.md"), "a task in Day");
    }

    /// 0.11 review M1: a stopped session that couldn't be logged keeps its
    /// own end, and doesn't count on.
    #[test]
    fn an_unlogged_session_keeps_its_end() {
        let mut r = Running::new("/v".into(), task(), at("2026-11-06T10:00:00+01:00"), true);
        let e = time::session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:00+01:00"));
        r.keep_unlogged(&e);
        assert!(!r.working() && r.pomodoro.is_none());
        let later = at("2026-11-06T18:00:00+01:00");
        assert_eq!(r.stop(later).unwrap(), e);
        assert_eq!(r.clock_seconds(later, &PomodoroPrefs::default()), 25 * 60);
        assert!(r.advance(later, &PomodoroPrefs::default()).is_none());
        let back: Running = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(back.same_timer(&r));
    }

    /// 0.11 review L7: time entries under another spelling of the key go to
    /// that list, not a second one.
    #[test]
    fn entries_go_to_the_list_the_note_has() {
        let e = time::session(at("2026-11-06T10:00:00+01:00"), at("2026-11-06T10:25:00+01:00"));
        let note = "---\ntype: task\nTime_Entries:\n  - start: 2026-11-05T10:00:00+01:00\n    end: 2026-11-05T10:05:00+01:00\n---\n";
        let out = record_entry(note, &task(), &e, time::Style::TaskNotes).unwrap();
        assert_eq!(out.matches("ntries:").count(), 1, "{out}");
        assert!(out.contains("  - start: 2026-11-06T10:00:00+01:00\n    end: 2026-11-06T10:25:00+01:00\n---"), "{out}");
    }
}
