// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Derived, rebuildable knowledge about the vault.
//!
//! [`Index`] is a plain synchronous data structure (easy to test).
//! [`IndexService`] runs it as a single-writer actor on a worker thread and
//! shares a read model behind an `RwLock`, so the UI reads without waiting
//! for parsing and never blocks typing on indexing.

mod actor;
pub mod attach;
pub mod chunk;
mod fulltext;
mod fuzzy;
mod graph;
mod hybrid;
mod index;
mod record;
mod sections;
pub mod semantic;
pub mod tokenize;
pub mod views;

pub use actor::{IndexMsg, IndexService, IndexStatus, SemanticBuilder};
pub use attach::{AttachmentKind, AttachmentText, TextSource};
pub use fulltext::{Query, SearchHit};
pub use fuzzy::QuickOpenHit;
pub use graph::{Backlink, Direction, LinkGraph};
pub use hybrid::HybridHit;
pub use index::Index;
pub use record::NoteRecord;
pub use sections::{SearchFilters, SearchKind, SectionHit};
