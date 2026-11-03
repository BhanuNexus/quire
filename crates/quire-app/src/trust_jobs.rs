// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-C's entry point into the workspace.
//!
//! Trust tools (design §C): citation drift and verify checks, claims and
//! contradictions, staleness and the thinking timeline.
//!
//! `features.rs` calls the functions below for every Trust-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-C. State lives in
//! `ws.features.trust`; the jobs share [`TrustShared`] with it.

pub mod citations;
pub mod claims;
pub mod stale;
pub mod timeline;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use quire_agent::{Actor, Change, ChangeSet};
use quire_index::semantic::SemanticIndex;
use quire_intel::anchor::{self, Anchor, AnchorStatus};
use quire_intel::chat::{ChatModel, LanguageModelChat, Locality};
use quire_markdown::Value;
use quire_vault::{NotePath, VaultEvent};

use crate::Command;
use crate::commands::Owner;
use crate::decor::{ActionId, Decoration, DecorationEvent, StatusItem, Tone, WidgetId};
use crate::document::Document;
use crate::settings::TrustSettings;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

pub use citations::CiteCheck;
pub use claims::ClaimsStatus;
pub use stale::{StalePrefs, StaleRun};
pub use timeline::{TimelineResult, TimelineState, Topic};

/// What the trust jobs and the workspace share.
pub struct TrustShared {
    /// False once the vault closed: the jobs end.
    pub(crate) alive: AtomicBool,
    /// Bumped by every change to the notes.
    pub changes: AtomicU64,
    /// Notes changed since the citation check last looked.
    pub(crate) drift_changed: Mutex<Vec<NotePath>>,
    /// "Check citations" requests.
    pub(crate) check_now: Mutex<Vec<NotePath>>,
    /// Finished requests: (note, citations, verified by a model).
    pub(crate) checked: Mutex<Vec<(NotePath, usize, bool)>>,
    /// The latest check of each citing note.
    pub citations: Mutex<HashMap<NotePath, Vec<CiteCheck>>>,
    /// Bumped when results change (decorations, Inbox).
    pub generation: AtomicU64,
    /// Score staleness now.
    pub stale_now: AtomicBool,
    pub stale: Mutex<StaleRun>,
    pub claims: Mutex<ClaimsStatus>,
    settings: Mutex<TrustSettings>,
    chat: Mutex<Option<Arc<dyn ChatModel>>>,
    semantic: Mutex<Option<Arc<SemanticIndex>>>,
    pub semantic_version: AtomicU64,
}

impl Default for TrustShared {
    fn default() -> Self {
        Self {
            alive: AtomicBool::new(true),
            changes: AtomicU64::new(0),
            drift_changed: Mutex::default(),
            check_now: Mutex::default(),
            checked: Mutex::default(),
            citations: Mutex::default(),
            generation: AtomicU64::new(0),
            stale_now: AtomicBool::new(false),
            stale: Mutex::default(),
            claims: Mutex::default(),
            settings: Mutex::default(),
            chat: Mutex::default(),
            semantic: Mutex::default(),
            semantic_version: AtomicU64::new(0),
        }
    }
}

impl TrustShared {
    pub fn settings(&self) -> TrustSettings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_settings(&self, s: TrustSettings) {
        if let Ok(mut g) = self.settings.lock() {
            *g = s;
        }
    }

    /// The AI model jobs may use (only for features the person turned on).
    pub fn chat(&self) -> Option<Arc<dyn ChatModel>> {
        self.chat.lock().ok().and_then(|c| c.clone())
    }

    pub fn set_chat(&self, chat: Option<Arc<dyn ChatModel>>) {
        if let Ok(mut g) = self.chat.lock() {
            *g = chat;
        }
    }

    pub fn semantic(&self) -> Option<Arc<SemanticIndex>> {
        self.semantic.lock().ok().and_then(|s| s.clone())
    }

    pub fn set_semantic(&self, sem: Option<Arc<SemanticIndex>>, version: u64) {
        if let Ok(mut g) = self.semantic.lock() {
            *g = sem;
        }
        self.semantic_version.store(version, Ordering::Release);
    }

    pub(crate) fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// "Ask agent to reconcile" waiting for the agent panel to pick it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRequest {
    /// What to ask the agent, ready to send.
    pub prompt: String,
    pub anchors: Vec<Anchor>,
}

/// WS-C's state, kept in `ws.features.trust`.
pub struct TrustState {
    pub shared: Arc<TrustShared>,
    pub timeline: TimelineState,
    /// Passages shown highlighted ("Open both"): note and 1-based lines.
    pub highlights: Vec<(NotePath, (u32, u32))>,
    /// For the agent panel: a contradiction to reconcile.
    pub reconcile: Option<ReconcileRequest>,
    /// Flagged citations per citing note (for the banner).
    flagged: HashMap<NotePath, usize>,
    seen_generation: u64,
    seen_semantic: u64,
    chat_key: Option<String>,
    settings_seen: Option<TrustSettings>,
}

impl Default for TrustState {
    fn default() -> Self {
        Self {
            shared: Arc::default(),
            timeline: TimelineState::default(),
            highlights: Vec::new(),
            reconcile: None,
            flagged: HashMap::new(),
            seen_generation: 0,
            seen_semantic: u64::MAX,
            chat_key: None,
            settings_seen: None,
        }
    }
}

impl std::fmt::Debug for TrustState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustState").field("highlights", &self.highlights).finish_non_exhaustive()
    }
}

impl Drop for TrustState {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
    }
}

impl TrustState {
    /// How many citations in `path` need a look.
    pub fn flagged(&self, path: &NotePath) -> usize {
        self.flagged.get(path).copied().unwrap_or(0)
    }
}

// ------------------------------------------------------------------ model

/// Where the configured provider runs.
fn locality(settings: &crate::Settings) -> Locality {
    match settings.ai_provider.as_str() {
        "anthropic" | "openai" => Locality::Cloud,
        _ => {
            let (base, _) = crate::ai::resolved(settings);
            let local = ["localhost", "127.0.0.1", "[::1]"].iter().any(|h| base.contains(h));
            if local { Locality::LocalServer } else { Locality::Cloud }
        }
    }
}

impl Workspace {
    /// The configured AI model as a chat model, when there is one (not the
    /// offline stand-in). Trust features use it only once turned on.
    pub fn trust_chat(&self) -> Option<Arc<dyn ChatModel>> {
        if self.assistant.is_offline() {
            return None;
        }
        Some(Arc::new(LanguageModelChat::new(Arc::clone(&self.assistant), locality(&self.settings))))
    }

    /// The latest citation checks of `path`.
    pub fn citation_checks(&self, path: &NotePath) -> Vec<CiteCheck> {
        self.features.trust.shared.citations.lock().ok().and_then(|m| m.get(path).cloned()).unwrap_or_default()
    }

    /// Check the citations of the active note now (with the AI model when
    /// one is set up and citation checks are on).
    pub fn check_citations(&mut self) {
        let Some((path, text)) = self.active_doc().and_then(|d| d.path.clone().map(|p| (p, d.text().to_string()))) else { return };
        let n = anchor::citations(&text).len();
        if n == 0 {
            self.notify(NoticeLevel::Info, "This note has no citations to check. Saved answers cite their sources in footnotes.");
            return;
        }
        // The check reads the saved text.
        self.save_all();
        let shared = &self.features.trust.shared;
        if let Ok(mut q) = shared.check_now.lock() {
            q.push(path);
        }
        let how = if self.settings.trust.verify_citations && self.trust_chat().is_some() {
            format!("Checking {n} citation{} with {}…", if n == 1 { "" } else { "s" }, self.assistant_name())
        } else {
            format!("Checking {n} citation{}…", if n == 1 { "" } else { "s" })
        };
        self.notify(NoticeLevel::Info, how);
    }

    /// Set properties of a note as the person's own edit: in the open
    /// document when it is open (one undo step), else on disk with history.
    pub fn set_note_properties(&mut self, path: &NotePath, set: &[(&str, Value)]) -> Result<(), String> {
        let open = self.active_doc().and_then(|d| d.path.clone()).as_ref() == Some(path);
        if open {
            for (k, v) in set {
                if !self.set_property_path(&[k], v) {
                    return Err(format!("couldn't set {k}"));
                }
            }
            return Ok(());
        }
        let Some(store) = self.agent_store().cloned() else { return Err("no vault is open".into()) };
        self.save_all();
        let cs = ChangeSet::one(Change::SetProperties {
            path: path.clone(),
            set: set.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect(),
            remove: Vec::new(),
        });
        store.submit(&Actor::human(&store.device().id), cs).map(drop).map_err(|e| e.to_string())
    }

    /// "Mark reviewed": `reviewed: <today>` (design §C.6).
    pub fn mark_reviewed(&mut self, path: &NotePath) {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        match self.set_note_properties(path, &[("reviewed", Value::Str(today))]) {
            Ok(()) => {
                let item = quire_agent::inbox::InboxItem::Stale { path: path.clone(), score: 0.0, reasons: Vec::new() };
                if let Some(store) = self.agent_store() {
                    let _ = store.inbox().resolve(&item.fingerprint());
                }
                self.features.agent.reload_inbox_soon();
                self.notify(NoticeLevel::Info, format!("Marked “{}” as reviewed today", path.stem()));
            }
            Err(e) => self.notify(NoticeLevel::Error, format!("Couldn't mark it reviewed: {e}")),
        }
    }

    /// "Mark superseded": `superseded_by: [[by]]` on `path` (design §C.5).
    pub fn mark_superseded(&mut self, path: &NotePath, by: &NotePath, fingerprint: Option<&str>) {
        let link = format!("[[{}]]", by.without_ext());
        match self.set_note_properties(path, &[("superseded_by", Value::Str(link))]) {
            Ok(()) => {
                if let (Some(store), Some(fp)) = (self.agent_store(), fingerprint) {
                    let _ = store.inbox().resolve(fp);
                }
                self.features.agent.reload_inbox_soon();
                self.notify(NoticeLevel::Info, format!("Marked “{}” as replaced by “{}”", path.stem(), by.stem()));
            }
            Err(e) => self.notify(NoticeLevel::Error, format!("Couldn't mark it: {e}")),
        }
    }

    /// "Open both": the two notes side by side, the passages highlighted.
    pub fn open_side_by_side(&mut self, a: &Anchor, b: &Anchor) {
        let text = |ws: &Self, p: &NotePath| ws.index().read().source(p).map(str::to_string);
        let lines = |ws: &Self, x: &Anchor| text(ws, &x.path).and_then(|t| anchor::resolve(x, &t, None).lines(x.lines)).unwrap_or(x.lines);
        let (la, lb) = (lines(self, a), lines(self, b));
        self.features.trust.highlights = vec![(a.path.clone(), la), (b.path.clone(), lb)];
        self.features.decor_generation += 1;
        if self.open_note(&a.path).is_none() {
            return;
        }
        self.jump_to_line(la.0);
        if a.path != b.path && self.open_note(&b.path).is_some() {
            self.jump_to_line(lb.0);
            self.request_ui(UiRequest::Split);
        }
        self.request_ui(UiRequest::RevealCaret);
    }

    fn jump_to_line(&mut self, line: u32) {
        let byte = self.active_doc().map_or(0, |d| {
            let t = d.text();
            t.split_inclusive('\n').take(line.saturating_sub(1) as usize).map(str::len).sum()
        });
        self.jump_to_byte(byte);
    }

    /// "Ask agent to reconcile": the agent panel gets both passages; what it
    /// changes arrives as a suggestion. Without the panel, the request is copied.
    pub fn reconcile(&mut self, a: &Anchor, b: &Anchor) {
        let prompt = format!(
            "Two of my notes may disagree. Read both passages, decide what is true now (ask me if you can't tell), and suggest edits that make the notes agree — for example mark the outdated one with `superseded_by` or correct it. Don't change anything else.\n\n1. [[{}]] (lines {}–{}): “{}”\n2. [[{}]] (lines {}–{}): “{}”",
            a.path.without_ext(),
            a.lines.0,
            a.lines.1,
            a.quote,
            b.path.without_ext(),
            b.lines.0,
            b.lines.1,
            b.quote
        );
        self.features.trust.reconcile = Some(ReconcileRequest { prompt: prompt.clone(), anchors: vec![a.clone(), b.clone()] });
        if self.command_available(Command::ToggleAgentPanel) {
            self.request_ui(UiRequest::Feature(Command::ToggleAgentPanel));
        } else {
            self.request_ui(UiRequest::Copy(prompt));
            self.notify(
                NoticeLevel::Info,
                "Copied a request to reconcile the two notes. Paste it into your AI agent; its changes arrive in the Inbox.",
            );
        }
    }

    /// Update a drifted citation to what the cited note says now.
    pub fn update_citation(&mut self, in_note: &NotePath, label: &str) {
        let Some(check) = self.citation_checks(in_note).into_iter().find(|c| c.citation.label == label) else { return };
        let (Some(target), Some(a)) = (check.target.clone(), check.anchor.clone()) else {
            self.notify(NoticeLevel::Info, "The cited note is gone; remove the footnote or link another note.");
            return;
        };
        let Some(text) = self.index().read().source(&target).map(str::to_string) else { return };
        let Some(lines) = check.status.lines(a.lines) else {
            self.notify(NoticeLevel::Info, "The quoted passage isn't in the note any more; edit the footnote by hand.");
            return;
        };
        let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        let s = starts.get(lines.0 as usize - 1).copied().unwrap_or(0);
        let e = starts.get(lines.1 as usize).copied().unwrap_or(text.len());
        let fresh = anchor::make_anchor(&target, &text, s..e);
        let line = anchor::footnote(label, &fresh);
        let Some(doc_text) = self.doc_text(in_note) else { return };
        let Some(at) = footnote_range(&doc_text, label) else { return };
        let mut new_text = doc_text.clone();
        new_text.replace_range(at, &line);
        self.replace_note_text(in_note, &doc_text, new_text);
        if let Ok(mut q) = self.features.trust.shared.drift_changed.lock() {
            q.push(in_note.clone());
        }
    }

    fn doc_text(&self, path: &NotePath) -> Option<String> {
        self.docs()
            .iter()
            .find(|d| d.path.as_ref() == Some(path))
            .map(|d| d.text().to_string())
            .or_else(|| self.vault().and_then(|v| v.read(path).ok()))
    }

    /// The person's edit of a whole note: undoable when it is open, with history when it isn't.
    fn replace_note_text(&mut self, path: &NotePath, old: &str, new: String) {
        if let Some(i) = self.docs().iter().position(|d| d.path.as_ref() == Some(path)) {
            let id = self.docs()[i].id;
            if let Some(doc) = self.docs.iter_mut().find(|d| d.id == id) {
                let tx = quire_text::Transaction::replace(0, doc.buffer().len_chars(), new);
                let _ = doc.apply(tx, quire_text::EditKind::Other);
            }
            self.save(id);
            return;
        }
        let Some(store) = self.agent_store().cloned() else { return };
        let mut cs = ChangeSet::one(Change::Edit { path: path.clone(), new_text: new });
        cs.expected = vec![(path.clone(), quire_vault::ContentHash::of(old))];
        if let Err(e) = store.submit(&Actor::human(&store.device().id), cs) {
            self.notify(NoticeLevel::Error, format!("Couldn't update the citation: {e}"));
        }
    }
}

/// The byte range of footnote `label`'s line (without its newline).
pub fn footnote_range(text: &str, label: &str) -> Option<std::ops::Range<usize>> {
    let mark = format!("[^{label}]:");
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with(&mark) {
            return Some(at..at + line.trim_end_matches(['\n', '\r']).len());
        }
        at += line.len();
    }
    None
}

/// The 0-based line of footnote `label` in `text`.
fn footnote_line(text: &str, label: &str) -> Option<usize> {
    let mark = format!("[^{label}]:");
    text.lines().position(|l| l.starts_with(&mark))
}

/// A widget id for a citation's note under its footnote.
pub fn widget_id(path: &NotePath, label: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    label.hash(&mut h);
    h.finish() & !0xF
}

/// Actions on a citation widget (low 4 bits of the action id).
pub mod action {
    pub const UPDATE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CHECK: u64 = 3;
}

impl Workspace {
    /// The citation a widget is about: (citing note, check).
    pub fn citation_for_widget(&self, id: u64) -> Option<(NotePath, CiteCheck)> {
        let id = id & !0xF;
        let m = self.features.trust.shared.citations.lock().ok()?;
        m.iter().find_map(|(p, cs)| cs.iter().find(|c| widget_id(p, &c.citation.label) == id).map(|c| (p.clone(), c.clone())))
    }
}

// ------------------------------------------------------------------ hooks

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let _ = now;
    if ws.agent_store().is_none() {
        return false;
    }
    let mut repaint = false;
    // What the jobs need to know, when it changes.
    if ws.features.trust.settings_seen.as_ref() != Some(&ws.settings.trust) {
        ws.features.trust.settings_seen = Some(ws.settings.trust.clone());
        ws.features.trust.shared.set_settings(ws.settings.trust.clone());
    }
    let key = format!("{}|{}|{}", ws.assistant.name(), ws.settings.ai_provider, ws.settings.ai_base_url);
    if ws.features.trust.chat_key.as_ref() != Some(&key) {
        ws.features.trust.chat_key = Some(key);
        let chat = ws.trust_chat();
        ws.features.trust.shared.set_chat(chat);
    }
    let sv = ws.index().semantic_version();
    if sv != ws.features.trust.seen_semantic {
        ws.features.trust.seen_semantic = sv;
        let sem = ws.index().semantic();
        ws.features.trust.shared.set_semantic(sem, sv);
    }
    // Results from the jobs.
    let g = ws.features.trust.shared.generation.load(Ordering::Acquire);
    if g != ws.features.trust.seen_generation {
        ws.features.trust.seen_generation = g;
        let flagged: HashMap<NotePath, usize> = ws
            .features
            .trust
            .shared
            .citations
            .lock()
            .map(|m| m.iter().map(|(p, cs)| (p.clone(), cs.iter().filter(|c| c.flagged()).count())).filter(|(_, n)| *n > 0).collect())
            .unwrap_or_default();
        ws.features.trust.flagged = flagged;
        ws.features.decor_generation += 1;
        ws.features.agent.reload_inbox_soon();
        repaint = true;
    }
    let done: Vec<(NotePath, usize, bool)> =
        ws.features.trust.shared.checked.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
    for (path, n, verified) in done {
        let checks = ws.citation_checks(&path);
        let moved = checks.iter().filter(|c| !c.status.holds()).count();
        let unsupported = checks.iter().filter(|c| matches!(c.support, Some((anchor::Support::No, _)))).count();
        let weak = checks.iter().filter(|c| c.weak && c.support.is_none()).count();
        let mut parts = Vec::new();
        if moved > 0 {
            parts.push(format!("{moved} no longer match{} the cited text", if moved == 1 { "es" } else { "" }));
        }
        if unsupported > 0 {
            parts.push(format!(
                "{unsupported} {} supported by what {} cite{}",
                if unsupported == 1 { "isn't" } else { "aren't" },
                if unsupported == 1 { "it" } else { "they" },
                if unsupported == 1 { "s" } else { "" }
            ));
        }
        if weak > 0 && !verified {
            parts.push(format!("{weak} share{} few words with the cited text", if weak == 1 { "s" } else { "" }));
        }
        let text = if parts.is_empty() {
            format!(
                "All {n} citation{} in “{}” check out{}",
                if n == 1 { "" } else { "s" },
                path.stem(),
                if verified { "" } else { " (offline: the quotes are still there)" }
            )
        } else {
            format!("Citations in “{}”: {}", path.stem(), parts.join("; "))
        };
        ws.notify(if parts.is_empty() { NoticeLevel::Info } else { NoticeLevel::Warn }, text);
        repaint = true;
    }
    repaint |= ws.features.trust.timeline.poll();
    repaint
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    // Poll for job results and the timeline while something is pending.
    let pending = ws.features.trust.timeline.busy()
        || ws.features.trust.shared.check_now.lock().is_ok_and(|q| !q.is_empty())
        || ws.features.trust.shared.checked.lock().is_ok_and(|q| !q.is_empty());
    pending.then(|| Instant::now() + std::time::Duration::from_millis(200))
}

/// Whether a Trust-owned command is ready to offer.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    matches!(cmd, Command::CheckCitations | Command::ThinkingTimeline | Command::MarkReviewed) && ws.agent_store().is_some()
}

/// A Trust-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::ThinkingTimeline => {
            if ws.features.trust.timeline.topic_text.is_empty()
                && let Some(p) = ws.active_doc().and_then(|d| d.path.clone())
            {
                ws.features.trust.timeline.topic_text = format!("[[{}]]", p.without_ext());
            }
            Some(UiRequest::Feature(cmd))
        }
        Command::CheckCitations => {
            ws.check_citations();
            None
        }
        Command::MarkReviewed => {
            match ws.active_doc().and_then(|d| d.path.clone()) {
                Some(p) => ws.mark_reviewed(&p),
                None => ws.notify(NoticeLevel::Info, "Open a note to mark it as reviewed"),
            }
            None
        }
        _ => None,
    }
}

/// Decorations for `doc`: flagged citations get a tint and a note under
/// the footnote; "Open both" passages are highlighted.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let Some(path) = &doc.path else { return };
    for (p, (a, b)) in &ws.features.trust.highlights {
        if p == path {
            out.push(Decoration::tint(*a as usize - 1..*b as usize, Tone::Info));
        }
    }
    if ws.features.trust.flagged(path) == 0 {
        return;
    }
    let text = doc.text();
    for c in ws.citation_checks(path).iter().filter(|c| c.flagged()) {
        let Some(line) = footnote_line(&text, &c.citation.label) else { continue };
        out.push(Decoration::tint(line..line + 1, Tone::Warning));
        out.push(Decoration::widget(line, WidgetId { owner: Owner::Trust, id: widget_id(path, &c.citation.label) }));
    }
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let Some((in_note, check)) = ws.citation_for_widget(ev.action.id) else { return };
    match ev.action.id & 0xF {
        action::UPDATE => ws.update_citation(&in_note, &check.citation.label),
        action::OPEN => {
            if let (Some(t), Some(a)) = (check.target, check.anchor) {
                let lines = check.status.lines(a.lines).unwrap_or(a.lines);
                ws.features.trust.highlights = vec![(t.clone(), lines)];
                if ws.open_note(&t).is_some() {
                    ws.jump_to_line(lines.0);
                    ws.request_ui(UiRequest::RevealCaret);
                }
            }
        }
        action::CHECK => ws.check_citations(),
        _ => {}
    }
}

/// An action id for a widget's button.
pub const fn widget_action(widget: u64, act: u64) -> ActionId {
    ActionId { owner: Owner::Trust, id: (widget & !0xF) | (act & 0xF) }
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    let (VaultEvent::Changed(p) | VaultEvent::Removed(p)) = ev else { return };
    let shared = &ws.features.trust.shared;
    shared.changes.fetch_add(1, Ordering::AcqRel);
    if let Ok(mut q) = shared.drift_changed.lock() {
        q.push(p.clone());
    }
    if matches!(ev, VaultEvent::Removed(_)) {
        ws.features.trust.highlights.retain(|(h, _)| h != p);
    }
}

/// A vault was opened: state starts fresh and the jobs start.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.trust = TrustState::default();
    let shared = Arc::clone(&ws.features.trust.shared);
    ws.jobs.add(Box::new(citations::CitationsJob::new(Arc::clone(&shared))));
    ws.jobs.add(Box::new(stale::StaleJob::new(Arc::clone(&shared))));
    ws.jobs.add(Box::new(claims::ClaimsJob::new(shared)));
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let Some(path) = ws.active_doc().and_then(|d| d.path.as_ref()) else { return };
    let n = ws.features.trust.flagged(path);
    if n > 0 {
        out.push(StatusItem {
            owner: Owner::Trust,
            text: format!("⚠ {n} citation{}", if n == 1 { "" } else { "s" }),
            tooltip: "Citations in this note that no longer match, or seem not to support the sentence citing them".into(),
            command: Some(Command::CheckCitations),
            warn: true,
        });
    }
}

/// Whether `status` is worth showing for an anchor (drift items).
pub fn drift_words(status: &AnchorStatus) -> &'static str {
    match status {
        AnchorStatus::Exact | AnchorStatus::Moved { .. } => "still matches",
        AnchorStatus::Drifted { .. } => "the cited text changed",
        AnchorStatus::Lost => "the cited text is gone",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::Vault;

    #[test]
    fn footnotes_are_found_by_label() {
        let t = "x [^1]\n\n[^1]: [[A]] — “q” <!-- quire:cite v=1 -->\n[^10]: other\n";
        assert_eq!(footnote_line(t, "1"), Some(2));
        assert_eq!(&t[footnote_range(t, "10").unwrap()], "[^10]: other");
        assert_eq!(footnote_line(t, "2"), None);
        let p = NotePath::new("A.md");
        assert_eq!(widget_id(&p, "1") & 0xF, 0);
        assert_eq!(widget_action(widget_id(&p, "1"), action::OPEN).id & 0xF, action::OPEN);
    }

    #[test]
    fn mark_reviewed_writes_today_and_commands_are_offered() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("A.md", "# A\nbody\n"), ("B.md", "# B\n")]));
        assert!(ws.command_available(Command::MarkReviewed));
        assert!(ws.command_available(Command::CheckCitations));
        assert!(ws.command_available(Command::ThinkingTimeline));
        ws.open_note(&NotePath::new("A.md"));
        ws.run(Command::MarkReviewed);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(ws.active_doc().unwrap().text().contains(&format!("reviewed: {today}")));
        // A note that isn't open: on disk, with history.
        ws.mark_superseded(&NotePath::new("B.md"), &NotePath::new("A.md"), None);
        assert!(ws.vault().unwrap().read(&NotePath::new("B.md")).unwrap().contains("superseded_by:"));
        ws.run(Command::CheckCitations);
        assert!(ws.notices.last().unwrap().text.contains("no citations"));
        assert_eq!(ws.run(Command::ThinkingTimeline), Some(UiRequest::Feature(Command::ThinkingTimeline)));
        assert_eq!(ws.features.trust.timeline.topic_text, "[[A]]");
    }
}
