// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Who did something, and the record of it kept with each version
//! (design §2.5, docs/formats/provenance.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use quire_intel::anchor::Anchor;
use quire_vault::ContentHash;
use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::ids::{ProposalId, RunId};

/// Unknown fields kept on rewrite (design §2.4).
pub type Other = Map<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// The person using Quire.
    Human,
    /// An AI agent: MCP clients, the agent panel, an ACP agent.
    Agent,
    /// Quire's own background work (lint, index.md, revert).
    Job,
    /// Content brought in from outside (URL capture, importers).
    Import,
}

/// What a client says about itself (MCP `clientInfo`). Never trusted for permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub self_reported: bool,
}

/// How the actor reached Quire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    McpStdio,
    McpHttp,
    Panel,
    Acp,
    Cli,
    App,
    /// Approved in the agent's own app (MCP elicitation, design §4.5).
    ClientApproval,
    /// Arrived from another device through Quire Sync.
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    /// `human`, `mcp:claude-code`, `oauth:claude.ai`, `panel:anthropic`,
    /// `acp:codex`, `job:lint`, `import:url`. Assigned by Quire, not the client.
    pub id: String,
    /// Shown in the UI: "Claude Code".
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
    pub via: Via,
    /// Device id from `<config>/device.toml`.
    #[serde(default)]
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// W3C `traceparent` from MCP `_meta`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    #[serde(flatten)]
    pub other: Other,
}

impl Actor {
    fn new(kind: ActorKind, id: impl Into<String>, label: impl Into<String>, via: Via, device: &str) -> Self {
        Self {
            kind,
            id: id.into(),
            label: label.into(),
            client: None,
            via,
            device: device.to_string(),
            model: None,
            trace: None,
            other: Other::new(),
        }
    }

    /// The person at this device.
    pub fn human(device: &str) -> Self {
        Self::new(ActorKind::Human, "human", "You", Via::App, device)
    }

    /// An agent, e.g. `Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, device)`.
    pub fn agent(id: impl Into<String>, label: impl Into<String>, via: Via, device: &str) -> Self {
        Self::new(ActorKind::Agent, id, label, via, device)
    }

    /// Quire's own job `name` (`lint`, `index`, `revert`): id `job:<name>`.
    pub fn job(name: &str, device: &str) -> Self {
        Self::new(ActorKind::Job, format!("job:{name}"), name, Via::App, device)
    }

    /// Imported content from `source` (`url`, `logseq`): id `import:<source>`.
    pub fn import(source: &str, device: &str) -> Self {
        Self::new(ActorKind::Import, format!("import:{source}"), source, Via::App, device)
    }

    pub fn is_human(&self) -> bool {
        self.kind == ActorKind::Human
    }
}

/// This device, as stored in `<config>/device.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// `d_` + random characters; never reused.
    pub id: String,
    /// Shown in history: "Asha's laptop".
    pub name: String,
}

impl DeviceInfo {
    /// A new device with a fresh id.
    pub fn generate(name: impl Into<String>) -> Self {
        Self { id: crate::ids::random_id("d_", 12), name: name.into() }
    }
}

/// Time source; tests inject [`FixedClock`].
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn new(ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(ms)))
    }

    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::Release);
    }

    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::AcqRel);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    #[default]
    Trusted,
    /// Derived from captured, imported or `raw/` content (design §A.3).
    Untrusted,
}

/// What happened, in provenance and the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Write,
    Create,
    Rename,
    Delete,
    Propose,
    Accept,
    Reject,
    Revert,
    Read,
    Batch,
}

/// A note or URL a change was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// `quire://note/raw/paper.md` or `https://…`.
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(flatten)]
    pub other: Other,
}

impl SourceRef {
    pub fn uri(uri: impl Into<String>) -> Self {
        Self { uri: uri.into(), anchor: None, other: Other::new() }
    }
}

/// Who made a version: the `<ms>.prov.json` next to a history snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub format: u32,
    /// Hash of the version this describes.
    pub hash: ContentHash,
    pub actor: Actor,
    pub op: Op,
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalId>,
    /// Set when a person accepted an agent's suggestion: the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_from: Option<Actor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
    #[serde(default)]
    pub trust: Trust,
    #[serde(flatten)]
    pub other: Other,
}

pub const PROVENANCE_FORMAT: u32 = 1;

impl Provenance {
    /// A provenance record for `text`, written by `actor` now.
    pub fn new(actor: Actor, op: Op, text: &str, at_ms: i64) -> Self {
        Self {
            format: PROVENANCE_FORMAT,
            hash: ContentHash::of(text),
            trust: if actor.kind == ActorKind::Import { Trust::Untrusted } else { Trust::Trusted },
            actor,
            op,
            at_ms,
            run: None,
            proposal: None,
            accepted_from: None,
            sources: Vec::new(),
            other: Other::new(),
        }
    }

    /// The JSON written by `Vault::snapshot_with`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Read a `.prov.json`; `None` if it isn't one this version understands.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let p: Self = serde_json::from_value(v.clone()).ok()?;
        (p.format <= PROVENANCE_FORMAT).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_round_trips_and_keeps_unknown_fields() {
        let json = serde_json::json!({
            "format": 1, "hash": ContentHash::of("x"), "op": "write", "at_ms": 5,
            "actor": {"kind": "agent", "id": "mcp:claude-code", "label": "Claude Code", "via": "mcp-stdio", "device": "d_1", "future": 1},
            "accepted_from": null, "trust": "trusted", "newer_field": [1, 2]
        });
        let p = Provenance::from_json(&json).unwrap();
        assert_eq!(p.actor.via, Via::McpStdio);
        let back = p.to_json();
        assert_eq!(back["newer_field"], serde_json::json!([1, 2]));
        assert_eq!(back["actor"]["future"], 1);
        let mut newer = json;
        newer["format"] = 2.into();
        assert!(Provenance::from_json(&newer).is_none());
        assert_eq!(Provenance::new(Actor::import("url", "d"), Op::Create, "t", 1).trust, Trust::Untrusted);
    }

    #[test]
    fn device_ids_are_random() {
        let (a, b) = (DeviceInfo::generate("A"), DeviceInfo::generate("B"));
        assert!(a.id.starts_with("d_") && a.id.len() == 14);
        assert_ne!(a.id, b.id);
    }
}
