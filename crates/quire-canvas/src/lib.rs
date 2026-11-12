// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON Canvas 1.0 (`.canvas` files) and ink sketches (design 0.9 §G.1).
//! No egui here: the model, its canonical file form, geometry, the edit
//! operations agents send, and the ink codec.
//!
//! * [`model`] — nodes and edges, keeping keys Quire doesn't know
//! * [`model::to_canonical`] — one node or edge per line, so line-based
//!   diff, merge, sync and proposals work per node
//! * [`geom`] — rectangles, edge curves, hit tests, alignment
//! * [`ops`] — `canvas_apply` operations and a node-level diff
//! * [`ink`] — strokes as SVG paths with their points kept in `data-` attributes
//! * [`excalidraw`] — Excalidraw drawings read as pictures (SVG and draw ops)
//! * [`drawing`] — Excalidraw drawings as editable files (0.13), on [`ojson`]

pub mod drawing;
pub mod excalidraw;
pub mod geom;
pub mod ink;
pub mod model;
pub mod ojson;
pub mod ops;

pub use model::{Canvas, Color, Edge, Node, NodeKind, parse, to_canonical};
