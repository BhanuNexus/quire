// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Inbox's data: everything waiting for the person, loaded off the UI
//! thread and grouped for the Inbox tab (design §A.5, §7.3).
//!
//! * Suggested changes, grouped by run (or by proposal when there is no run).
//! * Checks (lint findings), batched by check.
//! * Findings of other features (contradictions, staleness, citations).
//! * Permissions: a policy or instructions change waiting for approval.
//!
//! Snoozes are per device (`state.toml`); dismissals are the inbox store's.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use quire_agent::approval::DeviceState;
use quire_agent::inbox::{InboxItem, LintCheck, StoredItem};
use quire_agent::instructions::Instructions;
use quire_agent::{AgentStore, PolicyState, Proposal, ProposalStatus, ProposalSummary};

/// How often checks and staleness come back to the badge.
pub const DIGEST_MS: i64 = 7 * 86_400_000;

/// What the Inbox shows, as last loaded.
#[derive(Clone, Default)]
pub struct InboxData {
    /// Waiting proposals, oldest first.
    pub pending: Vec<Proposal>,
    /// Stored findings (lint, contradictions, staleness, …), newest first.
    pub findings: Vec<StoredItem>,
    /// Resolved proposals, newest first (loaded for "Show resolved").
    pub resolved: Vec<ProposalSummary>,
    pub policy: Option<Arc<PolicyState>>,
    pub instructions: Option<Instructions>,
    pub device: DeviceState,
    /// The vault's `[lint]` table (which checks are warnings).
    pub lint_prefs: quire_agent::lint::LintPrefs,
    pub loaded_at_ms: i64,
}

/// A section of the Inbox (also the kind filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Suggested,
    Checks,
    Contradictions,
    Stale,
    Citations,
    Permissions,
    Other,
}

impl Section {
    pub const ALL: [Self; 7] =
        [Self::Suggested, Self::Checks, Self::Contradictions, Self::Stale, Self::Citations, Self::Permissions, Self::Other];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Suggested => "Suggested changes",
            Self::Checks => "Checks",
            Self::Contradictions => "Contradictions",
            Self::Stale => "Getting stale",
            Self::Citations => "Citations",
            Self::Permissions => "Permissions",
            Self::Other => "Problems",
        }
    }

    pub const fn of(item: &InboxItem) -> Self {
        match item {
            InboxItem::Proposal(_) => Self::Suggested,
            InboxItem::Lint { .. } => Self::Checks,
            InboxItem::Contradiction { .. } => Self::Contradictions,
            InboxItem::Stale { .. } => Self::Stale,
            InboxItem::CitationDrift { .. } | InboxItem::VerifyFailed { .. } => Self::Citations,
            InboxItem::PolicyChanged { .. } | InboxItem::InstructionsChanged { .. } => Self::Permissions,
            InboxItem::JobFailed { .. } => Self::Other,
        }
    }
}

/// Proposals that belong together: one agent run, or one proposal.
#[derive(Debug, Clone)]
pub struct Group<'a> {
    pub actor_label: &'a str,
    pub actor_id: &'a str,
    /// The device the suggestions came from, when not this one: the name
    /// it reported about itself ("" when none).
    pub device_name: Option<&'a str>,
    /// That device's id.
    pub device_id: Option<&'a str>,
    /// When the first of them was made.
    pub at_ms: i64,
    pub title: &'a str,
    pub proposals: Vec<&'a Proposal>,
}

impl Group<'_> {
    /// How many notes the group's waiting changes touch.
    pub fn changes(&self) -> usize {
        self.proposals.iter().map(|p| p.pending_changes().count()).sum()
    }
}

/// Filters chosen in the Inbox tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub section: Option<Section>,
    /// Actor id.
    pub agent: Option<String>,
    /// Folder prefix (`notes/`).
    pub folder: Option<String>,
    pub show_snoozed: bool,
    pub show_resolved: bool,
}

fn in_folder(path: &str, folder: Option<&String>) -> bool {
    folder.is_none_or(|f| {
        let f = f.trim_matches('/');
        f.is_empty() || path.to_lowercase().starts_with(&format!("{}/", f.to_lowercase()))
    })
}

/// The path a finding is about.
pub fn item_path(item: &InboxItem) -> Option<&str> {
    match item {
        InboxItem::Lint { path, .. } | InboxItem::Stale { path, .. } | InboxItem::InstructionsChanged { path, .. } => Some(path.as_str()),
        InboxItem::CitationDrift { in_note, .. } | InboxItem::VerifyFailed { in_note, .. } => Some(in_note.as_str()),
        InboxItem::Contradiction { a, .. } => Some(a.path.as_str()),
        InboxItem::Proposal(p) => p.files.first().map(quire_vault::NotePath::as_str),
        InboxItem::PolicyChanged { .. } | InboxItem::JobFailed { .. } => None,
    }
}

/// The fingerprint a proposal is snoozed under (as `InboxItem::fingerprint`
/// gives for it, without building the summary).
pub fn proposal_fingerprint(p: &Proposal) -> String {
    quire_vault::ContentHash::of(&format!("proposal:{}", p.id)).hex()[..16].to_string()
}

impl InboxData {
    pub fn snoozed(&self, fingerprint: &str) -> bool {
        self.device.snoozed.get(fingerprint).is_some_and(|until| *until > self.loaded_at_ms)
    }

    /// Waiting proposals that pass `f`, grouped by run, oldest group first.
    pub fn groups(&self, f: &Filters) -> Vec<Group<'_>> {
        if f.section.is_some_and(|s| s != Section::Suggested) {
            return Vec::new();
        }
        let mut by_key: BTreeMap<String, Group<'_>> = BTreeMap::new();
        for p in &self.pending {
            if f.agent.as_ref().is_some_and(|a| *a != p.actor.id) || (!f.show_snoozed && self.snoozed(&proposal_fingerprint(p))) {
                continue;
            }
            if !p.pending_changes().filter_map(|c| c.subject()).any(|path| in_folder(path.as_str(), f.folder.as_ref())) {
                continue;
            }
            let origin = p.origin();
            let key = p.run.as_ref().map_or_else(|| p.id.to_string(), |r| format!("{}|{r}|{}", p.actor.id, origin.map_or("", |o| o.0)));
            let g = by_key.entry(key).or_insert_with(|| Group {
                actor_label: &p.actor.label,
                actor_id: &p.actor.id,
                device_name: origin.map(|(_, name)| name),
                device_id: origin.map(|(id, _)| id),
                at_ms: p.created,
                title: &p.title,
                proposals: Vec::new(),
            });
            g.at_ms = g.at_ms.min(p.created);
            g.proposals.push(p);
        }
        let mut v: Vec<Group<'_>> = by_key.into_values().collect();
        v.sort_by_key(|g| g.at_ms);
        v
    }

    /// Stored findings that pass `f`, by section, with lint batched per check.
    pub fn findings(&self, f: &Filters) -> Vec<(Section, Vec<&StoredItem>)> {
        let mut out: BTreeMap<Section, Vec<&StoredItem>> = BTreeMap::new();
        for s in &self.findings {
            let sec = Section::of(&s.item);
            if f.section.is_some_and(|x| x != sec) || (!f.show_snoozed && self.snoozed(&s.fingerprint)) {
                continue;
            }
            if f.agent.is_some() {
                continue;
            }
            if f.folder.is_some() && !item_path(&s.item).is_some_and(|p| in_folder(p, f.folder.as_ref())) {
                continue;
            }
            out.entry(sec).or_default().push(s);
        }
        for v in out.values_mut() {
            v.sort_by(|a, b| lint_order(&a.item).cmp(&lint_order(&b.item)).then_with(|| item_path(&a.item).cmp(&item_path(&b.item))));
        }
        out.into_iter().collect()
    }

    /// The policy or instructions wait for approval on this device.
    pub fn permission_items(&self) -> usize {
        usize::from(self.policy.as_ref().is_some_and(|p| p.needs_review()))
            + usize::from(self.instructions.as_ref().is_some_and(|i| !i.approved))
    }

    /// Checks and staleness reach the badge once a week (design §A.5's
    /// weekly digest): when the person hasn't looked at them for 7 days.
    pub const fn digest_due(&self) -> bool {
        self.loaded_at_ms - self.device.digest_seen >= DIGEST_MS
    }

    /// What the status-bar badge counts: waiting proposals (not snoozed),
    /// other findings, approvals, and — as a weekly digest — checks at `on`
    /// level and stale notes found since they were last looked at.
    pub fn badge(&self, warn_checks: &dyn Fn(LintCheck) -> bool) -> usize {
        let proposals = self.pending.iter().filter(|p| !self.snoozed(&proposal_fingerprint(p))).count();
        let digest = self.digest_due();
        let findings = self
            .findings
            .iter()
            .filter(|s| !self.snoozed(&s.fingerprint))
            .filter(|s| match &s.item {
                InboxItem::Lint { check, .. } => digest && !warn_checks(*check) && s.created > self.device.digest_seen,
                InboxItem::Stale { .. } => digest && s.created > self.device.digest_seen,
                _ => true,
            })
            .count();
        proposals + findings + self.permission_items()
    }

    /// Agents with waiting proposals: (id, label).
    pub fn agents(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.pending.iter().map(|p| (p.actor.id.clone(), p.actor.label.clone())).collect();
        v.sort();
        v.dedup();
        v
    }
}

fn lint_order(i: &InboxItem) -> u8 {
    match i {
        InboxItem::Lint { check, .. } => *check as u8,
        _ => 0,
    }
}

/// Loads [`InboxData`] on a worker thread, at most once per debounce.
#[derive(Default)]
pub struct InboxLoader {
    rx: Option<Receiver<InboxData>>,
    /// Load again at this time (debounced requests).
    due: Option<Instant>,
    /// A load was asked for while one was running.
    again: bool,
    want_resolved: bool,
    /// Expiry runs at most this often.
    last_expire: Option<Instant>,
}

impl InboxLoader {
    /// Ask for a reload after `delay` (the earliest request wins).
    pub fn request(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.due = Some(self.due.map_or(at, |d| d.min(at)));
    }

    pub fn set_want_resolved(&mut self, on: bool) {
        if on != self.want_resolved {
            self.want_resolved = on;
            self.request(Duration::ZERO);
        }
    }

    pub const fn deadline(&self) -> Option<Instant> {
        self.due
    }

    pub const fn loading(&self) -> bool {
        self.rx.is_some()
    }

    /// Start a due load; take a finished one. Returns new data, if any.
    pub fn poll(&mut self, store: Option<&AgentStore>, now: Instant) -> Option<InboxData> {
        let mut out = None;
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(d) => {
                    out = Some(d);
                    self.rx = None;
                }
                Err(TryRecvError::Disconnected) => self.rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.rx.is_some() {
            if self.due.is_some_and(|d| d <= now) {
                self.again = true;
                self.due = None;
            }
            return out;
        }
        let due = self.due.is_some_and(|d| d <= now) || std::mem::take(&mut self.again);
        if due && let Some(store) = store {
            self.due = None;
            let store = store.clone();
            let resolved = self.want_resolved;
            let expire = self.last_expire.is_none_or(|t| now.duration_since(t) > Duration::from_hours(6));
            if expire {
                self.last_expire = Some(now);
            }
            let (tx, rx) = std::sync::mpsc::channel();
            self.rx = Some(rx);
            std::thread::Builder::new()
                .name("quire-inbox".into())
                .spawn(move || {
                    let _ = tx.send(load(&store, resolved, expire));
                })
                .ok();
        }
        out
    }
}

/// Read everything the Inbox shows (on a worker thread).
pub fn load(store: &AgentStore, resolved: bool, expire: bool) -> InboxData {
    if expire {
        store.expire();
    }
    let pending: Vec<Proposal> = quire_agent::proposal::ProposalStore::new(store.vault().clone())
        .list()
        .into_iter()
        .filter(|p| p.status == ProposalStatus::Pending)
        .collect();
    let resolved = if resolved { store.archived_proposals().iter().take(200).map(Proposal::summary).collect() } else { Vec::new() };
    store.refresh();
    InboxData {
        pending,
        findings: store.inbox().stored(),
        resolved,
        policy: Some(store.policy_state()),
        instructions: store.instructions(),
        device: store.device_state(),
        lint_prefs: super::lint::prefs(store.vault()),
        loaded_at_ms: store.now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use quire_agent::{Actor, Change, ChangeSet, DeviceInfo, FixedClock, RunId, Via};
    use quire_vault::Vault;

    use super::*;

    #[test]
    fn groups_by_run_filters_and_snoozes() {
        let v = Vault::in_memory(&[("A.md", "a\n"), ("B.md", "b\n"), ("notes/C.md", "c\n")]);
        let store = AgentStore::open(&v, DeviceInfo { id: "d".into(), name: "D".into() }, FixedClock::new(1_793_000_000_000));
        let claude = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d");
        let other = Actor::agent("mcp:other", "Other", Via::McpStdio, "d");
        let run = RunId::new(1);
        for (who, path, run) in [(&claude, "A.md", Some(run.clone())), (&claude, "B.md", Some(run)), (&other, "notes/C.md", None)] {
            let cs = ChangeSet { run, ..ChangeSet::one(Change::Edit { path: path.into(), new_text: "x\n".into() }) };
            store.submit(who, cs).unwrap();
        }
        store.inbox().upsert(InboxItem::Lint { check: LintCheck::Orphan, path: "A.md".into(), detail: "x".into(), fix: None });
        let data = load(&store, false, true);
        let all = Filters::default();
        let groups = data.groups(&all);
        assert_eq!(groups.len(), 2, "one run, one lone proposal");
        assert_eq!(groups[0].proposals.len(), 2);
        assert_eq!(groups[0].changes(), 2);
        let only_other = Filters { agent: Some("mcp:other".into()), ..Filters::default() };
        assert_eq!(data.groups(&only_other).len(), 1);
        assert!(data.findings(&only_other).is_empty());
        let notes = Filters { folder: Some("notes".into()), ..Filters::default() };
        assert_eq!(data.groups(&notes).len(), 1);
        assert_eq!(data.findings(&all).len(), 1);
        assert_eq!(data.badge(&|_| true), 3, "orphan is a warning: not in the badge");
        assert_eq!(data.badge(&|_| false), 4);
        // Looked at the checks just now: they wait for next week's digest.
        store.update_device_state(|s| s.digest_seen = 1_793_000_000_000).unwrap();
        let seen = load(&store, false, false);
        assert!(!seen.digest_due());
        assert_eq!(seen.badge(&|_| false), 3);
        store.update_device_state(|s| s.digest_seen = 0).unwrap();
        assert_eq!(proposal_fingerprint(&data.pending[0]), InboxItem::Proposal(data.pending[0].summary()).fingerprint());
        // Snooze a proposal for a day.
        let fp = proposal_fingerprint(data.pending.iter().find(|p| p.actor.id == "mcp:other").unwrap());
        store
            .update_device_state(|s| {
                s.snoozed.insert(fp, 1_793_000_000_000 + 86_400_000);
            })
            .unwrap();
        let data = load(&store, true, false);
        assert_eq!(data.groups(&all).len(), 1);
        assert_eq!(data.groups(&Filters { show_snoozed: true, ..Filters::default() }).len(), 2);
    }
}
