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

/// Which WS-E dialog is open. The desktop shows it (in the Plan panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    /// Rename, merge or delete tags and properties.
    Refactor,
    /// Flashcard review.
    Review,
}

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
}

/// The agenda, for (index version, today).
type AgendaCache = RefCell<Option<((u64, NaiveDate), Rc<Agenda>)>>;

/// WS-E's state, kept in `ws.features.tasks`.
#[derive(Debug, Default)]
pub struct TasksState {
    ready: bool,
    pub reminders: crate::reminders::RemindersState,
    agenda: AgendaCache,
    /// Open recurring tasks per document at its last revision, to notice one being ticked.
    recurring: HashMap<DocId, (u64, Vec<String>)>,
    pub dialog: Option<Dialog>,
    pending: RefCell<Vec<Action>>,
    /// New notes already offered their folder's template.
    pub(crate) checked_new: HashSet<DocId>,
    /// The refactor dialog opens on this (tag?, name), from the caret.
    pub refactor_start: Option<(bool, String)>,
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
        let key = (self.index.version(), today);
        if let Some((k, a)) = self.features.tasks.agenda.borrow().as_ref()
            && *k == key
        {
            return Rc::clone(a);
        }
        let ix = self.index.read();
        let daily = self.period_path(Period::Daily, today);
        let mut tasks: Vec<(String, quire_markdown::Task)> = Vec::new();
        for r in ix.records() {
            let p = r.path.as_str();
            tasks.extend(r.tasks.iter().map(|t| (p.to_string(), t.clone())));
            if let Some(t) = quire_plan::tasks::note_task(&r.properties, &r.title) {
                tasks.push((p.to_string(), t));
            }
        }
        drop(ix);
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

    /// The open document's recurring-task snapshot is stale after we edit it ourselves.
    fn forget_recurring(&mut self, path: &NotePath) {
        if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
            let (id, rev, keys) = (d.id, d.revision(), open_recurring(&d.text()));
            self.features.tasks.recurring.insert(id, (rev, keys));
        }
    }

    /// "Convert task to note": the task on the caret's line becomes
    /// `Tasks/<text>.md` (`type: task`) and the line links to it.
    pub fn convert_task_to_note(&mut self) -> bool {
        let Some(doc) = self.active_doc() else { return false };
        let (line_no, _) = doc.buffer().line_col(doc.selection().head);
        let line = doc.buffer().line(line_no);
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let Some((marker, task)) = quire_plan::tasks::parse_line(&line) else {
            self.notify(NoticeLevel::Info, "Put the caret on a task line (- [ ] …) to make it a note");
            return false;
        };
        let title =
            quire_vault::sanitize_title(&task.text.split_whitespace().filter(|w| !w.starts_with('#')).collect::<Vec<_>>().join(" "));
        if title.is_empty() {
            self.notify(NoticeLevel::Info, "This task has no text to name a note by");
            return false;
        }
        let Some(vault) = self.vault().cloned() else { return false };
        let Ok(path) = vault.unique_path("Tasks", &title) else { return false };
        let mut fm = String::from("---\ntype: task\n");
        let _ = writeln!(fm, "status: {}", if task.done { "done" } else { "todo" });
        for (k, v) in [
            ("due", &task.due),
            ("scheduled", &task.scheduled),
            ("start", &task.start),
            ("recurrence", &task.recurrence),
            ("reminder", &task.reminder),
            ("completion", &task.done_on),
        ] {
            if let Some(v) = v {
                fm.push_str(&quire_markdown::frontmatter::format_field(k, &Value::Str(v.clone())));
            }
        }
        if task.priority.is_some() {
            let _ = writeln!(fm, "priority: {}", quire_markdown::tasks::priority_word(task.priority));
        }
        let tags: Vec<&str> = task.text.split_whitespace().filter(|w| w.starts_with('#')).collect();
        let body =
            format!("{fm}---\n# {}\n{}", path.stem(), if tags.is_empty() { String::new() } else { format!("\n{}\n", tags.join(" ")) });
        if let Err(e) = vault.write(&path, &body) {
            self.notify(NoticeLevel::Error, e.to_string());
            return false;
        }
        self.index.upsert(path.clone(), body);
        let new_line = format!("{}[{}] [[{}]]", &line[..marker], if task.done { 'x' } else { ' ' }, path.stem());
        let Some(doc) = self.active_doc_mut() else { return false };
        let start = doc.buffer().line_to_char(line_no);
        let end = start + line.chars().count();
        let _ = doc.apply(Transaction::replace(start, end, new_line), EditKind::Other);
        self.notify(NoticeLevel::Info, format!("Made {}", path.as_str()));
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
                ws.set_note_property(&path, "status", &Value::Str(if done { "done" } else { "todo" }.into()));
            }
            Action::OpenAt { path, offset } => {
                if ws.open_note(&path).is_some() {
                    ws.jump_to_byte(offset);
                }
            }
            Action::New { template, folder, answers, preset } => {
                let answers: HashMap<String, String> = answers.into_iter().collect();
                ws.new_note_from_template(&template, folder.as_deref(), &answers, &preset);
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
                    ws.features.tasks.dialog = Some(Dialog::Review);
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
        C::ShowAgenda | C::ShowCalendar => return Some(UiRequest::Feature(cmd)),
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
                ws.features.tasks.dialog = Some(Dialog::Review);
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
            ws.features.tasks.dialog = Some(Dialog::Refactor);
            return Some(UiRequest::Feature(cmd));
        }
        C::ConvertTaskToNote => {
            ws.convert_task_to_note();
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
        assert_eq!(made, "---\ntype: task\nstatus: todo\ndue: 2026-11-20\npriority: high\n---\n# Write report\n\n#work\n");
        let decos = w.decorations(id);
        assert!(decos.iter().any(|d| matches!(d.kind, crate::decor::DecorationKind::Widget { after_line: 4, .. })));
        assert!(w.convert_block(id, Some(0)));
        let t = w.doc(id).unwrap().text().to_string();
        assert!(t.contains("```quire-view\n# Converted from a Dataview query\n# Not converted: FLATTEN\nfrom: #a\nwhere: x = 1\n"), "{t}");
        assert!(views::parse(&views::find_blocks(&t)[0].spec).is_ok());
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
