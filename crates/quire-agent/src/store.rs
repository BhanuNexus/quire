// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! [`AgentStore`]: the one write path for everything that isn't the person
//! typing (design §2.5). MCP, the agent panel, ACP, the CLI and Quire's
//! jobs all call [`AgentStore::submit`], so policy is enforced in one place.
//!
//! A change set is applied when every change in it may be applied directly
//! (agent zones, new files in `raw/`); otherwise the whole set becomes one
//! proposal and no file is touched. Applied changes keep a history snapshot
//! before and after (the after one with provenance) and a ledger entry.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use quire_markdown::frontmatter;
use quire_vault::{ContentHash, NotePath, Vault};
use unicode_normalization::UnicodeNormalization;

use crate::actor::{Actor, ActorKind, Clock, DeviceInfo, Op, Provenance};
use crate::change::{Change, ChangeSet, SubmitOutcome, WriteError};
use crate::ids::{BatchId, ProposalId};
use crate::inbox::InboxStore;
use crate::ledger::{Ledger, LedgerEntry, LedgerQuery};
use crate::policy::{Mode, OpKind, POLICY_PATH, Policy, Route, hard_rule};
use crate::proposal::{
    Decided, HunkDecision, Proposal, ProposalChange, ProposalFilter, ProposalStatus, ProposalStore, ProposalSummary, Review,
};
use crate::revert::{RevertPlan, RevertSelector};

/// The policy file as last read: (hash of its text, parsed policy or why not).
type CachedPolicy = Option<(ContentHash, Result<Policy, String>)>;

struct Inner {
    vault: Vault,
    device: DeviceInfo,
    clock: Arc<dyn Clock>,
    policy: RwLock<CachedPolicy>,
    /// Grant caps per actor id (stdio without `--allow-write` is `Read`).
    caps: RwLock<HashMap<String, Mode>>,
}

/// Agent state of one vault on this device. Cheap to clone.
#[derive(Clone)]
pub struct AgentStore {
    inner: Arc<Inner>,
}

/// A change with its paths checked and its resulting text worked out.
struct Planned {
    change: Change,
    route: Route,
    /// Text before (edits, deletes, renames).
    before: Option<String>,
    /// Text after (creates and edits).
    after: Option<String>,
}

const fn rank(m: Mode) -> u8 {
    match m {
        Mode::Deny => 0,
        Mode::Read => 1,
        Mode::Propose => 2,
        Mode::CreateOnly => 3,
        Mode::Direct => 4,
    }
}

/// A vault path an agent may name: NFC, relative, no `..`, no hidden
/// segments except `.quire` (which policy then denies), no control characters.
pub fn canonical_path(p: &NotePath) -> Result<NotePath, WriteError> {
    let s: String = p.as_str().nfc().collect();
    let bad = |reason: &str| Err(WriteError::InvalidPath { reason: format!("{s}: {reason}") });
    if s.is_empty() || s.ends_with('/') {
        return bad("not a file path");
    }
    if s.chars().any(char::is_control) {
        return bad("contains control characters");
    }
    if s.starts_with('/') || s.contains(':') {
        return bad("must be relative to the vault");
    }
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return bad("has an empty, `.` or `..` segment");
        }
        if seg.starts_with('.') && !seg.eq_ignore_ascii_case(".quire") {
            return bad("is inside a hidden folder");
        }
    }
    Ok(NotePath::new(s))
}

impl AgentStore {
    pub fn open(vault: &Vault, device: DeviceInfo, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(Inner { vault: vault.clone(), device, clock, policy: RwLock::new(None), caps: RwLock::new(HashMap::new()) }),
        }
    }

    pub fn vault(&self) -> &Vault {
        &self.inner.vault
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.inner.device
    }

    pub fn now_ms(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    /// Limit what `actor_id` may do whatever the zones say (a client's grant).
    pub fn set_cap(&self, actor_id: &str, cap: Mode) {
        self.inner.caps.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(actor_id.to_string(), cap);
    }

    /// The vault's policy file, re-read when it changes. `None` when there
    /// is none (or it can't be read): then the defaults apply.
    pub fn file_policy(&self) -> Option<Policy> {
        let text = self.inner.vault.storage().read(&NotePath::new(POLICY_PATH)).ok()?;
        let hash = ContentHash::of(&text);
        if let Some((h, p)) = self.inner.policy.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
            && *h == hash
        {
            return p.as_ref().ok().cloned();
        }
        let parsed = Policy::parse(&text);
        let out = parsed.as_ref().ok().cloned();
        *self.inner.policy.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((hash, parsed));
        out
    }

    /// The policy in force: the file's, or the defaults. Limits come from here.
    pub fn policy(&self) -> Policy {
        self.file_policy().unwrap_or_default()
    }

    /// What `actor` may do to `path` for `op`. People are never limited.
    /// For anyone else the most restrictive of these applies: the vault's
    /// zones, the default zones (a policy file can only narrow them until
    /// device approval exists), the hard rules, and the actor's grant.
    pub fn effective_mode(&self, actor: &Actor, path: &NotePath, op: OpKind) -> Mode {
        self.effective(actor, path, op).0
    }

    /// The mode and the zone (folder prefix, or `default`) it came from.
    fn effective(&self, actor: &Actor, path: &NotePath, op: OpKind) -> (Mode, String) {
        if actor.is_human() {
            return (Mode::Direct, String::new());
        }
        let p = path.as_str();
        let defaults = Policy::default();
        let file = self.file_policy();
        let zone_of = |pol: &Policy| pol.zone_for(p).map_or_else(|| "default".to_string(), |z| z.path.clone());
        let mut candidates: Vec<(Mode, String)> = Vec::new();
        if actor.kind == ActorKind::Agent {
            if let Some(f) = &file {
                candidates.push((f.zone_mode(p), zone_of(f)));
            }
            candidates.push((defaults.zone_mode(p), zone_of(&defaults)));
        } else {
            candidates.push((Mode::Direct, String::new()));
        }
        if let Some(rule) = hard_rule(p, &self.inner.vault.config.templates_folder) {
            candidates.push((rule.cap(), p.to_string()));
        }
        if let Some(cap) = self.inner.caps.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(&actor.id) {
            candidates.push((*cap, "grant".into()));
        }
        // At equal outcome, report the narrower mode (a read-only grant reads as `read`).
        candidates.into_iter().min_by_key(|(m, _)| (m.route(op), rank(*m))).unwrap_or((Mode::Deny, String::new()))
    }

    fn plan_one(&self, actor: &Actor, change: Change, limit: u64) -> Result<Planned, WriteError> {
        let vault = &self.inner.vault;
        let read = |p: &NotePath| vault.storage().read(p).map_err(|_| WriteError::NotFound { path: p.clone() });
        let (change, op, before, after) = match change {
            Change::Create { path, text } => {
                let path = canonical_path(&path)?;
                if vault.exists(&path) {
                    return Err(WriteError::AlreadyExists { path });
                }
                (Change::Create { path, text: text.clone() }, OpKind::Create, None, Some(text))
            }
            Change::Edit { path, new_text } => {
                let path = canonical_path(&path)?;
                let before = read(&path)?;
                (Change::Edit { path, new_text: new_text.clone() }, OpKind::Edit, Some(before), Some(new_text))
            }
            Change::SetProperties { path, set, remove } => {
                let path = canonical_path(&path)?;
                let before = read(&path)?;
                let mut text = before.clone();
                for (k, v) in &set {
                    text = frontmatter::set_field(&text, k, v).apply(&text);
                }
                for k in &remove {
                    if let Some(e) = frontmatter::remove_field(&text, k) {
                        text = e.apply(&text);
                    }
                }
                (Change::Edit { path, new_text: text.clone() }, OpKind::Edit, Some(before), Some(text))
            }
            Change::Rename { from, to, update_links } => {
                let (from, to) = (canonical_path(&from)?, canonical_path(&to)?);
                let before = read(&from)?;
                if vault.exists(&to) {
                    return Err(WriteError::AlreadyExists { path: to });
                }
                // Renaming needs both: leaving the old place and arriving at the new one.
                let (to_mode, to_zone) = self.effective(actor, &to, OpKind::Create);
                if to_mode.route(OpKind::Create) == Route::Deny {
                    return Err(WriteError::PermissionDenied { path: to, zone: to_zone, mode: to_mode });
                }
                (Change::Rename { from, to, update_links }, OpKind::Rename, Some(before), None)
            }
            Change::Delete { path } => {
                let path = canonical_path(&path)?;
                let before = read(&path)?;
                (Change::Delete { path }, OpKind::Delete, Some(before), None)
            }
        };
        if let Some(t) = &after
            && t.len() as u64 > limit
        {
            return Err(WriteError::TooLarge { limit });
        }
        let path = change.path().clone();
        let (mode, zone) = self.effective(actor, &path, op);
        let mut route = mode.route(op);
        if let Change::Rename { to, .. } = &change {
            route = route.min(self.effective(actor, to, OpKind::Create).0.route(OpKind::Create));
        }
        match route {
            Route::Deny => Err(WriteError::PermissionDenied { path, zone, mode }),
            Route::Immutable => Err(WriteError::RawIsImmutable { path }),
            Route::Propose | Route::Apply => Ok(Planned { change, route, before, after }),
        }
    }

    /// Apply or propose `cs` as `actor`. Nothing is written when an error is returned.
    pub fn submit(&self, actor: &Actor, mut cs: ChangeSet) -> Result<SubmitOutcome, WriteError> {
        let policy = self.policy();
        let changes = std::mem::take(&mut cs.changes);
        if changes.len() as u64 > u64::from(policy.limits.max_files_per_change) {
            return Err(WriteError::TooLarge { limit: u64::from(policy.limits.max_files_per_change) });
        }
        let mut planned = Vec::with_capacity(changes.len());
        for change in changes {
            planned.push(self.plan_one(actor, change, policy.limits.max_write_bytes)?);
        }
        for (path, expected) in &cs.expected {
            let path = canonical_path(path)?;
            if let Ok(text) = self.inner.vault.storage().read(&path) {
                let current = ContentHash::of(&text);
                if current != *expected {
                    // The served-version cache (WS-B) will fill in the diff and a merged preview.
                    return Err(WriteError::VersionConflict {
                        path,
                        expected: expected.clone(),
                        current,
                        diff_since: String::new(),
                        merged_preview: None,
                    });
                }
            }
        }
        if planned.iter().all(|p| p.route == Route::Apply) { self.apply(actor, &cs, planned) } else { self.propose(actor, &cs, &planned) }
    }

    fn ledger(&self) -> Ledger {
        Ledger::new(self.inner.vault.clone(), &self.inner.device.id)
    }

    fn provenance(&self, actor: &Actor, cs: &ChangeSet, op: Op, text: &str) -> serde_json::Value {
        let mut p = Provenance::new(actor.clone(), op, text, self.now_ms());
        p.run.clone_from(&cs.run);
        p.sources.clone_from(&cs.sources);
        p.to_json()
    }

    /// Newest snapshot time of `path`, after a write.
    fn newest_snapshot(&self, path: &NotePath) -> Option<i64> {
        self.inner.vault.history(path).first().map(|s| s.at_ms)
    }

    fn apply(&self, actor: &Actor, cs: &ChangeSet, planned: Vec<Planned>) -> Result<SubmitOutcome, WriteError> {
        let vault = &self.inner.vault;
        let now = self.now_ms();
        let batch = (planned.len() > 1).then(|| BatchId::new(now).to_string());
        let mut versions = Vec::new();
        for p in planned {
            let mut entry = LedgerEntry::new(actor.clone(), Op::Write, now);
            entry.run.clone_from(&cs.run);
            entry.batch.clone_from(&batch);
            match p.change {
                Change::Create { path, text } => {
                    vault.storage().write_atomic(&path, &text)?;
                    let _ = vault.snapshot_with(&path, &text, now, true, Some(&self.provenance(actor, cs, Op::Create, &text)));
                    entry.op = Op::Create;
                    entry.after = Some(ContentHash::of(&text));
                    entry.snap_after = self.newest_snapshot(&path);
                    entry.bytes = Some(text.len() as u64);
                    versions.push((path.clone(), ContentHash::of(&text)));
                    entry.path = Some(path);
                }
                Change::Edit { path, new_text } => {
                    let before = p.before.unwrap_or_default();
                    if before != new_text {
                        // Keep what was there (if history doesn't have it yet), then the new version.
                        let _ = vault.snapshot(&path, &before, now - 1, true);
                        entry.snap_before = self.newest_snapshot(&path);
                        vault.storage().write_atomic(&path, &new_text)?;
                        let _ = vault.snapshot_with(&path, &new_text, now, true, Some(&self.provenance(actor, cs, Op::Write, &new_text)));
                        entry.snap_after = self.newest_snapshot(&path);
                    }
                    entry.before = Some(ContentHash::of(&before));
                    entry.after = Some(ContentHash::of(&new_text));
                    entry.bytes = Some(new_text.len() as u64);
                    versions.push((path.clone(), ContentHash::of(&new_text)));
                    entry.path = Some(path);
                }
                Change::Rename { from, to, update_links } => {
                    if update_links {
                        // Link rewrites in other notes are part of the rename (WS-A records them in the batch).
                        if from.folder() == to.folder() {
                            vault.rename_note(&from, to.stem())?;
                        } else {
                            let moved = vault.move_note(&from, to.folder())?;
                            if moved.new_path != to {
                                vault.rename_note(&moved.new_path, to.stem())?;
                            }
                        }
                    } else {
                        vault.storage().rename(&from, &to)?;
                        vault.move_history(&from, &to)?;
                    }
                    let text = p.before.unwrap_or_default();
                    entry.op = Op::Rename;
                    entry.before = Some(ContentHash::of(&text));
                    entry.to = Some(to.clone());
                    versions.push((to, ContentHash::of(&text)));
                    entry.path = Some(from);
                }
                Change::Delete { path } => {
                    let text = p.before.unwrap_or_default();
                    let _ = vault.snapshot(&path, &text, now - 1, true);
                    entry.snap_before = self.newest_snapshot(&path);
                    vault.trash(&path)?;
                    entry.op = Op::Delete;
                    entry.before = Some(ContentHash::of(&text));
                    entry.path = Some(path);
                }
                Change::SetProperties { .. } => {} // planned as an edit
            }
            // The ledger is a record, not the write: a failure to log never undoes the change.
            let _ = self.ledger().append(&entry);
        }
        Ok(SubmitOutcome::Applied { versions })
    }

    fn propose(&self, actor: &Actor, cs: &ChangeSet, planned: &[Planned]) -> Result<SubmitOutcome, WriteError> {
        let now = self.now_ms();
        let id = ProposalId::new(now);
        let changes = planned
            .iter()
            .map(|p| match &p.change {
                Change::Create { path, text } => ProposalChange::create(path, text),
                Change::Edit { path, new_text } => ProposalChange::edit(path, p.before.as_deref().unwrap_or_default(), new_text),
                Change::Rename { from, to, update_links } => ProposalChange::rename(from, to, *update_links),
                Change::Delete { path } => ProposalChange::delete(path),
                Change::SetProperties { path, .. } => {
                    ProposalChange::edit(path, p.before.as_deref().unwrap_or_default(), p.after.as_deref().unwrap_or_default())
                }
            })
            .collect();
        let title = if cs.title.trim().is_empty() {
            format!("Changes to {}", planned.first().map(|p| p.change.path().stem().to_string()).unwrap_or_default())
        } else {
            cs.title.clone()
        };
        let proposal = Proposal {
            format: crate::proposal::PROPOSAL_FORMAT,
            id: id.clone(),
            created: now,
            updated: now,
            status: ProposalStatus::Pending,
            actor: actor.clone(),
            run: cs.run.clone(),
            title,
            rationale: cs.rationale.clone(),
            sources: cs.sources.clone(),
            changes,
            other: crate::actor::Other::new(),
        };
        ProposalStore::new(self.inner.vault.clone()).save(&proposal)?;
        for p in planned {
            let mut e = LedgerEntry::new(actor.clone(), Op::Propose, now).path(p.change.path());
            e.proposal = Some(id.clone());
            e.run.clone_from(&cs.run);
            e.before = p.before.as_deref().map(ContentHash::of);
            e.after = p.after.as_deref().map(ContentHash::of);
            let _ = self.ledger().append(&e);
        }
        // Amending the same actor's pending proposal instead is WS-A's (design §A.4).
        Ok(SubmitOutcome::Proposed { id, amended: false })
    }

    pub fn proposals(&self, filter: &ProposalFilter) -> Vec<ProposalSummary> {
        ProposalStore::new(self.inner.vault.clone()).list().iter().map(Proposal::summary).filter(|s| filter.matches(s)).collect()
    }

    pub fn proposal(&self, id: &ProposalId) -> Option<Proposal> {
        ProposalStore::new(self.inner.vault.clone()).load(id)
    }

    /// The hunks of `path`'s change in proposal `id`, rebased on `current`
    /// (the open note's buffer, or the file). Not available yet (WS-A).
    #[expect(clippy::unused_self, reason = "WS-A implements this against the store's proposal files")]
    pub fn review(&self, id: &ProposalId, path: &NotePath, current: &str) -> Review {
        let _ = (id, path, current);
        Review::default()
    }

    /// Apply `decisions` to `current`. Until hunk review exists this
    /// changes nothing and leaves the proposal pending (WS-A).
    #[expect(clippy::unused_self, reason = "WS-A implements this against the store's proposal files")]
    pub fn decide(&self, id: &ProposalId, path: &NotePath, decisions: &[HunkDecision], current: &str) -> Decided {
        let _ = (id, path, decisions);
        Decided { text: current.to_string(), status: ProposalStatus::Pending, pending: 0 }
    }

    /// Append to this device's ledger.
    pub fn record(&self, entry: &LedgerEntry) {
        let _ = self.ledger().append(entry);
    }

    /// Ledger entries of this device.
    pub fn log(&self, q: &LedgerQuery) -> Vec<LedgerEntry> {
        self.ledger().read(q)
    }

    /// A dry run of undoing the selected events. Lists the events; the
    /// steps arrive with WS-A.
    pub fn revert_plan(&self, sel: &RevertSelector) -> RevertPlan {
        let q = LedgerQuery { since: sel.since, actor: sel.actor.clone(), run: sel.run.clone() };
        let events: Vec<LedgerEntry> = self
            .log(&q)
            .into_iter()
            .filter(|e| matches!(e.op, Op::Write | Op::Create | Op::Rename | Op::Delete))
            .filter(|e| sel.paths.as_ref().is_none_or(|ps| e.path.as_ref().is_some_and(|p| ps.contains(p))))
            .collect();
        RevertPlan {
            events,
            steps: Vec::new(),
            note: Some("Undoing agent changes isn't available yet; use Version history for each note.".into()),
        }
    }

    pub fn inbox(&self) -> InboxStore {
        InboxStore::new(self.inner.vault.clone(), Arc::clone(&self.inner.clock))
    }
}
