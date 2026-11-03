// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pure-Rust typesetting for Quire: no JavaScript engine, no network, no
//! external programs.
//!
//! * [`math_svg`] — LaTeX math to SVG (LaTeX → Typst math with `mitex`, laid
//!   out by the Typst compiler, exported with `typst-svg`).
//! * [`diagram_svg`] — Mermaid diagrams to SVG (`mermaid-rs-renderer`).
//! * [`markdown_pdf`] — a Markdown note to PDF (Markdown → Typst markup →
//!   `typst-pdf`), with math, diagrams, callouts, tables and images.
//!
//! Fonts are embedded (Inter, `JetBrains Mono NL`, STIX Two Math)
//! and parsed once per process. Math and diagram renders are cached.

mod cache;
mod diagram;
pub mod fonts;
mod markdown;
mod math;
mod world;

pub use diagram::diagram_svg;
pub use markdown::{BIB_STYLES, BibCite, Bibliography, CiteForm, ImageLoader, PageSize, PdfOptions, cite_placeholder, markdown_pdf};
pub use math::{MathStyle, math_svg};

/// A rendered SVG and its size in points.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// The SVG document.
    pub svg: String,
    /// Width in points.
    pub width_pt: f32,
    /// Height in points.
    pub height_pt: f32,
    /// Distance from the top edge to the text baseline, in points, so inline
    /// math can sit on the surrounding line.
    pub baseline_pt: f32,
}

/// Why something could not be typeset. The messages are short and meant for
/// showing to the user (e.g. under a broken formula).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypesetError {
    /// The LaTeX could not be converted.
    #[error("LaTeX error: {0}")]
    Latex(String),
    /// Typst rejected the converted input.
    #[error("{0}")]
    Typst(String),
    /// The Mermaid source could not be rendered.
    #[error("diagram error: {0}")]
    Diagram(String),
    /// A parameter was out of range.
    #[error("{0}")]
    Invalid(String),
    /// A dependency failed unexpectedly (a caught panic, an export error).
    #[error("internal error: {0}")]
    Internal(String),
}

/// `s` as a Typst string literal, quotes included.
pub(crate) fn typst_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
