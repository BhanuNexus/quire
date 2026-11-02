// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDF annotations as a Markdown note (design §G.2, docs/formats/pdf-annotations.md).
//!
//! `paper.pdf`'s highlights live in `paper (annotations).md` beside it. Each
//! highlight is a quote callout with a block id, the reader's comment, and
//! a `<!-- quire:pdf-anchor {json} -->` comment that places it on the page.

use quire_markdown::comment;
use quire_vault::NotePath;
use serde::{Deserialize, Serialize};

pub const ANCHOR_KIND: &str = "pdf-anchor";

/// Where a highlight is: the JSON inside `quire:pdf-anchor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PdfAnchor {
    /// Page, 1-based.
    pub p: u32,
    /// Rectangles in PDF points, `[x0, y0, x1, y1]`.
    pub q: Vec<[f32; 4]>,
    /// The highlighted text.
    pub t: String,
    /// Up to 32 characters before and after, for re-anchoring.
    #[serde(default)]
    pub pre: String,
    #[serde(default)]
    pub suf: String,
    /// Highlight colour name (`yellow`).
    #[serde(default)]
    pub c: String,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// The annotation note for a PDF: same folder, `<stem> (annotations).md`.
pub fn note_for(pdf: &NotePath) -> NotePath {
    NotePath::join(pdf.folder(), &format!("{} (annotations).md", pdf.stem()))
}

/// The comment line for an anchor.
pub fn anchor_comment(a: &PdfAnchor) -> String {
    comment::format(ANCHOR_KIND, &serde_json::to_string(a).unwrap_or_default())
}

/// Every readable anchor in an annotation note, with the comment's byte range.
pub fn anchors(src: &str) -> Vec<(PdfAnchor, std::ops::Range<usize>)> {
    comment::find(src, ANCHOR_KIND).into_iter().filter_map(|c| Some((serde_json::from_str(&c.body).ok()?, c.range))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_round_trip_in_a_note() {
        assert_eq!(note_for(&"papers/rope.pdf".into()).as_str(), "papers/rope (annotations).md");
        let a = PdfAnchor {
            p: 3,
            q: vec![[72.0, 512.3, 301.2, 524.9]],
            t: "Ropes are --> trees.".into(),
            pre: "…".into(),
            suf: String::new(),
            c: "yellow".into(),
            other: serde_json::Map::new(),
        };
        let note = format!(
            "> [!quote] p. 3 · [[rope.pdf#page=3&annot=h7f3a]]\n> Ropes are trees.\n^h7f3a\n\nMy comment.\n{}\n",
            anchor_comment(&a)
        );
        let found = anchors(&note);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, a);
    }
}
