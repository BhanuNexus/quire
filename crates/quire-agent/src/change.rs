// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a non-human writer asks for ([`ChangeSet`]) and what it gets back
//! ([`SubmitOutcome`] or a [`WriteError`] with a stable code, design §4.4).

use quire_markdown::Value;
use quire_vault::{ContentHash, NotePath};
use serde::{Deserialize, Serialize};

use crate::actor::SourceRef;
use crate::ids::{ProposalId, RunId};
use crate::policy::Mode;

/// One change to the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    Create {
        path: NotePath,
        text: String,
    },
    Edit {
        path: NotePath,
        new_text: String,
    },
    Rename {
        from: NotePath,
        to: NotePath,
        update_links: bool,
    },
    /// To `.trash/`.
    Delete {
        path: NotePath,
    },
    SetProperties {
        path: NotePath,
        set: Vec<(String, Value)>,
        remove: Vec<String>,
    },
}

impl Change {
    /// The path the change is about (the old path for a rename).
    pub const fn path(&self) -> &NotePath {
        match self {
            Self::Create { path, .. } | Self::Edit { path, .. } | Self::Delete { path } | Self::SetProperties { path, .. } => path,
            Self::Rename { from, .. } => from,
        }
    }
}

/// Changes submitted together: applied together, or proposed together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// One line for the inbox: "Add undo details to Rope Notes".
    #[serde(default)]
    pub title: String,
    /// The agent's reason; shown as plain text, never interpreted.
    #[serde(default)]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
    /// Versions the writer last read; a mismatch is a `VersionConflict`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected: Vec<(NotePath, ContentHash)>,
}

impl ChangeSet {
    pub fn one(change: Change) -> Self {
        Self { changes: vec![change], ..Self::default() }
    }

    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SubmitOutcome {
    /// Written to disk; the new version of each written note.
    Applied { versions: Vec<(NotePath, ContentHash)> },
    /// Stored as a proposal for the person to review; the files are untouched.
    /// `amended` when it extended the writer's own pending proposal.
    Proposed { id: ProposalId, amended: bool },
}

/// A broken write contract rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub field: String,
    pub rule: String,
    pub message: String,
}

/// Why a write was refused. [`WriteError::code`] is the stable code agents see.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("{path} is in zone {zone}, where you may only {mode:?}")]
    PermissionDenied { path: NotePath, zone: String, mode: Mode },
    #[error("{path} is a source in raw/ and can't be changed; add a new note instead")]
    RawIsImmutable { path: NotePath },
    #[error("{path} doesn't exist")]
    NotFound { path: NotePath },
    #[error("{path} already exists")]
    AlreadyExists { path: NotePath },
    #[error("{path} changed since you read it (you had {expected}, now {current})")]
    VersionConflict { path: NotePath, expected: ContentHash, current: ContentHash, diff_since: String, merged_preview: Option<String> },
    #[error("the text to replace occurs {matches} times in {path}; include more context")]
    NotUnique { path: NotePath, matches: usize },
    #[error("the text to replace isn't in {path}")]
    NoMatch { path: NotePath },
    #[error("the change breaks the “{contract}” contract")]
    ContractViolation { contract: String, violations: Vec<Violation> },
    #[error("too many writes; retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("the change is larger than the limit ({limit})")]
    TooLarge { limit: u64 },
    #[error("too many suggestions are waiting for review")]
    InboxFull,
    #[error("invalid path: {reason}")]
    InvalidPath { reason: String },
    #[error("couldn't write: {0}")]
    Io(String),
}

impl WriteError {
    /// The stable `snake_case` code (`structuredContent.error` in MCP).
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PermissionDenied { .. } => "permission_denied",
            Self::RawIsImmutable { .. } => "raw_is_immutable",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyExists { .. } => "already_exists",
            Self::VersionConflict { .. } => "version_conflict",
            Self::NotUnique { .. } => "not_unique",
            Self::NoMatch { .. } => "no_match",
            Self::ContractViolation { .. } => "contract_violation",
            Self::RateLimited { .. } => "rate_limited",
            Self::TooLarge { .. } => "too_large",
            Self::InboxFull => "inbox_full",
            Self::InvalidPath { .. } => "invalid_path",
            Self::Io(_) => "io",
        }
    }

    /// `{"error": code, …details}` for structured tool results.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        let mut v = match self {
            Self::PermissionDenied { path, zone, mode } => json!({"path": path, "zone": zone, "mode": mode}),
            Self::RawIsImmutable { path } | Self::NotFound { path } | Self::AlreadyExists { path } | Self::NoMatch { path } => {
                json!({"path": path})
            }
            Self::VersionConflict { path, expected, current, diff_since, merged_preview } => {
                json!({"path": path, "expected": expected, "current": current, "diff_since": diff_since, "merged_preview": merged_preview})
            }
            Self::NotUnique { path, matches } => json!({"path": path, "matches": matches}),
            Self::ContractViolation { contract, violations } => json!({"contract": contract, "violations": violations}),
            Self::RateLimited { retry_after_ms } => json!({"retry_after_ms": retry_after_ms}),
            Self::TooLarge { limit } => json!({"limit": limit}),
            Self::InboxFull => json!({}),
            Self::InvalidPath { reason } => json!({"reason": reason}),
            Self::Io(m) => json!({"message": m}),
        };
        v["error"] = self.code().into();
        v["message"] = self.to_string().into();
        v
    }
}

impl From<quire_vault::VaultError> for WriteError {
    fn from(e: quire_vault::VaultError) -> Self {
        match e {
            quire_vault::VaultError::NotFound(p) => Self::NotFound { path: NotePath::new(p) },
            quire_vault::VaultError::Exists(p) => Self::AlreadyExists { path: NotePath::new(p) },
            quire_vault::VaultError::InvalidName(n) => Self::InvalidPath { reason: n },
            other => Self::Io(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_sets_serialize_readably() {
        let cs = ChangeSet::one(Change::SetProperties {
            path: "A.md".into(),
            set: vec![("status".into(), Value::Str("done".into()))],
            remove: vec![],
        })
        .titled("Mark done");
        let v = serde_json::to_value(&cs).unwrap();
        assert_eq!(v["changes"][0], serde_json::json!({"op": "set_properties", "path": "A.md", "set": [["status", "done"]], "remove": []}));
        assert_eq!(serde_json::from_value::<ChangeSet>(v).unwrap(), cs);
    }

    #[test]
    fn errors_have_stable_codes() {
        let e = WriteError::RawIsImmutable { path: "raw/x.md".into() };
        let j = e.to_json();
        assert_eq!(j["error"], "raw_is_immutable");
        assert_eq!(j["path"], "raw/x.md");
        assert!(j["message"].as_str().unwrap().contains("raw/"));
        assert_eq!(WriteError::from(quire_vault::VaultError::NotFound("A.md".into())).code(), "not_found");
    }
}
