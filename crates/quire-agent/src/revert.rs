// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Undoing what an agent (or a batch) did, from the ledger (design §A.1).
//!
//! [`RevertPlan`] is a dry run: the steps that would undo the selected
//! events. Planning the steps (three-way merges against later human edits)
//! is WS-A's; today the plan lists the matching events and no steps.

use quire_vault::NotePath;

use crate::ids::RunId;
use crate::ledger::LedgerEntry;

/// Which events to undo; empty fields match everything.
#[derive(Debug, Clone, Default)]
pub struct RevertSelector {
    /// Actor id (`mcp:claude-code`).
    pub actor: Option<String>,
    pub run: Option<RunId>,
    /// Unix ms.
    pub since: Option<i64>,
    pub paths: Option<Vec<NotePath>>,
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
    /// The undo conflicts with later edits: make a revert proposal instead.
    Propose { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertStep {
    pub path: NotePath,
    pub action: RevertAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevertPlan {
    /// The ledger events selected, oldest first.
    pub events: Vec<LedgerEntry>,
    pub steps: Vec<RevertStep>,
    /// Why there are no steps, when there aren't.
    pub note: Option<String>,
}
