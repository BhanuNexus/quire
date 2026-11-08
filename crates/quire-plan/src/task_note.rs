// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task notes: a note that is one task (design 0.9 §E.2, 0.11-tasks §1).
//!
//! A note is a task when its front matter says `type: task`, or when its
//! front-matter tags hold the task tag (`task` by default, as TaskNotes
//! does). Its properties are read under Quire's names and TaskNotes'
//! (`time_estimate` or `timeEstimate`, `completion` or `completedDate`, …),
//! so a vault can move between the two. New values are written under the
//! name the note already uses, else in the vault's style
//! (`[tasks] field_names` in vault.toml). See docs/formats/tasks.md.

use std::collections::BTreeMap;

use chrono::{Duration, FixedOffset, NaiveDate};
use quire_markdown::{Task, Value};
use serde::{Deserialize, Serialize};

use crate::recur;
use crate::time::{self, Style, TimeEntry};

/// A task note property, by meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prop {
    Status,
    Priority,
    Due,
    Scheduled,
    Start,
    Contexts,
    Projects,
    Estimate,
    Entries,
    /// Minutes logged without sessions (an inline task's total, carried over).
    Spent,
    Completed,
    Created,
    Recurrence,
    Reminder,
    /// Dates a recurring task was done on.
    CompleteInstances,
}

impl Prop {
    /// The names read, Quire's first, then TaskNotes', then older aliases.
    pub const fn names(self) -> &'static [&'static str] {
        match self {
            Self::Status => &["status"],
            Self::Priority => &["priority"],
            Self::Due => &["due"],
            Self::Scheduled => &["scheduled"],
            Self::Start => &["start"],
            Self::Contexts => &["contexts", "context"],
            Self::Projects => &["projects", "project"],
            Self::Estimate => &["time_estimate", "timeEstimate", "estimate"],
            Self::Entries => &["time_entries", "timeEntries"],
            Self::Spent => &["time_spent", "timeSpent"],
            Self::Completed => &["completion", "completedDate", "done"],
            Self::Created => &["created", "dateCreated"],
            Self::Recurrence => &["recurrence", "repeat"],
            Self::Reminder => &["reminder", "remind"],
            Self::CompleteInstances => &["complete_instances"],
        }
    }

    /// The name a new value is written under, in `style`.
    pub const fn write_name(self, style: Style) -> &'static str {
        match (self, style) {
            (Self::Estimate, Style::TaskNotes) => "timeEstimate",
            (Self::Entries, Style::TaskNotes) => "timeEntries",
            (Self::Spent, Style::TaskNotes) => "timeSpent",
            (Self::Completed, Style::TaskNotes) => "completedDate",
            (Self::Created, Style::TaskNotes) => "dateCreated",
            (p, _) => p.names()[0],
        }
    }
}

/// `[tasks]` in `.quire/shared/vault.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskPrefs {
    /// Where "New task…" and "Convert task to note" put notes.
    pub folder: String,
    /// A note with this tag in its front matter is a task (TaskNotes' default is `task`).
    pub tag: String,
    /// Status values in order: the kanban's columns. The first is a new task's.
    pub statuses: Vec<String>,
    /// Statuses that count as done (and which one ticking a task sets: the first).
    pub done_statuses: Vec<String>,
    /// Tags offered when making a task.
    pub tags: Vec<String>,
    /// `quire` (`time_entries`, `start`/`end`) or `tasknotes` (`timeEntries`,
    /// `startTime`/`endTime`, `timeEstimate`, `completedDate`).
    pub field_names: String,
    pub pomodoro: PomodoroPrefs,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for TaskPrefs {
    fn default() -> Self {
        Self {
            folder: "Tasks".into(),
            tag: "task".into(),
            statuses: ["todo", "doing", "done"].map(String::from).to_vec(),
            done_statuses: ["done", "cancelled"].map(String::from).to_vec(),
            tags: Vec::new(),
            field_names: "quire".into(),
            pomodoro: PomodoroPrefs::default(),
            other: toml::Table::new(),
        }
    }
}

/// `[tasks.pomodoro]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PomodoroPrefs {
    /// Minutes of work.
    pub work: u32,
    pub short_break: u32,
    pub long_break: u32,
    /// A long break after this many work sessions.
    pub long_every: u32,
    /// Start the next work session when a break ends.
    pub auto_continue: bool,
}

impl Default for PomodoroPrefs {
    fn default() -> Self {
        Self { work: 25, short_break: 5, long_break: 15, long_every: 4, auto_continue: true }
    }
}

/// Statuses that mean done whatever the vault says (Quire's, TaskNotes', Obsidian's).
const BUILTIN_DONE: [&str; 7] = ["done", "x", "completed", "complete", "cancelled", "canceled", "closed"];

impl TaskPrefs {
    pub fn style(&self) -> Style {
        if self.field_names.eq_ignore_ascii_case("tasknotes") { Style::TaskNotes } else { Style::Quire }
    }

    pub fn is_done(&self, status: &str) -> bool {
        let s = status.trim();
        BUILTIN_DONE.iter().any(|d| d.eq_ignore_ascii_case(s)) || self.done_statuses.iter().any(|d| d.eq_ignore_ascii_case(s))
    }

    /// The status ticking a task sets.
    pub fn done_status(&self) -> String {
        self.done_statuses.first().cloned().unwrap_or_else(|| "done".into())
    }

    /// A new (or reopened) task's status.
    pub fn open_status(&self) -> String {
        self.statuses.iter().find(|s| !self.is_done(s)).cloned().unwrap_or_else(|| "todo".into())
    }

    /// The kanban's columns: the configured statuses, then any others found.
    pub fn columns<'a>(&self, found: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut out: Vec<String> = self.statuses.clone();
        for f in found {
            if !out.iter().any(|c| c.eq_ignore_ascii_case(f)) {
                out.push(f.to_string());
            }
        }
        out
    }
}

/// A task note, read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskNote {
    /// As the agenda and views use it (the title is its text; offset 0).
    pub task: Task,
    /// As written (`todo`, `in-progress`); empty when missing.
    pub status: String,
    pub contexts: Vec<String>,
    /// Links as written (`[[Website]]`).
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    /// Minutes.
    pub estimate: Option<u32>,
    pub entries: Vec<TimeEntry>,
    /// Minutes logged without sessions (`time_spent: 1h`).
    pub spent_extra: u32,
}

impl TaskNote {
    /// Minutes spent: the sessions (open ones up to `now`) and any extra.
    pub fn spent(&self, now: chrono::DateTime<FixedOffset>) -> u32 {
        time::total_minutes(&self.entries, now).saturating_add(self.spent_extra)
    }
}

fn get(props: &BTreeMap<String, Value>, p: Prop) -> Option<(&str, &Value)> {
    p.names().iter().find_map(|n| props.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(k, v)| (k.as_str(), v)))
}

fn get_str(props: &BTreeMap<String, Value>, p: Prop) -> Option<String> {
    get(props, p).and_then(|(_, v)| v.as_str()).map(str::to_string).filter(|s| !s.trim().is_empty())
}

/// The key a note already uses for `p`, else its name in `style`.
pub fn key_for(props: &BTreeMap<String, Value>, p: Prop, style: Style) -> String {
    get(props, p).map_or_else(|| p.write_name(style).to_string(), |(k, _)| k.to_string())
}

/// Front-matter tags (without `#`).
fn fm_tags(props: &BTreeMap<String, Value>) -> Vec<String> {
    ["tags", "tag"]
        .iter()
        .filter_map(|k| props.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v))
        .flat_map(Value::as_list)
        .map(|t| t.trim_start_matches('#').to_string())
        .collect()
}

/// Whether the front matter makes a note a task.
pub fn is_task_note(props: &BTreeMap<String, Value>, tag: &str) -> bool {
    let typed =
        props.iter().any(|(k, v)| k.eq_ignore_ascii_case("type") && v.as_str().is_some_and(|t| t.trim().eq_ignore_ascii_case("task")));
    typed || (!tag.is_empty() && fm_tags(props).iter().any(|t| t.eq_ignore_ascii_case(tag)))
}

/// A list property: `[a, b]`, a block list, or `a, b`.
fn list(props: &BTreeMap<String, Value>, p: Prop) -> Vec<String> {
    get(props, p).map(|(_, v)| v.as_list()).unwrap_or_default()
}

/// Read a task note. `offset` is the local offset for instants written
/// without one. `None` when the note isn't a task.
pub fn read(props: &BTreeMap<String, Value>, title: &str, prefs: &TaskPrefs, offset: FixedOffset) -> Option<TaskNote> {
    if !is_task_note(props, &prefs.tag) {
        return None;
    }
    let status = get_str(props, Prop::Status).unwrap_or_default();
    let entries = match get(props, Prop::Entries) {
        Some((_, Value::Str(raw))) => time::parse_entries(raw, offset),
        _ => Vec::new(),
    };
    let task = Task {
        done: prefs.is_done(&status),
        text: title.to_string(),
        due: get_str(props, Prop::Due),
        scheduled: get_str(props, Prop::Scheduled),
        start: get_str(props, Prop::Start),
        done_on: get_str(props, Prop::Completed),
        created: get_str(props, Prop::Created),
        recurrence: get_str(props, Prop::Recurrence),
        priority: get_str(props, Prop::Priority).and_then(|p| quire_markdown::tasks::parse_priority(&p)),
        reminder: get_str(props, Prop::Reminder),
        ..Task::default()
    };
    Some(TaskNote {
        task,
        status,
        contexts: list(props, Prop::Contexts),
        projects: list(props, Prop::Projects),
        tags: fm_tags(props).into_iter().filter(|t| !t.eq_ignore_ascii_case(&prefs.tag)).collect(),
        estimate: get_str(props, Prop::Estimate).and_then(|e| time::parse_duration(&e)),
        entries,
        spent_extra: get_str(props, Prop::Spent).and_then(|e| time::parse_duration(&e)).unwrap_or(0),
    })
}

/// A note as a [`Task`] for the agenda and views, with the default prefs.
pub fn note_task(props: &BTreeMap<String, Value>, title: &str) -> Option<Task> {
    read(props, title, &TaskPrefs::default(), FixedOffset::east_opt(0)?).map(|n| n.task)
}

/// Property changes, in order: `(key, Some(value))` sets, `None` removes.
pub type Changes = Vec<(String, Option<Value>)>;

fn date(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?.get(..10)?, "%Y-%m-%d").ok()
}

/// Shift a date (keeping a time after it) by `days`.
fn shift(s: &str, days: i64) -> Option<String> {
    let d = date(Some(s))? + Duration::days(days);
    Some(format!("{}{}", d.format("%Y-%m-%d"), s.get(10..).unwrap_or("")))
}

/// What ticking (or unticking) a task note changes.
///
/// * Open → done: `status` becomes the done status and the completion date
///   is today. A recurring note instead stays open: its dates move to the
///   next occurrence (by the same number of days) and today is added to
///   `complete_instances`, as TaskNotes does.
/// * Done → open: the open status, and the completion date goes.
pub fn toggle_changes(props: &BTreeMap<String, Value>, prefs: &TaskPrefs, today: NaiveDate) -> Changes {
    let style = prefs.style();
    let key = |p: Prop| key_for(props, p, style);
    let status = get_str(props, Prop::Status).unwrap_or_default();
    let today_s = today.format("%Y-%m-%d").to_string();
    if prefs.is_done(&status) {
        let mut c = vec![(key(Prop::Status), Some(Value::Str(prefs.open_status())))];
        if get(props, Prop::Completed).is_some() {
            c.push((key(Prop::Completed), None));
        }
        return c;
    }
    let done = vec![(key(Prop::Status), Some(Value::Str(prefs.done_status()))), (key(Prop::Completed), Some(Value::Str(today_s.clone())))];
    let Some(rule) = get_str(props, Prop::Recurrence).and_then(|r| recur::parse(&r).ok()) else { return done };
    let (due, sched, start) = (get_str(props, Prop::Due), get_str(props, Prop::Scheduled), get_str(props, Prop::Start));
    let anchor = [date(due.as_deref()), date(sched.as_deref()), date(start.as_deref())].into_iter().flatten().next();
    let base = if rule.when_done { today } else { anchor.unwrap_or(today) };
    let Some(next) = recur::next_occurrence(&rule, base) else { return done };
    let delta = (next - anchor.unwrap_or(base)).num_days();
    let mut c = Changes::new();
    for (p, v) in [(Prop::Due, &due), (Prop::Scheduled, &sched), (Prop::Start, &start)] {
        if let Some(s) = v.as_deref().and_then(|s| shift(s, delta)) {
            c.push((key(p), Some(Value::Str(s))));
        }
    }
    if anchor.is_none() {
        // Nothing to move: the next one is due on its date.
        c.push((key(Prop::Due), Some(Value::Str(next.format("%Y-%m-%d").to_string()))));
    }
    let mut done_on = list(props, Prop::CompleteInstances);
    done_on.push(today_s);
    c.push((key(Prop::CompleteInstances), Some(Value::List(done_on))));
    c
}

/// The front matter of a new task note (status first, then what's given).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewTask {
    pub title: String,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub scheduled: Option<String>,
    pub contexts: Vec<String>,
    pub projects: Vec<String>,
    pub tags: Vec<String>,
    pub estimate: Option<u32>,
    pub recurrence: Option<String>,
    pub reminder: Option<String>,
    pub completed: Option<String>,
    pub start: Option<String>,
    /// Minutes already spent (an inline task's `[time:: …]`).
    pub spent: Option<u32>,
    pub created: Option<String>,
}

/// The text of a new task note: front matter, the title as a heading, and `body`.
pub fn new_note(t: &NewTask, prefs: &TaskPrefs, body: &str) -> String {
    use quire_markdown::frontmatter::format_field;
    let style = prefs.style();
    let mut fm = String::from("---\n");
    let tasknotes = style == Style::TaskNotes;
    if !tasknotes {
        fm.push_str("type: task\n");
    }
    let s = |v: &str| Value::Str(v.to_string());
    fm.push_str(&format_field("status", &s(t.status.as_deref().unwrap_or(&prefs.open_status()))));
    let opt = [
        (Prop::Priority, t.priority.as_deref()),
        (Prop::Due, t.due.as_deref()),
        (Prop::Scheduled, t.scheduled.as_deref()),
        (Prop::Start, t.start.as_deref()),
        (Prop::Recurrence, t.recurrence.as_deref()),
        (Prop::Reminder, t.reminder.as_deref()),
        (Prop::Completed, t.completed.as_deref()),
        (Prop::Created, t.created.as_deref()),
    ];
    for (p, v) in opt {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            fm.push_str(&format_field(p.write_name(style), &s(v)));
        }
    }
    for (p, v) in [(Prop::Contexts, &t.contexts), (Prop::Projects, &t.projects)] {
        if !v.is_empty() {
            fm.push_str(&format_field(p.write_name(style), &Value::List(v.clone())));
        }
    }
    let mut tags = t.tags.clone();
    if tasknotes && !prefs.tag.is_empty() && !tags.iter().any(|x| x.eq_ignore_ascii_case(&prefs.tag)) {
        tags.insert(0, prefs.tag.clone());
    }
    if !tags.is_empty() {
        fm.push_str(&format_field("tags", &Value::List(tags)));
    }
    if let Some(m) = t.estimate {
        let v = if tasknotes { m.to_string() } else { time::format_duration(m) };
        fm.push_str(&format_field(Prop::Estimate.write_name(style), &s(&v)));
    }
    if let Some(m) = t.spent.filter(|m| *m > 0) {
        fm.push_str(&format_field(Prop::Spent.write_name(style), &s(&time::format_duration(m))));
    }
    fm.push_str("---\n");
    format!("{fm}# {}\n{}", t.title, if body.is_empty() { String::new() } else { format!("\n{body}") })
}

/// "Convert task to note": what an inline task's note says.
///
/// The title is the text without its tags (tags go to the note's `tags`);
/// dates, priority, recurrence, reminder, `[time:: …]` and `[estimate:: …]`
/// carry over.
pub fn from_inline(t: &Task) -> NewTask {
    let words: Vec<&str> = t.text.split_whitespace().collect();
    let title = words.iter().filter(|w| !is_tag(w)).copied().collect::<Vec<_>>().join(" ");
    let tags = words.iter().filter(|w| is_tag(w)).map(|w| w.trim_start_matches('#').to_string()).collect();
    NewTask {
        title,
        status: t.done.then(|| "done".to_string()),
        priority: t.priority.map(|p| quire_markdown::tasks::priority_word(Some(p)).to_string()),
        due: t.due.clone(),
        scheduled: t.scheduled.clone(),
        recurrence: t.recurrence.clone(),
        reminder: t.reminder.clone(),
        completed: t.done_on.clone(),
        created: t.created.clone(),
        tags,
        estimate: t.estimate.as_deref().and_then(time::parse_duration),
        spent: t.time_spent.as_deref().and_then(time::parse_duration),
        start: t.start.clone(),
        ..NewTask::default()
    }
}

fn is_tag(w: &str) -> bool {
    w.len() > 1 && w.starts_with('#') && !w[1..].bytes().all(|b| b.is_ascii_digit())
}

/// "Convert task note back to a task line": the line for `note`.
///
/// It follows `prefix` (the indentation and bullet, `- `). Fields are
/// written in `format`; time spent and the estimate as `[time:: …]` and
/// `[estimate:: …]`.
pub fn to_inline(prefix: &str, n: &TaskNote, now: chrono::DateTime<FixedOffset>, format: quire_markdown::tasks::Format) -> String {
    use quire_markdown::tasks::{self as t, Field};
    let mut text = n.task.text.clone();
    for tag in &n.tags {
        text.push_str(" #");
        text.push_str(tag);
    }
    let mut line = format!("{prefix}[{}] {text}", if n.task.done { 'x' } else { ' ' });
    let marker = prefix.len();
    let task = &n.task;
    let prio = task.priority.map(|p| t::priority_word(Some(p)).to_string());
    for (f, v) in [
        (Field::Priority, &prio),
        (Field::Recurrence, &task.recurrence),
        (Field::Created, &task.created),
        (Field::Start, &task.start),
        (Field::Scheduled, &task.scheduled),
        (Field::Due, &task.due),
        (Field::Reminder, &task.reminder),
        (Field::Done, &task.done_on.clone().filter(|_| task.done)),
    ] {
        if let Some(v) = v.as_deref().filter(|v| !v.trim().is_empty()) {
            // Dates keep their day (a task line has no times, but a reminder does).
            let v = if f == Field::Reminder {
                v.replacen('T', " ", 1)
            } else {
                v.get(..10).filter(|_| f != Field::Recurrence && f != Field::Priority).unwrap_or(v).to_string()
            };
            line = t::set_field(&line, marker, f, Some(&v), format);
        }
    }
    let spent = n.spent(now);
    if spent > 0 {
        line = t::set_inline_field(&line, marker, "time", Some(&time::format_duration(spent)));
    }
    if let Some(e) = n.estimate {
        line = t::set_inline_field(&line, marker, "estimate", Some(&time::format_duration(e)));
    }
    line
}

/// What "Convert task to note" leaves on the line (0.11 review M4).
///
/// A task note has no place for these: the checkbox character (`[/]`
/// stays `[/]`), a Day Planner time at the start, fields Quire doesn't use
/// (`[client:: Acme]`, `🆔 abc`, `⛔ xyz`, `🏁 …`, `❌ …`, kept verbatim),
/// and the `^block-id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineRest {
    pub checkbox: char,
    pub time_block: Option<String>,
    pub fields: Vec<String>,
    pub block_id: Option<String>,
}

impl LineRest {
    /// What `line` (a task line, its `[` at `marker`) keeps.
    pub fn of(line: &str, marker: usize) -> Self {
        use quire_markdown::tasks::{Format, field_spans};
        let checkbox = line[marker..].chars().nth(1).unwrap_or(' ');
        let after = (marker + 3).min(line.len());
        let body = after + line[after..].len() - line[after..].trim_start().len();
        let mut rest = Self { checkbox, ..Self::default() };
        rest.time_block = quire_markdown::tasks::time_block_text(&line[body..]).map(str::to_string);
        for f in field_spans(line, body) {
            let text = &line[f.range.clone()];
            let used = match f.format {
                Format::Dataview => {
                    let key = text[1..text.len() - 1].split_once("::").map_or("", |(k, _)| k.trim()).to_lowercase();
                    f.field.is_some() || key == "time" || key == "estimate"
                }
                Format::Emoji => f.field.is_some() || text.starts_with('#'),
            };
            if !used {
                rest.fields.push(text.to_string());
            }
        }
        let content = line.trim_end();
        if let Some((_, id)) = content.rsplit_once(" ^")
            && !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            rest.block_id = Some(id.to_string());
        }
        rest
    }

    /// The line linking to the note `stem`, with what stays on it.
    pub fn link_line(&self, prefix: &str, stem: &str) -> String {
        let mut line = format!("{prefix}[{}] ", self.checkbox);
        if let Some(t) = &self.time_block {
            line.push_str(t);
            line.push(' ');
        }
        line.push_str("[[");
        line.push_str(stem);
        line.push_str("]]");
        for f in &self.fields {
            line.push(' ');
            line.push_str(f);
        }
        if let Some(id) = &self.block_id {
            line.push_str(" ^");
            line.push_str(id);
        }
        line
    }

    /// Put what stayed on the line back on a line [`to_inline`] wrote
    /// (before its `^block-id`, which `to_inline` doesn't write). The
    /// checkbox follows the note's `status` (`[/]` for the second status,
    /// `[-]` for cancelled); an open note keeps a checkbox of its own
    /// (`[?]`, `[>]`).
    pub fn restore(&self, new_line: &str, marker: usize, status: &str, prefs: &TaskPrefs) -> String {
        let mut out = new_line.to_string();
        let c = self.checkbox;
        let own = (!matches!(c, ' ' | 'x' | 'X' | '/' | '-') && !prefs.is_done(status)).then_some(c);
        let want = checkbox_for_status(status, prefs).filter(|c| *c != 'x').or(own);
        if let Some(c) = want
            && out.is_char_boundary(marker + 1)
            && out[marker..].starts_with("[ ]")
        {
            out.replace_range(marker + 1..marker + 2, &c.to_string());
        }
        if let Some(t) = &self.time_block {
            let at = marker + 4;
            if out.is_char_boundary(at) {
                out.insert_str(at, &format!("{t} "));
            }
        }
        for f in &self.fields {
            out.push(' ');
            out.push_str(f);
        }
        if let Some(id) = &self.block_id {
            out.push_str(" ^");
            out.push_str(id);
        }
        out
    }
}

/// The note status a checkbox character means: done for `x`, the second
/// status (`doing`) for `/` (in progress), a done status named
/// `cancelled` for `-`, else the open status.
pub fn status_for_checkbox(c: char, prefs: &TaskPrefs) -> Option<String> {
    match c {
        'x' | 'X' => Some(prefs.done_status()),
        '/' => prefs.statuses.get(1).filter(|s| !prefs.is_done(s)).cloned(),
        '-' => prefs.done_statuses.iter().find(|s| s.eq_ignore_ascii_case("cancelled") || s.eq_ignore_ascii_case("canceled")).cloned(),
        _ => None,
    }
}

/// The checkbox character a note status means on a task line (the
/// reverse of [`status_for_checkbox`]); `None` for the open status.
pub fn checkbox_for_status(status: &str, prefs: &TaskPrefs) -> Option<char> {
    if status.eq_ignore_ascii_case("cancelled") || status.eq_ignore_ascii_case("canceled") {
        return Some('-');
    }
    if prefs.is_done(status) {
        return Some('x');
    }
    prefs.statuses.get(1).filter(|s| s.eq_ignore_ascii_case(status)).map(|_| '/')
}

/// The front-matter keys "Convert note to task" can't put on a line.
///
/// The note must stay when there are any (0.11 review M5). Empty when the
/// line carries everything: `type`, a status that is the open one or a done one, the
/// dates, priority, recurrence, reminder, tags, the estimate, time spent,
/// and time entries without descriptions. A key in YAML comments or a
/// shape the reader doesn't take counts as left over too.
pub fn left_on_note(props: &BTreeMap<String, Value>, raw_front_matter: &str, prefs: &TaskPrefs) -> Vec<String> {
    let carried = |k: &str| {
        let low = k.to_ascii_lowercase();
        if matches!(low.as_str(), "type" | "tags" | "tag" | "title") {
            return true;
        }
        [
            Prop::Priority,
            Prop::Due,
            Prop::Scheduled,
            Prop::Start,
            Prop::Estimate,
            Prop::Spent,
            Prop::Completed,
            Prop::Created,
            Prop::Recurrence,
            Prop::Reminder,
            Prop::Entries,
            Prop::Status,
        ]
        .iter()
        .any(|p| p.names().iter().any(|n| n.eq_ignore_ascii_case(k)))
    };
    let mut out: Vec<String> = props.keys().filter(|k| !carried(k)).cloned().collect();
    if let Some(s) = get_str(props, Prop::Status)
        && checkbox_for_status(&s, prefs).is_none()
        && !s.eq_ignore_ascii_case(&prefs.open_status())
    {
        out.push(format!("status ({s})"));
    }
    if let Some((k, Value::Str(raw))) = get(props, Prop::Entries)
        && raw.lines().any(|l| {
            let t = l.trim_start().trim_start_matches("- ").trim_start();
            !(t.is_empty() || ["start", "end", "startTime", "endTime", "duration"].iter().any(|f| t.starts_with(&format!("{f}:"))))
        })
    {
        out.push(format!("{k} (descriptions)"));
    }
    if raw_front_matter.lines().any(|l| l.trim_start().starts_with('#') || l.contains(" #")) {
        out.push("comments".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(src: &str) -> BTreeMap<String, Value> {
        quire_markdown::frontmatter::parse(src).unwrap().fields
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    #[test]
    fn reads_quire_and_tasknotes_names() {
        let quire = props(
            "---\ntype: task\nstatus: doing\npriority: high\ndue: 2026-11-20\ncontexts: [\"@home\"]\nprojects:\n  - \"[[Website]]\"\ntime_estimate: 1h30m\ntime_entries:\n  - start: 2026-11-06T10:00:00+01:00\n    end: 2026-11-06T10:25:00+01:00\ntime_spent: 15m\ntags: [work]\n---\n",
        );
        let n = read(&quire, "Write report", &TaskPrefs::default(), utc()).unwrap();
        assert_eq!(n.status, "doing");
        assert!(!n.task.done);
        assert_eq!(n.task.priority, Some(1));
        assert_eq!(
            (n.contexts.clone(), n.projects.clone(), n.tags.clone()),
            (vec!["@home".into()], vec!["[[Website]]".into()], vec!["work".into()])
        );
        assert_eq!(n.estimate, Some(90));
        assert_eq!(n.spent(chrono::DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z").unwrap()), 40);
        // TaskNotes' defaults: the `task` tag, camelCase names, minutes as numbers.
        let tn = props(
            "---\ntitle: Call Bo\nstatus: in-progress\npriority: normal\ntags:\n  - task\n  - calls\ntimeEstimate: 45\ncompletedDate: 2026-11-01\ndateCreated: 2026-10-30T09:00:00.000+01:00\ntimeEntries:\n  - startTime: \"2026-11-06T09:00:00.000Z\"\n    endTime: \"2026-11-06T09:30:00.000Z\"\n    description: dialled\nrecurrence: FREQ=WEEKLY;BYDAY=MO\n---\n",
        );
        let n = read(&tn, "Call Bo", &TaskPrefs::default(), utc()).unwrap();
        assert_eq!((n.status.as_str(), n.task.priority, n.estimate), ("in-progress", None, Some(45)));
        assert_eq!(n.tags, ["calls"]);
        assert_eq!(n.entries.len(), 1);
        assert_eq!(n.task.done_on.as_deref(), Some("2026-11-01"));
        assert_eq!(n.task.recurrence.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert!(read(&props("---\ntags: [tasks]\n---\n"), "x", &TaskPrefs::default(), utc()).is_none());
        let none = TaskPrefs { tag: String::new(), ..TaskPrefs::default() };
        assert!(read(&tn, "x", &none, utc()).is_none(), "the tag can be turned off");
        // Done-ness: built-in words and the vault's own.
        let p = TaskPrefs { done_statuses: vec!["shipped".into()], ..TaskPrefs::default() };
        assert!(p.is_done("Shipped") && p.is_done("cancelled") && !p.is_done("doing"));
        assert_eq!(p.columns(["doing", "blocked"]), ["todo", "doing", "done", "blocked"]);
    }

    #[test]
    fn ticking_plain_and_recurring_notes() {
        let p = TaskPrefs::default();
        let plain = props("---\ntype: task\nstatus: todo\n---\n");
        assert_eq!(
            toggle_changes(&plain, &p, d("2026-11-06")),
            vec![("status".into(), Some(Value::Str("done".into()))), ("completion".into(), Some(Value::Str("2026-11-06".into())))]
        );
        let done = props("---\ntype: task\nstatus: done\ncompletedDate: 2026-11-06\n---\n");
        assert_eq!(
            toggle_changes(&done, &p, d("2026-11-07")),
            vec![("status".into(), Some(Value::Str("todo".into()))), ("completedDate".into(), None)]
        );
        let rec = props(
            "---\ntype: task\nstatus: todo\nrecurrence: every week\nscheduled: 2026-11-04\ndue: 2026-11-06\ncomplete_instances: [2026-10-30]\n---\n",
        );
        assert_eq!(
            toggle_changes(&rec, &p, d("2026-11-06")),
            vec![
                ("due".into(), Some(Value::Str("2026-11-13".into()))),
                ("scheduled".into(), Some(Value::Str("2026-11-11".into()))),
                ("complete_instances".into(), Some(Value::List(vec!["2026-10-30".into(), "2026-11-06".into()]))),
            ]
        );
        let rrule = props("---\ntags: [task]\nstatus: open\nrecurrence: FREQ=WEEKLY;BYDAY=MO\n---\n");
        let c = toggle_changes(&rrule, &p, d("2026-11-06"));
        assert_eq!(c[0], ("due".into(), Some(Value::Str("2026-11-09".into()))), "undated: due on the next Monday");
    }

    /// Seeded property test: ticking a recurring task note moves every date
    /// by the same number of days to the rule's next occurrence, which is
    /// always after the old date, and never marks it done.
    #[test]
    fn recurring_task_note_properties() {
        let mut seed = 0x5851_f42d_4c95_7f2d_u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let rules = [
            "every day",
            "every 3 days",
            "every week",
            "every 2 weeks on Mon, Thu",
            "every weekday",
            "every month on the 1st",
            "every month on the last Friday",
            "every year in March on the 15th",
            "RRULE:FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=-1",
            "every month",
        ];
        let p = TaskPrefs::default();
        for _ in 0..3_000 {
            let rule = rules[rnd(rules.len() as u64) as usize];
            let due = d("2020-01-01") + Duration::days(rnd(3_000) as i64);
            let lead = rnd(10) as i64;
            let sched = due - Duration::days(lead);
            let src = format!("---\ntype: task\nstatus: todo\nrecurrence: {rule}\ndue: {due}\nscheduled: {sched}\n---\n");
            let c = toggle_changes(&props(&src), &p, due + Duration::days(rnd(5) as i64));
            let get = |k: &str| c.iter().find(|(key, _)| key == k).and_then(|(_, v)| v.as_ref()).and_then(Value::as_str).map(d);
            let (Some(new_due), Some(new_sched)) = (get("due"), get("scheduled")) else { panic!("{rule} from {due}: {c:?}") };
            assert!(new_due > due, "{rule}: {new_due} after {due}");
            assert_eq!(new_due - new_sched, due - sched, "{rule}: dates move together");
            assert_eq!(Some(new_due), recur::next_occurrence(&recur::parse(rule).unwrap(), due), "{rule}");
            assert!(!c.iter().any(|(k, _)| k == "status"), "a recurring note stays open");
        }
    }

    #[test]
    fn new_notes_in_both_styles() {
        let t = NewTask {
            title: "Write report".into(),
            due: Some("2026-11-20".into()),
            contexts: vec!["@desk".into()],
            projects: vec!["[[Website]]".into()],
            estimate: Some(90),
            spent: Some(25),
            ..NewTask::default()
        };
        assert_eq!(
            new_note(&t, &TaskPrefs::default(), "#work"),
            "---\ntype: task\nstatus: todo\ndue: 2026-11-20\ncontexts: [\"@desk\"]\nprojects: [\"[[Website]]\"]\ntime_estimate: 1h30m\ntime_spent: 25m\n---\n# Write report\n\n#work"
        );
        let tn = TaskPrefs {
            field_names: "tasknotes".into(),
            statuses: vec!["open".into(), "in-progress".into(), "done".into()],
            ..TaskPrefs::default()
        };
        let text = new_note(&t, &tn, "");
        assert_eq!(
            text,
            "---\nstatus: open\ndue: 2026-11-20\ncontexts: [\"@desk\"]\nprojects: [\"[[Website]]\"]\ntags: [task]\ntimeEstimate: 90\ntimeSpent: 25m\n---\n# Write report\n"
        );
        let n = read(&quire_markdown::frontmatter::parse(&text).unwrap().fields, "Write report", &tn, utc()).unwrap();
        assert_eq!((n.estimate, n.spent_extra, n.projects.len()), (Some(90), 25, 1));
    }

    /// Inline → note → inline keeps every field (property test over random lines).
    #[test]
    fn converting_there_and_back_keeps_fields() {
        use quire_markdown::tasks::Field as F;
        use quire_markdown::tasks::Format;
        let mut seed = 0x1234_5678_9abc_def1_u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..2_000 {
            let mut line = String::from("- [ ] Write the report");
            if rnd(2) == 0 {
                line.push_str(" #work");
            }
            let format = if rnd(2) == 0 { Format::Emoji } else { Format::Dataview };
            let date = |r: u64| (d("2026-01-01") + Duration::days(r as i64)).format("%Y-%m-%d").to_string();
            let mut fields: Vec<(quire_markdown::tasks::Field, String)> = Vec::new();
            for f in [F::Due, F::Scheduled, F::Start, F::Created] {
                if rnd(2) == 0 {
                    fields.push((f, date(rnd(400))));
                }
            }
            if rnd(3) == 0 {
                fields.push((F::Priority, ["highest", "high", "medium", "low", "lowest"][rnd(5) as usize].into()));
            }
            if rnd(3) == 0 {
                fields.push((F::Recurrence, "every week".into()));
            }
            if rnd(3) == 0 {
                fields.push((F::Reminder, format!("{} 09:30", date(rnd(400)))));
            }
            for (f, v) in &fields {
                line = quire_markdown::tasks::set_field(&line, 2, *f, Some(v), format);
            }
            if rnd(2) == 0 {
                line = quire_markdown::tasks::set_inline_field(&line, 2, "time", Some(&time::format_duration(rnd(500) as u32 + 1)));
            }
            if rnd(2) == 0 {
                line = quire_markdown::tasks::set_inline_field(&line, 2, "estimate", Some(&time::format_duration(rnd(500) as u32 + 1)));
            }
            let (_, task) = crate::tasks::parse_line(&line).unwrap();
            let new = from_inline(&task);
            let text = new_note(&new, &TaskPrefs::default(), "");
            let props = quire_markdown::frontmatter::parse(&text).unwrap().fields;
            let note = read(&props, &new.title, &TaskPrefs::default(), utc()).unwrap();
            let back = to_inline("- ", &note, chrono::DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z").unwrap(), format);
            let (_, again) = crate::tasks::parse_line(&back).unwrap();
            assert_eq!(again, task, "{line}\n{text}\n{back}");
        }
    }
}
