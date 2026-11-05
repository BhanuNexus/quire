// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The ledger: one JSON line per agent event.
//!
//! Per device and month, at `.quire/agent/ledger/<device-id>/<yyyy-mm>.jsonl`
//! (design §A.1, docs/formats/ledger.md). Appends lock the file; readers
//! skip lines they can't parse or that have a newer `v`. With shared review
//! history on, other devices' ledgers arrive read-only under
//! `.quire/shared/agent/ledger/` ([`crate::shared`]).

use quire_vault::{ContentHash, NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::actor::{Actor, Op, Other};
use crate::ids::{ProposalId, RunId};

pub const LEDGER_DIR: &str = ".quire/agent/ledger";
pub const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub v: u32,
    /// Unix ms.
    pub at: i64,
    pub actor: Actor,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<NotePath>,
    /// New path of a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<NotePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<ContentHash>,
    /// History snapshot times (Unix ms) holding the before and after text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap_before: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Shared by every entry of one batch (undone together).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    /// For `accept` and `reject`: the actor id of the agent whose suggestion it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(flatten)]
    pub other: Other,
}

impl LedgerEntry {
    pub fn new(actor: Actor, op: Op, at: i64) -> Self {
        Self {
            v: LEDGER_VERSION,
            at,
            actor,
            op,
            path: None,
            to: None,
            before: None,
            after: None,
            snap_before: None,
            snap_after: None,
            proposal: None,
            run: None,
            bytes: None,
            batch: None,
            from: None,
            other: Other::new(),
        }
    }

    #[must_use]
    pub fn path(mut self, p: &NotePath) -> Self {
        self.path = Some(p.clone());
        self
    }
}

/// Which entries to read.
#[derive(Debug, Clone, Default)]
pub struct LedgerQuery {
    /// Unix ms; entries at or after it.
    pub since: Option<i64>,
    /// Actor id (`mcp:claude-code`).
    pub actor: Option<String>,
    pub run: Option<RunId>,
}

/// The ledger of one device.
#[derive(Clone)]
pub struct Ledger {
    vault: Vault,
    device: String,
}

fn month_of(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms).map_or_else(|| "1970-01".into(), |d| d.format("%Y-%m").to_string())
}

impl Ledger {
    pub fn new(vault: Vault, device: &str) -> Self {
        Self { vault, device: device.to_string() }
    }

    fn folder(&self) -> String {
        format!("{LEDGER_DIR}/{}", self.device)
    }

    /// Append one entry (one locked write of one line).
    pub fn append(&self, entry: &LedgerEntry) -> Result<(), String> {
        let line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
        let file = NotePath::new(format!("{}/{}.jsonl", self.folder(), month_of(entry.at)));
        self.vault.storage().append_line(&file, &line).map_err(|e| e.to_string())
    }

    /// Drop `read` events older than `before` (Unix ms) from the months
    /// before the current one (nothing appends to those any more). Other
    /// lines, including ones this version can't read, are kept as they are.
    /// Returns how many lines were dropped.
    pub fn prune_reads(&self, before: i64, now_ms: i64) -> usize {
        let current = month_of(now_ms);
        let mut dropped = 0;
        for f in self.vault.storage().list_under(&self.folder()).unwrap_or_default() {
            if f.file_name().strip_suffix(".jsonl").is_none_or(|m| m >= current.as_str()) {
                continue;
            }
            let Ok(text) = self.vault.storage().read(&f) else { continue };
            let old_read = |l: &str| {
                serde_json::from_str::<serde_json::Value>(l).ok().is_some_and(|v| {
                    v.get("op").and_then(|o| o.as_str()) == Some("read")
                        && v.get("at").and_then(serde_json::Value::as_i64).is_some_and(|t| t < before)
                })
            };
            let kept: Vec<&str> = text.lines().filter(|l| !old_read(l)).collect();
            let n = text.lines().count() - kept.len();
            if n == 0 {
                continue;
            }
            let mut out = kept.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            if self.vault.storage().write_atomic(&f, &out).is_ok() {
                dropped += n;
            }
        }
        dropped
    }

    /// The ledger files and when they last changed (to notice new entries
    /// from other processes).
    pub fn stamp(&self) -> Vec<(NotePath, Option<std::time::SystemTime>)> {
        let mut files = self.vault.storage().list_under(&self.folder()).unwrap_or_default();
        files.sort();
        files.into_iter().map(|f| (f.clone(), self.vault.storage().modified(&f).ok())).collect()
    }

    /// Matching entries, oldest first. Unreadable lines and lines from a
    /// newer version are skipped (and stay in the file).
    pub fn read(&self, q: &LedgerQuery) -> Vec<LedgerEntry> {
        Self::read_folder(&self.vault, &self.folder(), q)
    }

    /// Matching entries of the month files in `folder` (one device's
    /// ledger, here or a shared copy), oldest first.
    pub(crate) fn read_folder(vault: &Vault, folder: &str, q: &LedgerQuery) -> Vec<LedgerEntry> {
        let since_month = q.since.map(month_of);
        let mut files = vault.storage().list_under(folder).unwrap_or_default();
        files.retain(|f| f.file_name().strip_suffix(".jsonl").is_some_and(|m| since_month.as_deref().is_none_or(|s| m >= s)));
        files.sort();
        let mut out: Vec<LedgerEntry> = files
            .iter()
            .filter_map(|f| vault.storage().read(f).ok())
            .flat_map(|text| text.lines().filter_map(|l| serde_json::from_str::<LedgerEntry>(l).ok()).collect::<Vec<_>>())
            .filter(|e| e.v <= LEDGER_VERSION)
            .filter(|e| q.since.is_none_or(|s| e.at >= s))
            .filter(|e| q.actor.as_deref().is_none_or(|a| e.actor.id == a))
            .filter(|e| q.run.as_ref().is_none_or(|r| e.run.as_ref() == Some(r)))
            .collect();
        out.sort_by_key(|e| e.at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_back_across_months() {
        let v = Vault::in_memory(&[]);
        let l = Ledger::new(v.clone(), "d_1");
        let agent = Actor::agent("mcp:claude-code", "Claude Code", crate::Via::McpStdio, "d_1");
        let oct = 1_793_000_000_000; // 2026-10-26
        let nov = oct + 10 * 86_400_000;
        let mut e = LedgerEntry::new(agent, Op::Write, oct).path(&"A.md".into());
        e.after = Some(ContentHash::of("x"));
        l.append(&e).unwrap();
        l.append(&LedgerEntry::new(Actor::human("d_1"), Op::Accept, nov)).unwrap();
        // A newer version's line and a torn line are skipped, not fatal.
        let file = NotePath::new(format!("{LEDGER_DIR}/d_1/{}.jsonl", month_of(nov)));
        v.storage().append_line(&file, r#"{"v":2,"at":1,"op":"teleport"}"#).unwrap();
        v.storage().append_line(&file, r#"{"v":1,"at":"#).unwrap();
        let all = l.read(&LedgerQuery::default());
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], e);
        assert_eq!(l.read(&LedgerQuery { since: Some(nov), ..Default::default() }).len(), 1);
        assert_eq!(l.read(&LedgerQuery { actor: Some("mcp:claude-code".into()), ..Default::default() }).len(), 1);
        assert_eq!(v.storage().list_under(&format!("{LEDGER_DIR}/d_1")).unwrap().len(), 2, "one file per month");
        let line = v.storage().read(&NotePath::new(format!("{LEDGER_DIR}/d_1/{}.jsonl", month_of(oct)))).unwrap();
        assert!(line.starts_with(r#"{"v":1,"at":1793000000000,"actor":{"kind":"agent""#), "{line}");
        // Reads older than 30 days go; everything else stays.
        let agent = Actor::agent("mcp:x", "X", crate::Via::McpStdio, "d_1");
        l.append(&LedgerEntry::new(agent, Op::Read, oct + 1).path(&"A.md".into())).unwrap();
        assert_eq!(l.prune_reads(oct + 2, nov + 40 * 86_400_000), 1);
        assert_eq!(l.read(&LedgerQuery::default()).len(), 2);
        assert_eq!(l.prune_reads(oct + 2, oct + 3), 0, "the current month is left alone");
    }
}
