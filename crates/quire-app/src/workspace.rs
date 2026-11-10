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

use crate::commands::{Command, Owner};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    Publish,
    NewProject,
    Shortcuts,
    Tour,
    SyncConflicts,
    SyncSettings,
    /// Pick a folder for a new note.
    NewInFolder,
    /// Pick a folder to move the active note to.
    MoveNote,
    /// Show two notes side by side (or go back to one).
    Split,
    /// Show these two notes side by side, the first on the left (opening
    /// them as needed); unlike `Split`, never closes a split.
    SideBySide(NotePath, NotePath),
    Graph,
    /// Put this text on the clipboard.
    Copy(String),
    /// Put this HTML on the clipboard, with its Markdown as the plain text
    /// ("Copy as HTML"). The frontend says whether it went as HTML.
    CopyHtml(crate::copy_html::CopiedHtml),
    /// The quick-capture box.
    Capture,
    /// A 0.9 feature's UI (a panel tab, a sidebar tab, a dialog); the
    /// frontend's feature registry shows it.
    Feature(Command),
    /// Scroll the editor to the caret (a feature moved it, e.g. to the
    /// next suggested change), without taking focus from where it is.
    RevealCaret,
    /// Open a file that isn't a Markdown note (`.base`, `.canvas`, `.pdf`)
    /// in the frontend's editor for it (a surface).
    OpenFile(NotePath),
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
type InsightsKey = (DocId, u64, u64, u64);
type InsightsCache = RefCell<Option<(InsightsKey, Rc<Insights>)>>;
/// Insights for a big note, being worked out on another thread.
type InsightsJob = RefCell<Option<(InsightsKey, Arc<std::sync::Mutex<Option<Insights>>>)>>;

/// Notes bigger than this get their insights worked out off the UI thread.
const INSIGHTS_SYNC_BYTES: usize = 64 * 1024;

pub struct Workspace {
    pub settings: Settings,
    pub focus_mode: bool,
    pub(crate) vault: Option<Vault>,
    vault_root: Option<PathBuf>,
    /// 0.12: a file opened on its own, outside any vault (`open_single_file`).
    single_file: Option<PathBuf>,
    pub(crate) index: IndexService,
    pub(crate) watcher: Option<VaultWatcher>,
    /// The watcher and the index load wait for `start_deferred`.
    pub(crate) deferred_start: bool,
    pub(crate) docs: Vec<Document>,
    pub(crate) active: Option<usize>,
    next_id: u64,
    pub(crate) notices: Vec<Notice>,
    pub(crate) assistant: Arc<dyn LanguageModel>,
    pending_assist: Option<PendingAssist>,
    pub assist_result: Option<AssistResult>,
    insights_cache: InsightsCache,
    insights_job: InsightsJob,
    #[cfg(feature = "typeset")]
    pub(crate) pdf_job: crate::export_pdf::PdfJob,
    /// Version history, written off the UI thread.
    pub(crate) history: crate::history_ops::HistoryWriter,
    pub find: crate::find_state::FindState,
    pub ask: crate::ask_state::AskState,
    pub(crate) file_cache: crate::attach::FileCache,
    pub(crate) view_cache: crate::properties::ViewCache,
    /// Recently active notes, most recent first.
    recent: Vec<NotePath>,
    pub(crate) conflict_count_cache: std::cell::Cell<Option<(u64, usize)>>,
    pub updates: crate::updates::UpdateState,
    pub sync: crate::vault_sync::SyncHandle,
    /// 0.9 features' state; see `features.rs`.
    pub features: crate::features::Features,
    /// Background jobs (design §9).
    pub jobs: crate::jobs::JobRunner,
    /// The agent store of the open vault.
    pub(crate) agent: Option<quire_agent::AgentStore>,
    device: quire_agent::DeviceInfo,
    /// UI asked for outside a command's return value ([`Workspace::request_ui`]).
    ui_requests: Vec<UiRequest>,
    /// The chat model features use ([`crate::ai::chat_model`]), built when
    /// AI settings change rather than on every call (reading a key may ask
    /// the OS keychain); `chat_generation` counts the changes.
    pub(crate) chat: Option<Arc<dyn quire_intel::chat::ChatModel>>,
    pub(crate) chat_generation: u64,
}

impl Workspace {
    pub fn new(settings: Settings) -> Self {
        let assistant = crate::ai::make_assistant(&settings);
        let chat = crate::ai::chat_model(&settings);
        Self {
            settings,
            focus_mode: false,
            vault: None,
            vault_root: None,
            single_file: None,
            index: IndexService::spawn(),
            watcher: None,
            deferred_start: false,
            docs: Vec::new(),
            active: None,
            next_id: 1,
            notices: Vec::new(),
            assistant,
            pending_assist: None,
            assist_result: None,
            insights_cache: RefCell::new(None),
            insights_job: RefCell::new(None),
            #[cfg(feature = "typeset")]
            pdf_job: None,
            history: crate::history_ops::HistoryWriter::spawn(),
            find: crate::find_state::FindState::default(),
            ask: crate::ask_state::AskState::default(),
            file_cache: crate::attach::FileCache::default(),
            view_cache: crate::properties::ViewCache::default(),
            recent: Vec::new(),
            conflict_count_cache: std::cell::Cell::new(None),
            updates: crate::updates::UpdateState::default(),
            sync: crate::vault_sync::SyncHandle::default(),
            features: crate::features::Features::default(),
            jobs: crate::jobs::JobRunner::new(),
            agent: None,
            device: crate::device::this_device(),
            ui_requests: Vec::new(),
            chat,
            chat_generation: 0,
        }
    }

    /// Ask the frontend for some UI from anywhere a command's return value
    /// can't carry it: a click in a note widget that opens a feature's
    /// dialog (`UiRequest::Feature`), a feature that moved the caret
    /// (`UiRequest::RevealCaret`), a link to a canvas (`UiRequest::OpenFile`).
    /// The frontend takes them once a frame, in order.
    pub fn request_ui(&mut self, req: UiRequest) {
        self.ui_requests.push(req);
    }

    /// The UI requests made since the last call (the frontend, once a frame).
    pub fn take_ui_requests(&mut self) -> Vec<UiRequest> {
        std::mem::take(&mut self.ui_requests)
    }

    /// Whether UI requests wait (the frontend repaints for them).
    pub const fn ui_requested(&self) -> bool {
        !self.ui_requests.is_empty()
    }

    /// This device (id and name, from `<config>/device.toml`).
    pub const fn device(&self) -> &quire_agent::DeviceInfo {
        &self.device
    }

    /// The open vault's agent store: proposals, ledger, inbox, policy.
    pub const fn agent_store(&self) -> Option<&quire_agent::AgentStore> {
        self.agent.as_ref()
    }

    /// Set up what 0.9 features need for a newly opened vault.
    fn vault_attached(&mut self, vault: Vault) {
        let agent = quire_agent::AgentStore::open(&vault, self.device.clone(), Arc::new(quire_agent::SystemClock));
        self.jobs.set_context(Some(crate::jobs::JobCx {
            // No keys: background jobs never read a locked folder (0.11).
            vault: vault.without_keys(),
            index: self.index.handle(),
            agent: agent.clone(),
            chat: None,
            user_active: self.jobs.user_active_flag(),
            cancel: quire_intel::chat::CancelFlag::new(),
        }));
        self.agent = Some(agent);
        self.features.decor_generation += 1;
        // The features see the vault that was just opened.
        self.vault = Some(vault);
        crate::features::vault_opened(self);
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
        if self.single_file.is_some() {
            return "Single file".into();
        }
        self.vault_root.as_deref().and_then(Path::file_name).map_or_else(|| "Notes".into(), |n| n.to_string_lossy().into_owned())
    }

    pub fn index(&self) -> &IndexService {
        &self.index
    }

    pub fn open_vault(&mut self, root: &Path) -> Result<(), String> {
        self.open_vault_deferred(root)?;
        self.start_deferred();
        Ok(())
    }

    /// 0.12 single-file mode: open one Markdown file without making its
    /// folder a vault. The folder isn't scanned or watched, nothing is
    /// written next to the file but the file itself, the last vault and
    /// its session stay as they were, and Quire's own data (history, the
    /// index) is kept in memory (`quire_vault::SingleFile`).
    pub fn open_single_file(&mut self, file: &Path) -> Result<DocId, String> {
        let (vault, note) = Vault::open_single_file(file).map_err(|e| e.to_string())?;
        self.save_all();
        self.docs.retain(|d| d.path.is_none());
        self.active = if self.docs.is_empty() { None } else { Some(0) };
        self.vault_root = None;
        self.watcher = None;
        self.deferred_start = false;
        self.single_file = file.canonicalize().ok().or_else(|| Some(file.to_path_buf()));
        self.attach_vault(vault);
        self.open_note(&note).ok_or_else(|| format!("Couldn't open {}", file.display()))
    }

    /// The file open in single-file mode, if that is what this is.
    pub fn single_file(&self) -> Option<&Path> {
        self.single_file.as_deref()
    }

    /// [`Self::open_vault`], except that reading every note into the index,
    /// watching the folder and syncing wait for [`Self::start_deferred`].
    /// The app opens its startup vault this way and starts them after its
    /// first frame, so a large vault doesn't delay the window (design §9:
    /// nothing but settings is read before the first frame).
    ///
    /// Sync starts only once the watcher runs, so every note it writes is
    /// seen and merged into the notes open here, never overwritten by them.
    pub fn open_vault_deferred(&mut self, root: &Path) -> Result<(), String> {
        // 0.11: the vault's locked folders are remembered on this device, apart from their key files.
        let vault = crate::secure_state::open_vault_now(root)?;
        self.save_all();
        self.docs.retain(|d| d.path.is_none());
        self.active = if self.docs.is_empty() { None } else { Some(0) };
        self.vault_root = Some(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
        self.single_file = None;
        self.watcher = None;
        self.deferred_start = true;
        self.vault_attached(vault);
        self.settings.last_vault = self.vault_root.clone();
        // The neural embedding cache lives inside the vault. 0.11: a vault
        // with AI off gets no embedder or chat model, from the start.
        self.apply_embedder();
        self.apply_ai_switch();
        Ok(())
    }

    /// Start what [`Self::open_vault_deferred`] left: the folder watcher and
    /// reading every note into the index. Does nothing when nothing waits.
    pub fn start_deferred(&mut self) {
        if !std::mem::take(&mut self.deferred_start) {
            return;
        }
        let (Some(vault), Some(root)) = (self.vault.clone(), self.vault_root.clone()) else { return };
        // 0.12: the on-device AI engine is started from a thread made here, at
        // normal priority (a job thread's nice 19 would carry over to it).
        quire_ai::engine::prepare_spawner();
        // 0.11: the locked folders are checked against the disk, in the background.
        crate::secure_state::start_pass(self);
        self.watcher = match VaultWatcher::start(&root) {
            Ok(w) => Some(w),
            Err(e) => {
                self.notify(NoticeLevel::Warn, format!("Not watching for outside changes: {e}"));
                None
            }
        };
        // Notes opened before the watcher ran (the restored session) may have
        // changed on disk since they were read: take those changes in now.
        self.recheck_open_notes();
        self.index.load(vault);
        // Only now: what sync writes from here on reaches the open notes.
        self.start_sync();
    }

    /// Bring every open note up to date with its file on disk, as if the
    /// watcher had seen it change (merged into unsaved edits, or reloaded).
    fn recheck_open_notes(&mut self) {
        let Some(vault) = self.vault.clone() else { return };
        let open: Vec<NotePath> = self.docs.iter().filter_map(|d| d.path.clone()).collect();
        for p in open {
            if let Ok(content) = vault.read(&p)
                && self.docs.iter().any(|d| d.path.as_ref() == Some(&p) && d.disk_text().is_some() && !d.matches_disk(&content))
            {
                self.history.keep(&vault, &p, &content, crate::history_ops::now_ms(), true);
                self.absorb_external_change(&p, &content);
            }
        }
    }

    /// Use an existing vault (tests, demos, in-memory scratch vaults).
    pub fn attach_vault(&mut self, vault: Vault) {
        self.index.load(vault.clone());
        self.vault_attached(vault);
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

    /// Make the open note `id` the active one. False if it isn't open.
    pub fn activate(&mut self, id: DocId) -> bool {
        match self.doc_pos(id) {
            Some(i) => {
                self.active = Some(i);
                true
            }
            None => false,
        }
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

    pub(crate) fn doc_pos(&self, id: DocId) -> Option<usize> {
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
            // 0.11: a locked note shows the folder's lock screen instead.
            Err(quire_vault::VaultError::Locked(_)) => {
                self.request_ui(UiRequest::OpenFile(quire_vault::lock::stored(path)));
                return None;
            }
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
        // `[[Projects/]]`: the folder's note, or the folder in the sidebar.
        if let Some(r) = crate::nav::folder_link(self, target) {
            return match r {
                crate::nav::FolderLink::Note(_) => self.active_doc().map(|d| d.id),
                crate::nav::FolderLink::Folder(_) => None,
            };
        }
        // WS-G: a PDF, canvas or other file opens in its editor (`[[paper.pdf#page=3]]`).
        if crate::canvas_state::open_file_link(self, target, heading) {
            return None;
        }
        let from = self.active_doc().and_then(|d| d.path.clone());
        // 0.11: a note in a locked folder isn't in the index; never make a new one in its place.
        let resolved = self.index.read().resolve(target, from.as_ref()).or_else(|| crate::secure_state::resolve_locked(self, target));
        let id = match resolved {
            Some(p) => self.open_note(&p)?,
            None if target.trim().is_empty() => self.active_doc()?.id,
            None => {
                let title = quire_vault::sanitize_title(target);
                let id = self.new_note(Some(&title))?;
                // 0.12: the vault's default template (or its folder's) makes the body.
                self.features.tasks.checked_new.insert(id);
                if !crate::templates_ui_state::apply_default_template(self, id) {
                    let body = format!("# {title}\n\n");
                    if let Some(doc) = self.active_doc_mut() {
                        let _ = doc.apply(
                            Transaction::insert(0, body.clone()).with_selection(Selection::cursor(body.chars().count())),
                            EditKind::Other,
                        );
                    }
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
        // `[[Note#^id]]`: a paragraph, not a heading.
        if let Some(id) = heading.strip_prefix('^') {
            let text = doc.text();
            if let Some(r) = quire_markdown::callout::find_block(&text, id) {
                let c = doc.byte_to_char(r.start);
                doc.set_selection(Selection::cursor(c));
            } else {
                self.notify(NoticeLevel::Warn, format!("No paragraph with id ^{id} in this note"));
            }
            return;
        }
        let target = quire_markdown::outline::slugify(heading);
        if let Some(h) = doc.outline().into_iter().find(|h| h.anchor == target || h.text.eq_ignore_ascii_case(heading)) {
            let c = doc.byte_to_char(h.offset);
            doc.set_selection(Selection::cursor(c));
        }
    }

    /// A `[[link]]` to the paragraph (or heading) under the caret. A
    /// paragraph without an id gets one (` ^abc123` at its end, undoable).
    pub fn paragraph_link(&mut self) -> Option<String> {
        let doc = self.active_doc()?;
        let Some(stem) = doc.path.as_ref().map(|p| p.stem().to_string()) else {
            self.notify(NoticeLevel::Warn, "Save the note first to link to it");
            return None;
        };
        let buf = doc.buffer();
        let line = buf.char_to_line(doc.selection().head);
        let text = doc.text();
        let line_text = buf.line(line);
        let caret_byte = buf.char_to_byte(buf.line_range(line).start);
        if quire_markdown::blocks::verbatim_blocks(&text).iter().any(|r| r.contains(&caret_byte)) || line_text.trim().is_empty() {
            self.notify(NoticeLevel::Warn, "Put the caret in a paragraph or list item to link to it");
            return None;
        }
        // A heading links by its text.
        let trimmed = line_text.trim_start();
        if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
            return Some(format!("[[{stem}#{}]]", trimmed.trim_start_matches('#').trim()));
        }
        // The block's last line: a list item is one line; a paragraph runs to the next blank line.
        let is_item = |t: &str| {
            let t = t.trim_start();
            t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("+ ")
                || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        // Where another kind of block starts, the paragraph has ended (an id
        // on a fence or `$$` line would break that block).
        let quote = line_text.trim_start().starts_with('>');
        let starts_block = |t: &str| {
            let t = t.trim_start();
            t.starts_with('#')
                || t.starts_with("```")
                || t.starts_with("~~~")
                || t.starts_with("$$")
                || t.starts_with('|')
                || t.starts_with('<')
                || t.starts_with("---")
                || (t.starts_with('>') != quote)
        };
        let mut last = line;
        let item = is_item(&line_text);
        while last + 1 < buf.len_lines() {
            let next = buf.line(last + 1);
            if next.trim().is_empty() || is_item(&next) || starts_block(&next) {
                break;
            }
            // A list item goes on through its indented continuation lines only.
            if item && !next.starts_with([' ', '\t']) {
                break;
            }
            last += 1;
        }
        let last_text = buf.line(last);
        if let Some((_, id)) = quire_markdown::callout::block_id(&last_text) {
            return Some(format!("[[{stem}#^{id}]]"));
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.as_nanos() as u64);
        let id = quire_markdown::callout::new_block_id(&text, seed);
        let end = buf.line_range(last).end;
        let sel = doc.selection();
        let doc = self.active_doc_mut()?;
        let tx = Transaction::insert(end, format!(" ^{id}")).with_selection(sel);
        doc.apply(tx, EditKind::Other).ok()?;
        Some(format!("[[{stem}#^{id}]]"))
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
        if let Some(orig) = self.docs[i].disk_text()
            && *orig != *text
        {
            self.history.keep_first(vault, &path, &orig, now - 1);
        }
        let prov = self.docs[i].next_save_provenance.as_ref().map(|p| {
            // The provenance describes exactly the text being saved.
            let mut p = p.clone();
            p.hash = quire_vault::ContentHash::of(&text);
            p.at_ms = now;
            p.to_json()
        });
        // Written only over what this note last read or wrote: a change that
        // landed meanwhile (sync, another program) and wasn't taken in yet is
        // merged first, never overwritten. A file gone meanwhile is put back.
        let base = self.docs[i].disk_text();
        let written = match &base {
            // 0.11: a locked folder's note is written encrypted, through its key.
            Some(b) if vault.locked_folder(&path).is_some() => crate::secure_state::save_locked(vault, &path, b, &text),
            Some(b) => vault.storage().write_bytes_if(
                &path,
                &|now| now.is_none_or(|n| String::from_utf8_lossy(n.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(n)) == **b),
                text.as_bytes(),
            ),
            None => vault.write(&path, &text).map(|()| true),
        };
        let written = match written {
            Ok(false) => {
                if let Ok(content) = vault.read(&path) {
                    let vault = vault.clone();
                    self.history.keep(&vault, &path, &content, now, true);
                    self.absorb_external_change(&path, &content);
                }
                return false;
            }
            Ok(true) => Ok(()),
            Err(e) => Err(e),
        };
        let Some(vault) = self.vault.as_ref() else { return false };
        match written {
            Ok(()) => {
                self.history.keep_with(vault, &path, &text, now, false, prov);
                self.docs[i].next_save_provenance = None;
                self.docs[i].mark_saved(&text);
                // Templates stay out of the index (and the Files tab), as at
                // start; so do files shown as text (a canvas under review).
                if path.is_markdown() && !vault.is_template(&path) {
                    self.index.upsert(path, text.to_string());
                }
                self.sync_nudge();
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save: {e}"));
                false
            }
        }
    }

    /// Save every note with unsaved edits: tabs, and notes edited through
    /// an embed. Untitled notes keep their names (unlike [`Self::save_all`]).
    pub fn save_open_notes(&mut self) {
        let dirty: Vec<DocId> = self.docs.iter().filter(|d| d.is_dirty() && d.path.is_some()).map(|d| d.id).collect();
        for id in dirty {
            self.save(id);
        }
        crate::nav::save_hidden_docs(self);
    }

    pub fn save_all(&mut self) {
        self.save_open_notes();
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
        // 0.11: a note in a locked folder keeps its neutral name: its first
        // line is its text, and file names aren't encrypted.
        if self.vault.as_ref().is_some_and(|v| v.locked_folder(&path).is_some()) {
            return;
        }
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
        #[cfg(feature = "typeset")]
        let mut changed = self.poll_pdf_job();
        #[cfg(not(feature = "typeset"))]
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
        // 0.12: the vault's file list arrives from its own thread.
        if self.file_list_pending() {
            self.refresh_file_cache();
            changed |= !self.file_list_pending();
        }
        changed |= self.poll_assist();
        changed |= self.poll_ask();
        self.maybe_check_updates();
        changed |= self.poll_updates();
        changed |= self.poll_sync();
        // Background jobs wait while the person types.
        let pause = Duration::from_millis(self.settings.jobs.pause_on_typing_ms);
        self.jobs.set_user_active(self.docs.iter().filter_map(Document::last_edit).any(|t| now.saturating_duration_since(t) < pause));
        changed |= crate::features::tick(self, now);
        changed
    }

    /// Earliest instant at which `tick` has work to do: autosave, or a feature's deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        let delay = Duration::from_millis(self.settings.autosave_ms);
        let autosave =
            self.docs.iter().filter(|d| d.path.is_some() && d.is_dirty()).filter_map(Document::last_edit).map(|t| t + delay).min();
        let listing = self.file_list_pending().then(|| Instant::now() + Duration::from_millis(50));
        autosave.into_iter().chain(listing).chain(crate::features::next_deadline(self)).min()
    }

    fn process_vault_events(&mut self) -> bool {
        let Some(w) = &self.watcher else { return false };
        let events: Vec<VaultEvent> = w.events.try_iter().collect();
        if events.is_empty() {
            return false;
        }
        let mut seen = std::collections::HashSet::new();
        for ev in events.iter().rev().cloned() {
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
                        self.history.keep(v, &p, &content, crate::history_ops::now_ms(), true);
                    }
                    if self.absorb_external_change(&p, &content) {
                        continue; // our own save echoing back
                    }
                    if self.vault.as_ref().is_some_and(|v| v.is_template(&p)) {
                        continue; // templates aren't indexed (as at start)
                    }
                    self.index.upsert(p, content);
                }
                VaultEvent::Removed(p) => {
                    if seen.insert(p.clone()) {
                        self.index.remove(p);
                    }
                }
                // PDFs, images and canvases: the features that use them
                // listen below. The list of files is read again.
                VaultEvent::AttachmentChanged(_) | VaultEvent::AttachmentRemoved(_) => {
                    self.file_cache.clear();
                }
            }
        }
        for ev in &events {
            crate::features::vault_event(self, ev);
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
    pub(crate) fn track_recent(&mut self) {
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

    /// Recently opened notes including the open one (Home is a tab of its own, so the note you
    /// just left is one of the notes to come back to).
    ///
    /// Never waits for the index worker (this runs every frame while Home is
    /// open): `None` while it holds the write lock.
    pub(crate) fn recent_with_active(&self, limit: usize) -> Option<Vec<NotePath>> {
        let ix = self.index.try_read()?;
        Some(self.recent.iter().filter(|p| ix.get(p).is_some()).take(limit).cloned().collect())
    }

    /// Re-index and re-point open documents after a rename or move.
    /// Returns how many other notes had links rewritten.
    fn apply_path_change(&mut self, vault: &Vault, report: &quire_vault::RenameReport) -> usize {
        // 0.11: a running timer follows its note.
        self.timer_path_moved(&report.old_path, &report.new_path);
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
        crate::agent_state::path_changed(self, &report.old_path, &report.new_path);
        // 0.11: a plain note moved into a locked folder: what this device
        // derived from its plain text (agent suggestions, caches) goes.
        if vault.locked_folder(&report.old_path).is_none()
            && let Some(f) = vault.locked_folder(&report.new_path)
        {
            crate::secure_state::forget_derived(self, &f.path);
        }
        report.updated.iter().filter(|p| **p != report.new_path).count()
    }

    /// Forget the per-note caches the window keeps (insights), e.g. after a
    /// locked folder was locked.
    pub(crate) fn clear_note_caches(&self) {
        *self.insights_cache.borrow_mut() = None;
        // 0.12: views and the agenda may hold text of notes of unlocked folders.
        self.view_cache.clear();
        self.features.tasks.agenda.borrow_mut().take();
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
        if crate::secure_state::refuse_export(self) {
            return None;
        }
        let doc = self.active_doc()?;
        let title = doc.title();
        // WS-G: `[@key]` citations as author-date text and a reference list.
        let text = crate::citations::render_source(self, &doc.text());
        let html = {
            let ix = self.index.read();
            let from = doc.path.clone();
            let vault = self.vault.as_ref();
            let body = quire_markdown::render::to_html(&text, &|l| {
                ix.resolve(&l.target, from.as_ref()).map(|p| {
                    // 0.10: an Excalidraw drawing goes in as an SVG picture.
                    if l.embed
                        && quire_canvas::excalidraw::is_drawing(p.as_str())
                        && let Some(svg) = vault.and_then(|v| crate::drawings::svg(v, &p, &|_| true))
                    {
                        return crate::drawings::data_url(&svg);
                    }
                    if l.embed { p.as_str().to_string() } else { format!("{}.html", quire_markdown::render::encode_url(p.stem())) }
                })
            });
            // The page is opened in a browser: the note's own HTML is
            // cleaned as for the web app and published sites.
            quire_markdown::render::html_document(&title, &crate::sanitize::note_html(&body))
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

    /// "Copy as HTML" (issue #17): the selection in the active note, or the
    /// whole note; or the note at `path` (from a note's menu), as it is in
    /// its tab if open. Rendered as published sites and the web app render
    /// notes: see [`crate::copy_html`].
    pub fn copy_as_html(&self, path: Option<&NotePath>) -> Option<crate::copy_html::CopiedHtml> {
        let doc = match path {
            Some(p) => self.docs.iter().find(|d| d.path.as_ref() == Some(p)),
            None => self.active_doc(),
        };
        let (src, selection) = match doc {
            Some(d) if path.is_none() && !d.selection().is_empty() => (d.buffer().slice(d.selection().range()), true),
            Some(d) => (d.text().to_string(), false),
            None => (self.vault.as_ref()?.read(path?).ok()?, false),
        };
        let note = path.cloned().or_else(|| doc.and_then(|d| d.path.clone()));
        let html = match &self.vault {
            Some(vault) => {
                let note = note.unwrap_or_else(|| NotePath::new(format!("{}.md", doc.map(Document::title).unwrap_or_default())));
                crate::copy_html::render(vault, &self.index.read(), &self.features.citations.library, &note, &src)
            }
            None => crate::copy_html::render_without_vault(&crate::citations::render_source(self, &src)),
        };
        Some(crate::copy_html::CopiedHtml { html, text: crate::copy_html::plain_text(&src), selection })
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
            // WS-G: a `[@key]` citation opens its PDF or library entry.
            None => crate::citations::follow_at_caret(self),
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
        // 0.11: Markdown-aware formatting toggles (`format_cmds`).
        if crate::format_cmds::handles(cmd) {
            return crate::format_cmds::run(self, cmd);
        }
        if cmd.owner() != Owner::Core {
            return crate::features::run(self, cmd);
        }
        match cmd {
            C::NewNote => {
                // In the open note's folder (where its template applies), unless set to the top.
                let folder = (self.settings.editing.new_note_folder != "root")
                    .then(|| self.active_doc().and_then(|d| d.path.as_ref()).map(|p| p.folder().to_string()))
                    .flatten()
                    .filter(|f| !f.is_empty());
                match folder {
                    Some(f) => {
                        if let Err(e) = self.new_note_in(&f) {
                            self.notify(NoticeLevel::Error, e);
                        }
                    }
                    None => {
                        self.new_note(None);
                    }
                }
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
            C::AddNextOccurrence
            | C::SelectAllOccurrences
            | C::AddCursorAbove
            | C::AddCursorBelow
            | C::ToggleFold
            | C::FoldAll
            | C::UnfoldAll => {
                return Some(UiRequest::Editor(cmd));
            }
            C::Scratchpad => {
                self.open_scratchpad("");
            }
            C::Save => {
                if let Some(id) = self.active_doc().map(|d| d.id) {
                    // 0.12: `[lint.format] on-save` tidies the note first (explicit saves only).
                    self.lint_before_save(id);
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
            C::CopyAsHtml => return self.copy_as_html(None).map(UiRequest::CopyHtml),
            #[cfg(feature = "typeset")]
            C::ExportPdf => self.export_active_pdf(),
            #[cfg(not(feature = "typeset"))]
            C::ExportPdf => self.notify(NoticeLevel::Info, "PDF export comes with the full edition of Quire (this is Quire Lite). Settings → About has the download; switching keeps your notes and settings."),
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
            C::SyncSettings => return Some(UiRequest::SyncSettings),
            C::SyncNow => {
                if self.sync.config.is_some() {
                    self.sync_nudge();
                } else {
                    return Some(UiRequest::SyncSettings);
                }
            }
            C::NewProject => return self.vault.is_some().then_some(UiRequest::NewProject),
            C::NewInFolder => return self.vault.is_some().then_some(UiRequest::NewInFolder),
            C::MoveNote => return self.active_doc().and_then(|d| d.path.as_ref()).map(|_| UiRequest::MoveNote),
            C::ImportNotes => return Some(UiRequest::Import),
            C::SplitEditor => return Some(UiRequest::Split),
            C::QuickCapture => return self.vault.is_some().then_some(UiRequest::Capture),
            C::CopyParagraphLink => {
                if let Some(link) = self.paragraph_link() {
                    self.notify(NoticeLevel::Info, format!("Copied {link}"));
                    return Some(UiRequest::Copy(link));
                }
            }
            C::ShowGraph => return self.vault.is_some().then_some(UiRequest::Graph),
            C::PublishSite => return self.vault.is_some().then_some(UiRequest::Publish),
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
            // 0.9 commands are handled above (`features::run`).
            C::OpenInbox
            | C::NextHunk
            | C::PrevHunk
            | C::AcceptHunk
            | C::AcceptAllInFile
            | C::RejectHunk
            | C::RejectAllInFile
            | C::ShowAgentActivity
            | C::UndoAgentChanges
            | C::CreateAgentInstructions
            | C::LintVault
            | C::AgentSettings
            | C::CaptureWebPage
            | C::ConnectAgent
            | C::CheckCitations
            | C::ThinkingTimeline
            | C::MarkReviewed
            | C::ToggleAgentPanel
            | C::IngestIntoWiki
            | C::AiSetup
            | C::ConnectExternalAgent
            | C::RecordVoiceNote
            | C::ShowAgenda
            | C::ShowCalendar
            | C::OpenWeeklyNote
            | C::OpenMonthlyNote
            | C::OpenYearlyNote
            | C::NextPeriod
            | C::PrevPeriod
            | C::StartReview
            | C::ReviewThisNote
            | C::AddCardIds
            | C::RenameTag
            | C::RenameProperty
            | C::ConvertTaskToNote
            | C::ConvertToView
            | C::ConvertNoteToTask
            | C::NewTask
            | C::StartStopTimer
            | C::StartPomodoro
            | C::StopTimer
            | C::TimeReport
            | C::ToggleVim
            | C::ChooseTheme
            | C::GitCommitNow
            | C::GitPull
            | C::GitPush
            | C::ToggleTwoPaneBrowser
            | C::OpenHomepage
            | C::NewCanvas
            | C::InsertSketch
            | C::InsertCitation
            | C::ImportBibliography
            | C::LockFolder
            | C::UnlockFolder
            | C::LockNow
            | C::ChangeFolderPassphrase
            | C::StopLockingFolder
            | C::AgentConnections
            | C::ShowRecoveryKey => return crate::features::run(self, cmd),
            // 0.11 editor: formatting runs in `format_cmds` above; the rest in `nav`.
            C::Heading4
            | C::Heading5
            | C::Heading6
            | C::BulletList
            | C::NumberedList
            | C::InsertMarkdownLink
            | C::InsertCallout
            | C::InsertTable
            | C::InlineMath
            | C::MathBlock
            | C::CodeBlock
            | C::InsertFootnote
            | C::InsertRule
            | C::InsertAttachment
            | C::ToggleToolbar
            | C::FocusToolbar
            | C::CustomizeCommands => return crate::features::run(self, cmd),
            // 0.12 typed links (`links_state`).
            C::AddTypedLink | C::GoToParent | C::GoToNextSibling | C::GoToPrevSibling | C::ShowHierarchy => {
                return crate::features::run(self, cmd);
            }
            // 0.12 switching tools (`switch_state`).
            C::LintNote | C::LintFolder | C::OpenHome | C::PinToHome | C::UndoImport => return crate::features::run(self, cmd),
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
        // 0.11: a locked note's text never goes to an AI provider.
        if crate::secure_state::refuse_ai_for_active(self) {
            return;
        }
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
        let key = (doc.id, self.index.version(), self.index.semantic_version(), self.features.secure.locked_version);
        if let Some((k, v)) = &*self.insights_cache.borrow()
            && *k == key
        {
            return Rc::clone(v);
        }
        let text = doc.text();
        let path = doc.path.clone();
        let large = doc.large;
        let plain = doc.extracted().plain.clone();
        if text.len() <= INSIGHTS_SYNC_BYTES {
            let ix = self.index.read();
            let mut ins = compute_insights(&ix, self.index.semantic().as_deref(), path.as_ref(), &text, &plain, large);
            drop(ix);
            self.add_overlay_insights(&mut ins, path.as_ref());
            let rc = Rc::new(ins);
            *self.insights_cache.borrow_mut() = Some((key, Rc::clone(&rc)));
            return rc;
        }
        // A big note: keep showing the last insights for it while new ones
        // are worked out, so typing never waits on them.
        let mut job = self.insights_job.borrow_mut();
        let done = job.as_ref().filter(|(k, _)| *k == key).and_then(|(_, slot)| slot.lock().ok()?.take());
        if let Some(mut ins) = done {
            *job = None;
            self.add_overlay_insights(&mut ins, path.as_ref());
            let rc = Rc::new(ins);
            *self.insights_cache.borrow_mut() = Some((key, Rc::clone(&rc)));
            return rc;
        }
        if job.as_ref().is_none_or(|(k, _)| *k != key) {
            let slot = Arc::new(std::sync::Mutex::new(None));
            let (out, index, semantic, text) = (Arc::clone(&slot), self.index.shared(), self.index.semantic(), text.to_string());
            std::thread::spawn(move || {
                quire_index::background::lower_priority();
                let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                let ins = compute_insights(&ix, semantic.as_deref(), path.as_ref(), &text, &plain, large);
                if let Ok(mut o) = out.lock() {
                    *o = Some(ins);
                }
            });
            *job = Some((key, slot));
        }
        match &*self.insights_cache.borrow() {
            Some((k, v)) if k.0 == key.0 => Rc::clone(v),
            _ => Rc::default(),
        }
    }

    /// 0.12: backlinks the vault index can't give (from or to notes of
    /// unlocked folders), from the in-memory overlay.
    fn add_overlay_insights(&self, ins: &mut Insights, path: Option<&NotePath>) {
        if let Some(p) = path {
            ins.backlinks.extend(self.overlay_backlinks(p));
        }
    }

    /// True while insights for a big note are being worked out.
    pub fn insights_pending(&self) -> bool {
        self.insights_job.borrow().is_some()
    }

    /// Outgoing internal links of the active note, resolved.
    pub fn outgoing(&self) -> Vec<(String, Option<NotePath>)> {
        let Some(doc) = self.active_doc() else {
            return vec![];
        };
        let mut seen = std::collections::HashSet::new();
        // 0.12: links to and from notes of unlocked folders resolve too.
        self.with_notes(|ix| {
            doc.extracted()
                .links
                .iter()
                .filter(|l| matches!(l.kind, LinkKind::Wiki | LinkKind::Relative) && !l.target.is_empty())
                .filter(|l| seen.insert(l.target.to_lowercase()))
                .map(|l| (l.target.clone(), ix.resolve(&l.target, doc.path.as_ref())))
                .collect()
        })
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
        self.jobs.set_context(None);
        self.save_all();
        // 0.11: edits in a locked folder that couldn't be saved are kept encrypted.
        self.keep_unsaved_locked();
        self.history.flush();
        // A single file isn't a vault: the last vault's tabs stay as they were.
        if self.single_file.is_none()
            && let Some(p) = Session::path()
        {
            let _ = self.session().save_to(&p);
        }
        let _ = self.settings.save();
    }
}

/// Backlinks, related notes and suggestions for one note.
fn compute_insights(
    ix: &quire_index::Index,
    semantic: Option<&quire_index::semantic::SemanticIndex>,
    path: Option<&NotePath>,
    text: &str,
    plain: &str,
    large: bool,
) -> Insights {
    let mut ins = Insights::default();
    if let Some(path) = path {
        ins.backlinks = ix.backlinks(path).to_vec();
        ins.unlinked = quire_intel::unlinked_mentions(ix, path);
        ins.related = quire_intel::related_notes_with(ix, semantic, path, 6);
        ins.tag_suggestions = quire_intel::suggest_tags(ix, path, 5);
    }
    if !large {
        ins.link_suggestions = quire_intel::suggest_links(text, ix, path).into_iter().take(20).collect();
        ins.summary = quire_intel::summarize(plain, 3);
    }
    ins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_links_put_ids_where_they_belong() {
        let mut w = ws();
        let p = NotePath::new("Para.md");
        let src = "Some text\n~~~\ncode\n~~~\n\nThe energy is\n$$\nE=mc^2\n$$\n\n- item\n  more of it\n- next\n\n# Head\n";
        w.vault().unwrap().write(&p, src).unwrap();
        w.open_note(&p).unwrap();
        let link_at = |w: &mut Workspace, needle: &str| {
            let d = w.active_doc_mut().unwrap();
            let at = d.text().find(needle).unwrap();
            let c = d.byte_to_char(at);
            d.set_selection(Selection::cursor(c));
            w.paragraph_link().unwrap()
        };
        let l1 = link_at(&mut w, "Some text");
        let l2 = link_at(&mut w, "The energy");
        let l3 = link_at(&mut w, "item");
        assert_eq!(link_at(&mut w, "Head"), "[[Para#Head]]");
        let text = w.active_doc().unwrap().text().to_string();
        let id = |l: &str| l.trim_end_matches("]]").rsplit('^').next().unwrap().to_string();
        assert!(text.contains(&format!("Some text ^{}\n~~~", id(&l1))), "{text}");
        assert!(text.contains(&format!("The energy is ^{}\n$$", id(&l2))), "{text}");
        assert!(text.contains(&format!("  more of it ^{}\n- next", id(&l3))), "{text}");
        // The fence and the math still close.
        assert_eq!(quire_markdown::blocks::figures(&text).len(), 1);
        // Asking again reuses the id.
        assert_eq!(link_at(&mut w, "The energy"), l2);
    }

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

    /// The joins the integration added (docs/design/F0-notes.md, wave 1).
    #[test]
    fn integration_wave_1_joins() {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[
            ("Calls/Kickoff.md", "# Kickoff\n"),
            ("Projects/Projects.md", "# All projects\n"),
            ("Areas/x.md", "x"),
            ("Templates/Call.md", "---\nquire-template:\n  applies_to: [Calls]\n  name: \"Call {{prompt:Who}}\"\n---\n# Call\n"),
            ("People/Ada.md", "---\ntitle: Ada Lovelace\nauthor:\n  name: Ada\n  born: 1815\n---\nBody\n"),
        ]));
        w.index().flush();
        // Ctrl+N: in the open note's folder, where its template applies.
        w.open_note(&NotePath::new("Calls/Kickoff.md")).unwrap();
        assert_eq!(w.run(Command::NewNote), None);
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().folder(), "Calls");
        w.settings.editing.new_note_folder = "root".into();
        w.run(Command::NewNote);
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().folder(), "");
        // Follow link on a folder: its note, or the folder in the sidebar.
        assert!(w.open_link("Projects/", None).is_some());
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Projects/Projects.md");
        assert!(w.open_link("Areas/", None).is_none());
        assert!(!w.vault().unwrap().exists(&NotePath::new("Areas.md")), "no note made for a folder link");
        // A template's name: with a typed title, the title wins.
        let id = w
            .new_note_from_template_titled(
                &NotePath::new("Templates/Call.md"),
                Some("Board sync"),
                None,
                &std::collections::HashMap::new(),
                &[],
            )
            .unwrap();
        assert_eq!(w.doc(id).unwrap().path.as_ref().unwrap().stem(), "Board sync");
        // Nested properties edit one line.
        w.open_note(&NotePath::new("People/Ada.md")).unwrap();
        assert!(w.set_property_path(&["author", "born"], &quire_markdown::Value::Str("1816".into())));
        assert!(w.active_doc().unwrap().text().contains("  born: 1816\n"));
        assert!(w.remove_property_path(&["author", "name"]));
        assert!(!w.active_doc().unwrap().text().contains("name: Ada"));
        // UI requests queue up for the frontend.
        w.request_ui(UiRequest::RevealCaret);
        assert!(w.ui_requested());
        assert_eq!(w.take_ui_requests(), vec![UiRequest::RevealCaret]);
        assert!(!w.ui_requested());
        // The graph shows file names unless titles are turned on.
        let g = w.link_graph(&crate::graph::Scope::All { orphans: true });
        assert!(g.nodes.iter().any(|n| n.title == "Ada"));
        w.settings.editing.display_titles = true;
        let g = w.link_graph(&crate::graph::Scope::All { orphans: true });
        assert!(g.nodes.iter().any(|n| n.title == "Ada Lovelace"));
    }

    /// An agent's suggestion accepted in the editor is saved with provenance
    /// that Version history shows (WS-A's review, F0's history).
    #[test]
    fn accepted_suggestions_carry_provenance_into_history() {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[("Rope.md", "# Rope\nOne.\n")]));
        let store = w.agent_store().unwrap().clone();
        let agent = quire_agent::Actor::agent("mcp:claude-code", "Claude Code", quire_agent::Via::McpStdio, "d");
        let cs = quire_agent::ChangeSet::one(quire_agent::Change::Edit { path: "Rope.md".into(), new_text: "# Rope\nOne!\n".into() });
        let quire_agent::SubmitOutcome::Proposed { id, .. } = store.submit(&agent, cs).unwrap() else {
            panic!("a human note gets a proposal")
        };
        assert!(w.start_review(&id, Some(&NotePath::new("Rope.md"))));
        w.review_all_in_file(true);
        w.history.flush();
        assert_eq!(&*w.active_doc().unwrap().text(), "# Rope\nOne!\n");
        let newest = w.note_history().into_iter().next().unwrap();
        let prov = w.snapshot_provenance(&newest).expect("accepted versions say who");
        assert_eq!(prov.accepted_from.map(|a| a.id), Some("mcp:claude-code".to_string()));
        assert!(w.features.agent.review.is_none(), "nothing left to review");
    }
}
