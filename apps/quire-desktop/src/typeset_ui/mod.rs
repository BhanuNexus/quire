// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Math and diagrams as textures, rendered off the UI thread.
//!
//! The editor and preview ask for a formula or a Mermaid diagram every frame;
//! the first ask queues it for a worker thread (Typst for math, the Mermaid
//! renderer for diagrams, then resvg to pixels) and returns `Pending`. When
//! the picture is ready the UI repaints and gets `Ready`. A formula that
//! doesn't parse comes back `Failed` with the reason. Results are kept per
//! (source, size, colour, scale) and dropped when there are too many.

use egui::{Color32, TextureHandle, Vec2};

/// Whether this build draws math and diagrams (Quire Lite doesn't; it shows their source).
pub(crate) const AVAILABLE: bool = cfg!(feature = "typeset");

#[cfg(feature = "typeset")]
mod engine;
#[cfg(feature = "typeset")]
pub(crate) use engine::request;

/// Quire Lite: nothing is ever drawn (callers check [`AVAILABLE`] first).
#[cfg(not(feature = "typeset"))]
pub(crate) fn request(_ctx: &egui::Context, _job: &Job) -> Out {
    Out::Pending
}

/// What to draw.
#[derive(Debug, Clone, PartialEq, Hash)]
pub(crate) enum Job {
    /// LaTeX math at `size` points in `color`; `display` for `$$ $$` blocks.
    Math { latex: String, display: bool, size: u32, color: [u8; 4] },
    /// A Mermaid diagram.
    Diagram { source: String, dark: bool },
}

/// A finished picture: its texture and size in points; `baseline` is the
/// distance from the top to the text baseline (math only).
#[derive(Clone)]
pub(crate) struct Picture {
    pub texture: TextureHandle,
    pub size: Vec2,
    pub baseline: f32,
}

#[derive(Clone)]
#[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Quire Lite never renders, so only Pending is made"))]
pub(crate) enum Out {
    Pending,
    Ready(Picture),
    Failed(String),
}

/// Text colour as RGBA for math.
pub(crate) fn rgba(c: Color32) -> [u8; 4] {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    [r, g, b, a]
}
