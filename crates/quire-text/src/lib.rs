// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Domain layer: the text model.
//!
//! Everything that edits text goes through a [`Transaction`]. Transactions are
//! invertible, which gives us undo without snapshots, and they map cleanly onto
//! CRDT operations should collaboration be added later.
//!
//! All positions in this crate are **char indices** (Unicode scalar values),
//! matching `ropey` and most UI toolkits' cursor models.

mod buffer;
mod change;
mod history;
pub mod motion;
pub mod multi;
mod selection;

pub use buffer::Buffer;
pub use change::{Assoc, Change, LineDelta, Transaction, TransactionError};
pub use history::{EditKind, History, UndoStep};
pub use selection::Selection;
