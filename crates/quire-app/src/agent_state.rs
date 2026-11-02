// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-A's entry point into the workspace.
//!
//! Agent store and review: the inbox, hunk review in the editor, agent activity and revert.
//!
//! `features.rs` calls the functions below for every Agent-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-A. State lives in
//! `ws.features.agent`.
//!
//! * [`review`] — a proposal under review in the editor
//! * [`inbox`] — what the Inbox tab shows, loaded off the UI thread
//! * [`lint`] — the vault lint job
//! * [`activity`] — the agent activity timeline and "undo an agent's changes"

#[path = "inbox.rs"]
pub mod inbox;
#[path = "lint.rs"]
pub mod lint;
#[path = "review.rs"]
pub mod review;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use quire_agent::inbox::LintCheck;
use quire_agent::lint::Level;
use quire_agent::watch::AgentWatcher;
use quire_vault::{NotePath, VaultEvent};

use self::inbox::{InboxData, InboxLoader};
use self::review::{AcceptRecord, HunkAct, ReviewSession, anchor_line, decode, encode};
use crate::Command;
use crate::Owner;
use crate::decor::{ActionId, Decoration, DecorationEvent, StatusItem, Tone, WidgetId};
use crate::document::{DocId, Document};
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// Widget ids of the "suggested changes here" bar under a note's first line.
pub const TITLE_WIDGET: u64 = 0xFFFF_0000;

/// A result computed on a worker thread.
pub struct Bg<T> {
    rx: Option<Receiver<T>>,
}

impl<T> Default for Bg<T> {
    fn default() -> Self {
        Self { rx: None }
    }
}

impl<T: Send + 'static> Bg<T> {
    /// Start `f` unless one is already running.
    pub fn start(&mut self, f: impl FnOnce() -> T + Send + 'static) {
        if self.rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("quire-agent-bg".into())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .is_ok()
        {
            self.rx = Some(rx);
        }
    }

    pub fn poll(&mut self) -> Option<T> {
        let r = self.rx.as_ref()?.try_recv();
        match r {
            Ok(v) => {
                self.rx = None;
                Some(v)
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                None
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        }
    }

    pub const fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

/// The agent activity timeline's data (design §A.9).
pub mod activity {
    use quire_agent::{ActorKind, LedgerEntry, LedgerQuery, Op};

    /// Events of one actor that belong together: one run, one batch, or
    /// one burst of work (gaps under 10 minutes).
    #[derive(Debug, Clone)]
    pub struct Run {
        pub actor_id: String,
        pub actor_label: String,
        pub kind: ActorKind,
        /// The run id, when the agent gave one.
        pub id: Option<quire_agent::RunId>,
        pub batch: Option<String>,
        pub start: i64,
        pub end: i64,
        pub events: Vec<LedgerEntry>,
    }

    impl Run {
        /// Notes written (not proposed or read).
        pub fn writes(&self) -> usize {
            self.events.iter().filter(|e| matches!(e.op, Op::Write | Op::Create | Op::Rename | Op::Delete | Op::Revert)).count()
        }

        pub fn proposals(&self) -> Vec<quire_agent::ProposalId> {
            let mut v: Vec<_> = self.events.iter().filter(|e| e.op == Op::Propose).filter_map(|e| e.proposal.clone()).collect();
            v.dedup();
            v
        }
    }

    const GAP_MS: i64 = 10 * 60 * 1000;

    /// The runs in `entries` (oldest first), newest run first.
    pub fn runs(entries: Vec<LedgerEntry>) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::new();
        for e in entries.into_iter().filter(|e| e.op != Op::Batch) {
            let same = out.iter_mut().rev().find(|r| {
                r.actor_id == e.actor.id
                    && match (&e.run, &e.batch) {
                        (Some(run), _) => r.id.as_ref() == Some(run),
                        (None, Some(b)) => r.batch.as_ref() == Some(b),
                        (None, None) => r.id.is_none() && r.batch.is_none() && e.at - r.end < GAP_MS,
                    }
            });
            match same {
                Some(r) => {
                    r.end = r.end.max(e.at);
                    r.events.push(e);
                }
                None => out.push(Run {
                    actor_id: e.actor.id.clone(),
                    actor_label: e.actor.label.clone(),
                    kind: e.actor.kind,
                    id: e.run.clone(),
                    batch: e.batch.clone(),
                    start: e.at,
                    end: e.at,
                    events: vec![e],
                }),
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.end));
        out
    }

    /// The ledger of the last `days` days, as runs.
    pub fn load(store: &quire_agent::AgentStore, days: i64) -> Vec<Run> {
        let since = store.now_ms() - days * 86_400_000;
        runs(store.log(&LedgerQuery { since: Some(since), ..LedgerQuery::default() }))
    }
}

/// WS-A's state, kept in `ws.features.agent`.
#[derive(Default)]
pub struct AgentState {
    /// The proposal under review in the editor.
    pub review: Option<ReviewSession>,
    /// What the Inbox shows.
    pub inbox: InboxData,
    pub loader: InboxLoader,
    watcher: Option<AgentWatcher>,
    started: bool,
    /// Decided hunks the editor might undo.
    pub(crate) accept_undo: Vec<AcceptRecord>,
    /// (document, revision) last checked for undone accepts.
    undo_checked: Option<(DocId, u64)>,
    pub lint: Arc<lint::LintShared>,
    /// The agent instructions note was edited here and not yet saved.
    instructions_dirty: bool,
    /// The activity timeline, loaded when the tab is shown.
    pub activity: Vec<activity::Run>,
    pub activity_bg: Bg<Vec<activity::Run>>,
    pub activity_loaded: Option<Instant>,
    /// "Undo an agent's changes…" asked the Activity tab to open its form.
    pub open_revert: bool,
}

impl AgentState {
    /// Load the inbox again shortly (after decisions here).
    pub fn reload_inbox_soon(&mut self) {
        self.loader.request(Duration::from_millis(50));
    }

    /// Whether a lint check reports only as a warning (not in the badge).
    fn warn_only(c: LintCheck, prefs: &quire_agent::lint::LintPrefs) -> bool {
        prefs.level(c) != Level::On
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    if ws.agent_store().is_none() {
        return false;
    }
    let mut changed = false;
    if !ws.features.agent.started {
        ws.features.agent.started = true;
        // After the first frame: nothing here reads the disk on the UI thread.
        ws.features.agent.loader.request(Duration::from_millis(300));
    }
    if ws.features.agent.watcher.is_none()
        && ws.features.agent.inbox.loaded_at_ms > 0
        && let Some(root) = ws.vault_root().map(std::path::Path::to_path_buf)
    {
        ws.features.agent.watcher = AgentWatcher::start(&root).ok();
    }
    if ws.features.agent.watcher.as_ref().is_some_and(AgentWatcher::take) {
        // Another process wrote a proposal, a finding or the policy.
        ws.features.agent.loader.request(Duration::from_millis(200));
    }
    let store = ws.agent_store().cloned();
    if let Some(data) = ws.features.agent.loader.poll(store.as_ref(), now) {
        let review_stale = ws
            .features
            .agent
            .review
            .as_ref()
            .is_some_and(|r| data.pending.iter().find(|p| p.id == r.proposal.id).is_none_or(|p| p.updated != r.proposal.updated));
        ws.features.agent.inbox = data;
        ws.features.decor_generation += 1;
        if review_stale {
            ws.reload_review();
        }
        changed = true;
    }
    changed |= ws.refresh_review_hunks(false);
    // Undoing an accepted hunk in the editor brings the suggestion back.
    if let Some(r) = ws.features.agent.review.as_ref().and_then(|r| r.doc)
        && let Some(rev) = ws.doc(r).map(Document::revision)
        && ws.features.agent.undo_checked != Some((r, rev))
    {
        ws.features.agent.undo_checked = Some((r, rev));
        ws.review_watch_undo();
    } else if ws.features.agent.review.is_none() && !ws.features.agent.accept_undo.is_empty() {
        let key = ws.active_doc().map(|d| (d.id, d.revision()));
        if key != ws.features.agent.undo_checked {
            ws.features.agent.undo_checked = key;
            ws.review_watch_undo();
        }
    }
    approve_own_instruction_edits(ws);
    if let Some(runs) = ws.features.agent.activity_bg.poll() {
        ws.features.agent.activity = runs;
        ws.features.agent.activity_loaded = Some(now);
        changed = true;
    }
    changed
}

/// The person edited the agent instructions in Quire and saved them: that's
/// an approval on this device.
fn approve_own_instruction_edits(ws: &mut Workspace) {
    let Some((dirty, text)) = ws
        .docs()
        .iter()
        .find(|d| d.path.as_ref().is_some_and(quire_agent::instructions::is_instructions_name))
        .map(|d| (d.is_dirty(), d.text()))
    else {
        ws.features.agent.instructions_dirty = false;
        return;
    };
    if dirty {
        ws.features.agent.instructions_dirty = true;
        return;
    }
    if std::mem::take(&mut ws.features.agent.instructions_dirty)
        && let Some(store) = ws.agent_store()
        && store.instructions().is_some_and(|i| !i.approved && *i.text == *text)
    {
        let _ = store.approve_instructions();
        ws.features.agent.reload_inbox_soon();
    }
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let a = &ws.features.agent;
    let busy = (a.loader.loading() || a.activity_bg.busy()).then(|| Instant::now() + Duration::from_millis(100));
    // A long note under review is rebased after a pause in typing.
    let settle = a
        .review
        .as_ref()
        .and_then(|r| ws.doc(r.doc?))
        .filter(|d| d.buffer().len_lines() > review::LARGE_LINES)
        .and_then(Document::last_edit)
        .map(|t| t + review::SETTLE);
    [a.loader.deadline(), busy, settle].into_iter().flatten().min()
}

/// Whether a Agent-owned command is ready to offer. Commands that aren't
/// are hidden from the palette, the shortcut sheet and the keyboard, so a
/// build looks like 0.8 until a feature lands.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        // Review keys work only while reviewing, so they never take a key from typing.
        Command::NextHunk
        | Command::PrevHunk
        | Command::AcceptHunk
        | Command::AcceptAllInFile
        | Command::RejectHunk
        | Command::RejectAllInFile => ws.features.agent.review.is_some(),
        Command::OpenInbox
        | Command::ShowAgentActivity
        | Command::UndoAgentChanges
        | Command::CreateAgentInstructions
        | Command::LintVault
        | Command::AgentSettings => ws.agent.is_some(),
        _ => false,
    }
}

/// A Agent-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    use Command as C;
    let reviewing = ws.features.agent.review.is_some();
    match cmd {
        C::NextHunk | C::PrevHunk | C::AcceptHunk | C::RejectHunk | C::AcceptAllInFile | C::RejectAllInFile if !reviewing => {
            ws.notify(NoticeLevel::Info, "No suggested changes are being reviewed. Open one from the Inbox (Alt+I).");
            None
        }
        C::NextHunk => {
            ws.review_step(1);
            Some(UiRequest::Feature(cmd))
        }
        C::PrevHunk => {
            ws.review_step(-1);
            Some(UiRequest::Feature(cmd))
        }
        C::AcceptHunk | C::RejectHunk => {
            let k = ws.features.agent.review.as_ref().map_or(0, |r| r.current);
            if ws.features.agent.review.as_ref().is_some_and(|r| r.hunks.is_empty()) {
                ws.notify(NoticeLevel::Info, "Nothing to decide in this note; the rest is in the Inbox");
                return Some(UiRequest::Feature(C::OpenInbox));
            }
            ws.review_decide(k, if cmd == C::AcceptHunk { HunkAct::Accept } else { HunkAct::Reject });
            Some(UiRequest::Feature(cmd))
        }
        C::AcceptAllInFile | C::RejectAllInFile => {
            ws.review_all_in_file(cmd == C::AcceptAllInFile);
            Some(UiRequest::Feature(cmd))
        }
        C::OpenInbox => {
            ws.features.agent.loader.request(Duration::ZERO);
            Some(UiRequest::Feature(cmd))
        }
        C::ShowAgentActivity => {
            load_activity(ws);
            Some(UiRequest::Feature(cmd))
        }
        C::UndoAgentChanges => {
            load_activity(ws);
            ws.features.agent.open_revert = true;
            Some(UiRequest::Feature(cmd))
        }
        C::LintVault => {
            ws.features.agent.lint.now.store(true, Ordering::Release);
            ws.notify(NoticeLevel::Info, "Checking the vault; findings appear under Checks in the Inbox");
            Some(UiRequest::Feature(cmd))
        }
        C::CreateAgentInstructions => {
            let store = ws.agent_store().cloned()?;
            match store.create_instructions() {
                Ok(p) => {
                    ws.index.upsert(p.clone(), ws.vault().and_then(|v| v.read(&p).ok()).unwrap_or_default());
                    ws.open_note(&p);
                    ws.notify(NoticeLevel::Info, "Created AGENTS.md. Agents read it first; edit it to describe this vault.");
                    ws.features.agent.reload_inbox_soon();
                }
                Err(e) => {
                    if let Some(p) = quire_agent::instructions::find(store.vault()) {
                        ws.open_note(&p);
                    }
                    ws.notify(NoticeLevel::Info, e);
                }
            }
            None
        }
        C::AgentSettings => Some(UiRequest::Feature(cmd)),
        _ => {
            ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…')));
            None
        }
    }
}

/// Load the activity timeline (30 days) off the UI thread.
pub fn load_activity(ws: &mut Workspace) {
    if let Some(store) = ws.agent_store().cloned() {
        ws.features.agent.activity_bg.start(move || activity::load(&store, 30));
    }
}

/// `lines` moved by the edits since the hunks were worked out.
fn shift(line: usize, deltas: &[quire_text::LineDelta]) -> usize {
    let mut l = line;
    for d in deltas {
        if l >= d.start_line + d.old_lines {
            l = l - d.old_lines + d.new_lines;
        } else if l > d.start_line {
            l = d.start_line + (l - d.start_line).min(d.new_lines);
        }
    }
    l
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let a = &ws.features.agent;
    if let Some(r) = a.review.as_ref().filter(|r| r.doc == Some(doc.id)) {
        let Some(deltas) = doc.line_deltas_since(r.computed.1) else { return };
        let gen_ = r.generation & 0xFF_FFFF;
        let last = doc.buffer().len_lines().saturating_sub(1);
        for (k, h) in r.hunks.iter().enumerate() {
            let (s, e) = (shift(h.range_in_current.start as usize, &deltas), shift(h.range_in_current.end as usize, &deltas));
            if e > s {
                out.push(Decoration::tint(s..e, if h.conflict { Tone::Conflict } else { Tone::Removed }));
                out.push(Decoration::strike(s..e));
            }
            let mut moved = h.clone();
            moved.range_in_current = u32::try_from(s).unwrap_or(0)..u32::try_from(e).unwrap_or(0);
            let at = anchor_line(&moved).min(last);
            out.push(Decoration::widget(at, WidgetId { owner: Owner::Agent, id: encode(gen_, k, HunkAct::Select) }));
        }
        return;
    }
    // A note with suggestions waiting: say so under its first line.
    let Some(path) = &doc.path else { return };
    if let Some(i) = a.inbox.pending.iter().position(|p| {
        p.pending_changes()
            .any(|c| c.path.as_ref() == Some(path) && matches!(c.op, quire_agent::ChangeOp::Edit | quire_agent::ChangeOp::Create))
            && !a.inbox.snoozed(&inbox::proposal_fingerprint(p))
    }) {
        out.push(Decoration::widget(0, WidgetId { owner: Owner::Agent, id: TITLE_WIDGET | i as u64 }));
    }
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let ActionId { id, .. } = ev.action;
    if id & 0xFFFF_0000 == TITLE_WIDGET && id >> 32 == 0 {
        let i = usize::try_from(id & 0xFFFF).unwrap_or(usize::MAX);
        let target = ws.features.agent.inbox.pending.get(i).map(|p| p.id.clone());
        let path = ws.doc(ev.doc).and_then(|d| d.path.clone());
        if let Some(pid) = target {
            ws.start_review(&pid, path.as_ref());
        }
        return;
    }
    let Some((gen_, k, act)) = decode(id) else { return };
    let current = ws.features.agent.review.as_ref().is_some_and(|r| r.hunk_for(gen_, k).is_some());
    if !current {
        ws.notify(NoticeLevel::Info, "That suggestion changed; it's shown again as it is now");
        ws.refresh_review_hunks(true);
        return;
    }
    match act {
        HunkAct::Accept | HunkAct::Reject | HunkAct::ConfirmEdit => ws.review_decide(k, act),
        HunkAct::Edit => ws.review_edit(k),
        HunkAct::CancelEdit => ws.review_cancel_edit(),
        HunkAct::Select => ws.review_goto(k),
        HunkAct::StartReview => {}
    }
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    match ev {
        VaultEvent::Changed(p) => {
            ws.features.agent.lint.changes.fetch_add(1, Ordering::AcqRel);
            if quire_agent::instructions::is_instructions_name(p)
                || ws.features.agent.inbox.pending.iter().any(|x| x.summary().files.contains(p))
            {
                ws.features.agent.loader.request(Duration::from_millis(300));
            }
        }
        VaultEvent::Removed(p) => {
            ws.features.agent.lint.changes.fetch_add(1, Ordering::AcqRel);
            if let Some(store) = ws.agent_store().cloned() {
                for line in store.note_removed(p) {
                    ws.notify(NoticeLevel::Info, line);
                }
            }
            ws.features.agent.loader.request(Duration::from_millis(300));
            if ws.features.agent.review.as_ref().is_some_and(|r| r.path.as_ref() == Some(p)) {
                ws.reload_review();
            }
        }
        VaultEvent::AttachmentChanged(_) | VaultEvent::AttachmentRemoved(_) => {
            ws.features.agent.lint.changes.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// The workspace renamed or moved a note (the one rename hook in
/// `workspace.rs`): waiting proposals follow it.
pub(crate) fn path_changed(ws: &mut Workspace, from: &NotePath, to: &NotePath) {
    if let Some(store) = ws.agent_store() {
        store.rename_hook(from, to);
    }
    if let Some(r) = ws.features.agent.review.as_mut()
        && r.path.as_ref() == Some(from)
    {
        r.path = Some(to.clone());
    }
    ws.features.agent.reload_inbox_soon();
    if ws.features.agent.review.is_some() {
        ws.reload_review();
    }
}

/// A vault was opened: state starts fresh (start jobs here: `ws.jobs.add(...)`).
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.agent = AgentState::default();
    let shared = Arc::clone(&ws.features.agent.lint);
    ws.jobs.add(Box::new(lint::LintJob::new(shared)));
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let a = &ws.features.agent;
    if let Some(r) = &a.review {
        let n = r.hunks.len();
        let text = if n == 0 {
            format!("Reviewing {}", r.proposal.actor.label)
        } else {
            format!("Reviewing {} · {} of {n}", r.proposal.actor.label, r.current + 1)
        };
        out.push(StatusItem {
            owner: Owner::Agent,
            text,
            tooltip: "Alt+] next · Alt+[ previous · Ctrl+Alt+Y accept · Ctrl+Alt+Backspace reject".into(),
            command: Some(Command::OpenInbox),
            warn: false,
        });
    }
    let n = a.inbox.badge(&|c| AgentState::warn_only(c, &a.inbox.lint_prefs));
    if n > 0 {
        let approvals = a.inbox.permission_items();
        out.push(StatusItem {
            owner: Owner::Agent,
            text: format!("{n} to review"),
            tooltip: if approvals > 0 {
                "Agent permissions changed; open the Inbox (Alt+I)".into()
            } else {
                "Open the Inbox (Alt+I)".into()
            },
            command: Some(Command::OpenInbox),
            warn: approvals > 0,
        });
    }
}
