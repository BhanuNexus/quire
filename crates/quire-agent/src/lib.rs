// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agent store: everything about non-human writers (design 0.9 §A).
//!
//! * [`actor`] — who did something, and provenance records
//! * [`policy`] — zones, modes and hard rules
//! * [`AgentStore::submit`] — the one write path: apply or propose
//! * [`proposal`] — proposal files; [`ledger`] — the event log
//! * [`inbox`] — findings waiting for the person; [`revert`] — undo plans
//!
//! File formats are described in `docs/formats/`.

pub mod actor;
pub mod change;
pub mod ids;
pub mod inbox;
pub mod ledger;
pub mod policy;
pub mod proposal;
pub mod revert;
mod store;

pub use actor::{Actor, ActorKind, ClientInfo, Clock, DeviceInfo, FixedClock, Op, Provenance, SourceRef, SystemClock, Trust, Via};
pub use change::{Change, ChangeSet, SubmitOutcome, Violation, WriteError};
pub use ids::{BatchId, ProposalId, RunId};
pub use inbox::{InboxItem, InboxStore};
pub use ledger::{LedgerEntry, LedgerQuery};
pub use policy::{Mode, OpKind, Policy};
pub use proposal::{Proposal, ProposalFilter, ProposalStatus, ProposalSummary};
pub use quire_vault::ContentHash;
pub use revert::{RevertPlan, RevertSelector};
pub use store::{AgentStore, canonical_path};

#[cfg(test)]
mod tests;
