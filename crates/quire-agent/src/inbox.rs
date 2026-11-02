// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The inbox: everything waiting for the person (design §A.5,
//! docs/formats/inbox.md).
//!
//! Proposals come from the proposal store. Every other item is a file
//! `.quire/agent/inbox/<fingerprint>.json`; the fingerprint is a stable hash
//! of the item's kind and subject, so producers can [`InboxStore::upsert`]
//! the same finding again without duplicates. Dismissals are appended to
//! `dismissed.jsonl` and hide that fingerprint until the item's content changes.

use std::sync::Arc;

use quire_intel::anchor::{Anchor, AnchorStatus};
use quire_vault::{ContentHash, NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::actor::{Clock, Other};
use crate::change::ChangeSet;
use crate::proposal::{ProposalFilter, ProposalStore, ProposalSummary};

pub const INBOX_DIR: &str = ".quire/agent/inbox";
pub const INBOX_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintCheck {
    BrokenLink,
    BrokenBlock,
    Orphan,
    MissingConcept,
    MissingProperty,
    DuplicateTitle,
    EmptyNote,
    StaleIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Contradiction,
    Tension,
}

/// Why a note looks stale (design §C.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StaleReason {
    Age { days: u32 },
    LinkedChanged { path: NotePath },
    TimeBound { phrase: String, count: u32 },
    ValidUntil { date: String },
    Superseded { by: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxItem {
    Proposal(ProposalSummary),
    Lint { check: LintCheck, path: NotePath, detail: String, fix: Option<ChangeSet> },
    Contradiction { a: Anchor, b: Anchor, verdict: Verdict, confidence: f32, rationale: String },
    Stale { path: NotePath, score: f32, reasons: Vec<StaleReason> },
    CitationDrift { in_note: NotePath, anchor: Anchor, status: AnchorStatus },
    VerifyFailed { in_note: NotePath, anchor: Anchor, claim: String },
    PolicyChanged { diff: String },
    InstructionsChanged { path: NotePath, diff: String },
    JobFailed { job: String, message: String },
}

fn anchor_key(a: &Anchor) -> String {
    format!("{}#{}", a.path, ContentHash::of(&a.quote).hex())
}

impl InboxItem {
    /// What the item is about; equal keys are the same finding.
    pub fn subject_key(&self) -> String {
        match self {
            Self::Proposal(p) => format!("proposal:{}", p.id),
            Self::Lint { check, path, .. } => format!("lint:{check:?}:{path}"),
            Self::Contradiction { a, b, .. } => {
                let mut pair = [anchor_key(a), anchor_key(b)];
                pair.sort();
                format!("contradiction:{}|{}", pair[0], pair[1])
            }
            Self::Stale { path, .. } => format!("stale:{path}"),
            Self::CitationDrift { in_note, anchor, .. } => format!("drift:{in_note}:{}", anchor_key(anchor)),
            Self::VerifyFailed { in_note, anchor, .. } => format!("verify:{in_note}:{}", anchor_key(anchor)),
            Self::PolicyChanged { .. } => "policy".into(),
            Self::InstructionsChanged { path, .. } => format!("instructions:{path}"),
            Self::JobFailed { job, .. } => format!("job:{job}"),
        }
    }

    /// 16 hex characters; the item's file name.
    pub fn fingerprint(&self) -> String {
        ContentHash::of(&self.subject_key()).hex()[..16].to_string()
    }

    /// Hash of the whole item: a dismissal lasts while this stays the same.
    pub fn content_hash(&self) -> ContentHash {
        ContentHash::of(&serde_json::to_string(self).unwrap_or_default())
    }
}

/// A stored item with its bookkeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredItem {
    pub format: u32,
    pub fingerprint: String,
    pub created: i64,
    pub updated: i64,
    pub subject: ContentHash,
    pub item: InboxItem,
    #[serde(flatten)]
    pub other: Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dismissal {
    v: u32,
    fingerprint: String,
    subject: ContentHash,
    at: i64,
}

#[derive(Clone)]
pub struct InboxStore {
    vault: Vault,
    clock: Arc<dyn Clock>,
}

impl InboxStore {
    pub fn new(vault: Vault, clock: Arc<dyn Clock>) -> Self {
        Self { vault, clock }
    }

    fn file(fingerprint: &str) -> NotePath {
        NotePath::new(format!("{INBOX_DIR}/{fingerprint}.json"))
    }

    fn dismissed_file() -> NotePath {
        NotePath::new(format!("{INBOX_DIR}/dismissed.jsonl"))
    }

    fn dismissals(&self) -> Vec<Dismissal> {
        self.vault
            .storage()
            .read(&Self::dismissed_file())
            .map(|t| t.lines().filter_map(|l| serde_json::from_str::<Dismissal>(l).ok()).filter(|d| d.v <= 1).collect())
            .unwrap_or_default()
    }

    fn load(&self, fingerprint: &str) -> Option<StoredItem> {
        let bytes = self.vault.storage().read_bytes(&Self::file(fingerprint)).ok()?;
        let s: StoredItem = serde_json::from_slice(&bytes).ok()?;
        (s.format <= INBOX_FORMAT).then_some(s)
    }

    /// Add or refresh a finding. Returns its fingerprint, or `None` when the
    /// person dismissed this very finding (same content) before. Proposals
    /// aren't stored here; they are listed from the proposal store.
    pub fn upsert(&self, item: InboxItem) -> Option<String> {
        let fingerprint = item.fingerprint();
        if matches!(item, InboxItem::Proposal(_)) {
            return Some(fingerprint);
        }
        let subject = item.content_hash();
        if self.dismissals().iter().any(|d| d.fingerprint == fingerprint && d.subject == subject) {
            return None;
        }
        let now = self.clock.now_ms();
        let existing = self.load(&fingerprint);
        if existing.as_ref().is_some_and(|e| e.subject == subject) {
            return Some(fingerprint);
        }
        let stored = StoredItem {
            format: INBOX_FORMAT,
            created: existing.as_ref().map_or(now, |e| e.created),
            other: existing.map(|e| e.other).unwrap_or_default(),
            fingerprint: fingerprint.clone(),
            updated: now,
            subject,
            item,
        };
        let bytes = serde_json::to_vec_pretty(&stored).ok()?;
        self.vault.storage().write_bytes_atomic(&Self::file(&fingerprint), &bytes).ok()?;
        Some(fingerprint)
    }

    /// Stored findings (not proposals), newest first.
    pub fn stored(&self) -> Vec<StoredItem> {
        let mut v: Vec<StoredItem> = self
            .vault
            .storage()
            .list_under(INBOX_DIR)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.file_name().strip_suffix(".json").map(str::to_string))
            .filter_map(|fp| self.load(&fp))
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated));
        v
    }

    /// Everything to show: pending proposals first, then findings.
    pub fn items(&self) -> Vec<InboxItem> {
        let proposals = ProposalStore::new(self.vault.clone());
        let pending = ProposalFilter::pending();
        proposals
            .list()
            .iter()
            .map(crate::proposal::Proposal::summary)
            .filter(|s| pending.matches(s))
            .map(InboxItem::Proposal)
            .chain(self.stored().into_iter().map(|s| s.item))
            .collect()
    }

    /// The person dismissed a finding: remove it and remember not to show it
    /// again until it changes.
    pub fn dismiss(&self, fingerprint: &str) -> Result<(), String> {
        let Some(s) = self.load(fingerprint) else { return Ok(()) };
        let d = Dismissal { v: 1, fingerprint: fingerprint.to_string(), subject: s.subject, at: self.clock.now_ms() };
        let line = serde_json::to_string(&d).map_err(|e| e.to_string())?;
        self.vault.storage().append_line(&Self::dismissed_file(), &line).map_err(|e| e.to_string())?;
        self.resolve(fingerprint)
    }

    /// The finding no longer applies (fixed): remove it without remembering.
    pub fn resolve(&self, fingerprint: &str) -> Result<(), String> {
        match self.vault.storage().remove(&Self::file(fingerprint)) {
            Ok(()) | Err(quire_vault::VaultError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedClock;

    fn lint(detail: &str) -> InboxItem {
        InboxItem::Lint { check: LintCheck::BrokenLink, path: "A.md".into(), detail: detail.into(), fix: None }
    }

    #[test]
    fn upserts_are_idempotent_and_dismissals_stick_until_it_changes() {
        let v = Vault::in_memory(&[]);
        let clock = FixedClock::new(1_000);
        let inbox = InboxStore::new(v, Arc::<FixedClock>::clone(&clock));
        let fp = inbox.upsert(lint("[[Missing]] doesn't resolve")).unwrap();
        clock.advance(10);
        assert_eq!(inbox.upsert(lint("[[Missing]] doesn't resolve")), Some(fp.clone()));
        assert_eq!(inbox.stored().len(), 1);
        assert_eq!(inbox.stored()[0].updated, 1_000, "unchanged item isn't rewritten");
        inbox.dismiss(&fp).unwrap();
        assert!(inbox.items().is_empty());
        assert_eq!(inbox.upsert(lint("[[Missing]] doesn't resolve")), None, "still dismissed");
        assert_eq!(inbox.upsert(lint("[[Missing]] and [[Gone]] don't resolve")), Some(fp), "changed: shown again");
        assert_eq!(inbox.items().len(), 1);
    }

    #[test]
    fn items_serialize_with_a_kind() {
        let item = InboxItem::JobFailed { job: "lint".into(), message: "boom".into() };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v, serde_json::json!({"kind": "job_failed", "job": "lint", "message": "boom"}));
        let stale = InboxItem::Stale { path: "A.md".into(), score: 0.7, reasons: vec![StaleReason::Age { days: 400 }] };
        let back: InboxItem = serde_json::from_value(serde_json::to_value(&stale).unwrap()).unwrap();
        assert_eq!(back, stale);
        assert_eq!(stale.fingerprint().len(), 16);
    }
}
