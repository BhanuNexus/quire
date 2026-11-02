// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reviewing and deciding proposals, and their lifecycle (design §A.4).
//!
//! * [`AgentStore::review`] — the hunks of one file of a proposal, rebased
//!   on the current text (the open note's buffer, or the file).
//! * [`AgentStore::decide`] / [`AgentStore::decide_checked`] — apply
//!   decisions to the current text and update the proposal. The caller puts
//!   the returned text in place (in the editor, as one undoable edit);
//!   [`AgentStore::accept_in_file`] does it for a note that isn't open.
//! * [`AgentStore::resolve_change`] / [`AgentStore::resolve_proposal`] —
//!   accept or reject whole changes (creates, renames, deletes, and edits of
//!   notes that aren't open). "Accept all" never applies conflict hunks.
//! * Lifecycle: expiry after 30 days, archive purge after 90, paths
//!   following renames, and proposals whose note was deleted.
//!
//! Every decision rereads the proposal file and refuses to overwrite one
//! that changed since the caller looked at it (another process decided):
//! the caller reviews again.

use quire_vault::{ContentHash, NotePath};

use crate::actor::{Actor, Op, Provenance};
use crate::change::WriteError;
use crate::ids::ProposalId;
use crate::ledger::LedgerEntry;
use crate::proposal::{
    ChangeOp, ChangeState, Decided, HunkAction, HunkDecision, Proposal, ProposalChange, ProposalStatus, ProposalStore, Review,
};
use crate::review::{self, ReviewCache};
use crate::store::AgentStore;

/// Pending proposals untouched for this long expire (archived, restorable).
pub const EXPIRE_AFTER_MS: i64 = 30 * 86_400_000;
/// Archived proposals are deleted after this long.
pub const PURGE_AFTER_MS: i64 = 90 * 86_400_000;

/// Why a decision wasn't recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecideError {
    /// Someone (another window or process) decided on this proposal since
    /// it was shown. Review it again.
    #[error("These suggestions changed while you were looking at them. Showing them again.")]
    Changed,
    #[error("{0}")]
    Write(#[from] WriteError),
}

/// What [`AgentStore::resolve_proposal`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveReport {
    /// Changes accepted or rejected.
    pub done: usize,
    /// Edits of open notes, left for the editor to apply.
    pub left: Vec<NotePath>,
    /// Changes that couldn't be applied, and why.
    pub failed: Vec<(NotePath, String)>,
    /// Conflict hunks left waiting ("Accept all" doesn't apply them).
    pub conflicts_left: usize,
}

/// The base and new text a change's hunks come from, for `current`.
fn texts(c: &ProposalChange, current: &str) -> Option<(String, String)> {
    match c.op {
        ChangeOp::Edit => Some((c.base_text()?, c.new_text.clone()?)),
        // A create into a path that now exists is an edit of what is there.
        ChangeOp::Create => Some((current.to_string(), c.text.clone()?)),
        ChangeOp::Rename | ChangeOp::Delete => None,
    }
}

fn change_index(p: &Proposal, path: &NotePath) -> Option<usize> {
    p.changes.iter().position(|c| c.state.is_none() && matches!(c.op, ChangeOp::Edit | ChangeOp::Create) && c.path.as_ref() == Some(path))
}

/// The state of a change whose last hunk was just decided.
fn final_state(decisions: &[HunkDecision]) -> ChangeState {
    let accepted = decisions.iter().any(|d| matches!(d.action, HunkAction::Accepted | HunkAction::Edited));
    let rejected = decisions.iter().any(|d| d.action == HunkAction::Rejected);
    match (accepted, rejected) {
        (true, true) => ChangeState::PartiallyApplied,
        (true, false) => ChangeState::Applied,
        (false, _) => ChangeState::Rejected,
    }
}

impl AgentStore {
    fn proposals_store(&self) -> ProposalStore {
        ProposalStore::new(self.vault().clone())
    }

    /// The hunks of `path`'s change in proposal `id`, rebased on `current`.
    /// Empty (but available) when the change is decided or isn't an edit.
    pub fn review(&self, id: &ProposalId, path: &NotePath, current: &str) -> Review {
        let Some(p) = self.proposal(id) else { return Review { hunks: Vec::new(), available: true } };
        let Some(c) = change_index(&p, path).map(|i| &p.changes[i]) else { return Review { hunks: Vec::new(), available: true } };
        let Some((base, new)) = texts(c, current) else { return Review { hunks: Vec::new(), available: true } };
        Review { hunks: review::review_hunks(path, &base, current, &new, c.review.as_ref()), available: true }
    }

    /// The provenance of a version the person made by accepting `p`'s
    /// suggestion (for `Document::next_save_provenance` and file writes).
    pub fn accepted_provenance(&self, p: &Proposal, text: &str) -> Provenance {
        let mut prov = Provenance::new(Actor::human(&self.device().id), Op::Accept, text, self.now_ms());
        prov.accepted_from = Some(p.actor.clone());
        prov.proposal = Some(p.id.clone());
        prov.run.clone_from(&p.run);
        prov.sources.clone_from(&p.sources);
        prov.trust = p.trust;
        prov
    }

    /// Apply `decisions` to `current` and record them. Returns the text to
    /// put in place of `current`. Unknown hunk ids are ignored.
    pub fn decide(&self, id: &ProposalId, path: &NotePath, decisions: &[HunkDecision], current: &str) -> Decided {
        match self.decide_checked(id, path, decisions, current, None) {
            Ok(d) => d,
            Err(_) => {
                Decided { text: current.to_string(), status: ProposalStatus::Pending, pending: 0, accepted: 0, rejected: 0, updated: 0 }
            }
        }
    }

    /// [`AgentStore::decide`], refusing when the proposal's `updated` isn't
    /// `seen` any more (someone else decided since it was shown).
    pub fn decide_checked(
        &self,
        id: &ProposalId,
        path: &NotePath,
        decisions: &[HunkDecision],
        current: &str,
        seen: Option<i64>,
    ) -> Result<Decided, DecideError> {
        self.decide_inner(id, path, decisions, current, seen, |_, _| Ok((None, None)))
    }

    /// Decide hunks of a note that isn't open: read it, apply, write it
    /// (with history before and after, and provenance), record.
    pub fn accept_in_file(
        &self,
        id: &ProposalId,
        path: &NotePath,
        decisions: &[HunkDecision],
        seen: Option<i64>,
    ) -> Result<Decided, DecideError> {
        let current = self.vault().read(path).map_err(|_| WriteError::NotFound { path: path.clone() })?;
        self.decide_inner(id, path, decisions, &current, seen, |p, text| {
            if text == current {
                return Ok((None, None));
            }
            let now = self.now_ms();
            let vault = self.vault();
            let _ = vault.snapshot(path, &current, now - 1, true);
            let before = self.newest_snapshot(path);
            vault.storage().write_atomic(path, text)?;
            let _ = vault.snapshot_with(path, text, now, true, Some(&self.accepted_provenance(p, text).to_json()));
            Ok((before, self.newest_snapshot(path)))
        })
    }

    fn decide_inner(
        &self,
        id: &ProposalId,
        path: &NotePath,
        decisions: &[HunkDecision],
        current: &str,
        seen: Option<i64>,
        commit: impl FnOnce(&Proposal, &str) -> Result<(Option<i64>, Option<i64>), WriteError>,
    ) -> Result<Decided, DecideError> {
        let store = self.proposals_store();
        let mut p = store.load(id).ok_or_else(|| WriteError::NotFound { path: NotePath::new(format!("{id}")) })?;
        let loaded = p.updated;
        if seen.is_some_and(|s| s != loaded) {
            return Err(DecideError::Changed);
        }
        let unchanged = |p: &Proposal| Decided {
            text: current.to_string(),
            status: p.status,
            pending: 0,
            accepted: 0,
            rejected: 0,
            updated: p.updated,
        };
        let Some(i) = change_index(&p, path) else { return Ok(unchanged(&p)) };
        let Some((base, new)) = texts(&p.changes[i], current) else { return Ok(unchanged(&p)) };
        let hunks = review::review_hunks(path, &base, current, &new, p.changes[i].review.as_ref());
        let now = self.now_ms().max(loaded + 1);
        let known: Vec<HunkDecision> =
            decisions.iter().filter(|d| hunks.iter().any(|h| h.id == d.hunk)).map(|d| HunkDecision { at: now, ..d.clone() }).collect();
        let applied = review::apply(current, &hunks, review::choices_from(&known));
        let (snap_before, snap_after) = commit(&p, &applied.text)?;
        let c = &mut p.changes[i];
        c.decisions.extend(known.iter().cloned());
        if applied.pending == 0 {
            c.state = Some(final_state(&c.decisions));
            c.review = None;
        } else {
            if c.op == ChangeOp::Create {
                // From now on it is an edit of the note that is there.
                c.op = ChangeOp::Edit;
                c.text = None;
            }
            c.set_base_text(&applied.base);
            c.new_text = Some(applied.new_text.clone());
            c.review = Some(ReviewCache::new(&applied.text, &applied.remaining));
        }
        p.updated = now;
        if let Some(s) = p.settled_status() {
            p.status = s;
        }
        if !store.save_if(&p, loaded)? {
            return Err(DecideError::Changed);
        }
        if p.status != ProposalStatus::Pending {
            let _ = store.archive(&p.id);
        }
        for (op, n) in [(Op::Accept, applied.accepted), (Op::Reject, applied.rejected)] {
            if n == 0 {
                continue;
            }
            let mut e = LedgerEntry::new(Actor::human(&self.device().id), op, now).path(path);
            e.proposal = Some(p.id.clone());
            e.run.clone_from(&p.run);
            e.from = Some(p.actor.id.clone());
            e.before = Some(ContentHash::of(current));
            e.after = Some(ContentHash::of(&applied.text));
            if op == Op::Accept {
                (e.snap_before, e.snap_after) = (snap_before, snap_after);
            }
            self.record(&e);
        }
        Ok(Decided {
            text: applied.text,
            status: p.status,
            pending: applied.pending,
            accepted: applied.accepted,
            rejected: applied.rejected,
            updated: p.updated,
        })
    }

    /// Set the state of a whole change (after the caller applied or dropped
    /// it some other way, e.g. a rename done by the editor), and settle.
    pub fn mark_change(&self, id: &ProposalId, subject: &NotePath, state: ChangeState) -> Result<(), WriteError> {
        let store = self.proposals_store();
        let Some(mut p) = store.load(id) else { return Err(WriteError::NotFound { path: subject.clone() }) };
        let Some(c) = p.changes.iter_mut().find(|c| c.state.is_none() && c.subject() == Some(subject)) else { return Ok(()) };
        c.state = Some(state);
        c.review = None;
        let op = if state == ChangeState::Rejected { Op::Reject } else { Op::Accept };
        self.settle(&store, &mut p)?;
        let mut e = LedgerEntry::new(Actor::human(&self.device().id), op, self.now_ms()).path(subject);
        e.proposal = Some(p.id.clone());
        e.from = Some(p.actor.id.clone());
        self.record(&e);
        Ok(())
    }

    fn settle(&self, store: &ProposalStore, p: &mut Proposal) -> Result<(), WriteError> {
        p.updated = self.now_ms().max(p.updated + 1);
        if let Some(s) = p.settled_status() {
            p.status = s;
        }
        store.save(p)?;
        if p.status != ProposalStatus::Pending {
            let _ = store.archive(&p.id);
        }
        Ok(())
    }

    /// Accept or reject one whole change of a proposal, by the path it is
    /// about (the old path of a rename). Edits apply to the file on disk;
    /// with `accept`, conflict hunks are left waiting. Returns the change's
    /// state afterwards (`None`: conflict hunks still wait).
    pub fn resolve_change(&self, id: &ProposalId, subject: &NotePath, accept: bool) -> Result<Option<ChangeState>, WriteError> {
        let store = self.proposals_store();
        let Some(mut p) = store.load(id) else { return Err(WriteError::NotFound { path: subject.clone() }) };
        let Some(i) = p.changes.iter().position(|c| c.state.is_none() && c.subject() == Some(subject)) else { return Ok(None) };
        let c = p.changes[i].clone();
        let vault = self.vault().clone();
        let edit_like = c.op == ChangeOp::Edit || (c.op == ChangeOp::Create && vault.exists(subject));
        if edit_like {
            let Ok(current) = vault.read(subject) else {
                return self.supersede(&store, &mut p, i).map(Some);
            };
            let hunks = self.review(id, subject, &current).hunks;
            let now = self.now_ms();
            let decisions: Vec<HunkDecision> = hunks
                .iter()
                .filter(|h| !accept || !h.conflict)
                .map(|h| HunkDecision {
                    hunk: h.id.clone(),
                    action: if accept { HunkAction::Accepted } else { HunkAction::Rejected },
                    at: now,
                    text: None,
                    other: crate::actor::Other::new(),
                })
                .collect();
            let d = if accept {
                self.accept_in_file(id, subject, &decisions, Some(p.updated))
            } else {
                self.decide_checked(id, subject, &decisions, &current, Some(p.updated))
            };
            return match d {
                Ok(d) if d.pending > 0 => Ok(None),
                Ok(_) => Ok(Some(if accept { ChangeState::Applied } else { ChangeState::Rejected })),
                Err(DecideError::Changed) => Err(WriteError::Io("the suggestion changed; try again".into())),
                Err(DecideError::Write(e)) => Err(e),
            };
        }
        if !accept {
            p.changes[i].state = Some(ChangeState::Rejected);
            self.settle(&store, &mut p)?;
            self.log_decision(&p, Op::Reject, subject, None);
            return Ok(Some(ChangeState::Rejected));
        }
        let now = self.now_ms();
        let human = Actor::human(&self.device().id);
        match c.op {
            ChangeOp::Create => {
                let text = c.text.unwrap_or_default();
                vault.storage().write_atomic(subject, &text)?;
                let _ = vault.snapshot_with(subject, &text, now, true, Some(&self.accepted_provenance(&p, &text).to_json()));
                self.log_decision(&p, Op::Accept, subject, Some(&text));
            }
            ChangeOp::Delete => {
                let Ok(text) = vault.read(subject) else { return self.supersede(&store, &mut p, i).map(Some) };
                let _ = vault.snapshot(subject, &text, now - 1, true);
                let snap = self.newest_snapshot(subject);
                vault.trash(subject)?;
                let mut e = LedgerEntry::new(human, Op::Delete, now).path(subject);
                e.proposal = Some(p.id.clone());
                e.from = Some(p.actor.id.clone());
                e.before = Some(ContentHash::of(&text));
                e.snap_before = snap;
                self.record(&e);
            }
            ChangeOp::Rename => {
                let to = c.to.clone().unwrap_or_else(|| subject.clone());
                if !vault.exists(subject) {
                    return self.supersede(&store, &mut p, i).map(Some);
                }
                if vault.exists(&to) && !to.as_str().eq_ignore_ascii_case(subject.as_str()) {
                    return Err(WriteError::AlreadyExists { path: to });
                }
                self.rename_file(subject, &to, c.update_links.unwrap_or(true))?;
                let mut e = LedgerEntry::new(human, Op::Rename, now).path(subject);
                e.to = Some(to.clone());
                e.proposal = Some(p.id.clone());
                e.from = Some(p.actor.id.clone());
                self.record(&e);
                // Other changes of this and other proposals follow the note.
                for other in &mut p.changes {
                    for f in [&mut other.path, &mut other.from] {
                        if f.as_ref() == Some(subject) && other.state.is_none() && other.op != ChangeOp::Rename {
                            *f = Some(to.clone());
                        }
                    }
                }
                self.rename_hook(subject, &to);
            }
            ChangeOp::Edit => {}
        }
        p.changes[i].state = Some(ChangeState::Applied);
        self.settle(&store, &mut p)?;
        Ok(Some(ChangeState::Applied))
    }

    fn log_decision(&self, p: &Proposal, op: Op, path: &NotePath, text: Option<&str>) {
        let mut e = LedgerEntry::new(Actor::human(&self.device().id), op, self.now_ms()).path(path);
        e.proposal = Some(p.id.clone());
        e.run.clone_from(&p.run);
        e.from = Some(p.actor.id.clone());
        e.after = text.map(ContentHash::of);
        e.snap_after = text.and_then(|_| self.newest_snapshot(path));
        self.record(&e);
    }

    fn supersede(&self, store: &ProposalStore, p: &mut Proposal, i: usize) -> Result<ChangeState, WriteError> {
        p.changes[i].state = Some(ChangeState::Superseded);
        self.settle(store, p)?;
        Ok(ChangeState::Superseded)
    }

    /// Accept or reject every waiting change of a proposal. Edits of notes
    /// for which `in_editor` is true are left for the caller (listed in the
    /// report), so they go into the open note as an undoable edit.
    pub fn resolve_proposal(&self, id: &ProposalId, accept: bool, in_editor: &dyn Fn(&NotePath) -> bool) -> ResolveReport {
        let mut report = ResolveReport::default();
        let Some(p) = self.proposal(id) else { return report };
        // Renames last, so edits find their notes where they are now.
        let mut subjects: Vec<(bool, NotePath, bool)> = p
            .pending_changes()
            .filter_map(|c| Some((c.op == ChangeOp::Rename, c.subject()?.clone(), matches!(c.op, ChangeOp::Edit | ChangeOp::Create))))
            .collect();
        subjects.sort_by_key(|(rename, _, _)| *rename);
        for (_, path, edit_like) in subjects {
            if edit_like && in_editor(&path) && self.vault().exists(&path) {
                report.left.push(path);
                continue;
            }
            match self.resolve_change(id, &path, accept) {
                Ok(Some(_)) => report.done += 1,
                Ok(None) => {
                    report.done += 1;
                    report.conflicts_left += self.vault().read(&path).map_or(0, |t| self.review(id, &path, &t).hunks.len());
                }
                Err(e) => report.failed.push((path, e.to_string())),
            }
        }
        report
    }

    /// Rewrite paths in waiting proposals after a note moved (the
    /// workspace's rename hook, and renames done through `submit`).
    pub fn rename_hook(&self, from: &NotePath, to: &NotePath) {
        let store = self.proposals_store();
        for mut p in store.list().into_iter().filter(|p| p.status == ProposalStatus::Pending) {
            let mut changed = false;
            for c in p.changes.iter_mut().filter(|c| c.state.is_none()) {
                for f in [&mut c.path, &mut c.from] {
                    if f.as_ref() == Some(from) {
                        *f = Some(to.clone());
                        changed = true;
                    }
                }
            }
            if changed {
                p.updated = self.now_ms().max(p.updated + 1);
                let _ = store.save(&p);
            }
        }
    }

    /// `path` was deleted: its waiting edits, renames and deletes no longer
    /// apply. Returns one line per proposal affected, for a notice.
    pub fn note_removed(&self, path: &NotePath) -> Vec<String> {
        if self.vault().exists(path) {
            return Vec::new();
        }
        let store = self.proposals_store();
        let mut out = Vec::new();
        for mut p in store.list().into_iter().filter(|p| p.status == ProposalStatus::Pending) {
            let mut hit = false;
            for c in p.changes.iter_mut().filter(|c| c.state.is_none() && c.op != ChangeOp::Create && c.subject() == Some(path)) {
                c.state = Some(ChangeState::Superseded);
                hit = true;
            }
            if hit && self.settle(&store, &mut p).is_ok() {
                out.push(format!("{}'s suggestion for “{}” no longer applies: the note was deleted", p.actor.label, path.stem()));
            }
        }
        out
    }

    /// Expire proposals waiting longer than 30 days (they stay restorable
    /// in the archive) and delete archived ones older than 90. Returns how
    /// many expired.
    pub fn expire(&self) -> usize {
        let store = self.proposals_store();
        let now = self.now_ms();
        let mut n = 0;
        for mut p in store.list().into_iter().filter(|p| p.status == ProposalStatus::Pending && now - p.updated > EXPIRE_AFTER_MS) {
            p.status = ProposalStatus::Expired;
            p.updated = now;
            if store.save(&p).is_ok() && store.archive(&p.id).is_ok() {
                n += 1;
            }
        }
        store.purge_archive(now - PURGE_AFTER_MS, now);
        // Read events are kept for 30 days (design §A.1).
        self.ledger().prune_reads(now - 30 * 86_400_000, now);
        n
    }

    /// Bring back an archived proposal (an expired one) as waiting.
    pub fn restore_proposal(&self, id: &ProposalId) -> Result<(), WriteError> {
        self.proposals_store().restore(id, self.now_ms())
    }

    /// Put a proposal back as it was before a decision the person undid in
    /// the editor, if nothing else changed it since (`updated` is still
    /// `updated_after`). True when it was put back.
    pub fn put_back(&self, prior: &Proposal, updated_after: i64) -> bool {
        let store = self.proposals_store();
        let now = self.now_ms();
        let (current, archived) = match store.load(&prior.id) {
            Some(p) => (p, false),
            None => match store.archived(now).into_iter().find(|p| p.id == prior.id) {
                Some(p) => (p, true),
                None => return false,
            },
        };
        if current.updated != updated_after {
            return false;
        }
        let mut back = prior.clone();
        back.updated = now.max(updated_after + 1);
        if store.save(&back).is_err() {
            return false;
        }
        if archived {
            store.remove_archived(&current);
        }
        let mut e = LedgerEntry::new(Actor::human(&self.device().id), Op::Revert, now);
        e.proposal = Some(prior.id.clone());
        e.from = Some(prior.actor.id.clone());
        self.record(&e);
        true
    }

    /// Resolved and expired proposals, newest first ("Show resolved").
    pub fn archived_proposals(&self) -> Vec<Proposal> {
        self.proposals_store().archived(self.now_ms())
    }
}
