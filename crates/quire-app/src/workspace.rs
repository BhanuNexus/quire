// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The application state machine. Frontends call methods / [`Workspace::run`]
//! and render what they read back; [`Workspace::tick`] drives time-based work
//! (autosave, watcher events, background results).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use quire_index::{Backlink, IndexService};
use quire_intel::llm::{Assist, LanguageModel, LlmError, build_request};
use quire_intel::{Calculator, LinkSuggestion, RelatedNote, UnlinkedMention, trailing_date_phrase};
use quire_markdown::{LinkKind, editing, table};
use quire_text::{Change, EditKind, Selection, Transaction, merge};
use quire_vault::{NotePath, Vault, VaultEvent, VaultWatcher};

use crate::commands::Command;
use crate::document::{DocId, Document, PendingMerge};
use crate::session::{OpenTab, Session};
use crate::settings::{Settings, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
    pub at: Instant,
}

/// The title a note's first line implies: `# Groceries` → `Groceries`, and
/// the line it's on. Front matter is skipped; long lines are cut at a word.
pub fn title_from_text(text: &str) -> Option<(String, usize)> {
    let body_start = quire_markdown::frontmatter::parse(text).map_or(0, |f| f.range.end);
    let skipped_lines = text[..body_start].matches('\n').count();
    let (n, line) = text[body_start..].lines().enumerate().find(|(_, l)| !l.trim().is_empty())?;
    let mut t = line.trim();
    t = t.trim_start_matches('#').trim_start();
    for marker in ["- [ ] ", "- [x] ", "- ", "* ", "> ", "+ "] {
        t = t.strip_prefix(marker).unwrap_or(t);
    }
    let plain: String = t.chars().filter(|c| !matches!(c, '*' | '_' | '`' | '[' | ']' | '=' | '~')).collect();
    let mut title = quire_vault::sanitize_title(&plain);
    if title.chars().count() > 60 {
        let cut: String = title.chars().take(60).collect();
        title = cut
            .rsplit_once(' ')
            .map_or_else(|| cut.clone(), |(a, _)| a.to_string())
            .trim_end_matches([',', ';', ':', '-'])
            .trim()
            .to_string();
    }
    (!title.is_empty()).then_some((title, skipped_lines + n))
}

/// Commands that need the frontend to show something (a picker or dialog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRequest {
    QuickOpen,
    CommandPalette,
    Search,
    Rename,
    PickTemplate,
    OpenVault,
    ConfirmDelete,
    ShowTasks,
    /// Show and focus the in-note find bar.
    Find,
    VaultReplace,
    Ask,
    /// A command the editor view owns (multi-cursor).
    Editor(Command),
    Settings,
    History,
    Import,
    NewProject,
    Shortcuts,
    Tour,
    SyncConflicts,
    /// Pick a folder for a new note.
    NewInFolder,
    /// Pick a folder to move the active note to.
    MoveNote,
}

/// Everything the insights panel shows for the active note.
#[derive(Debug, Clone, Default)]
pub struct Insights {
    pub backlinks: Vec<Backlink>,
    pub unlinked: Vec<UnlinkedMention>,
    pub related: Vec<RelatedNote>,
    pub link_suggestions: Vec<LinkSuggestion>,
    pub tag_suggestions: Vec<(String, f32)>,
    pub summary: Vec<String>,
}

pub struct AssistResult {
    pub doc: DocId,
    pub task: Assist,
    /// Selection the request was made for; `Replace` applies here.
    pub selection: Selection,
    pub output: Result<String, String>,
}

type PendingAssist = (DocId, Selection, Assist, Receiver<Result<String, LlmError>>);
type InsightsCache = RefCell<Option<((DocId, u64, u64), Rc<Insights>)>>;

pub struct Workspace {
    pub settings: Settings,
    pub focus_mode: bool,
    pub(crate) vault: Option<Vault>,
    vault_root: Option<PathBuf>,
    pub(crate) index: IndexService,
    watcher: Option<VaultWatcher>,
    pub(crate) docs: Vec<Document>,
    pub(crate) active: Option<usize>,
    next_id: u64,
    pub(crate) notices: Vec<Notice>,
    pub(crate) assistant: Arc<dyn LanguageModel>,
    pending_assist: Option<PendingAssist>,
    pub assist_result: Option<AssistResult>,
    insights_cache: InsightsCache,
    pub find: crate::find_state::FindState,
    pub ask: crate::ask_state::AskState,
    pub(crate) file_cache: crate::attach::FileCache,
    pub(crate) view_cache: crate::properties::ViewCache,
    /// Recently active notes, most recent first.
    recent: Vec<NotePath>,
    pub(crate) conflict_count_cache: std::cell::Cell<Option<(u64, usize)>>,
    pub updates: crate::updates::UpdateState,
}

impl Workspace {
    pub fn new(settings: Settings) -> Self {
        let assistant = crate::ai::make_assistant(&settings);
        Self {
            settings,
            focus_mode: false,
            vault: None,
            vault_root: None,
            index: IndexService::spawn(),
            watcher: None,
            docs: Vec::new(),
            active: None,
            next_id: 1,
            notices: Vec::new(),
            assistant,
            pending_assist: None,
            assist_result: None,
            insights_cache: RefCell::new(None),
            find: crate::find_state::FindState::default(),
            ask: crate::ask_state::AskState::default(),
            file_cache: crate::attach::FileCache::default(),
            view_cache: crate::properties::ViewCache::default(),
            recent: Vec::new(),
            conflict_count_cache: std::cell::Cell::new(None),
            updates: crate::updates::UpdateState::default(),
        }
    }

    // ------------------------------------------------------------------ notices

    pub fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notices.push(Notice { level, text: text.into(), at: Instant::now() });
        if self.notices.len() > 20 {
            self.notices.remove(0);
        }
    }

    /// Notices younger than `max_age`, newest last.
    pub fn notices(&self, max_age: Duration) -> impl Iterator<Item = &Notice> {
        let now = Instant::now();
        self.notices.iter().filter(move |n| now.duration_since(n.at) < max_age)
    }

    // ------------------------------------------------------------------ vault

    pub fn vault(&self) -> Option<&Vault> {
        self.vault.as_ref()
    }

    pub fn vault_root(&self) -> Option<&Path> {
        self.vault_root.as_deref()
    }

    pub fn vault_name(&self) -> String {
        self.vault_root.as_deref().and_then(Path::file_name).map_or_else(|| "Notes".into(), |n| n.to_string_lossy().into_owned())
    }

    pub fn index(&self) -> &IndexService {
        &self.index
    }

    pub fn open_vault(&mut self, root: &Path) -> Result<(), String> {
        let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
        self.save_all();
        self.docs.retain(|d| d.path.is_none());
        self.active = if self.docs.is_empty() { None } else { Some(0) };
        self.vault_root = Some(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
        self.watcher = match VaultWatcher::start(self.vault_root.as_deref().unwrap_or(root)) {
            Ok(w) => Some(w),
            Err(e) => {
                self.notify(NoticeLevel::Warn, format!("Not watching for outside changes: {e}"));
                None
            }
        };
        self.index.load(vault.clone());
        self.vault = Some(vault);
        self.settings.last_vault = self.vault_root.clone();
        // The neural embedding cache lives inside the vault.
        self.apply_embedder();
        Ok(())
    }

    /// Use an existing vault (tests, demos, in-memory scratch vaults).
    pub fn attach_vault(&mut self, vault: Vault) {
        self.index.load(vault.clone());
        self.vault = Some(vault);
    }

    /// Re-read every note (after a bulk change such as an import).
    pub fn reload_index(&self) {
        if let Some(v) = self.vault.clone() {
            self.index.load(v);
        }
    }

    // ------------------------------------------------------------------ documents

    pub fn docs(&self) -> &[Document] {
        &self.docs
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub fn set_active(&mut self, i: usize) {
        if i < self.docs.len() {
            self.active = Some(i);
        }
    }

    pub fn active_doc(&self) -> Option<&Document> {
        self.active.and_then(|i| self.docs.get(i))
    }

    pub fn active_doc_mut(&mut self) -> Option<&mut Document> {
        self.active.and_then(|i| self.docs.get_mut(i))
    }

    pub fn doc(&self, id: DocId) -> Option<&Document> {
        self.docs.iter().find(|d| d.id == id)
    }

    fn doc_pos(&self, id: DocId) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    fn large_threshold(&self) -> usize {
        self.settings.large_file_kb * 1024
    }

    fn push_doc(&mut self, path: Option<NotePath>, text: &str) -> DocId {
        let id = DocId(self.next_id);
        self.next_id += 1;
        let doc = Document::new(id, path, text, self.large_threshold());
        if doc.large {
            self.notify(NoticeLevel::Info, format!("{} is large; opened in plain mode", doc.title()));
        }
        self.docs.push(doc);
        self.active = Some(self.docs.len() - 1);
        id
    }

    pub fn open_note(&mut self, path: &NotePath) -> Option<DocId> {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(path)) {
            self.active = Some(i);
            return Some(self.docs[i].id);
        }
        let text = match self.vault.as_ref()?.read(path) {
            Ok(t) => t,
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                return None;
            }
        };
        Some(self.push_doc(Some(path.clone()), &text))
    }

    /// Open (or create) the scratchpad: an always-there note that is never indexed.
    pub fn open_scratchpad(&mut self, initial: &str) -> DocId {
        if let Some(i) = self.docs.iter().position(|d| d.path.is_none()) {
            self.active = Some(i);
            return self.docs[i].id;
        }
        self.push_doc(None, initial)
    }

    pub fn new_note(&mut self, title: Option<&str>) -> Option<DocId> {
        let vault = self.vault.as_ref()?;
        let title = title.unwrap_or("Untitled");
        let path = match vault.create_note("", title, "") {
            Ok(p) => p,
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                return None;
            }
        };
        self.index.upsert(path.clone(), String::new());
        let id = self.push_doc(Some(path), "");
        if let Some(d) = self.docs.last_mut() {
            d.auto_name = true;
        }
        Some(id)
    }

    pub fn templates(&self) -> Vec<NotePath> {
        self.vault.as_ref().map(Vault::templates).unwrap_or_default()
    }

    pub fn new_from_template(&mut self, template: &NotePath, title: &str) -> Option<DocId> {
        let (path, cursor) = match self.vault.as_ref()?.create_from_template("", title, template) {
            Ok(r) => r,
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                return None;
            }
        };
        let id = self.open_note(&path)?;
        if let (Some(c), Some(doc)) = (cursor, self.active_doc_mut()) {
            doc.set_selection(Selection::cursor(c));
        }
        self.reindex(id);
        Some(id)
    }

    pub fn open_daily(&mut self) -> Option<DocId> {
        let today = chrono::Local::now().date_naive();
        let (path, created) = match self.vault.as_ref()?.daily_note(today) {
            Ok(r) => r,
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                return None;
            }
        };
        let id = self.open_note(&path)?;
        if created {
            self.reindex(id);
            if let Some(doc) = self.active_doc_mut() {
                let end = doc.buffer().len_chars();
                doc.set_selection(Selection::cursor(end));
            }
        }
        Some(id)
    }

    /// Follow a wikilink target. Unresolved targets create the note (Obsidian behaviour).
    pub fn open_link(&mut self, target: &str, heading: Option<&str>) -> Option<DocId> {
        let from = self.active_doc().and_then(|d| d.path.clone());
        let resolved = self.index.read().resolve(target, from.as_ref());
        let id = match resolved {
            Some(p) => self.open_note(&p)?,
            None if target.trim().is_empty() => self.active_doc()?.id,
            None => {
                let title = quire_vault::sanitize_title(target);
                let id = self.new_note(Some(&title))?;
                let body = format!("# {title}\n\n");
                if let Some(doc) = self.active_doc_mut() {
                    let _ = doc.apply(
                        Transaction::insert(0, body.clone()).with_selection(Selection::cursor(body.chars().count())),
                        EditKind::Other,
                    );
                }
                self.notify(NoticeLevel::Info, format!("Created “{title}”"));
                id
            }
        };
        if let Some(h) = heading {
            self.jump_to_heading(h);
        }
        Some(id)
    }

    pub fn jump_to_heading(&mut self, heading: &str) {
        let Some(doc) = self.active_doc_mut() else {
            return;
        };
        let target = quire_markdown::outline::slugify(heading);
        if let Some(h) = doc.outline().into_iter().find(|h| h.anchor == target || h.text.eq_ignore_ascii_case(heading)) {
            let c = doc.byte_to_char(h.offset);
            doc.set_selection(Selection::cursor(c));
        }
    }

    pub fn jump_to_byte(&mut self, byte: usize) {
        if let Some(doc) = self.active_doc_mut() {
            let c = doc.byte_to_char(byte);
            doc.set_selection(Selection::cursor(c));
        }
    }

    pub fn close(&mut self, id: DocId) {
        let Some(i) = self.doc_pos(id) else { return };
        if self.docs[i].path.is_some() && self.docs[i].is_dirty() {
            self.save(id);
        }
        self.auto_name(id, true);
        let Some(i) = self.doc_pos(id) else { return };
        self.docs.remove(i);
        self.active = match self.active {
            _ if self.docs.is_empty() => None,
            Some(a) if a > i => Some(a - 1),
            Some(a) => Some(a.min(self.docs.len() - 1)),
            None => None,
        };
    }

    // ------------------------------------------------------------------ saving

    pub fn save(&mut self, id: DocId) -> bool {
        let Some(i) = self.doc_pos(id) else {
            return false;
        };
        let (Some(vault), Some(path)) = (self.vault.as_ref(), self.docs[i].path.clone()) else {
            return false;
        };
        let text = self.docs[i].text();
        // First save of a note Quire hasn't seen change before: keep the original too.
        let now = crate::history_ops::now_ms();
        if vault.history(&path).is_empty()
            && let Some(orig) = self.docs[i].disk_text()
            && *orig != *text
        {
            let _ = vault.snapshot(&path, &orig, now - 1, true);
        }
        match vault.write(&path, &text) {
            Ok(()) => {
                let _ = vault.snapshot(&path, &text, now, false);
                self.docs[i].mark_saved(&text);
                self.index.upsert(path, text.to_string());
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save: {e}"));
                false
            }
        }
    }

    pub fn save_all(&mut self) {
        let dirty: Vec<DocId> = self.docs.iter().filter(|d| d.is_dirty() && d.path.is_some()).map(|d| d.id).collect();
        for id in dirty {
            self.save(id);
        }
        // Leaving (quit, switching vaults): name new notes even mid-title.
        let unnamed: Vec<DocId> = self.docs.iter().filter(|d| d.auto_name).map(|d| d.id).collect();
        for id in unnamed {
            self.auto_name(id, true);
        }
    }

    /// Give an "Untitled" note the name of its first line. Unless `force`,
    /// waits until the caret has left that line, so the file isn't renamed
    /// on every pause while the title is still being typed.
    pub(crate) fn auto_name(&mut self, id: DocId, force: bool) {
        let Some(doc) = self.doc(id) else { return };
        let Some(path) = doc.path.clone().filter(|_| doc.auto_name && !doc.conflict) else { return };
        let text = doc.text();
        let Some((title, title_line)) = title_from_text(&text) else { return };
        let caret_line = doc.buffer().char_to_line(doc.selection().head);
        if (!force && caret_line == title_line) || title == path.stem() {
            return;
        }
        // Another note already has that name: "Title 1", "Title 2"…
        let title = match &self.vault {
            Some(v) if v.exists(&path.with_stem(&title)) && !path.with_stem(&title).as_str().eq_ignore_ascii_case(path.as_str()) => {
                v.unique_path(path.folder(), &title).map(|p| p.stem().to_string()).unwrap_or(title)
            }
            _ => title,
        };
        if title == path.stem() {
            return;
        }
        let _ = self.rename_doc(id, &title, false);
    }

    fn reindex(&self, id: DocId) {
        if let Some(d) = self.doc(id)
            && let Some(p) = d.path.clone()
        {
            let text = d.text().to_string();
            self.index.upsert(p, text);
        }
    }

    /// Time-driven work. Returns true if the UI should repaint.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        self.track_recent();
        let delay = Duration::from_millis(self.settings.autosave_ms);
        let due: Vec<DocId> = self
            .docs
            .iter()
            .filter(|d| d.path.is_some() && d.is_dirty() && !d.conflict && d.last_edit().is_some_and(|t| now.duration_since(t) >= delay))
            .map(|d| d.id)
            .collect();
        for id in due {
            changed |= self.save(id);
        }
        // New notes take their name from the first line once you move past it.
        let unnamed: Vec<DocId> = self.docs.iter().filter(|d| d.auto_name && !d.is_dirty()).map(|d| d.id).collect();
        for id in unnamed {
            let before = self.doc(id).and_then(|d| d.path.clone());
            self.auto_name(id, false);
            changed |= self.doc(id).and_then(|d| d.path.clone()) != before;
        }
        changed |= self.process_vault_events();
        changed |= self.poll_assist();
        changed |= self.poll_ask();
        self.maybe_check_updates();
        changed |= self.poll_updates();
        changed
    }

    /// Earliest instant at which `tick` has autosave work to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        let delay = Duration::from_millis(self.settings.autosave_ms);
        self.docs.iter().filter(|d| d.path.is_some() && d.is_dirty()).filter_map(Document::last_edit).map(|t| t + delay).min()
    }

    fn process_vault_events(&mut self) -> bool {
        let Some(w) = &self.watcher else { return false };
        let events: Vec<VaultEvent> = w.events.try_iter().collect();
        if events.is_empty() {
            return false;
        }
        let mut seen = std::collections::HashSet::new();
        for ev in events.into_iter().rev() {
            match ev {
                VaultEvent::Changed(p) => {
                    if !seen.insert(p.clone()) {
                        continue;
                    }
                    let Some(content) = self.vault.as_ref().and_then(|v| v.read(&p).ok()) else {
                        continue;
                    };
                    if !self.docs.iter().any(|d| d.path.as_ref() == Some(&p) && d.matches_disk(&content))
                        && let Some(v) = &self.vault
                    {
                        // Outside edits are versions too.
                        let _ = v.snapshot(&p, &content, crate::history_ops::now_ms(), true);
                    }
                    if self.absorb_external_change(&p, &content) {
                        continue; // our own save echoing back
                    }
                    self.index.upsert(p, content);
                }
                VaultEvent::Removed(p) => {
                    if seen.insert(p.clone()) {
                        self.index.remove(p);
                    }
                }
            }
        }
        true
    }

    /// Bring an open note up to date with a change made outside Quire.
    /// Clean notes reload; notes with unsaved edits are merged three-way
    /// (the text we last read or wrote is the common ancestor), or keep a
    /// pending merge for the user when both sides changed the same lines.
    /// Returns true when `content` is just our own save echoing back.
    pub(crate) fn absorb_external_change(&mut self, p: &NotePath, content: &str) -> bool {
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_ref() == Some(p)) {
            if doc.matches_disk(content) {
                return true;
            }
            let title = doc.title();
            if doc.is_dirty() {
                // Three-way merge: what we last read/wrote is the common ancestor.
                let mine = doc.text().to_string();
                let base = doc.disk_text();
                let r = merge::merge(base.as_deref(), &mine, content);
                if r.is_clean() {
                    doc.replace_minimally(&r.text);
                    doc.set_disk_base(content);
                    doc.conflict = false;
                    doc.pending_merge = None;
                    self.notify(NoticeLevel::Info, format!("Merged changes made elsewhere into “{title}” (Undo to take them out)"));
                } else {
                    let n = r.conflicts;
                    doc.conflict = true;
                    doc.pending_merge = Some(PendingMerge { theirs: content.to_string(), merged: r.text, conflicts: n });
                    self.notify(
                        NoticeLevel::Warn,
                        format!(
                            "“{title}” also changed elsewhere: {n} conflicting change{}. Choose what to keep above the note.",
                            if n == 1 { "" } else { "s" }
                        ),
                    );
                }
            } else {
                doc.reload(content);
                self.notify(NoticeLevel::Info, format!("Reloaded “{title}” (changed on disk)"));
            }
        }
        false
    }

    // ------------------------------------------------------------------ refactors

    pub fn rename_active(&mut self, new_title: &str) -> Result<(), String> {
        let id = self.active_doc().ok_or("no note open")?.id;
        self.rename_doc(id, new_title, true)?;
        // A name the user chose sticks.
        if let Some(i) = self.doc_pos(id) {
            self.docs[i].auto_name = false;
        }
        Ok(())
    }

    fn rename_doc(&mut self, id: DocId, new_title: &str, notify: bool) -> Result<(), String> {
        self.save(id);
        let old = self.doc(id).and_then(|d| d.path.clone()).ok_or("scratchpad can't be renamed")?;
        let vault = self.vault.clone().ok_or("no vault")?;
        let report = vault.rename_note(&old, new_title).map_err(|e| e.to_string())?;
        let n = self.apply_path_change(&vault, &report);
        self.retitle_front_matter(id, old.stem(), report.new_path.stem());
        if !notify {
            return Ok(());
        }
        self.notify(
            NoticeLevel::Info,
            format!("Renamed to “{}” · updated links in {n} note{}", report.new_path.stem(), if n == 1 { "" } else { "s" }),
        );
        Ok(())
    }

    /// A front-matter `title:` that repeated the old name follows a rename,
    /// so search, boards and views show the new name (an undoable edit).
    fn retitle_front_matter(&mut self, id: DocId, old_stem: &str, new_stem: &str) {
        let Some(i) = self.doc_pos(id) else { return };
        let text = self.docs[i].text().to_string();
        let same = quire_markdown::frontmatter::parse(&text).and_then(|fm| fm.title().map(|t| t.trim() == old_stem)).unwrap_or(false);
        if !same {
            return;
        }
        let edit = quire_markdown::frontmatter::set_field(&text, "title", &quire_markdown::Value::Str(new_stem.to_string()));
        let doc = &mut self.docs[i];
        let buf = doc.buffer();
        let (s, e) = (buf.byte_to_char(edit.range.start), buf.byte_to_char(edit.range.end));
        let head = doc.selection().head;
        let new_len = edit.text.chars().count();
        let caret = if head >= e { head - (e - s) + new_len } else { head.min(s + new_len) };
        if doc.apply(Transaction::replace(s, e, edit.text).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok() {
            self.save(id);
        }
    }

    /// Move the active note to `folder` ("" = vault root).
    pub fn move_active(&mut self, folder: &str) -> Result<(), String> {
        let id = self.active_doc().ok_or("no note open")?.id;
        self.save(id);
        let old = self.doc(id).and_then(|d| d.path.clone()).ok_or("scratchpad can't be moved")?;
        let vault = self.vault.clone().ok_or("no vault")?;
        let report = vault.move_note(&old, folder).map_err(|e| e.to_string())?;
        let n = self.apply_path_change(&vault, &report);
        let place =
            if report.new_path.folder().is_empty() { "the vault root".to_string() } else { format!("{}/", report.new_path.folder()) };
        let links = if n == 0 { String::new() } else { format!(" · updated links in {n} note{}", if n == 1 { "" } else { "s" }) };
        self.notify(NoticeLevel::Info, format!("Moved “{}” to {place}{links}", report.new_path.stem()));
        Ok(())
    }

    const RECENT_CAP: usize = 30;

    /// Put the active note at the front of the recent list.
    fn track_recent(&mut self) {
        let Some(p) = self.active_doc().and_then(|d| d.path.clone()) else { return };
        if self.recent.first() != Some(&p) {
            self.recent.retain(|r| r != &p);
            self.recent.insert(0, p);
            self.recent.truncate(Self::RECENT_CAP);
        }
    }

    /// Recently opened notes that still exist, most recent first, without
    /// the one you're looking at.
    pub fn recent_notes(&self, limit: usize) -> Vec<NotePath> {
        let active = self.active_doc().and_then(|d| d.path.as_ref());
        let ix = self.index.read();
        self.recent.iter().filter(|p| Some(*p) != active && ix.get(p).is_some()).take(limit).cloned().collect()
    }

    /// Re-index and re-point open documents after a rename or move.
    /// Returns how many other notes had links rewritten.
    fn apply_path_change(&mut self, vault: &Vault, report: &quire_vault::RenameReport) -> usize {
        for r in &mut self.recent {
            if *r == report.old_path {
                r.clone_from(&report.new_path);
            }
        }
        self.index.remove(report.old_path.clone());
        for p in std::iter::once(&report.new_path).chain(report.updated.iter()) {
            if let Ok(content) = vault.read(p) {
                self.index.upsert(p.clone(), content.clone());
                // Keep open documents in sync with rewritten links.
                let was = if p == &report.new_path { &report.old_path } else { p };
                if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_ref() == Some(was) || d.path.as_ref() == Some(p)) {
                    doc.path = Some(p.clone());
                    if !doc.matches_disk(&content) && !doc.is_dirty() {
                        doc.reload(&content);
                    }
                }
            }
        }
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_ref() == Some(&report.old_path)) {
            doc.path = Some(report.new_path.clone());
        }
        report.updated.iter().filter(|p| **p != report.new_path).count()
    }

    pub fn trash_active(&mut self) {
        let Some(doc) = self.active_doc() else { return };
        let (id, path) = (doc.id, doc.path.clone());
        let Some(path) = path else { return };
        if let Some(v) = &self.vault {
            match v.trash(&path) {
                Ok(()) => {
                    self.index.remove(path.clone());
                    if let Some(i) = self.doc_pos(id) {
                        self.docs.remove(i);
                        self.active = if self.docs.is_empty() { None } else { Some(i.min(self.docs.len() - 1)) };
                    }
                    self.notify(NoticeLevel::Info, format!("Moved “{}” to .trash", path.stem()));
                }
                Err(e) => self.notify(NoticeLevel::Error, e.to_string()),
            }
        }
    }

    /// Render the active note to standalone HTML in `Exports/` and return where it went.
    pub fn export_active_html(&mut self) -> Option<String> {
        let doc = self.active_doc()?;
        let title = doc.title();
        let text = doc.text();
        let html = {
            let ix = self.index.read();
            let from = doc.path.clone();
            quire_markdown::render::to_html_document(&title, &text, &|l| {
                ix.resolve(&l.target, from.as_ref()).map(|p| {
                    if l.embed { p.as_str().to_string() } else { format!("{}.html", quire_markdown::render::encode_url(p.stem())) }
                })
            })
        };
        let vault = self.vault.as_ref()?;
        let out = NotePath::new(format!("Exports/{title}.html"));
        match vault.write(&out, &html) {
            Ok(()) => {
                let shown = vault.storage().absolute(&out).map_or_else(|| out.to_string(), |p| p.display().to_string());
                self.notify(NoticeLevel::Info, format!("Exported to {shown}"));
                Some(shown)
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                None
            }
        }
    }

    // ------------------------------------------------------------------ editing

    fn edit_active(&mut self, f: impl FnOnce(&Document) -> Option<Transaction>) -> bool {
        let Some(doc) = self.active_doc_mut() else {
            return false;
        };
        match f(doc) {
            Some(tx) => doc.apply(tx, EditKind::Other).is_ok(),
            None => false,
        }
    }

    /// Smart Enter: calculator, natural dates, list continuation.
    /// Returns false when the frontend should insert a plain newline.
    pub fn smart_enter(&mut self) -> bool {
        let calc_on = self.settings.inline_calculator;
        let as_link = self.settings.dates_as_links;
        let Some(doc) = self.active_doc_mut() else {
            return false;
        };
        if doc.large || !doc.selection().is_empty() {
            return false;
        }
        let head = doc.selection().head;
        let buf = doc.buffer();
        let line = buf.char_to_line(head);
        let line_range = buf.line_range(line);
        let text = buf.line(line);
        let col = head - line_range.start;
        let before: String = text.chars().take(col).collect();

        if calc_on && head == line_range.end && before.trim_end().ends_with('=') {
            let mut calc = Calculator::new();
            let first = line.saturating_sub(200);
            let prev: Vec<String> = (first..line).map(|l| buf.line(l)).collect();
            calc.prime(prev.iter().map(String::as_str));
            if let Some(result) = calc.completion_for(&before) {
                let end = head + result.chars().count();
                return doc.apply(Transaction::insert(head, result).with_selection(Selection::cursor(end)), EditKind::Other).is_ok();
            }
        }

        let handled = if let Some(m) = trailing_date_phrase(&before, chrono::Local::now().date_naive()) {
            let iso = m.date.format("%Y-%m-%d").to_string();
            let rep = if as_link { format!("[[{iso}]]") } else { iso };
            let start = line_range.start + m.start_char;
            let caret = start + rep.chars().count();
            doc.apply(Transaction::replace(start, head, rep).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok()
        } else {
            false
        };
        let sel = doc.selection();
        if let Some(tx) = editing::on_enter(doc.buffer(), sel) {
            return doc.apply(tx, EditKind::Other).is_ok();
        }
        if handled {
            let at = doc.selection().head;
            let _ = doc.apply(Transaction::insert(at, "\n").with_selection(Selection::cursor(at + 1)), EditKind::Other);
        }
        handled
    }

    /// Space after `@phrase` turns it into a date. Returns true if consumed.
    pub fn smart_space(&mut self) -> bool {
        let as_link = self.settings.dates_as_links;
        let Some(doc) = self.active_doc_mut() else {
            return false;
        };
        let sel = doc.selection();
        if !sel.is_empty() || doc.large {
            return false;
        }
        let buf = doc.buffer();
        let line_start = buf.line_to_char(buf.char_to_line(sel.head));
        let before = buf.slice(line_start..sel.head);
        let Some(m) = trailing_date_phrase(&before, chrono::Local::now().date_naive()) else {
            return false;
        };
        let iso = m.date.format("%Y-%m-%d").to_string();
        let rep = if as_link { format!("[[{iso}]] ") } else { format!("{iso} ") };
        let start = line_start + m.start_char;
        let caret = start + rep.chars().count();
        doc.apply(Transaction::replace(start, sel.head, rep).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok()
    }

    pub fn smart_tab(&mut self, outdent: bool) -> bool {
        let unit = self.settings.indent_unit.clone();
        self.edit_active(|d| editing::indent(d.buffer(), d.selection(), &unit, outdent))
    }

    pub fn insert_text(&mut self, text: &str) {
        self.edit_active(|d| {
            let s = d.selection();
            let caret = s.start() + text.chars().count();
            Some(Transaction::replace(s.start(), s.end(), text).with_selection(Selection::cursor(caret)))
        });
    }

    /// Turn a suggested mention into a `[[link]]`.
    pub fn apply_link_suggestion(&mut self, s: &LinkSuggestion) {
        self.apply_link_suggestions(std::slice::from_ref(s));
    }

    pub fn apply_link_suggestions(&mut self, list: &[LinkSuggestion]) {
        self.edit_active(|d| {
            let changes: Vec<Change> = list
                .iter()
                .map(|s| {
                    let name = s.target.stem();
                    let link = if name.eq_ignore_ascii_case(&s.text) && name == s.text {
                        format!("[[{name}]]")
                    } else {
                        format!("[[{name}|{}]]", s.text)
                    };
                    Change::new(d.byte_to_char(s.range.start), d.byte_to_char(s.range.end), link)
                })
                .collect();
            Transaction::new(changes).ok()
        });
    }

    /// Wikilink under (or touching) the caret.
    pub fn link_at_caret(&self) -> Option<(String, Option<String>)> {
        let doc = self.active_doc()?;
        let byte = doc.buffer().char_to_byte(doc.selection().head);
        let ex = doc.extracted();
        let l = ex.links.iter().find(|l| l.range.start <= byte && byte <= l.range.end && l.is_internal())?;
        Some((l.target.clone(), l.heading.clone()))
    }

    pub fn follow_link_at_caret(&mut self) -> bool {
        match self.link_at_caret() {
            Some((t, h)) => self.open_link(&t, h.as_deref()).is_some(),
            None => false,
        }
    }

    fn insert_wikilink(&mut self) {
        self.edit_active(|d| {
            let s = d.selection();
            let inner = d.buffer().slice(s.range());
            let caret = if inner.is_empty() { s.start() + 2 } else { s.start() + inner.chars().count() + 4 };
            Some(Transaction::replace(s.start(), s.end(), format!("[[{inner}]]")).with_selection(Selection::cursor(caret)))
        });
    }

    // ------------------------------------------------------------------ commands

    /// Run a command. Returns a request when the frontend must show UI.
    pub fn run(&mut self, cmd: Command) -> Option<UiRequest> {
        use Command as C;
        match cmd {
            C::NewNote => {
                self.new_note(None);
            }
            C::NewFromTemplate => return Some(UiRequest::PickTemplate),
            C::DailyNote => {
                self.open_daily();
            }
            C::QuickOpen => return Some(UiRequest::QuickOpen),
            C::CommandPalette => return Some(UiRequest::CommandPalette),
            C::SearchVault => return Some(UiRequest::Search),
            C::ShowTasks => return Some(UiRequest::ShowTasks),
            C::OpenVault => return Some(UiRequest::OpenVault),
            C::RenameNote => return Some(UiRequest::Rename),
            C::DeleteNote => return Some(UiRequest::ConfirmDelete),
            C::Find | C::FindReplace => {
                self.find.open = true;
                self.find.replace_mode = cmd == C::FindReplace;
                return Some(UiRequest::Find);
            }
            C::FindNext | C::FindPrev => {
                if !self.find_step(cmd == C::FindPrev) {
                    return Some(UiRequest::Find);
                }
            }
            C::VaultReplace => {
                self.find.vault_open = true;
                return Some(UiRequest::VaultReplace);
            }
            C::AskVault => {
                self.ask.open = true;
                return Some(UiRequest::Ask);
            }
            C::SelectAll => {
                if let Some(d) = self.active_doc_mut() {
                    let n = d.buffer().len_chars();
                    d.set_selection(Selection::new(0, n));
                }
            }
            C::AddNextOccurrence | C::SelectAllOccurrences | C::AddCursorAbove | C::AddCursorBelow => {
                return Some(UiRequest::Editor(cmd));
            }
            C::Scratchpad => {
                self.open_scratchpad("");
            }
            C::Save => {
                if let Some(id) = self.active_doc().map(|d| d.id) {
                    self.save(id);
                }
            }
            C::CloseTab => {
                if let Some(id) = self.active_doc().map(|d| d.id) {
                    self.close(id);
                }
            }
            C::NextTab | C::PrevTab => {
                let n = self.docs.len();
                if let Some(a) = self.active.filter(|_| n > 0) {
                    self.active = Some(if cmd == C::NextTab { (a + 1) % n } else { (a + n - 1) % n });
                }
            }
            C::ExportHtml => {
                self.export_active_html();
            }
            C::Bold => self.wrap("**"),
            C::Italic => self.wrap("_"),
            C::InlineCode => self.wrap("`"),
            C::Strikethrough => self.wrap("~~"),
            C::Highlight => self.wrap("=="),
            C::Heading1 | C::Heading2 | C::Heading3 | C::Paragraph => {
                let level = match cmd {
                    C::Heading1 => 1,
                    C::Heading2 => 2,
                    C::Heading3 => 3,
                    _ => 0,
                };
                self.edit_active(|d| Some(editing::set_heading(d.buffer(), d.selection(), level)));
            }
            C::ToggleTask => {
                self.edit_active(|d| editing::toggle_task(d.buffer(), d.selection()));
            }
            C::Quote => {
                self.edit_active(|d| Some(editing::toggle_line_prefix(d.buffer(), d.selection(), "> ")));
            }
            C::FormatTable => {
                if !self.edit_active(|d| table::format_at(d.buffer(), d.selection())) {
                    self.notify(NoticeLevel::Info, "Caret isn't in a table (or it's already tidy)");
                }
            }
            C::InsertLink => self.insert_wikilink(),
            C::FollowLink => {
                if !self.follow_link_at_caret() {
                    self.notify(NoticeLevel::Info, "No link under the caret");
                }
            }
            C::InsertView => {
                // On its own line, so the fence is a real fence.
                let at_line_start = self.active_doc().is_some_and(|d| {
                    let b = d.buffer();
                    let s = d.selection().start();
                    s == b.line_to_char(b.char_to_line(s))
                });
                let lead = if at_line_start { "" } else { "\n" };
                self.insert_text(&format!("{lead}{}", crate::properties::VIEW_TEMPLATE));
            }
            C::ShowHistory => return Some(UiRequest::History),
            C::ShowShortcuts => return Some(UiRequest::Shortcuts),
            C::ResolveSyncConflicts => return Some(UiRequest::SyncConflicts),
            C::TakeTour => return Some(UiRequest::Tour),
            C::CheckForUpdates => self.check_for_updates(true),
            C::NewProject => return self.vault.is_some().then_some(UiRequest::NewProject),
            C::NewInFolder => return self.vault.is_some().then_some(UiRequest::NewInFolder),
            C::MoveNote => return self.active_doc().and_then(|d| d.path.as_ref()).map(|_| UiRequest::MoveNote),
            C::ImportNotes => return Some(UiRequest::Import),
            C::OpenSettings => return Some(UiRequest::Settings),
            C::InsertDate => {
                let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                self.insert_text(&today);
            }
            C::Undo => {
                if let Some(d) = self.active_doc_mut() {
                    d.undo();
                }
            }
            C::Redo => {
                if let Some(d) = self.active_doc_mut() {
                    d.redo();
                }
            }
            C::SwitchUndoBranch => {
                if let Some(d) = self.active_doc_mut() {
                    let n = d.undo_branches();
                    d.switch_undo_branch();
                    if n > 1 {
                        self.notify(NoticeLevel::Info, format!("Redo now follows another of {n} branches"));
                    }
                }
            }
            C::TogglePreview => self.settings.show_preview = !self.settings.show_preview,
            C::ToggleSidebar => self.settings.show_sidebar = !self.settings.show_sidebar,
            C::ToggleInsights => self.settings.show_insights = !self.settings.show_insights,
            C::FocusMode => self.focus_mode = !self.focus_mode,
            C::ToggleTheme => {
                self.settings.theme = match self.settings.theme {
                    Theme::Dark => Theme::Light,
                    _ => Theme::Dark,
                }
            }
            C::ToggleClassicEditor => {
                self.settings.editor = if self.settings.editor == "classic" { "fast".into() } else { "classic".into() };
                let name = self.settings.editor.clone();
                self.notify(NoticeLevel::Info, format!("Using the {name} editor"));
            }
            C::ZoomIn => self.settings.font_size = (self.settings.font_size + 1.0).min(40.0),
            C::ZoomOut => self.settings.font_size = (self.settings.font_size - 1.0).max(9.0),
            C::Summarize => self.start_assist(Assist::Summarize),
            C::EvaluateLine => self.evaluate_line(),
            C::LinkAllSuggestions => {
                let s = self.insights().link_suggestions.clone();
                let n = s.len();
                self.apply_link_suggestions(&s);
                self.notify(NoticeLevel::Info, format!("Linked {n} mention{}", if n == 1 { "" } else { "s" }));
            }
        }
        None
    }

    fn wrap(&mut self, marker: &str) {
        self.edit_active(|d| Some(editing::toggle_wrap(d.buffer(), d.selection(), marker)));
    }

    fn evaluate_line(&mut self) {
        let Some(doc) = self.active_doc_mut() else {
            return;
        };
        let buf = doc.buffer();
        let line = buf.char_to_line(doc.selection().head);
        let range = buf.line_range(line);
        let text = buf.line(line);
        let mut calc = Calculator::new();
        let prev: Vec<String> = (line.saturating_sub(200)..line).map(|l| buf.line(l)).collect();
        calc.prime(prev.iter().map(String::as_str));
        let expr = text.trim_end().trim_end_matches('=');
        if let Some(v) = calc.eval(expr) {
            let rep = format!("{} = {}", expr.trim_end(), quire_intel::format_number(v));
            let caret = range.start + rep.chars().count();
            let _ = doc.apply(Transaction::replace(range.start, range.end, rep).with_selection(Selection::cursor(caret)), EditKind::Other);
        } else {
            self.notify(NoticeLevel::Info, "This line isn't a calculation");
        }
    }

    // ------------------------------------------------------------------ assistant

    pub fn assistant_name(&self) -> &str {
        self.assistant.name()
    }

    pub fn assist_pending(&self) -> bool {
        self.pending_assist.is_some()
    }

    pub fn start_assist(&mut self, task: Assist) {
        let Some(doc) = self.active_doc() else { return };
        let sel = doc.selection();
        let text = if sel.is_empty() { doc.text().to_string() } else { doc.buffer().slice(sel.range()) };
        let text = if let Assist::Ask { .. } = task { self.context_for_question(&text) } else { text };
        let rx = quire_intel::llm::spawn(Arc::clone(&self.assistant), task.clone(), build_request(&task, &text));
        self.pending_assist = Some((doc.id, sel, task, rx));
    }

    fn context_for_question(&self, question: &str) -> String {
        let ix = self.index.read();
        ix.search(question, 6)
            .into_iter()
            .filter_map(|h| Some(format!("## [[{}]]\n{}\n", h.path.stem(), ix.source(&h.path)?.chars().take(4000).collect::<String>())))
            .collect()
    }

    fn poll_assist(&mut self) -> bool {
        let Some((_, _, _, rx)) = &self.pending_assist else {
            return false;
        };
        let Ok(res) = rx.try_recv() else { return false };
        let (doc, selection, task, _) = self.pending_assist.take().expect("checked above");
        self.assist_result = Some(AssistResult { doc, task, selection, output: res.map_err(|e| e.to_string()) });
        true
    }

    /// Insert the assistant's output below the selection, or replace it.
    pub fn accept_assist(&mut self, replace: bool) {
        let Some(res) = self.assist_result.take() else {
            return;
        };
        let Ok(text) = res.output else { return };
        let Some(i) = self.doc_pos(res.doc) else {
            return;
        };
        self.active = Some(i);
        let doc = &mut self.docs[i];
        let sel = res.selection.clamp(doc.buffer().len_chars());
        let tx = if replace && !sel.is_empty() {
            Transaction::replace(sel.start(), sel.end(), text)
        } else {
            let at = if sel.is_empty() { doc.buffer().len_chars() } else { sel.end() };
            Transaction::insert(at, format!("\n\n{text}\n"))
        };
        let _ = doc.apply(tx, EditKind::Other);
    }

    // ------------------------------------------------------------------ insights

    /// Panel data for the active note, recomputed only when the index changes.
    pub fn insights(&self) -> Rc<Insights> {
        let Some(doc) = self.active_doc() else {
            return Rc::default();
        };
        let key = (doc.id, self.index.version(), self.index.semantic_version());
        if let Some((k, v)) = &*self.insights_cache.borrow()
            && *k == key
        {
            return Rc::clone(v);
        }
        let ix = self.index.read();
        let text = doc.text();
        let mut ins = Insights::default();
        if let Some(path) = &doc.path {
            ins.backlinks = ix.backlinks(path).to_vec();
            ins.unlinked = quire_intel::unlinked_mentions(&ix, path);
            let sem = self.index.semantic();
            ins.related = quire_intel::related_notes_with(&ix, sem.as_deref(), path, 6);
            ins.tag_suggestions = quire_intel::suggest_tags(&ix, path, 5);
        }
        if !doc.large {
            ins.link_suggestions = quire_intel::suggest_links(&text, &ix, doc.path.as_ref()).into_iter().take(20).collect();
            ins.summary = quire_intel::summarize(&doc.extracted().plain, 3);
        }
        let rc = Rc::new(ins);
        *self.insights_cache.borrow_mut() = Some((key, Rc::clone(&rc)));
        rc
    }

    /// Outgoing internal links of the active note, resolved.
    pub fn outgoing(&self) -> Vec<(String, Option<NotePath>)> {
        let Some(doc) = self.active_doc() else {
            return vec![];
        };
        let ix = self.index.read();
        let mut seen = std::collections::HashSet::new();
        doc.extracted()
            .links
            .iter()
            .filter(|l| matches!(l.kind, LinkKind::Wiki | LinkKind::Relative) && !l.target.is_empty())
            .filter(|l| seen.insert(l.target.to_lowercase()))
            .map(|l| (l.target.clone(), ix.resolve(&l.target, doc.path.as_ref())))
            .collect()
    }

    // ------------------------------------------------------------------ session

    pub fn session(&self) -> Session {
        Session {
            vault: self.vault_root.clone(),
            tabs: self
                .docs
                .iter()
                .filter_map(|d| d.path.as_ref().map(|p| OpenTab { path: p.to_string(), caret: d.selection().head }))
                .collect(),
            active: self.active.unwrap_or(0),
            scratchpad: self.docs.iter().find(|d| d.path.is_none()).map(|d| d.text().to_string()).unwrap_or_default(),
            recent: self.recent.iter().map(ToString::to_string).collect(),
        }
    }

    pub fn restore(&mut self, s: &Session) {
        self.recent = s.recent.iter().map(NotePath::new).filter(|p| self.vault.as_ref().is_some_and(|v| v.exists(p))).collect();
        for tab in &s.tabs {
            let p = NotePath::new(&tab.path);
            if self.vault.as_ref().is_some_and(|v| v.exists(&p))
                && self.open_note(&p).is_some()
                && let Some(d) = self.active_doc_mut()
            {
                d.set_selection(Selection::cursor(tab.caret));
            }
        }
        if !s.scratchpad.is_empty() {
            self.open_scratchpad(&s.scratchpad);
        }
        if !self.docs.is_empty() {
            self.active = Some(s.active.min(self.docs.len() - 1));
        }
    }

    pub fn shutdown(&mut self) {
        self.save_all();
        if let Some(p) = Session::path() {
            let _ = self.session().save_to(&p);
        }
        let _ = self.settings.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_notes_follow_activation_renames_and_sessions() {
        let mut w = ws();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        w.track_recent();
        w.open_note(&NotePath::new("Rope Notes.md")).unwrap();
        w.track_recent();
        assert_eq!(w.recent_notes(5), vec![NotePath::new("Ideas.md")], "the active note is left out");
        w.rename_active("Ropes").unwrap();
        w.track_recent();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        w.track_recent();
        w.index().flush();
        assert_eq!(w.recent_notes(5), vec![NotePath::new("Ropes.md")], "renames are followed");
        let s = w.session();
        assert_eq!(s.recent, vec!["Ideas.md", "Ropes.md"]);
        let mut w2 = ws();
        w2.vault().unwrap().write(&NotePath::new("Ropes.md"), "x").unwrap();
        w2.index().upsert(NotePath::new("Ropes.md"), "x".into());
        w2.index().flush();
        w2.restore(&s);
        w2.track_recent();
        assert!(w2.recent_notes(5).contains(&NotePath::new("Ropes.md")));
    }

    #[test]
    fn titles_from_first_lines() {
        assert_eq!(title_from_text("# Groceries\nmilk"), Some(("Groceries".into(), 0)));
        assert_eq!(title_from_text("\n\n- [ ] call **Asha**: re [[Plan]]\n"), Some(("call Asha re Plan".into(), 2)));
        assert_eq!(title_from_text("---\ntags: [a]\n---\nMeeting notes / Q4?\n"), Some(("Meeting notes Q4".into(), 3)));
        let long = "word ".repeat(30);
        let (t, _) = title_from_text(&long).unwrap();
        assert!(t.chars().count() <= 60 && !t.ends_with(' '));
        assert_eq!(title_from_text("   \n# \n"), None);
    }

    #[test]
    fn new_notes_take_their_first_line_as_name() {
        let mut w = ws();
        let id = w.new_note(None).unwrap();
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Untitled.md");
        w.insert_text("# Ideas");
        w.save(id);
        // Still on the title line: not renamed yet (no churn while typing).
        w.auto_name(id, false);
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Untitled.md");
        w.insert_text("\nbody");
        w.save(id);
        w.auto_name(id, false);
        // "Ideas" exists already, so it becomes "Ideas 1".
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Ideas 1.md");
        let v = w.vault().unwrap();
        assert!(!v.exists(&NotePath::new("Untitled.md")));
        assert_eq!(v.read(&NotePath::new("Ideas 1.md")).unwrap(), "# Ideas\nbody");
        // The name follows later title edits until the user renames by hand.
        w.active_doc_mut().unwrap().set_selection(Selection::cursor(2));
        w.insert_text("Big ");
        w.active_doc_mut().unwrap().set_selection(Selection::cursor(13));
        w.save(id);
        w.auto_name(id, false);
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Big Ideas.md");
        w.rename_active("Mine").unwrap();
        w.close(id);
        assert!(w.vault().unwrap().exists(&NotePath::new("Mine.md")), "a hand-picked name sticks");
    }

    #[test]
    fn closing_names_a_note_even_mid_title() {
        let mut w = ws();
        let id = w.new_note(None).unwrap();
        w.insert_text("Shopping list");
        w.close(id);
        assert!(w.vault().unwrap().exists(&NotePath::new("Shopping list.md")));
        assert!(!w.vault().unwrap().exists(&NotePath::new("Untitled.md")));
        // Empty new notes keep their placeholder name.
        let id = w.new_note(None).unwrap();
        w.close(id);
        assert!(w.vault().unwrap().exists(&NotePath::new("Untitled.md")));
    }

    fn ws() -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[
            ("Ideas.md", "# Ideas\nWrite about Rope Notes.\n- [ ] first\n"),
            ("Rope Notes.md", "# Rope Notes\nRopes are trees.\n"),
        ]));
        w.index().flush();
        w
    }

    fn type_at_end(w: &mut Workspace, s: &str) {
        let d = w.active_doc_mut().unwrap();
        let end = d.buffer().len_chars();
        d.set_selection(Selection::cursor(end));
        let new = format!("{}{s}", d.text());
        d.apply_widget_text(&new, Selection::cursor(end + s.chars().count()));
    }

    #[test]
    fn smart_enter_continues_lists_and_calculates() {
        let mut w = ws();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        let d = w.active_doc_mut().unwrap();
        let end_of_task = d.text().find("first").unwrap() + 5;
        d.set_selection(Selection::cursor(end_of_task));
        assert!(w.smart_enter());
        assert!(w.active_doc().unwrap().text().contains("- [ ] first\n- [ ] \n"));

        type_at_end(&mut w, "\nrent = 1200\nrent * 12 =");
        assert!(w.smart_enter());
        assert!(w.active_doc().unwrap().text().ends_with("rent * 12 = 14,400"));
    }

    #[test]
    fn dates_and_links() {
        let mut w = ws();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        type_at_end(&mut w, "Due @tomorrow");
        assert!(w.smart_space());
        let tomorrow = (chrono::Local::now().date_naive() + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        assert!(w.active_doc().unwrap().text().ends_with(&format!("Due [[{tomorrow}]] ")));

        let ins = w.insights();
        assert_eq!(ins.link_suggestions.len(), 1);
        let s = ins.link_suggestions[0].clone();
        w.apply_link_suggestion(&s);
        assert!(w.active_doc().unwrap().text().contains("Write about [[Rope Notes]]."));
    }

    #[test]
    fn save_reindexes_and_unresolved_links_create_notes() {
        let mut w = ws();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        type_at_end(&mut w, "See [[Brand New]]");
        let id = w.active_doc().unwrap().id;
        assert!(w.save(id));
        w.index().flush();
        assert_eq!(w.index().read().graph().unresolved()[0].0, "Brand New");
        w.open_link("Brand New", None).unwrap();
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Brand New.md");
        assert!(w.active_doc().unwrap().text().starts_with("# Brand New"));
    }

    #[test]
    fn rename_updates_open_docs_and_links() {
        let mut w = ws();
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        type_at_end(&mut w, "Link: [[Rope Notes]]");
        w.save(w.active_doc().unwrap().id);
        w.open_note(&NotePath::new("Rope Notes.md")).unwrap();
        w.rename_active("Text Ropes").unwrap();
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Text Ropes.md");
        w.set_active(0);
        assert!(w.active_doc().unwrap().text().contains("[[Text Ropes]]"));

        // A front-matter title that repeated the old name follows the rename.
        w.open_note(&NotePath::new("Ideas.md")).unwrap();
        let d = w.active_doc_mut().unwrap();
        d.apply(Transaction::insert(0, "---\ntitle: Ideas\nstatus: draft\n---\n"), EditKind::Other).unwrap();
        w.rename_active("Big Ideas").unwrap();
        let t = w.active_doc().unwrap().text().to_string();
        assert!(t.starts_with("---\ntitle: Big Ideas\nstatus: draft\n---\n"), "{t}");
        w.open_note(&NotePath::new("Text Ropes.md")).unwrap();
        w.rename_active("Ropes").unwrap();
        assert!(!w.active_doc().unwrap().text().contains("title:"), "no title is added when there was none");
    }

    #[test]
    fn commands_format_text() {
        let mut w = ws();
        w.open_note(&NotePath::new("Rope Notes.md")).unwrap();
        let d = w.active_doc_mut().unwrap();
        let s = d.text().find("Ropes are").unwrap();
        d.set_selection(Selection::new(s, s + 5));
        w.run(Command::Bold);
        assert!(w.active_doc().unwrap().text().contains("**Ropes** are"));
        w.run(Command::Undo);
        assert!(w.active_doc().unwrap().text().contains("\nRopes are"));
        assert_eq!(w.run(Command::QuickOpen), Some(UiRequest::QuickOpen));
    }
}
