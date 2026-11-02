// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON Canvas 1.0 (`.canvas` files) and ink sketches (design 0.9 §G.1).
//! No egui here: the model, its canonical file form, and the ink codec.
//!
//! * [`model`] — nodes and edges, keeping keys Quire doesn't know
//! * [`model::to_canonical`] — one node or edge per line, so line-based
//!   diff, merge, sync and proposals work per node
//! * [`ink`] — strokes as SVG paths with their points kept in `data-` attributes

pub mod ink;
pub mod model;

pub use model::{Canvas, Edge, Node, parse, to_canonical};
