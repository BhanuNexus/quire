// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDF annotations as a Markdown note (design §G.2, docs/formats/pdf-annotations.md).
//!
//! `paper.pdf`'s highlights live in `paper (annotations).md` beside it. Each
//! highlight is a quote callout with a block id, the reader's comment, and
//! a `<!-- quire:pdf-anchor {json} -->` comment that places it on the page:
//!
//! ```markdown
//! > [!quote] p. 3 · [[paper.pdf#page=3&annot=h7f3a]]
//! > Ropes are balanced trees of strings.
//! ^h7f3a
//!
//! My comment.
//! <!-- quire:pdf-anchor {"p":3,"q":[[72.0,512.3,301.2,524.9]],"t":"…","pre":"…","suf":"…","c":"yellow"} -->
//! ```
//!
//! Edits here change only the entry they are about; the rest of the note
//! (headings, the person's own text) stays byte for byte.

use std::ops::Range;

use quire_markdown::{Value, comment, frontmatter};
use quire_vault::NotePath;
use serde::{Deserialize, Serialize};

pub const ANCHOR_KIND: &str = "pdf-anchor";

/// Highlight colours, by name (the first is the default).
pub const COLORS: [&str; 5] = ["yellow", "green", "blue", "pink", "purple"];

/// Where a highlight is: the JSON inside `quire:pdf-anchor`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PdfAnchor {
    /// Page, 1-based.
    pub p: u32,
    /// Rectangles in PDF points, `[x0, y0, x1, y1]`.
    pub q: Vec<[f32; 4]>,
    /// The highlighted text (empty for an area highlight).
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

impl PdfAnchor {
    /// An area highlight (a rectangle, no text).
    pub fn is_area(&self) -> bool {
        self.t.trim().is_empty()
    }
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
pub fn anchors(src: &str) -> Vec<(PdfAnchor, Range<usize>)> {
    comment::find(src, ANCHOR_KIND).into_iter().filter_map(|c| Some((serde_json::from_str(&c.body).ok()?, c.range))).collect()
}

/// A new, empty annotation note.
pub fn new_note(pdf: &NotePath, sha256: &str) -> String {
    format!("---\nannotates: \"[[{}]]\"\npdf_sha256: {sha256}\ntype: annotations\n---\n# Highlights: {}\n\n", pdf.file_name(), pdf.stem())
}

/// The `pdf_sha256` the note's highlights were placed on.
pub fn pdf_sha(note: &str) -> Option<String> {
    let fm = frontmatter::parse(note)?;
    fm.get("pdf_sha256").and_then(|v| v.as_str().map(str::to_string)).filter(|s| !s.is_empty())
}

/// Record that the highlights now sit on the PDF with this hash.
pub fn set_pdf_sha(note: &str, sha256: &str) -> String {
    let e = frontmatter::set_field(note, "pdf_sha256", &Value::Str(sha256.to_string()));
    let mut out = note.to_string();
    out.replace_range(e.range, &e.text);
    out
}

/// One highlight as written in the note.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub anchor: PdfAnchor,
    /// The quoted text as written (the callout's lines).
    pub quote: String,
    /// The person's comment under it.
    pub comment: String,
    /// The whole entry, from the callout to the end of the anchor's line.
    pub range: Range<usize>,
    /// Where the comment's text is (may be empty, between the id and the anchor).
    pub comment_range: Range<usize>,
}

fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(src: &str, at: usize) -> usize {
    src[at..].find('\n').map_or(src.len(), |i| at + i + 1)
}

/// The highlights of an annotation note, in order.
pub fn entries(src: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut floor = 0;
    for (anchor, r) in anchors(src) {
        let anchor_line = line_start(src, r.start);
        let end = line_end(src, r.end);
        // The callout header nearest above the anchor, after the previous entry.
        let mut start = None;
        let mut pos = anchor_line;
        while pos > floor {
            let ls = line_start(src, pos - 1);
            if src[ls..].starts_with("> [!quote]") || src[ls..].starts_with(">[!quote]") {
                start = Some(ls);
                break;
            }
            pos = ls;
        }
        let start = start.unwrap_or(anchor_line);
        let block = &src[start..anchor_line];
        let mut quote = Vec::new();
        let mut id = None;
        let mut comment_start = start;
        let mut offset = start;
        for (i, line) in block.split_inclusive('\n').enumerate() {
            let t = line.trim_end();
            offset += line.len();
            if i == 0 {
                id = t
                    .split("annot=")
                    .nth(1)
                    .map(|s| s.trim_end_matches("]]").split([']', '|', '&']).next().unwrap_or_default().to_string());
                comment_start = offset;
                continue;
            }
            if let Some(q) = t.strip_prefix('>') {
                quote.push(q.trim().to_string());
                comment_start = offset;
            } else if let Some(b) =
                t.strip_prefix('^').filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            {
                id = Some(b.to_string());
                comment_start = offset;
                break;
            } else {
                break;
            }
        }
        let comment_range = comment_start..anchor_line;
        let comment = src[comment_range.clone()].trim().to_string();
        out.push(Entry {
            id: id.filter(|i| !i.is_empty()).unwrap_or_default(),
            anchor,
            quote: quote.join("\n"),
            comment,
            range: start..end,
            comment_range,
        });
        floor = end;
    }
    out
}

/// The text of one entry (callout, id, comment, anchor). `quote` is the
/// text shown in the callout: the highlighted text, or an embed of an area's
/// picture.
pub fn entry_text(pdf: &NotePath, id: &str, a: &PdfAnchor, quote: &str, comment: &str) -> String {
    let mut s = format!("> [!quote] p. {} · [[{}#page={}&annot={id}]]\n", a.p, pdf.file_name(), a.p);
    let quote = if quote.trim().is_empty() { format!("(area on page {})", a.p) } else { quote.to_string() };
    for l in quote.lines() {
        s.push_str("> ");
        s.push_str(l.trim());
        s.push('\n');
    }
    s.push('^');
    s.push_str(id);
    s.push_str("\n\n");
    if !comment.trim().is_empty() {
        s.push_str(comment.trim());
        s.push('\n');
    }
    s.push_str(&anchor_comment(a));
    s.push('\n');
    s
}

/// `note` with a new highlight at the end.
pub fn add(note: &str, pdf: &NotePath, id: &str, a: &PdfAnchor, quote: &str, comment: &str) -> String {
    let mut out = note.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&entry_text(pdf, id, a, quote, comment));
    out
}

/// `note` with highlight `id`'s comment replaced.
pub fn set_comment(note: &str, id: &str, comment: &str) -> Option<String> {
    let e = entries(note).into_iter().find(|e| e.id == id)?;
    let mut out = note.to_string();
    let text = if comment.trim().is_empty() { "\n".to_string() } else { format!("\n{}\n", comment.trim()) };
    // The comment sits after the block id's line and its blank line.
    out.replace_range(e.comment_range, &text);
    Some(out)
}

/// `note` without highlight `id` (and the blank line after it).
pub fn remove(note: &str, id: &str) -> Option<String> {
    let e = entries(note).into_iter().find(|e| e.id == id)?;
    let mut end = e.range.end;
    if note[end..].starts_with('\n') {
        end += 1;
    }
    let mut out = note.to_string();
    out.replace_range(e.range.start..end, "");
    Some(out)
}

/// `note` with highlight `id` moved to `a` (after re-anchoring): its anchor
/// and the page in its heading change; the quote and comment stay.
pub fn move_anchor(note: &str, pdf: &NotePath, id: &str, a: &PdfAnchor) -> Option<String> {
    let e = entries(note).into_iter().find(|e| e.id == id)?;
    let (_, r) = anchors(&note[e.range.clone()]).into_iter().next()?;
    let mut out = note.to_string();
    out.replace_range(e.range.start + r.start..e.range.start + r.end, &anchor_comment(a));
    let head_end = line_end(&out, e.range.start);
    let head = &out[e.range.start..head_end];
    if head.starts_with("> [!quote] p. ") {
        let nl = if head.ends_with('\n') { "\n" } else { "" };
        let new_head = format!("> [!quote] p. {} · [[{}#page={}&annot={id}]]{nl}", a.p, pdf.file_name(), a.p);
        out.replace_range(e.range.start..head_end, &new_head);
    }
    Some(out)
}

/// A highlight id not used in `note`: `h` and four hex digits.
pub fn new_id(note: &str, seed: u64) -> String {
    let mut x = seed | 1;
    loop {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let id = format!("h{:04x}", x & 0xffff);
        if !note.contains(&format!("^{id}")) && !note.contains(&format!("annot={id}")) {
            return id;
        }
    }
}

/// Parse `[[paper.pdf#page=3&annot=h7f3a]]`'s part after `#`: page (1-based) and highlight id.
pub fn parse_fragment(fragment: &str) -> (Option<u32>, Option<String>) {
    let mut page = None;
    let mut annot = None;
    for kv in fragment.trim_start_matches('#').split('&') {
        match kv.split_once('=') {
            Some(("page", v)) => page = v.trim().parse().ok().filter(|p| *p > 0),
            Some(("annot", v)) if !v.trim().is_empty() => annot = Some(v.trim().to_string()),
            _ => {}
        }
    }
    (page, annot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(p: u32, t: &str) -> PdfAnchor {
        PdfAnchor {
            p,
            q: vec![[72.0, 512.3, 301.2, 524.9]],
            t: t.into(),
            pre: "…".into(),
            suf: String::new(),
            c: "yellow".into(),
            other: serde_json::Map::new(),
        }
    }

    #[test]
    fn anchors_round_trip_in_a_note() {
        assert_eq!(note_for(&"papers/rope.pdf".into()).as_str(), "papers/rope (annotations).md");
        let a = anchor(3, "Ropes are --> trees.");
        let note = format!(
            "> [!quote] p. 3 · [[rope.pdf#page=3&annot=h7f3a]]\n> Ropes are trees.\n^h7f3a\n\nMy comment.\n{}\n",
            anchor_comment(&a)
        );
        let found = anchors(&note);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, a);
    }

    #[test]
    fn highlights_are_added_edited_moved_and_removed_in_place() {
        let pdf = NotePath::new("papers/rope.pdf");
        let mut note = new_note(&pdf, "abc");
        note.push_str("My own notes stay.\n");
        assert_eq!(pdf_sha(&note).as_deref(), Some("abc"));
        let note = add(
            &note,
            &pdf,
            "h0001",
            &anchor(3, "Ropes are balanced trees of strings."),
            "Ropes are balanced trees of strings.",
            "Key idea.",
        );
        let note = add(&note, &pdf, "h0002", &anchor(5, ""), "", "");
        let e = entries(&note);
        assert_eq!(e.len(), 2, "{note}");
        assert_eq!(
            (e[0].id.as_str(), e[0].comment.as_str(), e[0].quote.as_str()),
            ("h0001", "Key idea.", "Ropes are balanced trees of strings.")
        );
        assert_eq!((e[1].id.as_str(), e[1].comment.as_str(), e[1].quote.as_str()), ("h0002", "", "(area on page 5)"));
        assert!(e[1].anchor.is_area());
        assert!(note.contains("My own notes stay.\n\n> [!quote] p. 3 · [[rope.pdf#page=3&annot=h0001]]\n"), "{note}");

        let edited = set_comment(&note, "h0002", "A figure\nover two lines").unwrap();
        assert_eq!(entries(&edited)[1].comment, "A figure\nover two lines");
        assert_eq!(entries(&edited)[0], entries(&note)[0], "the other entry is untouched");
        let cleared = set_comment(&edited, "h0001", "").unwrap();
        assert_eq!(entries(&cleared)[0].comment, "");
        assert_eq!(entries(&cleared).len(), 2);

        let mut moved_a = anchor(4, "Ropes are balanced trees of strings.");
        moved_a.q = vec![[1.0, 2.0, 3.0, 4.0]];
        let moved = move_anchor(&edited, &pdf, "h0001", &moved_a).unwrap();
        let m = entries(&moved);
        assert_eq!(m[0].anchor, moved_a);
        assert!(moved.contains("> [!quote] p. 4 · [[rope.pdf#page=4&annot=h0001]]\n"));
        assert_eq!(m[0].comment, "Key idea.");

        let gone = remove(&moved, "h0001").unwrap();
        assert_eq!(entries(&gone).len(), 1);
        assert!(gone.contains("My own notes stay.") && !gone.contains("h0001"));
        assert_eq!(remove(&gone, "nope"), None);
        let again = set_pdf_sha(&gone, "def");
        assert_eq!(pdf_sha(&again).as_deref(), Some("def"));
        assert_eq!(again.len(), gone.len(), "only the hash changed");
    }

    #[test]
    fn fragments_and_ids() {
        assert_eq!(parse_fragment("page=3&annot=h7f3a"), (Some(3), Some("h7f3a".into())));
        assert_eq!(parse_fragment("#page=0"), (None, None));
        assert_eq!(parse_fragment("annot=x"), (None, Some("x".into())));
        let id = new_id("^h0001", 7);
        assert!(id.starts_with('h') && id.len() == 5 && id != "h0001");
    }
}
