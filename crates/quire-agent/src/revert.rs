// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Undoing what an agent (or a batch) did, from the ledger (design §A.1):
//! "undo everything Claude Code did since 14:00".
//!
//! [`AgentStore::revert_plan`] is a dry run. For each note it takes the
//! version before the first selected change and after the last one, and the
//! note as it is now:
//!
//! * now equal to "after": go back to "before";
//! * otherwise undo three-way (`merge3(base = after, mine = now, theirs =
//!   before)`), which keeps edits made since; if those edits overlap the
//!   agent's, the note gets a **revert proposal** for review instead (no
//!   conflict markers are ever written);
//! * a note the agent created is trashed (or proposed for deletion if it was
//!   edited since); a renamed note is renamed back when the old name is
//!   free; a deleted one is restored.
//!
//! [`AgentStore::revert_apply`] carries the plan out as one batch with its
//! own ledger `batch` id, so the revert itself can be undone the same way.

use std::collections::BTreeMap;

use quire_vault::{ContentHash, NotePath};

use crate::actor::{Actor, ActorKind, Op, Provenance};
use crate::change::WriteError;
use crate::ids::{BatchId, ProposalId, RunId};
use crate::ledger::{LedgerEntry, LedgerQuery};
use crate::proposal::{Proposal, ProposalChange, ProposalStatus, ProposalStore};
use crate::store::AgentStore;

/// Which events to undo; empty fields match everything (except the person's
/// own edits, which are never undone this way).
#[derive(Debug, Clone, Default)]
pub struct RevertSelector {
    /// Actor id (`mcp:claude-code`).
    pub actor: Option<String>,
    pub run: Option<RunId>,
    /// Unix ms.
    pub since: Option<i64>,
    pub paths: Option<Vec<NotePath>>,
    /// One batch (`b_…`): a revert, a tag rename, a replace across the vault.
    pub batch: Option<String>,
    /// Unix ms; events after it are left alone.
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertAction {
    /// Write this text (a clean three-way undo).
    Restore { text: String },
    /// Move a file the agent created to the trash.
    Trash,
    /// Rename back to this path.
    RenameBack { to: NotePath },
    /// Bring back a deleted note from the trash or history.
    Undelete { text: String },
    /// The undo conflicts with later edits: a revert proposal from `base`
    /// (the agent's last version) to `text` (the version before it).
    Propose { base: String, text: String },
    /// A note the agent created was edited since: propose deleting it.
    ProposeDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertStep {
    pub path: NotePath,
    pub action: RevertAction,
}

impl RevertStep {
    /// One line for the dry-run list.
    pub fn describe(&self) -> String {
        let name = self.path.stem();
        match &self.action {
            RevertAction::Restore { .. } => format!("Undo the changes to “{name}”"),
            RevertAction::Trash => format!("Move “{name}” (created by the agent) to the trash"),
            RevertAction::RenameBack { to } => format!("Rename “{name}” back to “{}”", to.stem()),
            RevertAction::Undelete { .. } => format!("Bring back “{name}”"),
            RevertAction::Propose { .. } => format!("“{name}” was edited since: suggest the undo for review"),
            RevertAction::ProposeDelete => format!("Suggest deleting “{name}” (created by the agent and maybe edited since), for review"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevertPlan {
    /// The ledger events selected, oldest first.
    pub events: Vec<LedgerEntry>,
    pub steps: Vec<RevertStep>,
    /// Notes that can't be undone, and why.
    pub skipped: Vec<(NotePath, String)>,
    /// Why there is nothing to do, when there isn't.
    pub note: Option<String>,
}

/// What [`AgentStore::revert_apply`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevertReport {
    pub batch: String,
    /// Steps carried out directly.
    pub applied: usize,
    /// The proposal holding the undos that need review.
    pub proposal: Option<ProposalId>,
    pub failed: Vec<(NotePath, String)>,
}

const fn is_write(op: Op) -> bool {
    matches!(op, Op::Write | Op::Create | Op::Rename | Op::Delete | Op::Revert)
}

impl AgentStore {
    /// The text of `path` in history at `at` (a ledger `snap_*` time), or
    /// failing that, a version whose hash is `hash`.
    fn version(&self, paths: &[&NotePath], at: Option<i64>, hash: Option<&ContentHash>) -> Option<String> {
        for p in paths {
            let hist = self.vault().history(p);
            if let Some(t) = at
                && let Some(s) = hist.iter().find(|s| s.at_ms == t)
                && let Ok(text) = self.vault().read_snapshot(s)
                && hash.is_none_or(|h| ContentHash::of(&text) == *h)
            {
                return Some(text);
            }
            if let Some(h) = hash {
                for s in hist.iter().take(200) {
                    if let Ok(text) = self.vault().read_snapshot(s)
                        && ContentHash::of(&text) == *h
                    {
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    /// The selected events, oldest first. An agent's changes include the
    /// suggestions of its that the person accepted (`accept` entries naming
    /// it in `from`): undoing what an agent did undoes those too, keeping
    /// the person's own later edits like any other undo.
    fn revert_events(&self, sel: &RevertSelector) -> Vec<LedgerEntry> {
        let q = LedgerQuery { since: sel.since, actor: None, run: sel.run.clone() };
        let by = |e: &LedgerEntry| -> Option<String> {
            match e.op {
                Op::Accept => e.from.clone(),
                op if is_write(op) && e.actor.kind != ActorKind::Human => Some(e.actor.id.clone()),
                _ => None,
            }
        };
        self.log(&q)
            .into_iter()
            .filter(|e| by(e).is_some_and(|a| sel.actor.as_ref().is_none_or(|want| *want == a)))
            .filter(|e| e.op != Op::Accept || e.snap_after.is_some() || e.after.is_some())
            .filter(|e| sel.batch.as_ref().is_none_or(|b| e.batch.as_ref() == Some(b)))
            .filter(|e| sel.until.is_none_or(|u| e.at <= u))
            .filter(|e| {
                sel.paths
                    .as_ref()
                    .is_none_or(|ps| e.path.as_ref().is_some_and(|p| ps.contains(p)) || e.to.as_ref().is_some_and(|p| ps.contains(p)))
            })
            .collect()
    }

    /// A dry run of undoing the selected events.
    pub fn revert_plan(&self, sel: &RevertSelector) -> RevertPlan {
        let events = self.revert_events(sel);
        let mut plan = RevertPlan { events: events.clone(), ..RevertPlan::default() };
        if events.is_empty() {
            plan.note = Some("Nothing to undo: no matching changes on this device.".into());
            return plan;
        }
        let vault = self.vault();
        // Renames, newest first: back to the old name when it is free. Later
        // events are filed under the new name; they are undone at the old one.
        let mut renamed: BTreeMap<NotePath, NotePath> = BTreeMap::new();
        for e in events.iter().rev().filter(|e| e.op == Op::Rename) {
            let (Some(from), Some(to)) = (&e.path, &e.to) else { continue };
            let now_at = renamed.get(to).cloned().unwrap_or_else(|| to.clone());
            if !vault.exists(&now_at) {
                plan.skipped.push((to.clone(), "it isn't there any more".into()));
                continue;
            }
            if vault.exists(from) && !from.as_str().eq_ignore_ascii_case(now_at.as_str()) {
                plan.skipped.push((now_at, format!("“{}” is taken, so it can't be renamed back", from.stem())));
                continue;
            }
            plan.steps.push(RevertStep { path: now_at.clone(), action: RevertAction::RenameBack { to: from.clone() } });
            renamed.insert(now_at, from.clone());
        }
        let home = |p: &NotePath| -> NotePath {
            let mut p = p.clone();
            // Follow renames back to where the note will be after the rename steps.
            for _ in 0..16 {
                match renamed.get(&p) {
                    Some(next) if *next != p => p = next.clone(),
                    _ => break,
                }
            }
            p
        };
        // Where each note is now, by where it ends up.
        let now_at: BTreeMap<NotePath, NotePath> = renamed.iter().map(|(now, home)| (home.clone(), now.clone())).collect();
        // Content, per note (by where it ends up).
        let mut by_path: BTreeMap<NotePath, Vec<&LedgerEntry>> = BTreeMap::new();
        for e in events.iter().filter(|e| e.op != Op::Rename) {
            if let Some(p) = &e.path {
                by_path.entry(home(p)).or_default().push(e);
            }
        }
        for (path, evs) in by_path {
            let (Some(first), Some(last)) = (evs.first(), evs.last()) else { continue };
            let recorded: Vec<&NotePath> = evs.iter().filter_map(|e| e.path.as_ref()).collect();
            let here = now_at.get(&path).unwrap_or(&path);
            let mut where_ = vec![here, &path];
            where_.extend(recorded.iter().copied());
            let current = vault.read(here).ok();
            let created = first.op == Op::Create;
            let deleted = last.op == Op::Delete;
            let after = if deleted { None } else { self.version(&where_, last.snap_after, last.after.as_ref()) };
            if created {
                match (&current, &after) {
                    (None, _) => {}
                    (Some(c), Some(a)) if c == a => plan.steps.push(RevertStep { path, action: RevertAction::Trash }),
                    (Some(_), _) => plan.steps.push(RevertStep { path, action: RevertAction::ProposeDelete }),
                }
                continue;
            }
            let Some(before) = self.version(&where_, first.snap_before, first.before.as_ref()) else {
                plan.skipped.push((path, "the version before the change isn't in history any more".into()));
                continue;
            };
            if deleted {
                if current.is_some() {
                    plan.skipped.push((path, "a note with that name exists again".into()));
                } else {
                    plan.steps.push(RevertStep { path, action: RevertAction::Undelete { text: before } });
                }
                continue;
            }
            let Some(current) = current else {
                plan.skipped.push((path, "it was deleted since".into()));
                continue;
            };
            if current == before {
                continue;
            }
            let Some(after) = after else {
                plan.skipped.push((path, "the agent's version isn't in history any more".into()));
                continue;
            };
            if current == after {
                plan.steps.push(RevertStep { path, action: RevertAction::Restore { text: before } });
                continue;
            }
            let m = quire_text::merge::merge3(&after, &current, &before);
            if m.is_clean() {
                if m.text != current {
                    plan.steps.push(RevertStep { path, action: RevertAction::Restore { text: m.text } });
                }
            } else {
                plan.steps.push(RevertStep { path, action: RevertAction::Propose { base: after, text: before } });
            }
        }
        if plan.steps.is_empty() && plan.note.is_none() {
            plan.note = Some(if plan.skipped.is_empty() {
                "Nothing to undo: those changes are already gone.".into()
            } else {
                "Nothing can be undone.".into()
            });
        }
        plan
    }

    /// Carry out `plan` as one batch: direct undos are written (with history
    /// and a ledger `revert` entry each); conflicting ones go into one
    /// revert proposal. `title` names that proposal.
    pub fn revert_apply(&self, plan: &RevertPlan, title: &str) -> RevertReport {
        let now = self.now_ms();
        let batch = BatchId::new(now).to_string();
        let actor = Actor::job("revert", &self.device().id);
        let mut report = RevertReport { batch: batch.clone(), ..RevertReport::default() };
        let vault = self.vault().clone();
        let mut proposed: Vec<ProposalChange> = Vec::new();
        let prov = |text: &str| {
            let mut p = Provenance::new(actor.clone(), Op::Revert, text, now);
            p.other.insert("batch".into(), batch.clone().into());
            p.to_json()
        };
        // Renames first, so content steps find their notes.
        let mut steps: Vec<&RevertStep> = plan.steps.iter().collect();
        steps.sort_by_key(|s| !matches!(s.action, RevertAction::RenameBack { .. }));
        for s in steps {
            let mut e = LedgerEntry::new(actor.clone(), Op::Revert, now).path(&s.path);
            e.batch = Some(batch.clone());
            let r: Result<(), WriteError> = (|| {
                match &s.action {
                    RevertAction::Restore { text } => {
                        let cur = vault.read(&s.path)?;
                        let _ = vault.snapshot(&s.path, &cur, now - 1, true);
                        e.snap_before = self.newest_snapshot(&s.path);
                        e.before = Some(ContentHash::of(&cur));
                        vault.storage().write_atomic(&s.path, text)?;
                        let _ = vault.snapshot_with(&s.path, text, now, true, Some(&prov(text)));
                        e.snap_after = self.newest_snapshot(&s.path);
                        e.after = Some(ContentHash::of(text));
                    }
                    RevertAction::Trash => {
                        let cur = vault.read(&s.path)?;
                        let _ = vault.snapshot(&s.path, &cur, now - 1, true);
                        e.snap_before = self.newest_snapshot(&s.path);
                        e.before = Some(ContentHash::of(&cur));
                        vault.trash(&s.path)?;
                        e.op = Op::Delete;
                    }
                    RevertAction::RenameBack { to } => {
                        let text = vault.read(&s.path)?;
                        self.rename_file(&s.path, to, true)?;
                        self.rename_hook(&s.path, to);
                        e.op = Op::Rename;
                        e.to = Some(to.clone());
                        e.before = Some(ContentHash::of(&text));
                        e.after = Some(ContentHash::of(&text));
                    }
                    RevertAction::Undelete { text } => {
                        if vault.exists(&s.path) {
                            return Err(WriteError::AlreadyExists { path: s.path.clone() });
                        }
                        vault.storage().write_atomic(&s.path, text)?;
                        let _ = vault.snapshot_with(&s.path, text, now, true, Some(&prov(text)));
                        e.op = Op::Create;
                        e.snap_after = self.newest_snapshot(&s.path);
                        e.after = Some(ContentHash::of(text));
                    }
                    RevertAction::Propose { base, text } => {
                        proposed.push(ProposalChange::edit(&s.path, base, text));
                        return Ok(());
                    }
                    RevertAction::ProposeDelete => {
                        proposed.push(ProposalChange::delete(&s.path));
                        return Ok(());
                    }
                }
                self.record(&e);
                report.applied += 1;
                Ok(())
            })();
            if let Err(err) = r {
                report.failed.push((s.path.clone(), err.to_string()));
            }
        }
        if !proposed.is_empty() {
            let p = Proposal {
                format: crate::proposal::PROPOSAL_FORMAT,
                id: ProposalId::new(now),
                created: now,
                updated: now,
                status: ProposalStatus::Pending,
                actor: actor.clone(),
                run: None,
                title: title.to_string(),
                rationale: "These notes were edited after the changes being undone. Review the undo hunk by hunk.".into(),
                sources: Vec::new(),
                trust: crate::actor::Trust::Trusted,
                untrusted_source: None,
                changes: proposed,
                other: crate::actor::Other::new(),
            };
            match ProposalStore::new(vault).save(&p) {
                Ok(()) => report.proposal = Some(p.id),
                Err(e) => report.failed.push((NotePath::new(""), e.to_string())),
            }
        }
        if report.applied > 0 {
            let mut e = LedgerEntry::new(actor, Op::Batch, now);
            e.batch = Some(batch);
            self.record(&e);
        }
        report
    }

    /// The undo of `sel` as one proposal by `actor` (the MCP `revert` tool:
    /// agents never revert directly). `None` when there is nothing to undo.
    pub fn propose_revert(&self, sel: &RevertSelector, actor: &Actor, title: &str) -> Result<Option<ProposalId>, WriteError> {
        let plan = self.revert_plan(sel);
        let vault = self.vault();
        let mut changes = Vec::new();
        for s in &plan.steps {
            changes.push(match &s.action {
                RevertAction::Restore { text } => ProposalChange::edit(&s.path, &vault.read(&s.path)?, text),
                RevertAction::Propose { base, text } => ProposalChange::edit(&s.path, base, text),
                RevertAction::Trash | RevertAction::ProposeDelete => ProposalChange::delete(&s.path),
                RevertAction::RenameBack { to } => ProposalChange::rename(&s.path, to, true),
                RevertAction::Undelete { text } => ProposalChange::create(&s.path, text),
            });
        }
        if changes.is_empty() {
            return Ok(None);
        }
        let now = self.now_ms();
        let p = Proposal {
            format: crate::proposal::PROPOSAL_FORMAT,
            id: ProposalId::new(now),
            created: now,
            updated: now,
            status: ProposalStatus::Pending,
            actor: actor.clone(),
            run: None,
            title: title.to_string(),
            rationale: String::new(),
            sources: Vec::new(),
            trust: crate::actor::Trust::Trusted,
            untrusted_source: None,
            changes,
            other: crate::actor::Other::new(),
        };
        ProposalStore::new(vault.clone()).save(&p)?;
        let mut e = LedgerEntry::new(actor.clone(), Op::Propose, now);
        e.proposal = Some(p.id.clone());
        self.record(&e);
        Ok(Some(p.id))
    }
}
