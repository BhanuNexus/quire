// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agent store: everything about non-human writers (design 0.9 §A).
//!
//! * [`actor`] — who did something, and provenance records
//! * [`policy`] — zones, modes and hard rules
//! * [`AgentStore::submit`] — the one write path: apply or propose
//! * [`proposal`] — proposal files; [`ledger`] — the event log
//! * [`inbox`] — findings waiting for the person; [`revert`] — undo plans
//! * [`review`] — hunks and decisions; [`approval`] — what this device approved
//! * [`contract`] — write contracts; [`lint`] — vault checks
//! * [`fence`] — untrusted text in data blocks
//! * [`instructions`] — `AGENTS.md`; [`watch`] — noticing other processes
//! * [`shared`] — review history shared between devices (0.10)
//!
//! File formats are described in `docs/formats/`.

pub mod actor;
pub mod approval;
pub mod change;
pub mod contract;
pub mod fence;
pub mod ids;
pub mod inbox;
pub mod instructions;
pub mod ledger;
pub mod lint;
mod ops;
pub mod policy;
pub mod policy_edit;
pub mod proposal;
pub mod revert;
pub mod review;
pub mod shared;
mod store;
pub mod watch;

pub use actor::{Actor, ActorKind, ClientInfo, Clock, DeviceInfo, FixedClock, Op, Provenance, SourceRef, SystemClock, Trust, Via};
pub use change::{Change, ChangeSet, SubmitOutcome, Violation, WriteError};
pub use ids::{BatchId, ProposalId, RunId};
pub use inbox::{InboxItem, InboxStore};
pub use ledger::{LedgerEntry, LedgerQuery};
pub use ops::{DecideError, EXPIRE_AFTER_MS, PURGE_AFTER_MS, ResolveReport};
pub use policy::{Mode, OpKind, Policy};
pub use proposal::{
    ChangeOp, ChangeState, Decided, Hunk, HunkAction, HunkDecision, Proposal, ProposalFilter, ProposalStatus, ProposalSummary, Review,
};
pub use quire_vault::ContentHash;
pub use revert::{RevertAction, RevertPlan, RevertReport, RevertSelector, RevertStep};
pub use shared::{ImportReport, device_label};
pub use store::{AgentStore, PolicyState, Resolved, canonical_path, resolve_at, resolve_in, vault_file, vault_file_at};

#[cfg(test)]
mod shared_tests;
#[cfg(test)]
mod tests;
