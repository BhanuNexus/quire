// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pure Markdown intelligence: no I/O, no UI.
//!
//! * [`syntax`] — style runs for live highlighting
//! * [`outline`](mod@outline) — heading tree
//! * [`extract`](mod@extract) — links, wikilinks, tags, tasks, stats
//! * [`frontmatter`] — YAML-subset front matter
//! * [`render`] — HTML for preview and export
//! * [`editing`] / [`table`] — smart edits that return [`quire_text::Transaction`]s
//!
//! All ranges in this crate are **byte** ranges into the source `&str`
//! unless a function says otherwise; editing helpers convert to char indices.

pub mod blocks;
pub mod editing;
pub mod extract;
pub mod frontmatter;
pub mod outline;
pub mod render;
mod scan;
pub mod syntax;
pub mod table;

pub use extract::{Extracted, Link, LinkKind, Stats, Task, extract};
pub use frontmatter::{FrontMatter, Kind, Value};
pub use outline::{Heading, outline};
pub use scan::{Inline, WikiLink, scan_inlines, verbatim_ranges};
pub use syntax::{Style, StyleRun, highlight};

use pulldown_cmark::Options;

/// Parser options shared by every module so they agree on what the text means.
pub(crate) fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
}
