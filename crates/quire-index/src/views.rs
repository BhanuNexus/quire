// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-view` blocks: saved queries over note properties.
//!
//! ````text
//! ```quire-view
//! from: "#project"          # tag, folder ("Projects/"), "all" or "tasks"
//! where: status != done and due < 2026-12-01 and author.name = Ada
//! sort: due asc
//! group: status
//! layout: kanban            # table | list | kanban | calendar | agenda | timeline
//! fields: [title, status, due, tags]
//! date: due
//! new: Templates/Project.md # a "+ New" button: a note from this template
//! ```
//! ````
//!
//! A spec is plain text inside a fenced block, so the note stays portable
//! Markdown. [`run`] evaluates it against the read model. Fields may be
//! dotted paths into nested front matter (`author.name`). `from: tasks`
//! lists tasks instead of notes (fields `text, status, due, scheduled,
//! start, done, priority, recurrence, path, note, tags`). Dates compare
//! against `today`, `tomorrow`, `yesterday` and `today+7d`.
//!
//! [`dataview`] and [`tasks_query`] translate Obsidian's Dataview and Tasks
//! query blocks into view specs, saying what they can't translate.

use std::cmp::Ordering;

use quire_markdown::Task;
use quire_markdown::frontmatter::{self, Value};
use quire_vault::NotePath;

use crate::{Index, NoteRecord};

/// The fence info string that marks a view block.
pub const FENCE: &str = "quire-view";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Table,
    List,
    Kanban,
    Calendar,
    /// Overdue, today, the next seven days, later, no date.
    Agenda,
    /// By date, oldest first.
    Timeline,
}

impl Layout {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::List => "list",
            Self::Kanban => "kanban",
            Self::Calendar => "calendar",
            Self::Agenda => "agenda",
            Self::Timeline => "timeline",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    All,
    Tag(String),
    Folder(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Contains,
    Exists,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSpec {
    /// Any of these (an empty list means all notes).
    pub from: Vec<Source>,
    /// All of these.
    pub filters: Vec<Filter>,
    pub sort: Option<(String, bool)>,
    pub group: Option<String>,
    pub layout: Layout,
    pub fields: Vec<String>,
    pub date: Option<String>,
    /// Kanban column order; other values are appended.
    pub columns: Vec<String>,
    pub limit: Option<usize>,
    /// Rows are tasks (`from: tasks`) rather than notes.
    pub tasks: bool,
    /// Template for the "+ New" button (`new: Templates/Project.md`).
    pub new: Option<String>,
}

impl Default for ViewSpec {
    fn default() -> Self {
        Self {
            from: Vec::new(),
            filters: Vec::new(),
            sort: None,
            group: None,
            layout: Layout::Table,
            fields: vec!["title".into()],
            date: None,
            columns: Vec::new(),
            limit: None,
            tasks: false,
            new: None,
        }
    }
}

fn strip_comment(v: &str) -> &str {
    // `# comment` after whitespace, outside quotes.
    let mut in_quote = None;
    for (i, c) in v.char_indices() {
        match c {
            '"' | '\'' if in_quote == Some(c) => in_quote = None,
            '"' | '\'' if in_quote.is_none() => in_quote = Some(c),
            '#' if in_quote.is_none() && i > 0 && v[..i].ends_with([' ', '\t']) => return v[..i].trim_end(),
            _ => {}
        }
    }
    v.trim_end()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn list(v: &str) -> Vec<String> {
    let v = v.trim();
    let inner = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(v);
    inner.split(',').map(unquote).filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect()
}

fn parse_filter(clause: &str) -> Result<Filter, String> {
    let c = clause.trim();
    for (word, op) in [(" exists", Op::Exists), (" missing", Op::Missing)] {
        if let Some(f) = c.strip_suffix(word) {
            return Ok(Filter { field: f.trim().to_lowercase(), op, value: String::new() });
        }
    }
    if let Some((f, v)) = c.split_once(" contains ") {
        return Ok(Filter { field: f.trim().to_lowercase(), op: Op::Contains, value: unquote(v) });
    }
    for (sym, op) in [("!=", Op::Ne), (">=", Op::Ge), ("<=", Op::Le), ("=", Op::Eq), (">", Op::Gt), ("<", Op::Lt)] {
        if let Some((f, v)) = c.split_once(sym) {
            let field = f.trim().to_lowercase();
            if field.is_empty() {
                break;
            }
            return Ok(Filter { field, op, value: unquote(v) });
        }
    }
    Err(format!("can't read the condition “{c}” (try: status != done)"))
}

/// Parse a view block's body.
pub fn parse(src: &str) -> Result<ViewSpec, String> {
    let mut spec = ViewSpec::default();
    for line in src.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            return Err(format!("expected “key: value”, found “{t}”"));
        };
        let v = strip_comment(v.trim());
        match k.trim().to_lowercase().as_str() {
            "from" => {
                spec.from = v
                    .split(" or ")
                    .flat_map(|part| part.split(','))
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| match s.as_str() {
                        "all" | "*" => None,
                        t if t.eq_ignore_ascii_case("tasks") => {
                            spec.tasks = true;
                            None
                        }
                        t if t.starts_with('#') => Some(Source::Tag(t[1..].to_string())),
                        "./" => Some(Source::Folder(".".to_string())),
                        f => Some(Source::Folder(f.trim_end_matches('/').to_string())),
                    })
                    .collect();
            }
            "where" | "filter" => {
                for clause in v.split(" and ") {
                    spec.filters.push(parse_filter(clause)?);
                }
            }
            "sort" => {
                let mut parts = v.split_whitespace();
                let field = parts.next().ok_or("sort needs a field")?.to_lowercase();
                let desc = matches!(parts.next(), Some(d) if d.eq_ignore_ascii_case("desc"));
                spec.sort = Some((field, desc));
            }
            "group" | "group_by" => spec.group = Some(unquote(v).to_lowercase()),
            "layout" | "view" => {
                spec.layout = match unquote(v).to_lowercase().as_str() {
                    "table" => Layout::Table,
                    "list" => Layout::List,
                    "kanban" | "board" => Layout::Kanban,
                    "calendar" => Layout::Calendar,
                    "agenda" => Layout::Agenda,
                    "timeline" => Layout::Timeline,
                    other => return Err(format!("unknown layout “{other}” (table, list, kanban, calendar, agenda, timeline)")),
                }
            }
            "fields" | "columns_shown" => {
                spec.fields = list(v);
                if spec.fields.is_empty() {
                    spec.fields.push("title".into());
                }
            }
            "date" => spec.date = Some(unquote(v).to_lowercase()),
            "columns" => spec.columns = list(v).into_iter().map(|c| if c == "(none)" { String::new() } else { c }).collect(),
            "limit" => spec.limit = Some(v.trim().parse().map_err(|_| format!("limit must be a number, found “{v}”"))?),
            "new" => spec.new = Some(unquote(v)).filter(|t| !t.is_empty()),
            other => return Err(format!("unknown key “{other}”")),
        }
    }
    if spec.layout == Layout::Kanban && spec.group.is_none() {
        spec.group = Some("status".into());
    }
    if matches!(spec.layout, Layout::Calendar | Layout::Agenda | Layout::Timeline) && spec.date.is_none() {
        spec.date = Some(spec.sort.as_ref().map_or_else(|| "date".into(), |s| s.0.clone()));
    }
    if spec.tasks && spec.fields == ["title"] {
        spec.fields = vec!["text".into()];
    }
    Ok(spec)
}

/// A field's value for a note: built-ins first, then front matter.
pub fn field(r: &NoteRecord, name: &str) -> Option<String> {
    let v = match name {
        "title" => r.title.clone(),
        "name" => r.name.clone(),
        "path" | "file" => r.path.as_str().to_string(),
        "folder" => r.path.folder().to_string(),
        "tags" if !r.tags.is_empty() => r.tags.join(", "),
        "tasks" => r.tasks.len().to_string(),
        "open_tasks" => r.tasks.iter().filter(|t| !t.done).count().to_string(),
        "done_tasks" => r.tasks.iter().filter(|t| t.done).count().to_string(),
        "words" => r.stats.words.to_string(),
        "links" => r.links.len().to_string(),
        _ => return property(r, name).map(Value::display).filter(|s| !s.is_empty()),
    };
    Some(v)
}

/// A front-matter value by name or dotted path (`author.name`), ignoring case.
pub fn property<'a>(r: &'a NoteRecord, name: &str) -> Option<&'a Value> {
    let ci =
        |m: &'a std::collections::BTreeMap<String, Value>, k: &str| m.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v);
    if let Some(v) = ci(&r.properties, name) {
        return Some(v);
    }
    let mut parts = name.split('.');
    let mut v = ci(&r.properties, parts.next()?)?;
    for p in parts {
        v = ci(v.as_map()?, p)?;
    }
    Some(v)
}

fn list_values(r: &NoteRecord, name: &str) -> Vec<String> {
    if name == "tags" {
        return r.tags.clone();
    }
    match property(r, name) {
        Some(Value::List(v)) => v.clone(),
        Some(v) => vec![v.display()],
        None => field(r, name).into_iter().collect(),
    }
}

/// Compare as numbers, then dates/strings (ISO dates sort as strings).
pub fn compare(a: &str, b: &str) -> Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD` moved by `days`.
fn add_days(date: &str, days: i64) -> Option<String> {
    if !frontmatter::is_date(date) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| date.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) + days);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// `today`, `tomorrow`, `yesterday`, `today+7d`, `today-1w` as a date; other values as they are.
pub fn resolve_date(value: &str, today: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    let offset = match v.as_str() {
        "today" => Some(0),
        "tomorrow" => Some(1),
        "yesterday" => Some(-1),
        _ => v.strip_prefix("today").and_then(|rest| {
            let (sign, rest) = match rest.as_bytes().first() {
                Some(b'+') => (1, &rest[1..]),
                Some(b'-') => (-1, &rest[1..]),
                _ => return None,
            };
            let unit = rest.chars().last()?;
            let n: i64 = rest[..rest.len() - 1].parse().ok()?;
            let days = match unit {
                'd' => n,
                'w' => n * 7,
                _ => return None,
            };
            Some(sign * days)
        }),
    };
    offset.and_then(|d| add_days(today, d)).unwrap_or_else(|| value.to_string())
}

/// What a filter needs from a row (a note or a task).
trait Fields {
    fn get(&self, name: &str) -> Option<String>;
    fn list(&self, name: &str) -> Vec<String>;
}

impl Fields for NoteRecord {
    fn get(&self, name: &str) -> Option<String> {
        field(self, name)
    }
    fn list(&self, name: &str) -> Vec<String> {
        list_values(self, name)
    }
}

fn matches(r: &impl Fields, f: &Filter, today: &str) -> bool {
    let want = resolve_date(&f.value, today);
    let have = r.get(&f.field);
    match f.op {
        Op::Exists => have.is_some(),
        Op::Missing => have.is_none(),
        Op::Contains => {
            let w = want.to_lowercase();
            let w = w.trim_start_matches('#');
            r.list(&f.field).iter().any(|v| v.to_lowercase().trim_start_matches('#') == w)
                || have.is_some_and(|h| h.to_lowercase().contains(w))
        }
        Op::Ne => have.is_none_or(|h| !h.eq_ignore_ascii_case(&want) && !r.list(&f.field).iter().any(|v| v.eq_ignore_ascii_case(&want))),
        Op::Eq => have.is_some_and(|h| h.eq_ignore_ascii_case(&want) || r.list(&f.field).iter().any(|v| v.eq_ignore_ascii_case(&want))),
        Op::Gt | Op::Lt | Op::Ge | Op::Le => {
            let Some(h) = have else { return false };
            // Dates compare by their day so `due < 2026-12-01` works with times.
            let h = if frontmatter::is_date(&h) && frontmatter::is_date(&want) { h[..10].to_string() } else { h };
            let o = compare(&h, &want);
            match f.op {
                Op::Gt => o == Ordering::Greater,
                Op::Lt => o == Ordering::Less,
                Op::Ge => o != Ordering::Less,
                _ => o != Ordering::Greater,
            }
        }
    }
}

/// A task as a row of a `from: tasks` view.
struct TaskFields<'a> {
    note: &'a NoteRecord,
    task: Task,
}

impl Fields for TaskFields<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let t = &self.task;
        let v = match name {
            "text" | "description" | "title" => Some(t.text.clone()),
            "status" => Some(if t.done { "done" } else { "open" }.to_string()),
            "completed" => Some(t.done.to_string()),
            "due" => t.due.clone(),
            "scheduled" => t.scheduled.clone(),
            "start" => t.start.clone(),
            "done" | "completion" => t.done_on.clone(),
            "created" => t.created.clone(),
            "priority" => Some(quire_markdown::tasks::priority_word(t.priority).to_string()),
            "priority_rank" => Some(quire_markdown::tasks::priority_rank(t.priority).to_string()),
            "recurrence" | "repeat" => t.recurrence.clone(),
            "reminder" => t.reminder.clone(),
            "time" => t.time_block.as_ref().map(|(a, b)| if b.is_empty() { a.clone() } else { format!("{a}–{b}") }),
            // When it's planned: due, else scheduled, else start; a time
            // block in a daily note (`2026-11-03.md`) is on that day.
            "date" => t
                .due
                .clone()
                .or_else(|| t.scheduled.clone())
                .or_else(|| t.start.clone())
                .or_else(|| t.time_block.as_ref().map(|_| self.note.name.clone()).filter(|n| frontmatter::is_date(n))),
            "path" | "file" => Some(self.note.path.as_str().to_string()),
            "note" => Some(self.note.title.clone()),
            "folder" => Some(self.note.path.folder().to_string()),
            "tags" => Some(task_tags(&t.text).join(", ")).filter(|s| !s.is_empty()),
            other => field(self.note, other.strip_prefix("note.").unwrap_or(other)),
        };
        v.filter(|s| !s.is_empty())
    }
    fn list(&self, name: &str) -> Vec<String> {
        if name == "tags" {
            return task_tags(&self.task.text);
        }
        self.get(name).into_iter().collect()
    }
}

/// `#tags` in a task's text, without `#`.
fn task_tags(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|w| w.strip_prefix('#'))
        .map(|w| w.trim_end_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))).to_string())
        .filter(|w| !w.is_empty() && !w.bytes().all(|b| b.is_ascii_digit()))
        .collect()
}

/// A note with `type: task` as a task row (title is the text).
fn note_as_task(r: &NoteRecord) -> Option<Task> {
    let get = |k: &str| property(r, k).and_then(Value::as_str).map(str::to_string);
    if !get("type").is_some_and(|t| t.eq_ignore_ascii_case("task")) {
        return None;
    }
    let status = get("status").unwrap_or_default().to_ascii_lowercase();
    Some(Task {
        done: matches!(status.as_str(), "done" | "x" | "completed" | "complete" | "cancelled"),
        text: r.title.clone(),
        due: get("due"),
        scheduled: get("scheduled"),
        start: get("start"),
        done_on: get("completion").or_else(|| get("done")),
        created: get("created"),
        recurrence: get("recurrence").or_else(|| get("repeat")),
        priority: get("priority").and_then(|p| quire_markdown::tasks::parse_priority(&p)),
        reminder: get("reminder"),
        ..Task::default()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRow {
    pub path: NotePath,
    pub title: String,
    /// One per `spec.fields`, "" when missing.
    pub cells: Vec<String>,
    /// Group key (kanban column), "" when missing.
    pub group: String,
    /// `YYYY-MM-DD` for calendar layout.
    pub date: Option<String>,
    /// For a `from: tasks` row: the task.
    pub task: Option<TaskRow>,
}

/// The task behind a row of a `from: tasks` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub task: Task,
    /// A note with `type: task` rather than a checklist line.
    pub is_note: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewResult {
    pub rows: Vec<ViewRow>,
    /// Kanban columns in display order ("" = no value).
    pub groups: Vec<String>,
    /// Matches before `limit` was applied.
    pub total: usize,
}

/// Resolve `./x` against the folder of the note holding the view.
fn resolve_folder(f: &str, current: Option<&NotePath>) -> String {
    match f.strip_prefix("./").or_else(|| (f == ".").then_some("")) {
        Some(rest) => {
            let base = current.map_or("", |c| c.folder());
            let rest = rest.trim_end_matches('/');
            match (base.is_empty(), rest.is_empty()) {
                (true, _) => rest.to_string(),
                (false, true) => base.to_string(),
                (false, false) => format!("{base}/{rest}"),
            }
        }
        None => f.to_string(),
    }
}

fn in_folder(path: &str, folder: &str) -> bool {
    folder.is_empty()
        || (path.len() > folder.len() && path[..folder.len()].eq_ignore_ascii_case(folder) && path.as_bytes()[folder.len()] == b'/')
}

/// Agenda groups in display order.
pub const AGENDA_GROUPS: [&str; 5] = ["Overdue", "Today", "Next 7 days", "Later", "No date"];

/// The agenda group for a date.
pub fn agenda_group(date: Option<&str>, today: &str) -> &'static str {
    let Some(d) = date.filter(|d| frontmatter::is_date(d)).map(|d| &d[..10]) else { return AGENDA_GROUPS[4] };
    let week = resolve_date("today+7d", today);
    match d.cmp(today) {
        Ordering::Less => AGENDA_GROUPS[0],
        Ordering::Equal => AGENDA_GROUPS[1],
        Ordering::Greater if d <= week.as_str() => AGENDA_GROUPS[2],
        Ordering::Greater => AGENDA_GROUPS[3],
    }
}

fn sort_rows<T>(rows: &mut [(T, ViewRow)], spec: &ViewSpec, get: impl Fn(&T, &str) -> Option<String>) {
    match &spec.sort {
        Some((f, desc)) => rows.sort_by(|(a, ra), (b, rb)| {
            // Missing values sort last either way.
            let o = match (get(a, f), get(b, f)) {
                (Some(x), Some(y)) => {
                    let o = compare(&x, &y);
                    if *desc { o.reverse() } else { o }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            o.then_with(|| ra.title.to_lowercase().cmp(&rb.title.to_lowercase()))
        }),
        None if spec.tasks => rows.sort_by(|(a, ra), (b, rb)| {
            let key = |t: &T, r: &ViewRow| (r.task.as_ref().is_some_and(|t| t.task.done), get(t, "date").is_none(), get(t, "date"));
            key(a, ra).cmp(&key(b, rb)).then_with(|| ra.path.cmp(&rb.path))
        }),
        None => rows.sort_by_key(|(_, a)| a.title.to_lowercase()),
    }
}

/// Evaluate a spec. `today` is `YYYY-MM-DD`, used by `where: due < today`.
/// `current` is the note holding the view: it's left out of the results,
/// and `from: ./Folder/` is relative to it.
pub fn run(spec: &ViewSpec, index: &Index, today: &str, current: Option<&NotePath>) -> ViewResult {
    let folders: Vec<Option<String>> =
        spec.from.iter().map(|s| if let Source::Folder(f) = s { Some(resolve_folder(f, current)) } else { None }).collect();
    let notes = index.records().filter(|r| {
        spec.from.is_empty()
            || spec.from.iter().zip(&folders).any(|(s, folder)| match s {
                Source::All => true,
                Source::Tag(t) => r.has_tag(t),
                Source::Folder(_) => in_folder(r.path.as_str(), folder.as_deref().unwrap_or_default()),
            })
    });
    let date_of = |get: &dyn Fn(&str) -> Option<String>| {
        spec.date.as_deref().and_then(get).filter(|d| frontmatter::is_date(d)).map(|d| d[..10].to_string())
    };
    let mut rows: Vec<ViewRow> = if spec.tasks {
        let mut rows: Vec<(TaskFields<'_>, ViewRow)> = notes
            .flat_map(|r| {
                let as_note = note_as_task(r).map(|t| (t, true));
                r.tasks.iter().cloned().map(|t| (t, false)).chain(as_note).map(move |(task, is_note)| (r, task, is_note))
            })
            .map(|(r, task, is_note)| (TaskFields { note: r, task }, is_note))
            .filter(|(t, _)| spec.filters.iter().all(|f| matches(t, f, today)))
            .map(|(t, is_note)| {
                let cells = spec.fields.iter().map(|f| t.get(f).unwrap_or_default()).collect();
                let group = spec.group.as_deref().and_then(|g| t.list(g).into_iter().next()).unwrap_or_default();
                let date = date_of(&|d| t.get(d));
                let row = ViewRow {
                    path: t.note.path.clone(),
                    title: t.task.text.clone(),
                    cells,
                    group,
                    date,
                    task: Some(TaskRow { task: t.task.clone(), is_note }),
                };
                (t, row)
            })
            .collect();
        // Priorities sort by rank (highest first), not by name.
        sort_rows(&mut rows, spec, |t, f| t.get(if f == "priority" { "priority_rank" } else { f }));
        rows.into_iter().map(|(_, r)| r).collect()
    } else {
        let mut rows: Vec<(&NoteRecord, ViewRow)> = notes
            .filter(|r| Some(&r.path) != current)
            .filter(|r| spec.filters.iter().all(|f| matches(*r, f, today)))
            .map(|r| {
                let cells = spec.fields.iter().map(|f| field(r, f).unwrap_or_default()).collect();
                let group = spec.group.as_deref().and_then(|g| list_values(r, g).into_iter().next()).unwrap_or_default();
                let date = date_of(&|d| field(r, d));
                (r, ViewRow { path: r.path.clone(), title: r.title.clone(), cells, group, date, task: None })
            })
            .collect();
        sort_rows(&mut rows, spec, |r, f| field(r, f));
        rows.into_iter().map(|(_, r)| r).collect()
    };
    if matches!(spec.layout, Layout::Agenda | Layout::Timeline) {
        // Agenda: by date bucket; timeline: by date. Undated rows go last.
        rows.sort_by(|a, b| (a.date.is_none(), &a.date).cmp(&(b.date.is_none(), &b.date)));
        if spec.layout == Layout::Agenda {
            for r in &mut rows {
                r.group = agenda_group(r.date.as_deref(), today).to_string();
            }
        }
    }
    let total = rows.len();
    if let Some(l) = spec.limit {
        rows.truncate(l);
    }
    let mut groups = Vec::new();
    if spec.layout == Layout::Agenda {
        groups.extend(AGENDA_GROUPS.iter().filter(|g| rows.iter().any(|r| r.group == **g)).map(|g| (*g).to_string()));
    } else if spec.group.is_some() {
        groups.extend(spec.columns.iter().cloned());
        let mut seen: Vec<String> =
            rows.iter().map(|r| r.group.clone()).filter(|g| !groups.iter().any(|c| c.eq_ignore_ascii_case(g))).collect();
        seen.sort_by_key(|g| (g.is_empty(), g.to_lowercase()));
        seen.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        groups.extend(seen);
    }
    ViewResult { rows, groups, total }
}

/// A view block found in a note: the spec's text and where it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewBlock {
    pub spec: String,
    /// Line of the opening fence.
    pub open_line: usize,
    /// Line of the closing fence; the view renders below it.
    pub close_line: usize,
}

/// Find ```` ```quire-view ```` blocks in a note.
pub fn find_blocks(src: &str) -> Vec<ViewBlock> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String, String)> = None;
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        match &mut open {
            None => {
                let fence: String = t.chars().take_while(|&c| c == '`' || c == '~').collect();
                if fence.len() >= 3 && t[fence.len()..].trim() == FENCE {
                    open = Some((i, fence, String::new()));
                }
            }
            Some((start, fence, body)) => {
                if t.starts_with(fence.as_str()) && t.trim_end().chars().all(|c| c == fence.chars().next().unwrap_or('`')) {
                    out.push(ViewBlock { spec: std::mem::take(body), open_line: *start, close_line: i });
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    out
}

/// A query from another app translated into a view spec.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Translation {
    /// The `quire-view` text, without fences.
    pub spec: String,
    /// What couldn't be translated, in plain words. The view is shown
    /// without those parts.
    pub unsupported: Vec<String>,
}

impl Translation {
    fn push(&mut self, key: &str, value: &str) {
        self.spec.push_str(key);
        self.spec.push_str(": ");
        self.spec.push_str(value);
        self.spec.push('\n');
    }

    fn skip(&mut self, what: impl Into<String>) {
        let w = what.into();
        if !self.unsupported.contains(&w) {
            self.unsupported.push(w);
        }
    }
}

/// Quote a filter value if it has spaces or operator characters.
fn view_value(v: &str) -> String {
    if v.contains([' ', '=', '<', '>', '!', '#', ',']) || v.is_empty() { format!("\"{}\"", v.replace('"', "")) } else { v.to_string() }
}

/// Split at top-level occurrences of `sep` (case-insensitive word), outside quotes and brackets.
fn split_top(s: &str, sep: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    let lower = s.to_ascii_lowercase();
    let pat = format!(" {} ", sep.to_ascii_lowercase());
    let mut i = 0;
    let b = s.as_bytes();
    while i < s.len() {
        let c = b[i] as char;
        match c {
            '"' | '\'' if quote == Some(c) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(c),
            '(' | '[' if quote.is_none() => depth += 1,
            ')' | ']' if quote.is_none() => depth -= 1,
            _ => {}
        }
        if quote.is_none() && depth == 0 && lower[i..].starts_with(&pat) {
            out.push(s[start..i].trim().to_string());
            i += pat.len();
            start = i;
            continue;
        }
        i += c.len_utf8().max(1);
        while i < s.len() && !s.is_char_boundary(i) {
            i += 1;
        }
    }
    out.push(s[start..].trim().to_string());
    out.retain(|p| !p.is_empty());
    out
}

/// Dataview (DQL) queries: `TABLE/LIST/TASK … FROM … WHERE … SORT … LIMIT n`.
pub mod dataview {
    use super::{Translation, split_top, view_value};

    const CLAUSES: [&str; 6] = ["FROM", "WHERE", "SORT", "GROUP BY", "FLATTEN", "LIMIT"];

    /// A Dataview field as a view field, or why not.
    fn field(f: &str, tasks: bool) -> Result<String, String> {
        let f = f.trim();
        let lower = f.to_ascii_lowercase();
        Ok(match lower.as_str() {
            "file.name" => "name".into(),
            "file.link" | "file.file" => "title".into(),
            "file.folder" => "folder".into(),
            "file.path" => "path".into(),
            "file.tags" | "file.etags" => "tags".into(),
            "completed" if tasks => "status".into(),
            "text" if tasks => "text".into(),
            "completion" if tasks => "done".into(),
            l if l.starts_with("file.") || l.starts_with("this.") || l.contains('(') || l.contains(['+', '*', '/', '"', '[']) => {
                return Err(format!("the field “{f}”"));
            }
            _ => {
                if lower.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))) {
                    lower
                } else {
                    return Err(format!("the field “{f}”"));
                }
            }
        })
    }

    /// A literal: `"text"`, `date(today)`, `date(2026-12-01)`, a number or word.
    fn literal(v: &str) -> Option<String> {
        let v = v.trim();
        if let Some(inner) = v.strip_prefix("date(").and_then(|r| r.strip_suffix(')')) {
            let inner = inner.trim().trim_matches('"');
            return match inner.to_ascii_lowercase().as_str() {
                "today" | "tomorrow" | "yesterday" => Some(inner.to_ascii_lowercase()),
                d if quire_markdown::frontmatter::is_date(d) => Some(d[..10].to_string()),
                _ => None,
            };
        }
        if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
            return Some(v[1..v.len() - 1].to_string());
        }
        if v.contains(['(', '[', '"']) {
            return None;
        }
        Some(v.to_string())
    }

    fn condition(c: &str, tasks: bool, out: &mut Translation) -> Option<String> {
        let mut c = c.trim();
        // Outer brackets around the whole condition: `(a = 1)`.
        while c.starts_with('(') && c.ends_with(')') && c[1..c.len() - 1].find(')').is_none_or(|close| c[1..=close].contains('(')) {
            c = c[1..c.len() - 1].trim();
        }
        let lower = c.to_ascii_lowercase();
        if tasks && (lower == "!completed" || lower == "completed = false") {
            return Some("status = open".into());
        }
        if tasks && (lower == "completed" || lower == "completed = true") {
            return Some("status = done".into());
        }
        if let Some(args) = lower.strip_prefix("contains(").and_then(|r| r.strip_suffix(')')) {
            let raw = &c["contains(".len()..c.len() - 1];
            let _ = args;
            let (f, v) = raw.split_once(',')?;
            let f = field(f, tasks).map_err(|e| out.skip(e)).ok()?;
            let v = literal(v)?;
            return Some(format!("{f} contains {}", view_value(v.trim_start_matches('#'))));
        }
        for (sym, op) in [("!=", "!="), (">=", ">="), ("<=", "<="), ("=", "="), (">", ">"), ("<", "<")] {
            if let Some((f, v)) = c.split_once(sym) {
                let f = field(f, tasks).map_err(|e| out.skip(e)).ok()?;
                let v = v.trim();
                if v.eq_ignore_ascii_case("null") {
                    return Some(format!("{f} {}", if op == "=" { "missing" } else { "exists" }));
                }
                let v = literal(v)?;
                return Some(format!("{f} {op} {}", view_value(&v)));
            }
        }
        if let Some(f) = c.strip_prefix('!') {
            return field(f, tasks).ok().map(|f| format!("{f} missing"));
        }
        field(c, tasks).ok().map(|f| format!("{f} exists"))
    }

    /// Translate a ```` ```dataview ```` block.
    pub fn translate(query: &str) -> Translation {
        let mut out = Translation::default();
        let text: String =
            query.lines().map(|l| l.split("//").next().unwrap_or("").trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        let upper = text.to_ascii_uppercase();
        let kind = upper.split_whitespace().next().unwrap_or("");
        let tasks = kind == "TASK";
        let head_len = kind.len();
        // Find where each clause starts.
        let mut marks: Vec<(usize, &str)> = Vec::new();
        for c in CLAUSES {
            let mut from = 0;
            while let Some(p) = upper[from..]
                .find(&format!(" {c} "))
                .map(|p| p + from)
                .or_else(|| upper[from..].ends_with(&format!(" {c}")).then(|| upper.len() - c.len() - 1))
            {
                let in_quotes = text[..p].matches('"').count() % 2 == 1;
                if !in_quotes {
                    marks.push((p, c));
                }
                from = p + c.len() + 1;
                if from >= upper.len() {
                    break;
                }
            }
        }
        marks.sort_unstable();
        let head_end = marks.first().map_or(text.len(), |m| m.0);
        let head = text.get(head_len..head_end).unwrap_or("").trim();
        let clause = |c: &str| -> Vec<String> {
            marks
                .iter()
                .enumerate()
                .filter(|(_, m)| m.1 == c)
                .map(|(i, m)| {
                    let end = marks.get(i + 1).map_or(text.len(), |n| n.0);
                    text[m.0 + c.len() + 1..end].trim().to_string()
                })
                .collect()
        };
        let mut fields = Vec::new();
        let layout = match kind {
            "TABLE" => {
                let mut h = head;
                if h.to_ascii_uppercase().starts_with("WITHOUT ID") {
                    h = h[10..].trim();
                } else {
                    fields.push("title".to_string());
                }
                for col in split_top(&h.replace(',', " , "), ",") {
                    let expr = split_top(&col, "AS").into_iter().next().unwrap_or_default();
                    match field(&expr, false) {
                        Ok(f) => fields.push(f),
                        Err(e) => out.skip(e),
                    }
                }
                "table"
            }
            "LIST" => {
                fields.push("title".into());
                if !head.is_empty() {
                    match field(head, false) {
                        Ok(f) => fields.push(f),
                        Err(e) => out.skip(e),
                    }
                }
                "list"
            }
            "TASK" => "list",
            "CALENDAR" => {
                out.skip("CALENDAR queries");
                "calendar"
            }
            _ => {
                out.skip(format!("the query type “{kind}”"));
                "table"
            }
        };
        let mut sources: Vec<String> = Vec::new();
        if tasks {
            sources.push("tasks".into());
        }
        for f in clause("FROM") {
            for src in split_top(&f, "OR") {
                let s = src.trim();
                if s.starts_with('#') && !s.contains(' ') {
                    sources.push(s.to_string());
                } else if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
                    let folder = s[1..s.len() - 1].trim_end_matches('/');
                    sources.push(view_value(&format!("{folder}/")));
                } else {
                    out.skip(format!("the source “{s}” (only folders and #tags joined by “or”)"));
                }
            }
        }
        if !sources.is_empty() {
            out.push("from", &sources.join(", "));
        }
        let mut conds = Vec::new();
        for w in clause("WHERE") {
            if split_top(&w, "OR").len() > 1 {
                out.skip("“or” in WHERE");
                continue;
            }
            for c in split_top(&w, "AND") {
                match condition(&c, tasks, &mut out) {
                    Some(x) => conds.push(x),
                    None => out.skip(format!("the condition “{c}”")),
                }
            }
        }
        if !conds.is_empty() {
            out.push("where", &conds.join(" and "));
        }
        let sorts: Vec<String> =
            clause("SORT").iter().flat_map(|s| s.split(',').map(str::trim).map(str::to_string).collect::<Vec<_>>()).collect();
        if let Some(first) = sorts.first() {
            let mut parts = first.split_whitespace();
            let f = parts.next().unwrap_or("");
            let desc = parts.next().is_some_and(|d| d.eq_ignore_ascii_case("desc"));
            match field(f, tasks) {
                Ok(f) => out.push("sort", &format!("{f}{}", if desc { " desc" } else { "" })),
                Err(e) => out.skip(e),
            }
            if sorts.len() > 1 {
                out.skip("sorting by more than one field");
            }
        }
        for g in clause("GROUP BY") {
            match field(&g, tasks) {
                Ok(f) if tasks => out.push("group", &f),
                Ok(_) => out.skip("GROUP BY in tables and lists"),
                Err(_) => out.skip("GROUP BY with an expression"),
            }
        }
        if !clause("FLATTEN").is_empty() {
            out.skip("FLATTEN");
        }
        if let Some(l) = clause("LIMIT").first() {
            match l.trim().parse::<usize>() {
                Ok(n) => out.push("limit", &n.to_string()),
                Err(_) => out.skip(format!("LIMIT {l}")),
            }
        }
        if !fields.is_empty() && !tasks {
            out.push("fields", &format!("[{}]", fields.join(", ")));
        }
        out.push("layout", layout);
        out
    }
}

/// Obsidian Tasks query blocks (```` ```tasks ````), one instruction a line.
pub mod tasks_query {
    use super::{Translation, view_value};

    const DATE_FIELDS: [(&str, &str); 5] =
        [("due", "due"), ("scheduled", "scheduled"), ("starts", "start"), ("start", "start"), ("done", "done")];

    /// A date the views understand: a day, `today`/`tomorrow`/`yesterday`,
    /// `in 7 days`, `in 2 weeks`, `3 days ago`.
    fn date_word(v: &str) -> Option<String> {
        let v = v.trim().to_ascii_lowercase();
        let words: Vec<&str> = v.split_whitespace().collect();
        let span = |n: &str, unit: &str| -> Option<i64> {
            let n: i64 = n.parse().ok()?;
            match unit.trim_end_matches('s') {
                "day" => Some(n),
                "week" => Some(n * 7),
                _ => None,
            }
        };
        match words.as_slice() {
            [w @ ("today" | "tomorrow" | "yesterday")] => Some((*w).to_string()),
            [d] if quire_markdown::frontmatter::is_date(d) && d.len() == 10 => Some((*d).to_string()),
            ["in", n, unit] => span(n, unit).map(|d| format!("today+{d}d")),
            [n, unit, "ago"] => span(n, unit).map(|d| format!("today-{d}d")),
            _ => None,
        }
    }

    fn date_filter(field: &str, rest: &str) -> Option<Vec<String>> {
        let r = rest.trim().to_ascii_lowercase();
        let (op, v) = if let Some(v) = r.strip_prefix("on or before ") {
            ("<=", v)
        } else if let Some(v) = r.strip_prefix("on or after ") {
            (">=", v)
        } else if let Some(v) = r.strip_prefix("before ") {
            ("<", v)
        } else if let Some(v) = r.strip_prefix("after ") {
            (">", v)
        } else if let Some(v) = r.strip_prefix("on ") {
            ("=", v)
        } else if let Some(v) = r.strip_prefix("in ").or_else(|| r.strip_prefix("between ")) {
            let parts: Vec<&str> = v.split_whitespace().filter(|w| *w != "and").collect();
            if let [a, b] = parts.as_slice() {
                return Some(vec![format!("{field} >= {}", date_word(a)?), format!("{field} <= {}", date_word(b)?)]);
            }
            return None;
        } else {
            ("=", r.as_str())
        };
        let parts: Vec<&str> = v.split_whitespace().collect();
        if let [a, b] = parts.as_slice()
            && op == "="
            && quire_markdown::frontmatter::is_date(a)
        {
            return Some(vec![format!("{field} >= {}", date_word(a)?), format!("{field} <= {}", date_word(b)?)]);
        }
        Some(vec![format!("{field} {op} {}", date_word(v)?)])
    }

    fn sort_field(f: &str) -> Option<&'static str> {
        Some(match f {
            "due" => "due",
            "scheduled" => "scheduled",
            "start" => "start",
            "done" => "done",
            "priority" => "priority",
            "path" => "path",
            "filename" => "note",
            "description" => "text",
            "status" => "status",
            "created" => "created",
            _ => return None,
        })
    }

    /// Translate a ```` ```tasks ```` block.
    pub fn translate(query: &str) -> Translation {
        let mut out = Translation::default();
        let mut conds: Vec<String> = Vec::new();
        let (mut sort, mut group, mut limit) = (None, None, None);
        for line in query.lines() {
            let raw = line.trim();
            let l = raw.to_ascii_lowercase();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut handled = true;
            match l.as_str() {
                "not done" => conds.push("status = open".into()),
                "done" => conds.push("status = done".into()),
                "is recurring" => conds.push("recurrence exists".into()),
                "is not recurring" => conds.push("recurrence missing".into()),
                "explain" => out.skip("explain"),
                "short mode" | "full mode" | "short" | "full" => {}
                _ => handled = false,
            }
            if handled {
                continue;
            }
            if l.starts_with("hide ") || l.starts_with("show ") {
                continue; // display options don't change which tasks match
            }
            if l.starts_with('(') || l.starts_with("not (") || l.contains(") and (") || l.contains(") or (") {
                out.skip(format!("“{raw}” (boolean combinations)"));
                continue;
            }
            if l.starts_with("filter by function") || l.starts_with("sort by function") || l.starts_with("group by function") {
                out.skip("custom JavaScript filters");
                continue;
            }
            if let Some(f) = l.strip_prefix("has ").and_then(|r| r.strip_suffix(" date"))
                && let Some((_, q)) = DATE_FIELDS.iter().find(|(w, _)| *w == f)
            {
                conds.push(format!("{q} exists"));
                continue;
            }
            if let Some(f) = l.strip_prefix("no ").and_then(|r| r.strip_suffix(" date"))
                && let Some((_, q)) = DATE_FIELDS.iter().find(|(w, _)| *w == f)
            {
                conds.push(format!("{q} missing"));
                continue;
            }
            if let Some((word, q)) = DATE_FIELDS.iter().find(|(w, _)| l.starts_with(&format!("{w} "))) {
                match date_filter(q, &l[word.len()..]) {
                    Some(v) => conds.extend(v),
                    None => out.skip(format!("“{raw}”")),
                }
                continue;
            }
            if let Some(p) = l.strip_prefix("priority is ") {
                let p = p.trim();
                if p.starts_with("above") || p.starts_with("below") || p.starts_with("not") {
                    out.skip(format!("“{raw}”"));
                } else {
                    let w = if p == "none" { "normal" } else { p };
                    conds.push(format!("priority = {w}"));
                }
                continue;
            }
            let includes = |prefix: &str| -> Option<String> { l.strip_prefix(prefix).map(|_| raw[prefix.len()..].trim().to_string()) };
            if let Some(v) = includes("path includes ") {
                conds.push(format!("path contains {}", view_value(&v)));
                continue;
            }
            if let Some(v) = includes("description includes ") {
                conds.push(format!("text contains {}", view_value(&v)));
                continue;
            }
            if let Some(v) = includes("tag includes ").or_else(|| includes("tags include ")) {
                conds.push(format!("tags contains {}", view_value(v.trim_start_matches('#'))));
                continue;
            }
            if let Some(f) = l.strip_prefix("sort by ") {
                let mut parts = f.split_whitespace();
                let name = parts.next().unwrap_or("");
                let reverse = parts.any(|p| p == "reverse");
                match (sort_field(name), &sort) {
                    (Some(q), None) => sort = Some(format!("{q}{}", if reverse { " desc" } else { "" })),
                    (Some(_), Some(_)) => out.skip("sorting by more than one field"),
                    (None, _) => out.skip(format!("sorting by {name}")),
                }
                continue;
            }
            if let Some(f) = l.strip_prefix("group by ") {
                let f = f.split_whitespace().next().unwrap_or("");
                match (sort_field(f), &group) {
                    (Some(q), None) if matches!(q, "due" | "path" | "priority" | "note" | "status" | "scheduled" | "start" | "done") => {
                        group = Some(q);
                    }
                    (Some(_), Some(_)) => out.skip("grouping by more than one field"),
                    (_, _) if f == "folder" && group.is_none() => group = Some("folder"),
                    _ => out.skip(format!("grouping by {f}")),
                }
                continue;
            }
            if let Some(n) = l.strip_prefix("limit to ").or_else(|| l.strip_prefix("limit ")) {
                match n.split_whitespace().next().and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) if !n.to_string().is_empty() && !l.starts_with("limit groups") => limit = Some(n),
                    _ => out.skip(format!("“{raw}”")),
                }
                continue;
            }
            out.skip(format!("“{raw}”"));
        }
        out.push("from", "tasks");
        if !conds.is_empty() {
            out.push("where", &conds.join(" and "));
        }
        if let Some(s) = sort {
            out.push("sort", &s);
        }
        if let Some(g) = group {
            out.push("group", g);
        }
        if let Some(n) = limit {
            out.push("limit", &n.to_string());
        }
        out.push("layout", "list");
        out
    }
}

/// A Dataview or Tasks block found in a note, rendered by Quire in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatBlock {
    /// `dataview`, `tasks` or `dataviewjs`.
    pub lang: String,
    pub body: String,
    pub open_line: usize,
    pub close_line: usize,
}

/// Find ```` ```dataview ````, ```` ```dataviewjs ```` and ```` ```tasks ```` blocks.
pub fn find_compat_blocks(src: &str) -> Vec<CompatBlock> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String, String, String)> = None;
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        match &mut open {
            None => {
                let fence: String = t.chars().take_while(|&c| c == '`' || c == '~').collect();
                let lang = t[fence.len()..].trim().to_ascii_lowercase();
                if fence.len() >= 3 && matches!(lang.as_str(), "dataview" | "dataviewjs" | "tasks") {
                    open = Some((i, fence, lang, String::new()));
                }
            }
            Some((start, fence, lang, body)) => {
                if t.starts_with(fence.as_str()) && t.trim_end().chars().all(|c| c == fence.chars().next().unwrap_or('`')) {
                    out.push(CompatBlock { lang: std::mem::take(lang), body: std::mem::take(body), open_line: *start, close_line: i });
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    out
}

impl CompatBlock {
    /// The block as a view spec (`dataviewjs` can't be translated).
    pub fn translate(&self) -> Translation {
        match self.lang.as_str() {
            "dataview" => dataview::translate(&self.body),
            "tasks" => tasks_query::translate(&self.body),
            _ => Translation { spec: String::new(), unsupported: vec!["DataviewJS (it runs code, which Quire never does)".into()] },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut ix = Index::new();
        ix.upsert(
            NotePath::new("Projects/Alpha.md"),
            "---\nstatus: doing\ndue: 2026-11-20\npriority: 2\ntags: [project]\n---\n# Alpha\n- [ ] a\n- [x] b\n",
        );
        ix.upsert(
            NotePath::new("Projects/Beta.md"),
            "---\nstatus: done\ndue: 2026-11-03\npriority: 10\ntags: [project, rust]\n---\n# Beta\n",
        );
        ix.upsert(NotePath::new("Projects/Gamma.md"), "---\nstatus: todo\npriority: 1\n---\n# Gamma\nPart of #project\n");
        ix.upsert(NotePath::new("Notes/Other.md"), "---\nstatus: todo\n---\n# Other\n");
        ix.refresh();
        ix
    }

    fn titles(r: &ViewResult) -> Vec<&str> {
        r.rows.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn parses_specs() {
        let s = parse("from: \"#project\"   # tag\nwhere: status != done and due < 2026-12-01\nsort: due desc\nlayout: kanban\nfields: [Title, status, due]\n").unwrap();
        assert_eq!(s.from, vec![Source::Tag("project".into())]);
        assert_eq!(s.filters.len(), 2);
        assert_eq!(s.filters[0], Filter { field: "status".into(), op: Op::Ne, value: "done".into() });
        assert_eq!(s.sort, Some(("due".into(), true)));
        assert_eq!(s.group.as_deref(), Some("status"), "kanban groups by status by default");
        assert_eq!(s.fields, vec!["title", "status", "due"]);
        assert!(parse("layout: pie").is_err());
        assert!(parse("where: ???").is_err());
        assert!(parse("nonsense").is_err());
    }

    #[test]
    fn filters_sorts_and_groups() {
        let ix = index();
        let r =
            run(&parse("from: Projects/\nsort: priority desc\nfields: [title, priority, open_tasks]").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Beta", "Alpha", "Gamma"], "numeric sort, not string sort");
        assert_eq!(r.rows[1].cells, vec!["Alpha", "2", "1"]);
        let r = run(&parse("from: #project\nwhere: status != done").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Alpha", "Gamma"], "inline #tags count too");
        let r = run(&parse("where: due < 2026-11-10").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Beta"]);
        let r = run(&parse("where: due > today and tags contains project").unwrap(), &ix, "2026-11-05", None);
        assert_eq!(titles(&r), vec!["Alpha"]);
        let r = run(&parse("where: due missing").unwrap(), &ix, "", None);
        assert_eq!(titles(&r), vec!["Gamma", "Other"]);
        let r = run(&parse("from: Projects/\nlayout: kanban\ncolumns: [todo, doing, done, blocked]").unwrap(), &ix, "", None);
        assert_eq!(r.groups, vec!["todo", "doing", "done", "blocked"]);
        assert_eq!(r.rows.iter().find(|x| x.title == "Alpha").unwrap().group, "doing");
        let here = NotePath::new("Projects/Board.md");
        let r = run(&parse("from: ./").unwrap(), &ix, "", Some(&here));
        assert_eq!(titles(&r), vec!["Alpha", "Beta", "Gamma"], "relative to the view's note");
        let r = run(&parse("from: ./Sub/").unwrap(), &ix, "", Some(&here));
        assert!(r.rows.is_empty());
        let r = run(&parse("layout: calendar\ndate: due\nlimit: 1\nsort: due").unwrap(), &ix, "", None);
        assert_eq!((r.rows[0].date.as_deref(), r.total), (Some("2026-11-03"), 4));
    }

    #[test]
    fn dotted_fields_and_relative_dates() {
        let mut ix = index();
        ix.upsert(NotePath::new("Papers/P.md"), "---\nauthor:\n  name: Ada\n  org:\n    city: London\ndue: 2026-11-03\n---\n# P\n");
        ix.refresh();
        let r = run(&parse("where: author.name = ada\nfields: [title, author.org.city]").unwrap(), &ix, "2026-11-01", None);
        assert_eq!((titles(&r), r.rows[0].cells[1].as_str()), (vec!["P"], "London"));
        let r = run(&parse("where: due <= today+2d").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Beta", "P"]);
        assert_eq!(resolve_date("tomorrow", "2026-12-31"), "2027-01-01");
        assert_eq!(resolve_date("today-1w", "2026-03-04"), "2026-02-25");
        assert_eq!(resolve_date("today+3x", "2026-03-04"), "today+3x");
        assert_eq!(add_days("2024-02-28", 1).as_deref(), Some("2024-02-29"));
    }

    #[test]
    fn task_views() {
        let mut ix = Index::new();
        ix.upsert(
            NotePath::new("Work.md"),
            "# Work\n- [ ] Ship #release ⏫ 📅 2026-11-05\n- [x] Plan ✅ 2026-11-01\n- [ ] Write docs 🔽 📅 2026-10-30\n- [ ] Someday\n",
        );
        ix.upsert(NotePath::new("Tasks/Report.md"), "---\ntype: task\nstatus: todo\ndue: 2026-11-02\n---\n# Report\n");
        ix.refresh();
        let r = run(&parse("from: tasks\nwhere: status = open").unwrap(), &ix, "2026-11-02", None);
        assert_eq!(titles(&r), vec!["Write docs", "Report", "Ship #release", "Someday"], "by due date, undated last");
        assert!(r.rows[1].task.as_ref().unwrap().is_note);
        assert_eq!(r.rows[0].cells, vec!["Write docs"], "fields default to the text");
        let r = run(&parse("from: tasks\nwhere: tags contains release").unwrap(), &ix, "2026-11-02", None);
        assert_eq!(titles(&r), vec!["Ship #release"]);
        let r = run(&parse("from: tasks\nsort: priority").unwrap(), &ix, "2026-11-02", None);
        assert_eq!(r.rows[0].title, "Ship #release", "high before normal");
        assert_eq!(r.rows.last().unwrap().title, "Write docs", "low last");
        let r = run(&parse("from: tasks\nlayout: agenda\nwhere: status = open").unwrap(), &ix, "2026-11-02", None);
        assert_eq!(r.groups, vec!["Overdue", "Today", "Next 7 days", "No date"]);
        assert_eq!(r.rows[0].group, "Overdue");
        let r = run(&parse("from: tasks, Tasks/\nlayout: timeline").unwrap(), &ix, "2026-11-02", None);
        assert_eq!(titles(&r), vec!["Report"]);
        assert_eq!(r.rows[0].date.as_deref(), Some("2026-11-02"));
        let s = parse("from: Projects/\nnew: Templates/Project.md\nlayout: kanban").unwrap();
        assert_eq!(s.new.as_deref(), Some("Templates/Project.md"));
    }

    #[test]
    fn dataview_fixtures() {
        let cases = [
            (
                "TABLE status, due AS \"Due\", file.folder\nFROM \"Projects\" OR #project\nWHERE status != \"done\" AND due < date(today)\nSORT due DESC\nLIMIT 5",
                "from: Projects/, #project\nwhere: status != done and due < today\nsort: due desc\nlimit: 5\nfields: [title, status, due, folder]\nlayout: table\n",
                0,
            ),
            (
                "LIST FROM #book WHERE contains(author.name, \"Le Guin\")",
                "from: #book\nwhere: author.name contains \"Le Guin\"\nfields: [title]\nlayout: list\n",
                0,
            ),
            (
                "TASK FROM \"Work\" WHERE !completed AND due <= date(2026-12-01)\nGROUP BY file.link",
                "from: tasks, Work/\nwhere: status = open and due <= 2026-12-01\ngroup: title\nlayout: list\n",
                0,
            ),
            (
                "TABLE WITHOUT ID file.link, file.mtime FROM \"A\" and #b WHERE x = 1 OR y = 2 FLATTEN z",
                "fields: [title]\nlayout: table\n",
                4,
            ),
            ("CALENDAR file.day", "layout: calendar\n", 1),
        ];
        for (q, want, skipped) in cases {
            let t = dataview::translate(q);
            assert_eq!(t.spec, want, "{q}");
            assert_eq!(t.unsupported.len(), skipped, "{q}: {:?}", t.unsupported);
            assert!(parse(&t.spec).is_ok(), "{}", t.spec);
        }
    }

    #[test]
    fn tasks_query_fixtures() {
        assert_eq!(tasks_query::translate("due before in 7 days").spec, "from: tasks\nwhere: due < today+7d\nlayout: list\n");
        assert_eq!(tasks_query::translate("done after 2 weeks ago").spec, "from: tasks\nwhere: done > today-14d\nlayout: list\n");
        let t = tasks_query::translate(
            "not done\ndue before tomorrow\npath includes Work/\ntag includes #release\npriority is high\nsort by priority reverse\ngroup by path\nlimit to 20 tasks\nhide edit button\n",
        );
        assert_eq!(
            t.spec,
            "from: tasks\nwhere: status = open and due < tomorrow and path contains Work/ and tags contains release and priority = high\nsort: priority desc\ngroup: path\nlimit: 20\nlayout: list\n"
        );
        assert!(t.unsupported.is_empty(), "{:?}", t.unsupported);
        let t = tasks_query::translate(
            "done on 2026-11-01\nscheduled 2026-11-01 2026-11-30\nhas start date\nno due date\nis recurring\nexplain\nfilter by function task.urgency > 5\n(due today) OR (done)\nhappens before today\n",
        );
        assert_eq!(
            t.spec,
            "from: tasks\nwhere: done = 2026-11-01 and scheduled >= 2026-11-01 and scheduled <= 2026-11-30 and start exists and due missing and recurrence exists\nlayout: list\n"
        );
        assert_eq!(t.unsupported.len(), 4, "{:?}", t.unsupported);
        let spec = parse(&t.spec).unwrap();
        assert!(spec.tasks && spec.filters.len() == 6);
    }

    #[test]
    fn finds_compat_blocks() {
        let src = "# N\n```dataview\nLIST FROM #a\n```\n```tasks\nnot done\n```\n~~~dataviewjs\ndv.list()\n~~~\n```quire-view\nlayout: list\n```\n";
        let b = find_compat_blocks(src);
        assert_eq!(
            b.iter().map(|b| (b.lang.as_str(), b.open_line, b.close_line)).collect::<Vec<_>>(),
            [("dataview", 1, 3), ("tasks", 4, 6), ("dataviewjs", 7, 9)]
        );
        assert!(b[2].translate().unsupported[0].contains("DataviewJS"));
        assert_eq!(b[0].translate().spec, "from: #a\nfields: [title]\nlayout: list\n");
    }

    #[test]
    fn finds_blocks() {
        let src = "# Board\n```quire-view\nfrom: Projects/\n```\ntext\n~~~~ quire-view\nlayout: list\n~~~~\n```rust\nx\n```\n";
        let b = find_blocks(src);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].open_line, b[0].close_line, b[0].spec.as_str()), (1, 3, "from: Projects/\n"));
        assert_eq!((b[1].open_line, b[1].close_line), (5, 7));
    }
}
