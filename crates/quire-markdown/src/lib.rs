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
//! * [`tasks`] — task metadata; [`srs`] — flashcards; [`comment`] — `<!-- quire:… -->` machine data
//!
//! All ranges in this crate are **byte** ranges into the source `&str`
//! unless a function says otherwise; editing helpers convert to char indices.

pub mod blocks;
pub mod callout;
pub mod cite;
pub mod comment;
pub mod editing;
pub mod extract;
// 0.12: typed links (inline fields and link-valued properties).
pub mod fields;
// 0.11: the formatting toolbar and the Format commands, on every cursor.
pub mod format;
pub mod frontmatter;
pub mod outline;
pub mod render;
mod scan;
pub mod srs;
pub mod syntax;
pub mod table;
pub mod tasks;

pub use extract::{Extracted, Link, LinkKind, Stats, Task, extract};
pub use frontmatter::{FrontMatter, Kind, Value};
pub use outline::{Heading, outline};
pub use scan::{Inline, WikiLink, scan_inlines, verbatim_ranges};
pub use syntax::{Style, StyleRun, highlight};

use std::borrow::Cow;
use std::ops::Range;

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

/// Byte ranges (without the newline) of merge-conflict marker lines:
/// `<<<<<<< …`, `=======` and `>>>>>>> …`. A `=======` only counts inside an
/// open conflict, so ordinary setext headings are left alone.
pub fn conflict_marker_lines(src: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if !src.contains("<<<<<<<") {
        return out;
    }
    let mut open = false;
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        let is_marker = if text.starts_with("<<<<<<<") {
            open = true;
            true
        } else if open && text == "=======" {
            true
        } else if open && text.starts_with(">>>>>>>") {
            open = false;
            true
        } else {
            false
        };
        if is_marker {
            out.push(at..at + text.len());
        }
        at += line.len();
    }
    out
}

/// The text the parser sees: conflict marker lines blanked out (same byte
/// length) so they separate blocks instead of forming setext headings or
/// block quotes. Offsets stay valid for `src`.
pub(crate) fn parse_source(src: &str) -> Cow<'_, str> {
    let markers = conflict_marker_lines(src);
    if markers.is_empty() {
        return Cow::Borrowed(src);
    }
    let mut bytes = src.as_bytes().to_vec();
    for r in markers {
        bytes[r].fill(b' ');
    }
    // Only ASCII bytes inside whole lines were replaced by ASCII spaces.
    Cow::Owned(String::from_utf8(bytes).unwrap_or_else(|_| src.to_string()))
}

/// For rendered views: each conflict marker line becomes its own literal
/// paragraph, so it shows as text rather than Markdown syntax.
pub(crate) fn display_source(src: &str) -> Cow<'_, str> {
    let markers = conflict_marker_lines(src);
    if markers.is_empty() {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len() + markers.len() * 4);
    let mut pos = 0;
    for r in markers {
        out.push_str(&src[pos..r.start]);
        out.push_str("\n\\");
        out.push_str(&src[r.clone()]);
        out.push_str("\n\n");
        pos = src[r.end..].find('\n').map_or(src.len(), |n| r.end + n + 1);
    }
    out.push_str(&src[pos..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_markers_are_blocks() {
        let src = "Intro\n<<<<<<< This device\nmine\n=======\ntheirs\n>>>>>>> Changed elsewhere\nafter\n";
        assert_eq!(conflict_marker_lines(src).len(), 3);
        assert!(conflict_marker_lines("Title\n=======\n").is_empty(), "setext heading untouched");
        assert!(outline(src).is_empty(), "no setext heading from the separator");
        let p = parse_source(src);
        assert_eq!(p.len(), src.len());
        let shown = display_source(src);
        assert!(shown.contains("\n\\=======\n\n") && shown.contains("\\>>>>>>> Changed elsewhere"));
        let html = render::to_html(src, &|_| None);
        assert!(!html.contains("<h") && !html.contains("<blockquote>"), "{html}");
        let runs = highlight(src);
        let sep = src.find("=======").unwrap_or_default();
        assert!(runs.iter().any(|r| r.range.contains(&sep) && r.style.conflict));
        assert!(!runs.iter().any(|r| r.style.quote || r.style.heading > 0));
    }
}
