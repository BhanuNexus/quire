// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposals: an agent's suggested changes, one JSON file each, at
//! `.quire/agent/proposals/<id>.json` (design §A.4, docs/formats/proposals.md).
//!
//! This module owns the file format and its I/O. Rebasing, hunks and
//! decisions ([`Review`], [`Decided`]) are WS-A's; their types are here so
//! the UI and tools can be written against them.

use std::io::{Read, Write};
use std::ops::Range;

use base64::Engine as _;
use quire_vault::{ContentHash, NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::actor::{Actor, Other, SourceRef};
use crate::change::WriteError;
use crate::ids::{ProposalId, RunId};

pub const PROPOSALS_DIR: &str = ".quire/agent/proposals";
pub const PROPOSAL_FORMAT: u32 = 1;
/// Above this, `base_text` is stored gzipped and base64-encoded.
pub const INLINE_BASE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Applied,
    PartiallyApplied,
    Rejected,
    Expired,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Edit,
    Create,
    Rename,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkAction {
    Accepted,
    Rejected,
    /// Accepted with the person's own text.
    Edited,
}

/// A decision on one hunk. Hunk ids depend only on content, so decisions
/// stay attached when the proposal is rebased.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkDecision {
    /// `h_` + 12 hex.
    pub hunk: String,
    pub action: HunkAction,
    pub at: i64,
    /// The person's text for `edited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(flatten)]
    pub other: Other,
}

/// One file's part of a proposal.
///
/// Which fields are set depends on `op`: edit: `path`, `base`, `base_text` (or `base_text_gz`), `new_text`,
/// `decisions`; create: `path`, `text`; rename: `from`, `to`,
/// `update_links`; delete: `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalChange {
    pub op: ChangeOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<NotePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<NotePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<NotePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_links: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_text_gz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<HunkDecision>,
    #[serde(flatten)]
    pub other: Other,
}

impl ProposalChange {
    fn blank(op: ChangeOp) -> Self {
        Self {
            op,
            path: None,
            from: None,
            to: None,
            update_links: None,
            base: None,
            base_text: None,
            base_text_gz: None,
            new_text: None,
            text: None,
            decisions: Vec::new(),
            other: Other::new(),
        }
    }

    /// An edit of `path` from `base_text` to `new_text`.
    pub fn edit(path: &NotePath, base_text: &str, new_text: &str) -> Self {
        let mut c = Self::blank(ChangeOp::Edit);
        c.path = Some(path.clone());
        c.set_base_text(base_text);
        c.new_text = Some(new_text.to_string());
        c
    }

    pub fn create(path: &NotePath, text: &str) -> Self {
        let mut c = Self::blank(ChangeOp::Create);
        c.path = Some(path.clone());
        c.text = Some(text.to_string());
        c
    }

    pub fn rename(from: &NotePath, to: &NotePath, update_links: bool) -> Self {
        let mut c = Self::blank(ChangeOp::Rename);
        c.from = Some(from.clone());
        c.to = Some(to.clone());
        c.update_links = Some(update_links);
        c
    }

    pub fn delete(path: &NotePath) -> Self {
        let mut c = Self::blank(ChangeOp::Delete);
        c.path = Some(path.clone());
        c
    }

    /// Set the base text and its hash, compressing it when it's large.
    pub fn set_base_text(&mut self, text: &str) {
        self.base = Some(ContentHash::of(text));
        if text.len() > INLINE_BASE_LIMIT {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = e.write_all(text.as_bytes());
            self.base_text_gz = Some(base64::engine::general_purpose::STANDARD.encode(e.finish().unwrap_or_default()));
            self.base_text = None;
        } else {
            self.base_text = Some(text.to_string());
            self.base_text_gz = None;
        }
    }

    /// The base text, from whichever field holds it.
    pub fn base_text(&self) -> Option<String> {
        if let Some(t) = &self.base_text {
            return Some(t.clone());
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(self.base_text_gz.as_ref()?).ok()?;
        let mut out = String::new();
        flate2::read::GzDecoder::new(bytes.as_slice()).read_to_string(&mut out).ok()?;
        Some(out)
    }

    /// Every path this change touches.
    pub fn paths(&self) -> Vec<NotePath> {
        [&self.path, &self.from, &self.to].into_iter().flatten().cloned().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub format: u32,
    pub id: ProposalId,
    pub created: i64,
    pub updated: i64,
    pub status: ProposalStatus,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub title: String,
    /// The agent's words: untrusted, shown as plain text.
    #[serde(default)]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
    pub changes: Vec<ProposalChange>,
    #[serde(flatten)]
    pub other: Other,
}

impl Proposal {
    pub fn summary(&self) -> ProposalSummary {
        ProposalSummary {
            id: self.id.clone(),
            status: self.status,
            title: self.title.clone(),
            actor: self.actor.clone(),
            run: self.run.clone(),
            files: self.changes.iter().flat_map(ProposalChange::paths).collect(),
            created: self.created,
            updated: self.updated,
            decided_hunks: self.changes.iter().map(|c| c.decisions.len()).sum(),
        }
    }
}

/// A proposal as lists show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalSummary {
    pub id: ProposalId,
    pub status: ProposalStatus,
    pub title: String,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    pub files: Vec<NotePath>,
    pub created: i64,
    pub updated: i64,
    pub decided_hunks: usize,
}

/// Which proposals to list; empty fields match everything.
#[derive(Debug, Clone, Default)]
pub struct ProposalFilter {
    pub status: Option<ProposalStatus>,
    /// Actor id.
    pub actor: Option<String>,
    pub path: Option<NotePath>,
}

impl ProposalFilter {
    pub fn pending() -> Self {
        Self { status: Some(ProposalStatus::Pending), ..Self::default() }
    }

    pub fn matches(&self, p: &ProposalSummary) -> bool {
        self.status.is_none_or(|s| s == p.status)
            && self.actor.as_deref().is_none_or(|a| p.actor.id == a)
            && self.path.as_ref().is_none_or(|path| p.files.contains(path))
    }
}

/// One reviewable change within a file (WS-A computes these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// `h_` + 12 hex of sha256(path ∥ old lines ∥ new lines).
    pub id: String,
    /// 0-based line range in the current text that the hunk replaces.
    pub range_in_current: Range<u32>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The person changed these lines after the agent read them.
    pub conflict: bool,
}

/// The hunks of one file of a proposal, rebased on the current text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Review {
    pub hunks: Vec<Hunk>,
    /// `false` until hunk review is implemented: show the whole change instead.
    pub available: bool,
}

/// The result of deciding hunks: the text to put in place of `current`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub text: String,
    pub status: ProposalStatus,
    /// Hunks still waiting for a decision.
    pub pending: usize,
}

/// Proposal files on disk. Each file is written atomically; a file made by
/// a newer Quire is listed by nobody and never rewritten.
#[derive(Clone)]
pub struct ProposalStore {
    vault: Vault,
}

impl ProposalStore {
    pub fn new(vault: Vault) -> Self {
        Self { vault }
    }

    fn file(id: &ProposalId) -> NotePath {
        NotePath::new(format!("{PROPOSALS_DIR}/{id}.json"))
    }

    /// The format of the file on disk, if it exists and is JSON.
    fn format_on_disk(&self, id: &ProposalId) -> Option<u64> {
        let bytes = self.vault.storage().read_bytes(&Self::file(id)).ok()?;
        serde_json::from_slice::<serde_json::Value>(&bytes).ok()?.get("format")?.as_u64()
    }

    pub fn save(&self, p: &Proposal) -> Result<(), WriteError> {
        if self.format_on_disk(&p.id).is_some_and(|f| f > u64::from(PROPOSAL_FORMAT)) {
            return Err(WriteError::Io(format!("proposal {} was made by a newer Quire", p.id)));
        }
        let bytes = serde_json::to_vec_pretty(p).map_err(|e| WriteError::Io(e.to_string()))?;
        self.vault.storage().write_bytes_atomic(&Self::file(&p.id), &bytes)?;
        Ok(())
    }

    pub fn load(&self, id: &ProposalId) -> Option<Proposal> {
        let bytes = self.vault.storage().read_bytes(&Self::file(id)).ok()?;
        let p: Proposal = serde_json::from_slice(&bytes).ok()?;
        (p.format <= PROPOSAL_FORMAT).then_some(p)
    }

    /// Every readable proposal (not archived), oldest first.
    pub fn list(&self) -> Vec<Proposal> {
        let mut v: Vec<Proposal> = self
            .vault
            .storage()
            .list_under(PROPOSALS_DIR)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| ProposalId::parse(f.file_name().strip_suffix(".json")?))
            .filter_map(|id| self.load(&id))
            .collect();
        v.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// Move a resolved proposal to `archive/<yyyy-mm>/` (by its `updated` time).
    pub fn archive(&self, id: &ProposalId) -> Result<(), WriteError> {
        let Some(p) = self.load(id) else { return Err(WriteError::NotFound { path: Self::file(id) }) };
        let month = chrono::DateTime::from_timestamp_millis(p.updated).map_or_else(|| "1970-01".into(), |d| d.format("%Y-%m").to_string());
        let dest = NotePath::new(format!("{PROPOSALS_DIR}/archive/{month}/{id}.json"));
        let bytes = self.vault.storage().read_bytes(&Self::file(id))?;
        self.vault.storage().write_bytes_atomic(&dest, &bytes)?;
        self.vault.storage().remove(&Self::file(id))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Via;

    fn sample(id: &ProposalId) -> Proposal {
        Proposal {
            format: 1,
            id: id.clone(),
            created: 1_793_000_000_000,
            updated: 1_793_000_000_000,
            status: ProposalStatus::Pending,
            actor: Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_1"),
            run: None,
            title: "Add undo details".into(),
            rationale: "because".into(),
            sources: vec![SourceRef::uri("quire://note/raw/paper.md")],
            changes: vec![
                ProposalChange::edit(&"Rope Notes.md".into(), "# Rope\n", "# Rope\nUndo is cheap.\n"),
                ProposalChange::create(&"wiki/Ropes.md".into(), "# Ropes\n"),
                ProposalChange::rename(&"A.md".into(), &"B.md".into(), true),
                ProposalChange::delete(&"Old.md".into()),
            ],
            other: Other::new(),
        }
    }

    #[test]
    fn file_format_round_trips_and_keeps_unknown_fields() {
        let v = Vault::in_memory(&[]);
        let store = ProposalStore::new(v.clone());
        let id = ProposalId::new(1_793_000_000_000);
        store.save(&sample(&id)).unwrap();
        let path = NotePath::new(format!("{PROPOSALS_DIR}/{id}.json"));
        let mut json: serde_json::Value = serde_json::from_slice(&v.storage().read_bytes(&path).unwrap()).unwrap();
        assert_eq!(json["changes"][0]["op"], "edit");
        assert_eq!(json["changes"][0]["base"], ContentHash::of("# Rope\n").as_str());
        assert_eq!(json["changes"][2], serde_json::json!({"op": "rename", "from": "A.md", "to": "B.md", "update_links": true}));
        assert_eq!(json["actor"]["via"], "mcp-stdio");
        // A newer Quire added fields: they survive a rewrite.
        json["reviewer_note"] = "keep me".into();
        json["changes"][1]["template"] = "concept".into();
        v.storage().write_bytes_atomic(&path, &serde_json::to_vec(&json).unwrap()).unwrap();
        let mut p = store.load(&id).unwrap();
        p.status = ProposalStatus::Rejected;
        store.save(&p).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&v.storage().read_bytes(&path).unwrap()).unwrap();
        assert_eq!(back["reviewer_note"], "keep me");
        assert_eq!(back["changes"][1]["template"], "concept");
        assert_eq!(back["status"], "rejected");
        // A newer format is neither listed nor overwritten.
        json["format"] = 2.into();
        v.storage().write_bytes_atomic(&path, &serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(store.list().is_empty());
        assert!(store.save(&p).is_err());
    }

    #[test]
    fn large_bases_are_compressed() {
        let big = "line of text\n".repeat(10_000);
        let c = ProposalChange::edit(&"Big.md".into(), &big, "x");
        assert!(c.base_text.is_none() && c.base_text_gz.is_some());
        assert_eq!(c.base_text().as_deref(), Some(big.as_str()));
        assert_eq!(c.base, Some(ContentHash::of(&big)));
    }

    #[test]
    fn lists_filters_and_archives() {
        let v = Vault::in_memory(&[]);
        let store = ProposalStore::new(v);
        let (a, b) = (ProposalId::new(1), ProposalId::new(2));
        store.save(&sample(&a)).unwrap();
        let mut pb = sample(&b);
        pb.created = 2;
        pb.status = ProposalStatus::Applied;
        store.save(&pb).unwrap();
        let all = store.list();
        assert_eq!(all.len(), 2);
        let pending: Vec<_> = all.iter().map(Proposal::summary).filter(|s| ProposalFilter::pending().matches(s)).collect();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].files.len(), 5);
        let by_path = ProposalFilter { path: Some("Old.md".into()), ..Default::default() };
        assert!(by_path.matches(&pending[0]));
        store.archive(&b).unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.load(&b).is_none());
    }
}
