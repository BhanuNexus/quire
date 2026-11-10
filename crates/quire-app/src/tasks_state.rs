// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-E's entry point into the workspace.
//!
//! Planning and metadata: tasks, agenda, reminders and the commands of
//! periodic notes, flashcards, templates, Obsidian compatibility and
//! metadata refactors.
//!
//! `features.rs` calls the functions below for every Plan-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-E. State lives in
//! `ws.features.tasks`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate};
use quire_index::views::{self, CompatBlock};
use quire_markdown::Value;
use quire_markdown::tasks::{Format, marker_in_line};
use quire_plan::agenda::Agenda;
use quire_plan::periodic::Period;
use quire_text::{EditKind, Transaction};
use quire_vault::{NotePath, VaultEvent};

use crate::Command;
use crate::commands::Owner;
use crate::decor::{ActionId, Decoration, DecorationEvent, StatusItem, WidgetId};
use crate::document::{DocId, Document};
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// Work asked for while drawing (views and widgets see only `&Workspace`);
/// done on the next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Tick or untick the task at `offset` in `path` (its text guards against stale offsets).
    ToggleTask {
        path: NotePath,
        offset: usize,
        text: String,
    },
    /// A task note (`type: task`): set its status.
    ToggleTaskNote {
        path: NotePath,
        done: bool,
    },
    OpenAt {
        path: NotePath,
        offset: usize,
    },
    /// "+ New" in a view, with the template's questions answered.
    New {
        template: NotePath,
        /// 0.11: a title typed on a board ("+" in a column); else the template's `name:`.
        title: Option<String>,
        folder: Option<String>,
        answers: Vec<(String, String)>,
        preset: Vec<(String, Value)>,
    },
    /// Fill the `{{prompt:…}}` fields left in a note.
    FillTemplate {
        doc: DocId,
        answers: Vec<(String, String)>,
    },
    /// A cell edited or a card moved in a rendered Dataview/Tasks block or base.
    SetProperty {
        path: NotePath,
        key: String,
        value: Value,
    },
    ReviewNote(NotePath),
    /// Move a Kanban card (columns and positions as `obsidian::kanban` counts them).
    KanbanMove {
        doc: DocId,
        from: usize,
        card: usize,
        to: usize,
        index: usize,
    },
    /// Replace a Dataview/Tasks block with a quire-view.
    ConvertBlock {
        doc: DocId,
        block: usize,
    },
    OpenPeriod(Period, NaiveDate),
    /// Open an Obsidian `.base` as a note with quire-views.
    ConvertBase(NotePath),
    /// 0.11: start (or with `None` stop) the task timer.
    Timer {
        task: Option<quire_plan::timer::TaskRef>,
        pomodoro: bool,
    },
    /// 0.11: log a session that couldn't be logged to this task (`None`:
    /// its own task, again).
    LogUnlogged(Option<quire_plan::timer::TaskRef>),
    /// 0.11: throw an unlogged session away.
    DiscardUnlogged,
    /// 0.11: a task note's property set (`None` removes it), under the name the note uses.
    SetTaskProperty {
        path: NotePath,
        prop: quire_plan::task_note::Prop,
        value: Option<Value>,
    },
    /// 0.11: a board's setting changed (card order, swimlanes, a column colour).
    ViewSetting {
        target: BoardTarget,
        setting: BoardSetting,
    },
    /// 0.11: a field of an inline task line set (`None` removes it).
    SetTaskField {
        path: NotePath,
        offset: usize,
        text: String,
        field: quire_markdown::tasks::Field,
        value: Option<String>,
    },
    /// 0.11: a plain note in `folder` with these properties ("+" on a board without a template).
    NewNoteIn {
        folder: String,
        title: String,
        preset: Vec<(String, Value)>,
    },
    /// 0.11: a new task note (and open it?).
    NewTask(Box<quire_plan::task_note::NewTask>, bool),
}

/// Where a board's settings are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardTarget {
    /// A quire-view block in a note, found by its spec text.
    Block { note: NotePath, spec: String },
    /// View `view` of an Obsidian `.base` file.
    Base { path: NotePath, view: usize },
    /// 0.12: the agenda's own board. Its settings stay on this device
    /// ([`AGENDA_BOARD_FILE`]): it has no view in the vault to keep them in.
    Agenda,
}

/// The agenda board's settings on this device.
///
/// In the vault's state folder (`<config>/vaults/<key>/`): card order,
/// column colours and swimlanes, as the lines of a view
/// (`order.todo: [Tasks/A.md, Tasks/B.md]`). Card ids are note paths,
/// never task text.
pub const AGENDA_BOARD_FILE: &str = "agenda-board.json";

/// The agenda board's view, before this device's settings.
pub const AGENDA_BOARD_SPEC: &str =
    "from: tasks\nlayout: kanban\ngroup: status\nfields: [text, priority, due, time_spent, time_estimate, contexts]\n";

/// The agenda board's settings file.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AgendaBoardFile {
    #[serde(default)]
    format: u32,
    /// View lines this device added to [`AGENDA_BOARD_SPEC`].
    #[serde(default)]
    view: String,
}

/// A board setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardSetting {
    /// The hand-placed order of a column's cards (ids as `ViewRow::id`).
    Order { column: String, ids: Vec<String> },
    /// Swimlanes by a field, or none.
    Lanes(Option<String>),
    /// A column's colour, or none.
    Color { column: String, color: Option<String> },
}

/// The agenda, for (the notes' version, today).
type AgendaCache = RefCell<Option<(((u64, u64), NaiveDate), Rc<Agenda>)>>;

/// WS-E's state, kept in `ws.features.tasks`.
#[derive(Debug, Default)]
pub struct TasksState {
    ready: bool,
    pub reminders: crate::reminders::RemindersState,
    pub(crate) agenda: AgendaCache,
    /// 0.12: the agenda board's settings on this device, once read
    /// ([`AGENDA_BOARD_FILE`]); `agenda_board_file` overrides where (tests).
    agenda_board: RefCell<Option<String>>,
    pub agenda_board_file: Option<std::path::PathBuf>,
    /// Open recurring tasks per document at its last revision, to notice one being ticked.
    recurring: HashMap<DocId, (u64, Vec<String>)>,
    pending: RefCell<Vec<Action>>,
    /// New notes already offered their folder's template.
    pub(crate) checked_new: HashSet<DocId>,
    /// The refactor dialog opens on this (tag?, name), from the caret.
    pub refactor_start: Option<(bool, String)>,
    /// 0.11: the task timer and the vault's `[tasks]` prefs.
    pub timer: crate::task_timer::TimerState,
    /// 0.11: "New task…" from quick capture starts with this title.
    pub new_task_title: Option<String>,
}

// Decoration action and widget ids: the kind in the top byte.
const KIND_SHIFT: u32 = 56;
const W_COMPAT: u64 = 1;
const W_KANBAN: u64 = 2;
const W_FILL: u64 = 3;
const A_KANBAN: u64 = 3;
const A_CONVERT: u64 = 4;

const fn id(kind: u64, payload: u64) -> u64 {
    (kind << KIND_SHIFT) | (payload & ((1 << KIND_SHIFT) - 1))
}

/// A widget the desktop draws for WS-E under a line of a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWidget {
    /// The `n`th Dataview/Tasks block of the note: its results.
    Compat { doc: DocId, block: usize },
    /// A Kanban plugin board.
    Kanban { doc: DocId },
    /// Template fields left to fill in.
    Fill { doc: DocId },
}

impl PlanWidget {
    const fn encode(self) -> WidgetId {
        let (kind, doc, n) = match self {
            Self::Compat { doc, block } => (W_COMPAT, doc, block),
            Self::Kanban { doc } => (W_KANBAN, doc, 0),
            Self::Fill { doc } => (W_FILL, doc, 0),
        };
        WidgetId { owner: Owner::Plan, id: id(kind, ((n as u64 & 0xffff) << 32) | (doc.0 & 0xffff_ffff)) }
    }

    pub const fn decode(w: WidgetId) -> Option<Self> {
        let payload = w.id & ((1 << KIND_SHIFT) - 1);
        let doc = DocId(payload & 0xffff_ffff);
        let n = ((payload >> 32) & 0xffff) as usize;
        match w.id >> KIND_SHIFT {
            W_COMPAT => Some(Self::Compat { doc, block: n }),
            W_KANBAN => Some(Self::Kanban { doc }),
            W_FILL => Some(Self::Fill { doc }),
            _ => None,
        }
    }
}

/// The action id for moving a Kanban card (the desktop returns it from the board).
pub const fn kanban_move_action(from: usize, card: usize, to: usize, index: usize) -> ActionId {
    let p = ((from as u64 & 0xff) << 40) | ((card as u64 & 0xffff) << 24) | ((to as u64 & 0xff) << 16) | (index as u64 & 0xffff);
    ActionId { owner: Owner::Plan, id: id(A_KANBAN, p) }
}

/// The action id for converting the `n`th Dataview/Tasks block.
pub const fn convert_action(block: usize) -> ActionId {
    ActionId { owner: Owner::Plan, id: id(A_CONVERT, block as u64) }
}

impl Workspace {
    /// Ask for `a` to be done on the next tick (from drawing code).
    pub fn plan_request(&self, a: Action) {
        self.features.tasks.pending.borrow_mut().push(a);
    }

    /// Overdue, today, the next seven days and today's time blocks; cached until the index changes.
    pub fn agenda(&self) -> Rc<Agenda> {
        let today = Local::now().date_naive();
        // 0.12: with the notes of unlocked folders (memory only).
        let key = (self.notes_version(), today);
        if let Some((k, a)) = self.features.tasks.agenda.borrow().as_ref()
            && *k == key
        {
            return Rc::clone(a);
        }
        let daily = self.period_path(Period::Daily, today);
        let mut tasks: Vec<(String, quire_markdown::Task)> = Vec::new();
        self.with_notes(|n| {
            for r in n.records() {
                let p = r.path.as_str();
                tasks.extend(r.tasks.iter().map(|t| (p.to_string(), t.clone())));
                if let Some(t) = quire_plan::tasks::note_task(&r.properties, &r.title) {
                    tasks.push((p.to_string(), t));
                }
            }
        });
        let a = Rc::new(quire_plan::agenda::build(tasks.iter().map(|(p, t)| (p.as_str(), t)), today, Some(daily.as_str())));
        *self.features.tasks.agenda.borrow_mut() = Some((key, Rc::clone(&a)));
        a
    }

    /// The text of a note now: the open document's, else the file's.
    fn current_text(&self, path: &NotePath) -> Option<String> {
        if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
            return Some(d.text().to_string());
        }
        self.vault()?.read(path).ok()
    }

    /// Tick or untick a task. Completing a recurring task adds its next
    /// occurrence (above it, or below with `tasks.new_occurrence = "below"`).
    pub fn toggle_task(&mut self, path: &NotePath, offset: usize, text: &str) -> bool {
        let Some(src) = self.current_text(path) else { return false };
        let Some(line_start) = find_task_line(&src, offset, text) else {
            self.notify(NoticeLevel::Warn, format!("That task has moved in {}; open the note to change it", path.stem()));
            return false;
        };
        let line_end = src[line_start..].find('\n').map_or(src.len(), |i| line_start + i);
        let line = src[line_start..line_end].trim_end_matches('\r');
        let Some((marker, task)) = quire_plan::tasks::parse_line(line) else { return false };
        let today = Local::now().date_naive();
        let format = Format::from_setting(&self.settings.tasks.format);
        let replacement = if task.done {
            let undone = quire_markdown::tasks::set_done(line, marker, false);
            quire_markdown::tasks::set_field(&undone, marker, quire_markdown::tasks::Field::Done, None, format)
        } else {
            let Some(c) = quire_plan::tasks::complete(line, today, format) else { return false };
            if let Some(note) = &c.note {
                self.notify(NoticeLevel::Info, format!("Completed; no next occurrence: {note}"));
            }
            match c.next {
                Some(next) if self.settings.tasks.new_occurrence == "below" => format!("{}\n{next}", c.done),
                Some(next) => format!("{next}\n{}", c.done),
                None => c.done,
            }
        };
        let new_src = format!("{}{replacement}{}", &src[..line_start], &src[line_start + line.len()..]);
        let now = crate::history_ops::now_ms();
        match self.write_note_with_history(path, &src, &new_src, now) {
            Ok(_) => {
                self.forget_recurring(path);
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, e);
                false
            }
        }
    }

    /// Set one field of an inline task (a card's due date or priority),
    /// keeping the rest of its line.
    pub fn set_task_field(
        &mut self,
        path: &NotePath,
        offset: usize,
        text: &str,
        field: quire_markdown::tasks::Field,
        value: Option<&str>,
    ) -> bool {
        let Some(src) = self.current_text(path) else { return false };
        let Some(line_start) = find_task_line(&src, offset, text) else {
            self.notify(NoticeLevel::Warn, format!("That task has moved in {}; open the note to change it", path.stem()));
            return false;
        };
        let line_end = src[line_start..].find('\n').map_or(src.len(), |i| line_start + i);
        let line = src[line_start..line_end].trim_end_matches('\r');
        let Some(marker) = marker_in_line(line) else { return false };
        let format = Format::from_setting(&self.settings.tasks.format);
        let new_line = quire_markdown::tasks::set_field(line, marker, field, value, format);
        let new_src = format!("{}{new_line}{}", &src[..line_start], &src[line_start + line.len()..]);
        match self.write_note_with_history(path, &src, &new_src, crate::history_ops::now_ms()) {
            Ok(_) => true,
            Err(e) => {
                self.notify(NoticeLevel::Error, e);
                false
            }
        }
    }

    /// The open document's recurring-task snapshot is stale after we edit it ourselves.
    fn forget_recurring(&mut self, path: &NotePath) {
        if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
            let (id, rev, keys) = (d.id, d.revision(), open_recurring(&d.text()));
            self.features.tasks.recurring.insert(id, (rev, keys));
        }
    }

    /// "Convert task to note": the task on the caret's line becomes a note
    /// in the vault's task folder (`[tasks] folder`, `Tasks/`) and the line
    /// links to it. Dates, priority, recurrence, reminder, tags, time spent
    /// and the estimate carry over; the line keeps its indentation, bullet,
    /// checkbox character, a Day Planner time, the fields a note has no
    /// place for (`[client:: …]`, `🆔`, `⛔`) and its `^block-id`. One
    /// undoable edit to the line.
    ///
    /// A task in a locked folder becomes a note in that folder's task
    /// folder, sealed like any note there, named by date rather than by its
    /// words (0.11 review M7).
    pub fn convert_task_to_note(&mut self) -> bool {
        let Some(doc) = self.active_doc() else { return false };
        let doc_id = doc.id;
        let from = doc.path.clone();
        let (line_no, _) = doc.buffer().line_col(doc.selection().head);
        let line = doc.buffer().line(line_no);
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let Some((marker, task)) = quire_plan::tasks::parse_line(&line) else {
            self.notify(NoticeLevel::Info, "Put the caret on a task line (- [ ] …) to make it a note");
            return false;
        };
        let rest = quire_plan::task_note::LineRest::of(&line, marker);
        let prefs = self.task_prefs();
        let mut new = quire_plan::task_note::from_inline(&task);
        if let Some(s) = quire_plan::task_note::status_for_checkbox(rest.checkbox, &prefs) {
            new.status = Some(s);
        }
        if quire_vault::sanitize_title(&new.title).trim().is_empty() {
            self.notify(NoticeLevel::Info, "This task has no text to name a note by");
            return false;
        }
        if task.text.trim_start().starts_with("[[") && task.text.trim_end().ends_with("]]") {
            self.notify(NoticeLevel::Info, "This task already links to a note");
            return false;
        }
        let locked = from.as_ref().and_then(|p| self.locked_folder_of(p));
        let made = match &locked {
            Some(f) if !self.is_unlocked(&f.path) => {
                self.notify(NoticeLevel::Info, format!("Unlock {}/ first", f.path));
                return false;
            }
            Some(f) => {
                let folder = format!("{}/{}", f.path, prefs.folder.trim_matches('/'));
                let name = format!("Task {}", Local::now().format("%Y-%m-%d"));
                self.new_task_in(&new, folder.trim_end_matches('/'), Some(&name), false)
            }
            None => self.new_task(&new, false),
        };
        let Some(path) = made else { return false };
        let link = if locked.is_some() { format!("{}|{}", path.stem(), new.title) } else { path.stem().to_string() };
        let new_line = rest.link_line(&line[..marker], &link);
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == doc_id) else { return false };
        let start = doc.buffer().line_to_char(line_no);
        let end = start + line.chars().count();
        let _ = doc.apply(Transaction::replace(start, end, new_line), EditKind::Other);
        true
    }

    /// "Convert task note back to a task line": on a line `- [ ] [[Note]]`
    /// linking to a task note, the line becomes the task with its fields
    /// (time spent as `[time:: …]`), keeping what the line had besides the
    /// link. The note goes to `.trash` only when the line carries all of
    /// it: nothing in its body but its title, and no property a line can't
    /// hold (contexts, projects, other statuses, session notes, comments,
    /// any other key). Otherwise it stays, and the notice says why.
    pub fn convert_note_to_task(&mut self) -> bool {
        let Some(doc) = self.active_doc() else { return false };
        let doc_id = doc.id;
        let from = doc.path.clone();
        let (line_no, _) = doc.buffer().line_col(doc.selection().head);
        let line = doc.buffer().line(line_no);
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let target = quire_plan::tasks::parse_line(&line).and_then(|(m, t)| {
            let inner = t.text.trim().strip_prefix("[[")?.strip_suffix("]]")?;
            let name = inner.split('|').next()?.split('#').next()?.trim().to_string();
            Some((m, name))
        });
        let Some((marker, name)) = target else {
            self.notify(NoticeLevel::Info, "Put the caret on a task line that links to a task note (- [ ] [[Note]])");
            return false;
        };
        let found = self.index().read().resolve(&name, from.as_ref());
        let Some(path) = found.or_else(|| self.vault()?.resolve(&name, from.as_ref())) else {
            self.notify(NoticeLevel::Info, format!("There's no note “{name}”"));
            return false;
        };
        // A locked note's task goes only into a note of the same locked folder.
        let (note_locked, line_locked) = (self.locked_folder_of(&path), from.as_ref().and_then(|p| self.locked_folder_of(p)));
        if note_locked.as_ref().is_some_and(|n| line_locked.as_ref().is_none_or(|l| l.path != n.path)) {
            self.notify(NoticeLevel::Info, format!("“{}” is in a locked folder; its task can only go into a note there", path.stem()));
            return false;
        }
        let Some(note_text) = self.note_text_now_pub(&path) else { return false };
        let fm = quire_markdown::frontmatter::parse(&note_text);
        let props = fm.as_ref().map(|f| f.fields.clone()).unwrap_or_default();
        let prefs = self.task_prefs();
        let title = match props.iter().find(|(k, _)| k.eq_ignore_ascii_case("title")).and_then(|(_, v)| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => self.index().read().get(&path).map_or_else(|| path.stem().to_string(), |r| r.title.clone()),
        };
        // The heading is the title for a note named by date (a locked one).
        let body_start = fm.as_ref().map_or(0, |f| f.range.end);
        let body = &note_text[body_start..];
        let heading = body.lines().map(str::trim).find(|l| !l.is_empty()).and_then(|l| l.strip_prefix("# ")).map(str::trim);
        let title = heading.filter(|_| note_locked.is_some()).map_or(title, str::to_string);
        let Some(note) = quire_plan::task_note::read(&props, &title, &prefs, *Local::now().offset()) else {
            self.notify(NoticeLevel::Info, format!("“{}” isn't a task note", path.stem()));
            return false;
        };
        let format = Format::from_setting(&self.settings.tasks.format);
        let rest = quire_plan::task_note::LineRest::of(&line, marker);
        let new_line = quire_plan::task_note::to_inline(&line[..marker], &note, crate::task_timer::now(), format);
        let new_line = rest.restore(&new_line, marker, &note.status, &prefs);
        // Only a note the line carries all of goes: front matter whose
        // keys the line holds, a heading with its title, nothing else.
        let rest_lines: Vec<&str> = body.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let body_empty = rest_lines.is_empty()
            || (rest_lines.len() == 1 && rest_lines[0].trim_start_matches('#').trim() == title.trim() && rest_lines[0].starts_with("# "));
        let raw_fm = &note_text[..body_start];
        let left = quire_plan::task_note::left_on_note(&props, raw_fm, &prefs);
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == doc_id) else { return false };
        let start = doc.buffer().line_to_char(line_no);
        let end = start + line.chars().count();
        let _ = doc.apply(Transaction::replace(start, end, new_line), EditKind::Other);
        if body_empty && left.is_empty() && !self.docs.iter().any(|d| d.path.as_ref() == Some(&path)) {
            if let Some(v) = self.vault().cloned()
                && v.trash(&path).is_ok()
            {
                self.index.remove(path.clone());
                self.notify(NoticeLevel::Info, format!("Moved “{}” to .trash", path.stem()));
            }
        } else if !left.is_empty() {
            self.notify(NoticeLevel::Info, format!("Kept “{}”: a task line can't hold its {}", path.stem(), left.join(", ")));
        } else {
            self.notify(NoticeLevel::Info, format!("Kept “{}”: it has more than its properties", path.stem()));
        }
        true
    }

    /// "Convert to quire-view": the Dataview/Tasks block at the caret (or
    /// block `which`) becomes a quire-view block; what doesn't convert is
    /// noted in it. One undoable edit.
    pub fn convert_block(&mut self, doc: DocId, which: Option<usize>) -> bool {
        let Some(d) = self.doc(doc) else { return false };
        let text = d.text();
        let blocks = views::find_compat_blocks(&text);
        let caret_line = d.buffer().line_col(d.selection().head).0;
        let Some(b) = (match which {
            Some(i) => blocks.get(i),
            None => blocks.iter().find(|b| (b.open_line..=b.close_line).contains(&caret_line)),
        }) else {
            self.notify(NoticeLevel::Info, "Put the caret in a ```dataview or ```tasks block to convert it");
            return false;
        };
        if b.lang == "dataviewjs" {
            self.notify(NoticeLevel::Info, "DataviewJS runs code, which Quire never does; it can't be converted");
            return false;
        }
        let t = b.translate();
        let mut block = String::from("```quire-view\n");
        let source = if b.lang == "tasks" { "Tasks" } else { "Dataview" };
        let _ = writeln!(block, "# Converted from a {source} query");
        for u in &t.unsupported {
            let _ = writeln!(block, "# Not converted: {u}");
        }
        block.push_str(&t.spec);
        block.push_str("```");
        let (open, close) = (b.open_line, b.close_line);
        let Some(i) = self.docs.iter().position(|x| x.id == doc) else { return false };
        let buf = self.docs[i].buffer();
        let start = buf.line_to_char(open);
        let end_line_text = buf.line(close);
        let end = buf.line_to_char(close) + end_line_text.trim_end_matches(['\n', '\r']).chars().count();
        let _ = self.docs[i].apply(Transaction::replace(start, end, block), EditKind::Other);
        self.features.decor_generation += 1;
        true
    }
}

/// The start of the line holding the task at `offset` whose text is `text`;
/// searches the note if the offset went stale.
fn find_task_line(src: &str, offset: usize, text: &str) -> Option<usize> {
    let check = |start: usize| -> bool {
        let end = src[start..].find('\n').map_or(src.len(), |i| start + i);
        quire_plan::tasks::parse_line(&src[start..end]).is_some_and(|(_, t)| text.is_empty() || t.text == text)
    };
    if offset <= src.len() && src.is_char_boundary(offset.min(src.len())) {
        let start = src[..offset.min(src.len())].rfind('\n').map_or(0, |i| i + 1);
        if check(start) {
            return Some(start);
        }
    }
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        if marker_in_line(line).is_some() && check(at) {
            return Some(at);
        }
        at += line.len();
    }
    None
}

/// Open recurring task lines, as keys (the line without its checkbox state).
fn open_recurring(text: &str) -> Vec<String> {
    if !text.contains('🔁') && !text.contains("repeat::") {
        return Vec::new();
    }
    text.lines()
        .filter_map(|l| {
            let (_, t) = quire_plan::tasks::parse_line(l)?;
            (!t.done && t.recurrence.is_some()).then(|| l.trim().to_string())
        })
        .collect()
}

/// A recurring task just ticked in the editor gets its done date and next
/// occurrence, as its own undoable edit.
fn complete_ticked(ws: &mut Workspace) -> bool {
    let Some(doc) = ws.active_doc() else { return false };
    let (id, rev) = (doc.id, doc.revision());
    if ws.features.tasks.recurring.get(&id).is_some_and(|(r, _)| *r == rev) {
        return false;
    }
    let text = doc.text();
    let before = ws.features.tasks.recurring.get(&id).map(|(_, k)| k.clone());
    let now_open = open_recurring(&text);
    ws.features.tasks.recurring.insert(id, (rev, now_open.clone()));
    let Some(mut before) = before else { return false };
    for k in &now_open {
        if let Some(i) = before.iter().position(|b| b == k) {
            before.swap_remove(i);
        }
    }
    if before.is_empty() {
        return false;
    }
    // Lines that were open and are now ticked, without a done date yet.
    let format = Format::from_setting(&ws.settings.tasks.format);
    let today = Local::now().date_naive();
    let below = ws.settings.tasks.new_occurrence == "below";
    let mut edits: Vec<(usize, String, String)> = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let Some((marker, t)) = quire_plan::tasks::parse_line(l) else { continue };
        if !t.done || t.done_on.is_some() || t.recurrence.is_none() {
            continue;
        }
        let key = quire_markdown::tasks::set_done(l, marker, false).trim().to_string();
        let Some(i) = before.iter().position(|b| *b == key) else { continue };
        before.swap_remove(i);
        let open_line = quire_markdown::tasks::set_done(l, marker, false);
        if let Some(c) = quire_plan::tasks::complete(&open_line, today, format) {
            let new = match c.next {
                Some(next) if below => format!("{}\n{next}", c.done),
                Some(next) => format!("{next}\n{}", c.done),
                None => c.done,
            };
            edits.push((n, l.to_string(), new));
        }
    }
    if edits.is_empty() {
        return false;
    }
    let Some(i) = ws.docs.iter().position(|d| d.id == id) else { return false };
    let doc = &mut ws.docs[i];
    let changes: Vec<quire_text::Change> = edits
        .iter()
        .map(|(n, old, new)| {
            let start = doc.buffer().line_to_char(*n);
            quire_text::Change::new(start, start + old.chars().count(), new.clone())
        })
        .collect();
    let sel = doc.selection();
    if let Ok(tx) = Transaction::new(changes) {
        let tx = tx.with_selection(sel);
        let _ = doc.apply(tx, EditKind::Other);
    }
    let (rev, keys) = (doc.revision(), open_recurring(&doc.text()));
    ws.features.tasks.recurring.insert(id, (rev, keys));
    true
}

fn run_actions(ws: &mut Workspace) -> bool {
    let actions: Vec<Action> = std::mem::take(&mut *ws.features.tasks.pending.borrow_mut());
    if actions.is_empty() {
        return false;
    }
    for a in actions {
        match a {
            Action::ToggleTask { path, offset, text } => {
                ws.toggle_task(&path, offset, &text);
            }
            Action::ToggleTaskNote { path, done } => {
                // 0.11: the vault's statuses; a recurring note moves to its next date.
                let is_done = ws.task_notes().iter().find(|(p, _)| *p == path).is_some_and(|(_, n)| n.task.done);
                if is_done != done {
                    ws.toggle_task_note(&path);
                }
            }
            Action::Timer { task, pomodoro } => match task {
                Some(t) => {
                    ws.start_timer(t, pomodoro);
                }
                None => {
                    ws.stop_timer();
                }
            },
            Action::LogUnlogged(task) => {
                ws.log_unlogged_to(task);
            }
            Action::DiscardUnlogged => {
                ws.discard_unlogged();
            }
            Action::SetTaskProperty { path, prop, value } => {
                ws.set_task_property(&path, prop, value);
            }
            Action::ViewSetting { target, setting } => set_board(ws, &target, &setting),
            Action::SetTaskField { path, offset, text, field, value } => {
                ws.set_task_field(&path, offset, &text, field, value.as_deref());
            }
            Action::NewNoteIn { folder, title, preset } => new_note_in(ws, &folder, &title, &preset),
            Action::NewTask(t, open) => {
                ws.new_task(&t, open);
            }
            Action::OpenAt { path, offset } => {
                if ws.open_note(&path).is_some() {
                    ws.jump_to_byte(offset);
                }
            }
            Action::New { template, title, folder, answers, preset } => {
                if let Some(Err(why)) = folder.as_deref().map(safe_new_note_folder) {
                    ws.notify(NoticeLevel::Warn, why);
                    continue;
                }
                let answers: HashMap<String, String> = answers.into_iter().collect();
                ws.new_note_from_template_titled(&template, title.as_deref(), folder.as_deref(), &answers, &preset);
            }
            Action::FillTemplate { doc, answers } => {
                let answers: HashMap<String, String> = answers.into_iter().collect();
                ws.fill_template_fields(doc, &answers);
            }
            Action::SetProperty { path, key, value } => {
                if !ws.set_note_property(&path, &key, &value) {
                    ws.notify(NoticeLevel::Warn, format!("Couldn't change {key} in “{}”", path.stem()));
                }
            }
            Action::ReviewNote(path) => {
                if ws.srs_start(Some(&path)) {
                    // The review dialog, from a click in a note (no command ran).
                    ws.request_ui(UiRequest::Feature(Command::StartReview));
                } else {
                    ws.notify(NoticeLevel::Info, format!("No flashcards are due in {}", path.stem()));
                }
            }
            Action::KanbanMove { doc, from, card, to, index } => kanban_move(ws, doc, from, card, to, index),
            Action::ConvertBlock { doc, block } => {
                ws.convert_block(doc, Some(block));
            }
            Action::OpenPeriod(p, d) => {
                ws.open_period(p, d);
            }
            Action::ConvertBase(path) => convert_base(ws, &path),
        }
    }
    true
}

impl Workspace {
    fn agenda_board_path(&self) -> Option<std::path::PathBuf> {
        if let Some(p) = &self.features.tasks.agenda_board_file {
            return Some(p.clone());
        }
        if cfg!(test) {
            return None;
        }
        Some(quire_agent::approval::default_state_dir(self.vault_root()?)?.join(AGENDA_BOARD_FILE))
    }

    /// The agenda board's view: [`AGENDA_BOARD_SPEC`] and this device's
    /// settings (read once a vault).
    pub fn agenda_board_spec(&self) -> String {
        let mut cached = self.features.tasks.agenda_board.borrow_mut();
        let extra = cached.get_or_insert_with(|| {
            let read = self.agenda_board_path().and_then(|p| std::fs::read_to_string(p).ok());
            read.and_then(|t| serde_json::from_str::<AgendaBoardFile>(&t).ok()).map(|f| f.view).unwrap_or_default()
        });
        format!("{AGENDA_BOARD_SPEC}{extra}")
    }

    /// Set one of the agenda board's view lines on this device.
    fn set_agenda_board(&mut self, key: &str, value: Option<&str>) {
        let spec = self.agenda_board_spec();
        let src = format!("```quire-view\n{spec}```\n");
        let Some(b) = views::find_blocks(&src).into_iter().next() else { return };
        let Some(out) = views::set_block_key(&src, &b, key, value) else { return };
        let body = out.strip_prefix("```quire-view\n").and_then(|o| o.strip_suffix("```\n")).unwrap_or_default();
        let extra = body.strip_prefix(AGENDA_BOARD_SPEC).unwrap_or(body).to_string();
        if let Some(p) = self.agenda_board_path() {
            let file = AgendaBoardFile { format: 1, view: extra.clone() };
            let saved = p.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
                let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
                crate::atomic::write(&p, json.as_bytes(), false)
            });
            if let Err(e) = saved {
                self.notify(NoticeLevel::Warn, format!("Couldn't keep the board's order: {e}"));
            }
        }
        *self.features.tasks.agenda_board.borrow_mut() = Some(extra);
        self.features.decor_generation += 1;
    }
}

/// Write a board setting: one line of a quire-view block, or Quire's keys
/// in a `.base` view (`cardOrder`, `swimlaneBy`, `columnColors`); nothing
/// else in the file changes, and no note is rewritten. The agenda's board
/// keeps them on this device.
fn set_board(ws: &mut Workspace, target: &BoardTarget, setting: &BoardSetting) {
    let (key, value) = match setting {
        BoardSetting::Order { column, ids } => {
            (format!("order.{}", views::column_key(column)), (!ids.is_empty()).then(|| views::format_id_list(ids)))
        }
        BoardSetting::Lanes(l) => ("swimlanes".to_string(), l.clone()),
        BoardSetting::Color { column, color } => (format!("color.{}", views::column_key(column)), color.clone()),
    };
    if *target == BoardTarget::Agenda {
        ws.set_agenda_board(&key, value.as_deref());
        return;
    }
    match target {
        BoardTarget::Agenda => {}
        BoardTarget::Block { note, spec } => {
            let Some(src) = ws.current_text(note) else { return };
            let Some(b) = views::find_blocks(&src).into_iter().find(|b| b.spec == *spec) else {
                ws.notify(NoticeLevel::Warn, "The view changed; try again");
                return;
            };
            let Some(out) = views::set_block_key(&src, &b, &key, value.as_deref()) else { return };
            if out != src
                && let Err(e) = ws.write_note_with_history(note, &src, &out, crate::history_ops::now_ms())
            {
                ws.notify(NoticeLevel::Error, e);
            }
        }
        BoardTarget::Base { path, view } => {
            use quire_import::obsidian::base::{self as qb, YamlValue};
            let Some(vault) = ws.vault().cloned() else { return };
            let Ok(text) = vault.read(path) else { return };
            let Ok(base) = qb::parse(&text) else { return };
            let Some(spec) = base.views.get(*view).and_then(|v| views::parse(&v.spec).ok()) else { return };
            let out = match setting {
                BoardSetting::Order { column, ids } => {
                    let mut order = spec.order;
                    match order.iter_mut().find(|(c, _)| c.eq_ignore_ascii_case(column)) {
                        Some((_, v)) => v.clone_from(ids),
                        None => order.push((column.clone(), ids.clone())),
                    }
                    order.retain(|(_, v)| !v.is_empty());
                    let value = (!order.is_empty()).then(|| qb::card_order_yaml(&order));
                    qb::set_view_key(&text, *view, "cardOrder", value.as_ref())
                }
                BoardSetting::Lanes(l) => {
                    let value = l.as_ref().map(|l| YamlValue::Inline(quire_markdown::frontmatter::yaml_scalar(l)));
                    qb::set_view_key(&text, *view, "swimlaneBy", value.as_ref())
                }
                BoardSetting::Color { column, color } => {
                    let mut colors = spec.colors;
                    colors.retain(|(c, _)| !c.eq_ignore_ascii_case(column));
                    if let Some(c) = color {
                        colors.push((column.clone(), c.clone()));
                    }
                    let value = (!colors.is_empty()).then(|| qb::colors_yaml(&colors));
                    qb::set_view_key(&text, *view, "columnColors", value.as_ref())
                }
            };
            let Some(out) = out else {
                ws.notify(NoticeLevel::Warn, format!("{} keeps its views in a form Quire can't edit", path.stem()));
                return;
            };
            if out != text {
                let now = crate::history_ops::now_ms();
                let _ = vault.snapshot(path, &text, now - 1, true);
                // Only over the text it was worked out from (0.11 review L8).
                match vault.write_if(path, Some(&text), &out) {
                    Ok(true) => {
                        let _ = vault.snapshot(path, &out, now, true);
                        ws.features.decor_generation += 1;
                    }
                    Ok(false) => ws.notify(NoticeLevel::Warn, format!("{} changed just now; try again", path.stem())),
                    Err(e) => ws.notify(NoticeLevel::Error, format!("Couldn't save {}: {e}", path.stem())),
                }
            }
        }
    }
}

/// Whether a view's folder is one new notes may go to: inside the vault,
/// with no hidden segment (`.quire/`, `.trash/`, `.git/`, `x/.drafts/`).
pub(crate) fn safe_new_note_folder(folder: &str) -> Result<(), String> {
    let f = folder.replace('\\', "/");
    let bad = f.starts_with('/')
        || f.contains(':')
        || f.split('/').any(|seg| seg == ".." || seg.starts_with('.') || seg.chars().any(char::is_control));
    if bad { Err(format!("New notes can't go in “{folder}”: pick a folder inside the vault that isn't hidden")) } else { Ok(()) }
}

/// A note with `preset` front matter and its title as a heading, not opened.
fn new_note_in(ws: &mut Workspace, folder: &str, title: &str, preset: &[(String, Value)]) {
    let Some(vault) = ws.vault().cloned() else { return };
    // The folder comes from a view, which can come from a shared file: never
    // into `.quire/`, another hidden folder, or out of the vault (0.11
    // security review, board "+").
    if let Err(why) = safe_new_note_folder(folder) {
        ws.notify(NoticeLevel::Warn, why);
        return;
    }
    let Ok(path) = vault.unique_path(folder, title) else { return };
    let mut text = format!("# {}\n", path.stem());
    for (k, v) in preset {
        text = quire_markdown::frontmatter::set_field(&text, k, v).apply(&text);
    }
    match vault.write_if(&path, None, &text) {
        Ok(true) => {
            ws.index.upsert(path.clone(), text);
            ws.notify(NoticeLevel::Info, format!("Made {}", path.as_str()));
        }
        Ok(false) => ws.notify(NoticeLevel::Warn, format!("{} already exists", path.as_str())),
        Err(e) => ws.notify(NoticeLevel::Error, format!("Couldn't create {}: {e}", path.stem())),
    }
}

fn kanban_move(ws: &mut Workspace, doc: DocId, from: usize, card: usize, to: usize, index: usize) {
    let Some(i) = ws.docs.iter().position(|d| d.id == doc) else { return };
    let text = ws.docs[i].text().to_string();
    if let Some(out) = quire_import::obsidian::kanban::move_card(&text, from, card, to, index)
        && out != text
    {
        ws.docs[i].replace_minimally(&out);
        ws.features.decor_generation += 1;
    }
}

/// An Obsidian base as a new note with one quire-view per view.
fn convert_base(ws: &mut Workspace, path: &NotePath) {
    let Some(text) = ws.vault().and_then(|v| v.read(path).ok()) else { return };
    match quire_import::obsidian::base::parse(&text) {
        Ok(base) => {
            let title = path.stem().to_string();
            let note = quire_import::obsidian::base::to_note(&base, &title);
            let Some(vault) = ws.vault().cloned() else { return };
            let Ok(target) = vault.unique_path(path.folder(), &title) else { return };
            if ws.create_note_at(&target, &note, Some(0)).is_some() {
                ws.notify(NoticeLevel::Info, format!("Made {} from the base", target.as_str()));
            }
        }
        Err(e) => ws.notify(NoticeLevel::Error, e),
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let _ = now;
    if ws.vault().is_none() {
        return false;
    }
    let mut changed = !ws.features.tasks.ready;
    if changed {
        ws.features.tasks.ready = true;
        crate::periodic::load(ws);
    }
    if !ws.index.status().busy && ws.index.version() > 0 {
        crate::srs_state::ensure_loaded(ws);
        crate::srs_state::track_edits(ws);
    }
    changed |= crate::srs_state::poll_load(ws);
    changed |= crate::reminders::tick(ws);
    changed |= ws.timer_tick();
    changed |= ws.features.meta.poll();
    changed |= run_actions(ws);
    changed |= complete_ticked(ws);
    changed |= crate::templates_ui_state::fill_new_notes(ws);
    changed
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let now = Local::now().naive_local();
    let mut due = ws.features.tasks.reminders.next_due(now).map(|t| (t - now).to_std().unwrap_or_default());
    if ws.features.meta.busy() || !ws.features.tasks.pending.borrow().is_empty() {
        due = Some(due.map_or(Duration::from_millis(50), |d| d.min(Duration::from_millis(50))));
    }
    // 0.11: a running timer's clock ticks every second.
    if ws.timer().is_some() {
        let next = Duration::from_millis(1000 - u64::from(Local::now().timestamp_subsec_millis().min(999)));
        due = Some(due.map_or(next, |d| d.min(next)));
    }
    due.map(|d| Instant::now() + d)
}

/// Whether a Plan-owned command is ready to offer. All of WS-E's are; the
/// ones that need context say so when run without it.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    let _ = ws;
    matches!(cmd.owner(), Owner::Plan)
}

/// The tag or property name at the caret (`#tag` → tag), for the refactor dialog.
fn word_at_caret(ws: &Workspace) -> Option<(bool, String)> {
    let doc = ws.active_doc()?;
    let (line_no, col) = doc.buffer().line_col(doc.selection().head);
    let line = doc.buffer().line(line_no);
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '#' | '.');
    let (mut a, mut b) = (col.min(chars.len()), col.min(chars.len()));
    while a > 0 && is_word(chars[a - 1]) {
        a -= 1;
    }
    while b < chars.len() && is_word(chars[b]) {
        b += 1;
    }
    let w: String = chars[a..b].iter().collect();
    let w = w.trim_matches('.');
    if let Some(t) = w.strip_prefix('#').filter(|t| !t.is_empty()) {
        return Some((true, t.to_string()));
    }
    // `key:` at the start of a front-matter line.
    let t = line.trim_start();
    let fm_end = doc.extracted().front_matter.as_ref().map_or(0, |f| doc.buffer().char_to_line(doc.byte_to_char(f.range.end)));
    if line_no < fm_end
        && let Some((k, _)) = t.split_once(':')
    {
        return Some((false, k.trim().to_string()));
    }
    None
}

/// A Plan-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    use Command as C;
    let today = Local::now().date_naive();
    match cmd {
        C::ShowAgenda | C::ShowCalendar | C::NewTask | C::TimeReport => return Some(UiRequest::Feature(cmd)),
        C::OpenWeeklyNote | C::OpenMonthlyNote | C::OpenYearlyNote => {
            let p = match cmd {
                C::OpenWeeklyNote => Period::Weekly,
                C::OpenMonthlyNote => Period::Monthly,
                _ => Period::Yearly,
            };
            ws.open_period(p, today);
        }
        C::NextPeriod | C::PrevPeriod => {
            if !ws.step_period(if cmd == C::NextPeriod { 1 } else { -1 }) {
                ws.notify(NoticeLevel::Info, "Open a daily, weekly, monthly or yearly note first");
            }
        }
        C::StartReview | C::ReviewThisNote => {
            crate::srs_state::load(ws);
            let note = if cmd == C::ReviewThisNote { ws.active_doc().and_then(|d| d.path.clone()) } else { None };
            if cmd == C::ReviewThisNote && note.is_none() {
                ws.notify(NoticeLevel::Info, "Open a note with flashcards first");
                return None;
            }
            if ws.srs_start(note.as_ref()) {
                return Some(UiRequest::Feature(cmd));
            }
            let (due, new) = ws.srs_due_counts();
            let msg = if ws.srs_cards().is_empty() {
                "No flashcards yet. Write “Question :: Answer” in a note tagged #flashcards (or with srs: true).".to_string()
            } else if cmd == C::ReviewThisNote {
                "Nothing to review in this note today.".to_string()
            } else {
                format!("Nothing to review today ({due} due, {new} new).")
            };
            ws.notify(NoticeLevel::Info, msg);
        }
        C::AddCardIds => {
            let n = ws.srs_add_ids();
            ws.notify(
                NoticeLevel::Info,
                if n == 0 {
                    "Every flashcard in this note already has an id".to_string()
                } else {
                    format!("Added {n} card id{}", if n == 1 { "" } else { "s" })
                },
            );
        }
        C::RenameTag | C::RenameProperty => {
            let at = word_at_caret(ws);
            ws.features.tasks.refactor_start = match (cmd, at) {
                (C::RenameTag, Some((true, t))) => Some((true, t)),
                (C::RenameProperty, Some((false, k))) => Some((false, k)),
                (C::RenameTag, _) => Some((true, String::new())),
                _ => Some((false, String::new())),
            };
            ws.features.meta.preview = None;
            return Some(UiRequest::Feature(cmd));
        }
        C::ConvertTaskToNote => {
            ws.convert_task_to_note();
        }
        C::ConvertNoteToTask => {
            ws.convert_note_to_task();
        }
        C::StartStopTimer => {
            ws.toggle_timer_at_caret(false);
        }
        C::StartPomodoro => {
            ws.toggle_timer_at_caret(true);
        }
        C::StopTimer => {
            ws.timer_load();
            if ws.timer().is_none() {
                ws.notify(NoticeLevel::Info, "No timer is running");
            } else {
                ws.stop_timer();
            }
        }
        C::ConvertToView => {
            if let Some(id) = ws.active_doc().map(|d| d.id) {
                ws.convert_block(id, None);
            } else {
                ws.notify(NoticeLevel::Info, "Open a note with a ```dataview or ```tasks block first");
            }
        }
        _ => ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…'))),
    }
    None
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    if doc.large {
        return;
    }
    let text = doc.text();
    // Dataview and Tasks blocks: rendered under the block.
    if text.contains("```") || text.contains("~~~") {
        for (i, b) in views::find_compat_blocks(&text).iter().enumerate() {
            out.push(Decoration::widget(b.close_line, PlanWidget::Compat { doc: doc.id, block: i }.encode()));
        }
    }
    let fm = doc.extracted().front_matter.clone();
    // A Kanban plugin board: the board under the front matter.
    if let Some(f) = fm.as_ref().filter(|f| f.get("kanban-plugin").is_some()) {
        let last = doc.buffer().char_to_line(doc.byte_to_char(f.range.end)).saturating_sub(1);
        out.push(Decoration::widget(last, PlanWidget::Kanban { doc: doc.id }.encode()));
    }
    // Template questions left in the note: a small form under the first.
    if text.contains("{{")
        && let Some((r, _)) = quire_vault::templates::pending_prompts(&text).first()
    {
        let line = doc.buffer().char_to_line(doc.byte_to_char(r.start));
        out.push(Decoration::widget(line, PlanWidget::Fill { doc: doc.id }.encode()));
    }
    let _ = ws;
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let payload = ev.action.id & ((1 << KIND_SHIFT) - 1);
    match ev.action.id >> KIND_SHIFT {
        A_KANBAN => {
            let (from, card, to, index) = ((payload >> 40) & 0xff, (payload >> 24) & 0xffff, (payload >> 16) & 0xff, payload & 0xffff);
            kanban_move(ws, ev.doc, from as usize, card as usize, to as usize, index as usize);
        }
        A_CONVERT => {
            ws.convert_block(ev.doc, Some(payload as usize));
        }
        _ => {}
    }
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    // Templates aren't indexed; the dialog reads them fresh. Nothing else
    // here keeps file state beyond the index.
    let _ = (ws, ev);
}

/// A vault was opened: state starts fresh (start jobs here: `ws.jobs.add(...)`).
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.tasks = TasksState::default();
    ws.features.periodic = crate::periodic::PeriodicState::default();
    ws.features.srs = crate::srs_state::SrsState::default();
    ws.features.meta = crate::meta_refactor::MetaState::default();
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    if ws.vault().is_none() {
        return;
    }
    // 0.11: the running timer, and a click stops it.
    if let (Some(r), Some((secs, phase))) = (ws.timer(), ws.timer_clock()) {
        let clock = quire_plan::time::format_clock(secs);
        let (text, tooltip) = match phase {
            _ if r.unlogged.is_some() => (
                format!("⚠ {clock} not logged"),
                format!(
                    "A session on “{}” couldn't be logged — click to try again, or put the caret on a task and press Start/stop timer to log it there",
                    r.task.title
                ),
            ),
            Some(p) => (format!("🍅 {clock} {}", r.task.title), format!("{} · {} — click to stop", p.label(), r.task.title)),
            None => (format!("⏱ {clock} {}", r.task.title), format!("Timing “{}” — click to stop and log it", r.task.title)),
        };
        out.push(StatusItem { owner: Owner::Plan, text, tooltip, command: Some(Command::StopTimer), warn: false });
    }
    let reminders = ws.features.tasks.reminders.active().len();
    if reminders > 0 {
        out.push(StatusItem {
            owner: Owner::Plan,
            text: format!("{reminders} reminder{}", if reminders == 1 { "" } else { "s" }),
            tooltip: format!("{reminders} reminder{} — open the Agenda", if reminders == 1 { "" } else { "s" }),
            command: Some(Command::ShowAgenda),
            warn: true,
        });
    }
    if ws.features.srs.is_loaded() {
        let (due, new) = ws.srs_due_counts();
        if due + new > 0 {
            out.push(StatusItem {
                owner: Owner::Plan,
                text: format!(
                    "{} card{} to review",
                    due + new.min(crate::srs_state::NEW_PER_SESSION),
                    if due + new == 1 { "" } else { "s" }
                ),
                tooltip: format!("{due} due, {new} new — review (Ctrl+Alt+R)"),
                command: Some(Command::StartReview),
                warn: false,
            });
        }
    }
}

/// Compat blocks of a document, for the desktop's widget painter.
pub fn compat_blocks(doc: &Document) -> Vec<CompatBlock> {
    views::find_compat_blocks(&doc.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(files));
        w.index().flush();
        w
    }

    #[test]
    fn ticking_a_recurring_task_in_the_editor_adds_the_next() {
        let mut w = ws(&[("T.md", "# T\n- [ ] Rent 🔁 every month 📅 2026-12-01\n")]);
        let id = w.open_note(&"T.md".into()).unwrap();
        assert!(!complete_ticked(&mut w), "first look only remembers");
        let doc = w.active_doc_mut().unwrap();
        let at = doc.buffer().line_to_char(1) + 3;
        doc.apply(Transaction::replace(at, at + 1, "x"), EditKind::Other).unwrap();
        assert!(complete_ticked(&mut w));
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(
            &*w.doc(id).unwrap().text(),
            format!("# T\n- [ ] Rent 🔁 every month 📅 2027-01-01\n- [x] Rent 🔁 every month 📅 2026-12-01 ✅ {today}\n")
        );
        assert!(!complete_ticked(&mut w), "once");
        // Undo takes the completion back first.
        assert!(w.active_doc_mut().unwrap().undo());
        assert_eq!(&*w.doc(id).unwrap().text(), "# T\n- [x] Rent 🔁 every month 📅 2026-12-01\n");
    }

    #[test]
    fn toggles_tasks_in_closed_notes() {
        let mut w = ws(&[("T.md", "- [ ] Call 📅 2026-11-02\n- [ ] Water 🔁 every day 📅 2026-11-02\n")]);
        assert!(w.toggle_task(&"T.md".into(), 2, "Call"));
        let today = Local::now().format("%Y-%m-%d").to_string();
        let v = w.vault().unwrap().clone();
        assert_eq!(
            v.read(&"T.md".into()).unwrap(),
            format!("- [x] Call 📅 2026-11-02 ✅ {today}\n- [ ] Water 🔁 every day 📅 2026-11-02\n")
        );
        // A stale offset still finds the task by its text.
        assert!(w.toggle_task(&"T.md".into(), 2, "Water"));
        assert!(v.read(&"T.md".into()).unwrap().contains("- [ ] Water 🔁 every day 📅 2026-11-03\n- [x] Water"));
        assert!(w.toggle_task(&"T.md".into(), 2, "Call"), "untick");
        assert!(v.read(&"T.md".into()).unwrap().starts_with("- [ ] Call 📅 2026-11-02\n"));
        assert!(!v.history(&"T.md".into()).is_empty() || cfg!(test), "history is kept");
    }

    #[test]
    fn converts_tasks_and_blocks() {
        let mut w =
            ws(&[("N.md", "# N\n- [ ] Write report ⏫ 📅 2026-11-20 #work\n```dataview\nLIST FROM #a WHERE x = 1 FLATTEN y\n```\n")]);
        let id = w.open_note(&"N.md".into()).unwrap();
        let doc = w.active_doc_mut().unwrap();
        let c = doc.buffer().line_to_char(1) + 4;
        doc.set_selection(quire_text::Selection::cursor(c));
        assert!(w.convert_task_to_note());
        assert!(w.doc(id).unwrap().text().contains("- [ ] [[Write report]]\n"));
        let made = w.vault().unwrap().read(&"Tasks/Write report.md".into()).unwrap();
        let today = Local::now().format("%Y-%m-%d");
        assert_eq!(
            made,
            format!(
                "---\ntype: task\nstatus: todo\npriority: high\ndue: 2026-11-20\ncreated: {today}\ntags: [work]\n---\n# Write report\n"
            )
        );
        // And back: the line gets its fields again, and the empty note goes to the trash.
        let doc = w.active_doc_mut().unwrap();
        let c = doc.buffer().line_to_char(1) + 8;
        doc.set_selection(quire_text::Selection::cursor(c));
        assert!(w.convert_note_to_task());
        assert!(
            w.doc(id).unwrap().text().contains(&format!("- [ ] Write report #work ⏫ ➕ {today} 📅 2026-11-20\n")),
            "{}",
            w.doc(id).unwrap().text()
        );
        assert!(!w.vault().unwrap().exists(&"Tasks/Write report.md".into()));
        // A block id stays on the line.
        let doc = w.active_doc_mut().unwrap();
        let start = doc.buffer().line_to_char(1);
        let end = start + doc.buffer().line(1).trim_end().chars().count();
        let _ = doc.apply(Transaction::replace(start, end, "- [ ] Ship it ^ship".to_string()), EditKind::Other);
        doc.set_selection(quire_text::Selection::cursor(start + 3));
        assert!(w.convert_task_to_note());
        assert!(w.doc(id).unwrap().text().contains("- [ ] [[Ship it]] ^ship\n"));
        let decos = w.decorations(id);
        assert!(decos.iter().any(|d| matches!(d.kind, crate::decor::DecorationKind::Widget { after_line: 4, .. })));
        assert!(w.convert_block(id, Some(0)));
        let t = w.doc(id).unwrap().text().to_string();
        assert!(t.contains("```quire-view\n# Converted from a Dataview query\n# Not converted: FLATTEN\nfrom: #a\nwhere: x = 1\n"), "{t}");
        assert!(views::parse(&views::find_blocks(&t)[0].spec).is_ok());
    }

    fn caret_on_line(w: &mut Workspace, line: usize) {
        let doc = w.active_doc_mut().unwrap();
        let c = doc.buffer().line_to_char(line) + 3;
        doc.set_selection(quire_text::Selection::cursor(c));
    }

    /// 0.11 review M4 and M5: nothing on the line is dropped going to a
    /// note, and a note with more than a line can hold isn't trashed.
    #[test]
    fn conversions_keep_what_doesnt_fit() {
        let mut w = ws(&[("N.md", "# N\n- [/] 14:00 - 15:00 Call Bo [client:: Acme] 📅 2026-11-20 🆔 abc123 ⛔ xyz789 ^b1\n")]);
        let id = w.open_note(&"N.md".into()).unwrap();
        caret_on_line(&mut w, 1);
        assert!(w.convert_task_to_note());
        let line = w.doc(id).unwrap().buffer().line(1);
        assert_eq!(line, "- [/] 14:00 - 15:00 [[Call Bo]] [client:: Acme] 🆔 abc123 ⛔ xyz789 ^b1");
        let made = w.vault().unwrap().read(&"Tasks/Call Bo.md".into()).unwrap();
        assert!(made.contains("status: doing\n") && made.contains("due: 2026-11-20\n"), "{made}");
        // Back: the fields come back, and the note (all of it on the line) goes.
        caret_on_line(&mut w, 1);
        assert!(w.convert_note_to_task());
        let line = w.doc(id).unwrap().buffer().line(1);
        assert!(line.starts_with("- [/] 14:00 - 15:00 Call Bo"), "{line}");
        assert!(line.contains("📅 2026-11-20") && line.ends_with(" [client:: Acme] 🆔 abc123 ⛔ xyz789 ^b1"), "{line}");
        assert!(!w.vault().unwrap().exists(&"Tasks/Call Bo.md".into()), "the line holds all of it");
        // A note with properties a line can't hold stays.
        let v = w.vault().unwrap().clone();
        for extra in
            ["contexts: [\"@home\"]\n", "client: Acme\n", "status: blocked\n", "complete_instances: [2026-11-01]\n", "# a comment\n"]
        {
            let note = format!("---\ntype: task\n{extra}---\n# Keep\n");
            v.write(&"Tasks/Keep.md".into(), &note).unwrap();
            w.index().upsert("Tasks/Keep.md".into(), note.clone());
            w.index().flush();
            let doc = w.active_doc_mut().unwrap();
            let start = doc.buffer().line_to_char(1);
            let end = start + doc.buffer().line(1).chars().count();
            let _ = doc.apply(Transaction::replace(start, end, "- [ ] [[Keep]]".to_string()), EditKind::Other);
            caret_on_line(&mut w, 1);
            assert!(w.convert_note_to_task());
            assert_eq!(v.read(&"Tasks/Keep.md".into()).unwrap(), note, "{extra} keeps the note");
        }
    }

    /// 0.11 security review: board "+" never makes notes in hidden folders
    /// or outside the vault.
    #[test]
    fn board_plus_stays_in_the_vault() {
        for bad in [".quire/agent", "a/.drafts", "../x", "/etc", "C:/x", "a/../../b", ".trash"] {
            assert!(safe_new_note_folder(bad).is_err(), "{bad}");
        }
        for ok in ["", "Tasks", "Projects/2026", "Notes with spaces"] {
            assert!(safe_new_note_folder(ok).is_ok(), "{ok}");
        }
        let mut w = ws(&[("A.md", "x\n")]);
        new_note_in(&mut w, ".quire/agent", "Plan", &[]);
        assert!(!w.vault().unwrap().exists(&".quire/agent/Plan.md".into()));
    }

    #[test]
    fn the_agenda_boards_order_stays_on_this_device() {
        let note = |s: &str| format!("---\ntype: task\nstatus: open\n---\n# {s}\n");
        let (a, b, c) = (note("A"), note("B"), note("C"));
        let mut w = ws(&[("Tasks/A.md", &a), ("Tasks/B.md", &b), ("Tasks/C.md", &c)]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(AGENDA_BOARD_FILE);
        w.features.tasks.agenda_board_file = Some(file.clone());
        let order = |w: &Workspace| -> Vec<String> {
            let r = w.run_view(&w.agenda_board_spec(), None);
            r.as_ref().as_ref().unwrap().rows.iter().filter(|x| x.group == "open").map(|x| x.path.stem().to_string()).collect()
        };
        assert_eq!(order(&w), ["A", "B", "C"]);
        let ids = vec!["Tasks/C.md".to_string(), "Tasks/A.md".to_string()];
        set_board(&mut w, &BoardTarget::Agenda, &BoardSetting::Order { column: "open".into(), ids });
        set_board(&mut w, &BoardTarget::Agenda, &BoardSetting::Color { column: "open".into(), color: Some("teal".into()) });
        assert_eq!(order(&w), ["C", "A", "B"]);
        // Kept in the device's file, not in any note.
        assert!(std::fs::read_to_string(&file).unwrap().contains("order.open"));
        assert_eq!(w.vault().unwrap().read(&NotePath::new("Tasks/A.md")).unwrap(), a);
        // Another window on this device reads it.
        let mut w2 = ws(&[("Tasks/A.md", &a), ("Tasks/B.md", &b), ("Tasks/C.md", &c)]);
        w2.features.tasks.agenda_board_file = Some(file);
        assert_eq!(order(&w2), ["C", "A", "B"]);
        assert!(w2.agenda_board_spec().contains("color.open: teal"));
    }

    #[test]
    fn agenda_includes_task_notes() {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut w =
            ws(&[("A.md", &format!("- [ ] Today 📅 {today}\n")), ("Tasks/B.md", &format!("---\ntype: task\ndue: {today}\n---\n# B\n"))]);
        assert_eq!(w.agenda().today.len(), 2);
        assert!(run(&mut w, Command::ShowAgenda).is_some());
    }
}
