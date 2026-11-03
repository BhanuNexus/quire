// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reviewing an agent's suggestion in the editor (design §A.4, §7.3).
//!
//! A [`ReviewSession`] is one proposal open for review, on one of its notes.
//! Its hunks are worked out against the live buffer (rebased on every edit)
//! and drawn as decorations: the lines it would replace tinted and struck
//! through, and under them a widget with the agent's lines and
//! **Accept · Reject · Edit**. Accepting puts the result into the note as
//! one undoable edit, saved at once with provenance ("You, accepting Claude
//! Code's suggestion"). Undoing that edit puts the suggestion back.
//!
//! Proposals without an edit in an open note (new notes, renames,
//! deletions) are reviewed from the Inbox tab.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use quire_agent::{
    ChangeOp, ChangeState, DecideError, Hunk, HunkAction, HunkDecision, Proposal, ProposalId, ProposalStatus, review as hunks_mod,
};
use quire_vault::{ContentHash, NotePath};

use crate::document::DocId;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// Notes longer than this are rebased after a pause in typing, not on every key.
pub const LARGE_LINES: usize = 1_500;
/// The pause.
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// What a click on a hunk's widget asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkAct {
    Accept,
    Reject,
    /// Open the inline editor.
    Edit,
    /// Accept the text in the inline editor.
    ConfirmEdit,
    CancelEdit,
    /// Make it the current hunk.
    Select,
    /// Start reviewing the proposal shown on a note's title line.
    StartReview,
}

impl HunkAct {
    const ALL: [Self; 7] = [Self::Accept, Self::Reject, Self::Edit, Self::ConfirmEdit, Self::CancelEdit, Self::Select, Self::StartReview];

    const fn code(self) -> u64 {
        match self {
            Self::Accept => 1,
            Self::Reject => 2,
            Self::Edit => 3,
            Self::ConfirmEdit => 4,
            Self::CancelEdit => 5,
            Self::Select => 6,
            Self::StartReview => 7,
        }
    }
}

/// Decoration action and widget ids: `gen << 24 | index << 4 | act`.
pub fn encode(gen_: u64, index: usize, act: HunkAct) -> u64 {
    ((gen_ & 0xFF_FFFF) << 24) | ((index as u64 & 0xF_FFFF) << 4) | act.code()
}

/// (generation, hunk index, action).
pub fn decode(id: u64) -> Option<(u64, usize, HunkAct)> {
    let act = HunkAct::ALL.into_iter().find(|a| a.code() == id & 0xF)?;
    Some((id >> 24, usize::try_from((id >> 4) & 0xF_FFFF).ok()?, act))
}

/// A decided hunk the person might undo in the editor: if the note goes
/// back to `before`, the proposal goes back to `prior`.
pub(crate) struct AcceptRecord {
    pub doc: DocId,
    pub before: ContentHash,
    pub prior: Proposal,
    /// The proposal's `updated` after the decision.
    pub updated_after: i64,
}

/// One proposal under review in the editor.
pub struct ReviewSession {
    pub proposal: Proposal,
    /// The note being reviewed (`None`: nothing to review in the editor).
    pub path: Option<NotePath>,
    pub doc: Option<DocId>,
    pub hunks: Vec<Hunk>,
    /// (proposal `updated`, document revision) the hunks are for.
    pub computed: (i64, u64),
    /// The current hunk (Alt+] / Alt+[).
    pub current: usize,
    /// Changes whenever the hunk list does; actions from older widgets are refused.
    pub generation: u64,
    /// The hunk whose inline editor is open.
    pub editing: Option<usize>,
    /// The inline editor's text (the widget edits it during painting).
    pub edit_text: RefCell<String>,
    /// Give the inline editor focus on its next paint.
    pub edit_focus: Cell<bool>,
    pub started: Instant,
}

impl ReviewSession {
    /// The hunk index `k` when `id` belongs to the current generation.
    pub fn hunk_for(&self, gen_: u64, k: usize) -> Option<&Hunk> {
        (gen_ == self.generation & 0xFF_FFFF).then(|| self.hunks.get(k)).flatten()
    }

    /// Files of this proposal still waiting, in order.
    pub fn pending_files(&self) -> Vec<NotePath> {
        self.proposal.pending_changes().filter_map(|c| c.subject().cloned()).collect()
    }

    pub fn conflicts(&self) -> usize {
        self.hunks.iter().filter(|h| h.conflict).count()
    }
}

/// The line a hunk's widget is drawn under.
pub fn anchor_line(h: &Hunk) -> usize {
    let (s, e) = (h.range_in_current.start as usize, h.range_in_current.end as usize);
    if e > s { e - 1 } else { s.saturating_sub(1) }
}

/// Text without line endings, for showing lines.
pub fn display_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l.trim_end_matches(['\n', '\r']).to_string()).collect()
}

fn decision(h: &Hunk, action: HunkAction, text: Option<String>) -> HunkDecision {
    HunkDecision { hunk: h.id.clone(), action, at: 0, text, other: serde_json::Map::new() }
}

/// Whether `change` is reviewed in the editor (an edit, or a create of a note that exists).
fn reviewable_in_editor(ws: &Workspace, c: &quire_agent::proposal::ProposalChange) -> bool {
    match c.op {
        ChangeOp::Edit => true,
        ChangeOp::Create => c.path.as_ref().is_some_and(|p| ws.vault().is_some_and(|v| v.exists(p))),
        ChangeOp::Rename | ChangeOp::Delete => false,
    }
}

impl Workspace {
    fn review_mut(&mut self) -> Option<&mut ReviewSession> {
        self.features.agent.review.as_mut()
    }

    /// Open proposal `id` for review: its first waiting note (or `path`)
    /// in the editor, the caret on the first hunk. False if there's
    /// nothing to review.
    pub fn start_review(&mut self, id: &ProposalId, path: Option<&NotePath>) -> bool {
        let Some(store) = self.agent_store().cloned() else { return false };
        let Some(p) = store.proposal(id).filter(|p| p.status == ProposalStatus::Pending) else {
            self.notify(NoticeLevel::Info, "That suggestion was already handled");
            self.features.agent.reload_inbox_soon();
            return false;
        };
        let target = path.cloned().or_else(|| p.pending_changes().find(|c| reviewable_in_editor(self, c)).and_then(|c| c.path.clone()));
        let doc = target.as_ref().and_then(|t| self.open_note(t));
        let gen_ = self.features.agent.review.as_ref().map_or(1, |r| r.generation + 1);
        self.features.agent.review = Some(ReviewSession {
            proposal: p,
            path: target.filter(|_| doc.is_some()),
            doc,
            hunks: Vec::new(),
            computed: (i64::MIN, u64::MAX),
            current: 0,
            generation: gen_,
            editing: None,
            edit_text: RefCell::new(String::new()),
            edit_focus: Cell::new(false),
            started: Instant::now(),
        });
        self.refresh_review_hunks(true);
        self.review_goto(0);
        true
    }

    /// Stop reviewing (the suggestions stay in the inbox).
    pub fn end_review(&mut self) {
        self.features.agent.review = None;
        self.features.decor_generation += 1;
    }

    /// Work the hunks out again if the note or the proposal changed.
    pub(crate) fn refresh_review_hunks(&mut self, force: bool) -> bool {
        let Some(r) = self.features.agent.review.as_ref() else { return false };
        let (Some(doc_id), Some(path)) = (r.doc, r.path.clone()) else { return false };
        let Some(doc) = self.doc(doc_id) else {
            // The note was closed: the review ends; the suggestion waits in the Inbox.
            self.end_review();
            return true;
        };
        let key = (r.proposal.updated, doc.revision());
        if !force && r.computed == key {
            return false;
        }
        // In a long note, wait for a pause in typing; meanwhile the
        // decorations follow the edits line by line.
        if !force
            && r.computed.0 == key.0
            && doc.buffer().len_lines() > LARGE_LINES
            && doc.last_edit().is_some_and(|t| t.elapsed() < SETTLE)
        {
            return false;
        }
        let current = doc.text().to_string();
        let hunks = r
            .proposal
            .changes
            .iter()
            .find(|c| c.state.is_none() && c.path.as_ref() == Some(&path) && matches!(c.op, ChangeOp::Edit | ChangeOp::Create))
            .and_then(|c| {
                let (base, new) = match c.op {
                    ChangeOp::Create => (current.clone(), c.text.clone()?),
                    _ => (c.base_text()?, c.new_text.clone()?),
                };
                Some(hunks_mod::review_hunks(&path, &base, &current, &new, c.review.as_ref()))
            })
            .unwrap_or_default();
        let gen_changed = r.hunks.iter().map(|h| &h.id).ne(hunks.iter().map(|h| &h.id));
        if let Some(r) = self.review_mut() {
            r.computed = key;
            if gen_changed {
                r.generation += 1;
                r.editing = None;
            }
            r.current = r.current.min(hunks.len().saturating_sub(1));
            r.hunks = hunks;
        }
        self.features.decor_generation += 1;
        true
    }

    /// Make hunk `k` current and put the caret at its start.
    pub fn review_goto(&mut self, k: usize) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let Some(doc_id) = r.doc else { return };
        let Some(h) = r.hunks.get(k) else { return };
        let line = h.range_in_current.start as usize;
        if let Some(i) = self.docs.iter().position(|d| d.id == doc_id) {
            self.active = Some(i);
            let doc = &mut self.docs[i];
            let buf = doc.buffer();
            let at = buf.line_to_char(line.min(buf.len_lines().saturating_sub(1)));
            doc.set_selection(quire_text::Selection::cursor(at));
        }
        if let Some(r) = self.review_mut() {
            r.current = k;
        }
        self.request_ui(UiRequest::RevealCaret);
        self.features.decor_generation += 1;
    }

    /// Next (`+1`) or previous (`-1`) hunk, wrapping; past the last hunk of
    /// a note, the next note of the proposal.
    pub fn review_step(&mut self, dir: isize) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let n = r.hunks.len();
        if n == 0 {
            self.review_next_file();
            return;
        }
        let k = (r.current as isize + dir).rem_euclid(n as isize) as usize;
        self.review_goto(k);
    }

    /// Move on to the proposal's next waiting note, or end when there is none.
    fn review_next_file(&mut self) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let id = r.proposal.id.clone();
        let here = r.path.clone();
        let next = r
            .proposal
            .pending_changes()
            .filter(|c| reviewable_in_editor(self, c))
            .filter_map(|c| c.path.clone())
            .find(|p| Some(p) != here.as_ref());
        match next {
            Some(p) => {
                self.start_review(&id, Some(&p));
            }
            None if r.proposal.pending_changes().next().is_some() => {
                // Left: new notes, renames, deletions — in the Inbox card.
                if let Some(r) = self.review_mut() {
                    r.path = None;
                    r.doc = None;
                    r.hunks.clear();
                }
                self.features.decor_generation += 1;
            }
            None => self.end_review(),
        }
    }

    /// Reload the proposal under review from disk (after a decision, or
    /// when another process changed it). Ends the review when it's settled.
    pub(crate) fn reload_review(&mut self) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let Some(store) = self.agent_store().cloned() else { return };
        match store.proposal(&r.proposal.id).filter(|p| p.status == ProposalStatus::Pending) {
            Some(p) => {
                let changed = p.updated != r.proposal.updated;
                if let Some(r) = self.review_mut() {
                    r.proposal = p;
                }
                if changed {
                    self.refresh_review_hunks(true);
                }
                let path_done = self.features.agent.review.as_ref().is_some_and(|r| {
                    r.path.as_ref().is_some_and(|path| !r.proposal.pending_changes().any(|c| c.path.as_ref() == Some(path)))
                });
                if path_done {
                    self.review_next_file();
                }
            }
            None => self.end_review(),
        }
    }

    /// Decide hunk `k` of the note under review.
    pub fn review_decide(&mut self, k: usize, act: HunkAct) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let Some(h) = r.hunks.get(k).cloned() else { return };
        let d = match act {
            HunkAct::Accept => decision(&h, HunkAction::Accepted, None),
            HunkAct::Reject => decision(&h, HunkAction::Rejected, None),
            HunkAct::ConfirmEdit => decision(&h, HunkAction::Edited, Some(r.edit_text.borrow().clone())),
            _ => return,
        };
        self.review_apply(&[d], Some(k));
    }

    /// Accept (every hunk except conflicts) or reject every hunk of the note under review.
    pub fn review_all_in_file(&mut self, accept: bool) {
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let decisions: Vec<HunkDecision> = r
            .hunks
            .iter()
            .filter(|h| !accept || !h.conflict)
            .map(|h| decision(h, if accept { HunkAction::Accepted } else { HunkAction::Rejected }, None))
            .collect();
        let skipped = r.conflicts();
        if decisions.is_empty() {
            if skipped > 0 {
                self.notify(NoticeLevel::Info, "Only conflicting changes are left; accept or reject them one by one");
            }
            return;
        }
        self.review_apply(&decisions, None);
        if accept && skipped > 0 {
            self.notify(
                NoticeLevel::Info,
                format!(
                    "Accepted all but {skipped} conflicting change{}; those need a look of their own",
                    if skipped == 1 { "" } else { "s" }
                ),
            );
        }
    }

    /// Apply decisions to the live note as one undoable edit, save it, and
    /// update the review.
    fn review_apply(&mut self, decisions: &[HunkDecision], at: Option<usize>) {
        let Some(store) = self.agent_store().cloned() else { return };
        let Some(r) = self.features.agent.review.as_ref() else { return };
        let (Some(doc_id), Some(path)) = (r.doc, r.path.clone()) else { return };
        let Some(doc) = self.doc(doc_id) else { return };
        let current = doc.text().to_string();
        let prior = r.proposal.clone();
        match store.decide_checked(&prior.id, &path, decisions, &current, Some(prior.updated)) {
            Ok(d) => {
                if d.text != current
                    && let Some(i) = self.docs.iter().position(|x| x.id == doc_id)
                {
                    let prov = store.accepted_provenance(&prior, &d.text);
                    let doc = &mut self.docs[i];
                    doc.replace_minimally(&d.text);
                    doc.next_save_provenance = Some(prov);
                    self.save(doc_id);
                }
                if d.accepted > 0 {
                    let rec = AcceptRecord { doc: doc_id, before: ContentHash::of(&current), prior, updated_after: d.updated };
                    let undo = &mut self.features.agent.accept_undo;
                    undo.push(rec);
                    if undo.len() > 20 {
                        undo.remove(0);
                    }
                }
                self.features.agent.reload_inbox_soon();
                self.reload_review();
                if let Some(k) = at {
                    let n = self.features.agent.review.as_ref().map_or(0, |r| r.hunks.len());
                    if n > 0 {
                        self.review_goto(k.min(n - 1));
                    } else if self.features.agent.review.as_ref().is_some_and(|r| r.doc.is_some()) {
                        self.review_next_file();
                        if let Some(next) = self.features.agent.review.as_ref().filter(|r| !r.hunks.is_empty()).map(|_| 0) {
                            self.review_goto(next);
                        }
                    }
                }
                if d.status != ProposalStatus::Pending {
                    self.notify(NoticeLevel::Info, "All suggested changes reviewed");
                }
            }
            Err(DecideError::Changed) => {
                self.notify(NoticeLevel::Info, DecideError::Changed.to_string());
                self.reload_review();
            }
            Err(DecideError::Write(e)) => self.notify(NoticeLevel::Error, format!("Couldn't apply the suggestion: {e}")),
        }
    }

    /// Open the inline editor on hunk `k`, filled with the agent's lines.
    pub fn review_edit(&mut self, k: usize) {
        if let Some(r) = self.review_mut()
            && let Some(h) = r.hunks.get(k)
        {
            *r.edit_text.borrow_mut() = display_lines(&h.new_lines).join("\n");
            r.editing = Some(k);
            r.current = k;
            r.edit_focus.set(true);
        }
        self.features.decor_generation += 1;
    }

    pub fn review_cancel_edit(&mut self) {
        if let Some(r) = self.review_mut() {
            r.editing = None;
        }
        self.features.decor_generation += 1;
    }

    /// The editor undid an accepted hunk: put the suggestion back.
    pub(crate) fn review_watch_undo(&mut self) {
        if self.features.agent.accept_undo.is_empty() {
            return;
        }
        let Some(store) = self.agent_store().cloned() else { return };
        let mut restored = None;
        let mut keep = Vec::new();
        for rec in std::mem::take(&mut self.features.agent.accept_undo) {
            let Some(doc) = self.doc(rec.doc) else { continue };
            if restored.is_none() && ContentHash::of(&doc.text()) == rec.before && store.put_back(&rec.prior, rec.updated_after) {
                restored = Some((rec.prior.id.clone(), doc.path.clone()));
                continue;
            }
            keep.push(rec);
        }
        self.features.agent.accept_undo = keep;
        let Some((id, path)) = restored else { return };
        self.features.agent.reload_inbox_soon();
        if self.features.agent.review.as_ref().is_some_and(|r| r.proposal.id == id) {
            self.reload_review();
            self.refresh_review_hunks(true);
        } else if self.features.agent.review.is_none() {
            // The review had ended with that decision: pick it up again.
            self.start_review(&id, path.as_ref());
        }
    }

    /// Accept or reject a whole proposal: edits of open notes go into the
    /// editor (undoable), the rest is written to disk.
    pub fn resolve_whole_proposal(&mut self, id: &ProposalId, accept: bool) {
        let Some(store) = self.agent_store().cloned() else { return };
        let Some(p) = store.proposal(id) else { return };
        // Open notes the proposal renames or deletes: let the workspace move them.
        for c in p.pending_changes().filter(|c| matches!(c.op, ChangeOp::Rename | ChangeOp::Delete)) {
            let Some(subject) = c.subject().cloned() else { continue };
            let Some(doc_id) = self.docs.iter().find(|d| d.path.as_ref() == Some(&subject)).map(|d| d.id) else { continue };
            if !accept {
                continue;
            }
            match (c.op, c.to.clone()) {
                (ChangeOp::Rename, Some(to)) => match self.move_open_note(doc_id, &subject, &to) {
                    Ok(()) => {
                        let _ = store.mark_change(id, &to, ChangeState::Applied);
                    }
                    Err(e) => self.notify(NoticeLevel::Error, e),
                },
                _ => self.close(doc_id),
            }
        }
        let in_editor = |path: &NotePath| self.docs.iter().any(|d| d.path.as_ref() == Some(path));
        let report = store.resolve_proposal(id, accept, &in_editor);
        let mut conflicts = report.conflicts_left;
        for path in &report.left {
            let Some(doc_id) = self.docs.iter().find(|d| d.path.as_ref() == Some(path)).map(|d| d.id) else { continue };
            let current = self.doc(doc_id).map(|d| d.text().to_string()).unwrap_or_default();
            let hunks = store.review(id, path, &current).hunks;
            conflicts += if accept { hunks.iter().filter(|h| h.conflict).count() } else { 0 };
            let ds: Vec<HunkDecision> = hunks
                .iter()
                .filter(|h| !accept || !h.conflict)
                .map(|h| decision(h, if accept { HunkAction::Accepted } else { HunkAction::Rejected }, None))
                .collect();
            let Some(prior) = store.proposal(id) else { break };
            match store.decide_checked(id, path, &ds, &current, Some(prior.updated)) {
                Ok(d) if d.text != current => {
                    if let Some(i) = self.docs.iter().position(|x| x.id == doc_id) {
                        let prov = store.accepted_provenance(&prior, &d.text);
                        self.docs[i].replace_minimally(&d.text);
                        self.docs[i].next_save_provenance = Some(prov);
                        self.save(doc_id);
                        self.features.agent.accept_undo.push(AcceptRecord {
                            doc: doc_id,
                            before: ContentHash::of(&current),
                            prior,
                            updated_after: d.updated,
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => self.notify(NoticeLevel::Error, format!("Couldn't apply the suggestion to “{}”: {e}", path.stem())),
            }
        }
        for (path, why) in &report.failed {
            self.notify(NoticeLevel::Error, format!("“{}”: {why}", path.stem()));
        }
        if accept && conflicts > 0 {
            self.notify(
                NoticeLevel::Info,
                format!("{conflicts} conflicting change{} left for you to look at", if conflicts == 1 { " is" } else { "s are" }),
            );
        }
        self.features.agent.reload_inbox_soon();
        if self.features.agent.review.as_ref().is_some_and(|r| &r.proposal.id == id) {
            self.reload_review();
        }
        self.features.decor_generation += 1;
    }

    /// Move and rename the open note `doc_id` from `from` to `to` the way
    /// the person would (links follow, the tab keeps the note).
    fn move_open_note(&mut self, doc_id: DocId, from: &NotePath, to: &NotePath) -> Result<(), String> {
        let prev = self.active;
        self.activate(doc_id);
        let moved = if to.folder() == from.folder() { Ok(()) } else { self.move_active(to.folder()) };
        let r = moved.and_then(|()| if to.stem() == from.stem() { Ok(()) } else { self.rename_active(to.stem()) });
        self.active = prev.filter(|i| *i < self.docs.len()).or(self.active);
        r.map_err(|e| format!("Couldn't rename “{}”: {e}", from.stem()))
    }

    /// Accept or reject one whole change (a new note, a rename, a deletion)
    /// from the Inbox card.
    pub fn resolve_one_change(&mut self, id: &ProposalId, subject: &NotePath, accept: bool) {
        let Some(store) = self.agent_store().cloned() else { return };
        let open = self.docs.iter().find(|d| d.path.as_ref() == Some(subject)).map(|d| d.id);
        let Some((op, to)) =
            store.proposal(id).and_then(|p| p.pending_changes().find(|c| c.subject() == Some(subject)).map(|c| (c.op, c.to.clone())))
        else {
            return;
        };
        let result = match (op, open) {
            // An open note being renamed or deleted: the workspace moves or closes it first.
            (ChangeOp::Rename, Some(doc_id)) if accept => {
                let to = to.unwrap_or_else(|| subject.clone());
                self.move_open_note(doc_id, subject, &to)
                    .and_then(|()| store.mark_change(id, &to, ChangeState::Applied).map_err(|e| e.to_string()))
            }
            (ChangeOp::Delete, Some(doc_id)) if accept => {
                self.close(doc_id);
                store.resolve_change(id, subject, true).map(|_| ()).map_err(|e| format!("“{}”: {e}", subject.stem()))
            }
            // An edit of an open note goes through the editor (undoable).
            (ChangeOp::Edit | ChangeOp::Create, Some(_)) => {
                if self.start_review(id, Some(subject)) {
                    self.review_all_in_file(accept);
                }
                Ok(())
            }
            _ => store.resolve_change(id, subject, accept).map(|_| ()).map_err(|e| format!("“{}”: {e}", subject.stem())),
        };
        if let Err(e) = result {
            self.notify(NoticeLevel::Error, e);
        }
        self.features.agent.reload_inbox_soon();
        if self.features.agent.review.as_ref().is_some_and(|r| &r.proposal.id == id) {
            self.reload_review();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use quire_agent::{Actor, Change, ChangeSet, SubmitOutcome, Via};

    use crate::Settings;
    use crate::vault::Vault;

    /// Design §6.2 WS-A acceptance: an MCP `str_replace` on a person's note
    /// makes a proposal; its two hunks are accepted one by one in the open
    /// editor, and Ctrl+Z undoes the second (which comes back for review).
    #[test]
    fn two_hunks_accepted_one_by_one_and_the_second_undone() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[(
            "Rope Notes.md",
            "# Rope Notes
Ropes are trees.
They are fast.
The end.
",
        )]));
        let doc = ws.open_note(&"Rope Notes.md".into()).unwrap();
        let store = ws.agent_store().unwrap().clone();
        let agent = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d");
        // What `str_replace` sends: the whole new text.
        let new = "# Rope Notes
Ropes are balanced trees.
They are fast.
Undo is cheap.
";
        let SubmitOutcome::Proposed { id, .. } =
            store.submit(&agent, ChangeSet::one(Change::Edit { path: "Rope Notes.md".into(), new_text: new.into() })).unwrap()
        else {
            panic!("a person's note gets a proposal")
        };
        // The palette offers the inbox; review keys only while reviewing.
        assert!(ws.command_available(crate::Command::OpenInbox) && ws.command_available(crate::Command::LintVault));
        assert!(!ws.command_available(crate::Command::NextHunk));
        assert!(ws.start_review(&id, None));
        assert!(ws.command_available(crate::Command::NextHunk) && ws.command_available(crate::Command::AcceptHunk));
        assert_eq!(ws.features.agent.review.as_ref().unwrap().hunks.len(), 2);
        let decos = ws.decorations(doc);
        assert!(decos.iter().any(|d| matches!(d.kind, crate::decor::DecorationKind::Strike)));
        assert_eq!(decos.iter().filter(|d| matches!(d.kind, crate::decor::DecorationKind::Widget { .. })).count(), 2);
        ws.review_decide(0, HunkAct::Accept);
        assert_eq!(
            &*ws.doc(doc).unwrap().text(),
            "# Rope Notes
Ropes are balanced trees.
They are fast.
The end.
"
        );
        assert_eq!(ws.features.agent.review.as_ref().unwrap().hunks.len(), 1);
        ws.review_decide(0, HunkAct::Accept);
        assert_eq!(&*ws.doc(doc).unwrap().text(), new);
        assert!(ws.features.agent.review.is_none(), "all decided");
        assert!(store.proposal(&id).is_none(), "settled and archived");
        // Saved at once, with who made it.
        ws.history.flush();
        assert_eq!(ws.vault().unwrap().read(&"Rope Notes.md".into()).unwrap(), new);
        let h = ws.vault().unwrap().history(&"Rope Notes.md".into());
        let prov = quire_agent::Provenance::from_json(&ws.vault().unwrap().read_provenance(&h[0]).unwrap()).unwrap();
        assert!(prov.actor.is_human() && prov.accepted_from.is_some_and(|a| a.id == "mcp:claude-code"));
        // Ctrl+Z: the second hunk leaves the note and returns as a suggestion.
        assert!(ws.docs.iter_mut().find(|d| d.id == doc).unwrap().undo());
        assert_eq!(
            &*ws.doc(doc).unwrap().text(),
            "# Rope Notes
Ropes are balanced trees.
They are fast.
The end.
"
        );
        let _ = crate::agent_state::tick(&mut ws, Instant::now());
        let p = store.proposal(&id).expect("the suggestion is back");
        assert_eq!(p.status, ProposalStatus::Pending);
        let r = ws.features.agent.review.as_ref().expect("reviewing again");
        assert_eq!(r.hunks.len(), 1);
        assert_eq!(r.hunks[0].new_lines, ["Undo is cheap.\n"]);
    }

    #[test]
    fn edits_conflicts_and_whole_proposals_from_the_editor() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("N.md", "a\nb\nc\nd\ne\n"), ("Old.md", "old\n")]));
        let doc = ws.open_note(&"N.md".into()).unwrap();
        let store = ws.agent_store().unwrap().clone();
        let agent = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d");
        let cs = ChangeSet {
            changes: vec![
                Change::Edit { path: "N.md".into(), new_text: "A\nb\nc\nd\nE\n".into() },
                Change::Create { path: "New.md".into(), text: "new\n".into() },
                Change::Delete { path: "Old.md".into() },
            ],
            ..ChangeSet::default()
        };
        let SubmitOutcome::Proposed { id, .. } = store.submit(&agent, cs).unwrap() else { panic!() };
        // The person types over the last line before reviewing.
        let i = ws.docs.iter().position(|d| d.id == doc).unwrap();
        ws.docs[i].replace_minimally("a\nb\nc\nd\ne!\n");
        assert!(ws.start_review(&id, None));
        let r = ws.features.agent.review.as_ref().unwrap();
        assert_eq!(r.hunks.iter().map(|h| h.conflict).collect::<Vec<_>>(), [false, true]);
        // Edit the first hunk's text, then accept all: the conflict stays.
        ws.review_edit(0);
        *ws.features.agent.review.as_ref().unwrap().edit_text.borrow_mut() = "A, edited".into();
        ws.review_decide(0, HunkAct::ConfirmEdit);
        assert_eq!(&*ws.doc(doc).unwrap().text(), "A, edited\nb\nc\nd\ne!\n");
        ws.review_all_in_file(true);
        assert_eq!(ws.features.agent.review.as_ref().unwrap().hunks.len(), 1, "the conflict is left");
        ws.review_decide(0, HunkAct::Reject);
        assert_eq!(&*ws.doc(doc).unwrap().text(), "A, edited\nb\nc\nd\ne!\n");
        // The rest of the proposal: a new note and a deletion, from the Inbox card.
        ws.resolve_whole_proposal(&id, true);
        let v = ws.vault().unwrap();
        assert_eq!(v.read(&"New.md".into()).unwrap(), "new\n");
        assert!(!v.exists(&"Old.md".into()));
        assert!(store.proposal(&id).is_none());
    }

    /// The review keys move the caret and ask the frontend to scroll to it
    /// (the review banner shows where you are); they don't open the Inbox.
    #[test]
    fn review_keys_reveal_the_caret() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("N.md", "one\ntwo\nthree\n")]));
        let doc = ws.open_note(&"N.md".into()).unwrap();
        let store = ws.agent_store().unwrap().clone();
        let agent = Actor::agent("mcp:x", "X", Via::McpStdio, "d");
        let cs = ChangeSet::one(Change::Edit { path: "N.md".into(), new_text: "ONE\ntwo\nTHREE\n".into() });
        let SubmitOutcome::Proposed { id, .. } = store.submit(&agent, cs).unwrap() else { panic!() };
        assert!(ws.start_review(&id, None));
        let _ = ws.take_ui_requests();
        assert_eq!(ws.run(crate::Command::NextHunk), None);
        assert_eq!(ws.take_ui_requests(), vec![UiRequest::RevealCaret]);
        let r = ws.features.agent.review.as_ref().unwrap();
        assert_eq!(r.current, 1);
        let line = ws.doc(doc).unwrap().buffer().char_to_line(ws.doc(doc).unwrap().selection().head);
        assert_eq!(line, 2, "the caret is on the second hunk");
        assert_eq!(ws.run(crate::Command::AcceptHunk), None);
        assert!(ws.doc(doc).unwrap().text().contains("THREE"));
    }

    #[test]
    fn action_ids_round_trip() {
        for act in HunkAct::ALL {
            let id = encode(7, 199, act);
            assert_eq!(decode(id), Some((7, 199, act)));
        }
        assert_eq!(decode(0), None);
    }
}
