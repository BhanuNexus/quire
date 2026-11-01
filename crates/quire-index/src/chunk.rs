// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Split notes into retrievable passages that remember where they came from.

use std::ops::Range;

use quire_markdown::{extract, outline};
use quire_vault::NotePath;

/// Longest passage, in words, before a section is split at paragraph breaks.
pub const MAX_WORDS: usize = 350;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub path: NotePath,
    /// Nearest heading above the passage, for citations like `[[Note#Heading]]`.
    pub heading: Option<String>,
    /// Byte range in the note's source.
    pub range: Range<usize>,
    /// Plain prose of the passage (title prepended for context).
    pub text: String,
}

pub fn chunk_note(path: &NotePath, src: &str) -> Vec<Chunk> {
    let title = path.stem();
    let heads = outline(src);
    let mut bounds: Vec<(usize, Option<String>)> = vec![(0, None)];
    for h in &heads {
        if h.offset == 0 {
            bounds[0].1 = Some(h.text.clone());
        } else {
            bounds.push((h.offset, Some(h.text.clone())));
        }
    }
    let mut out = Vec::new();
    for (i, (start, heading)) in bounds.iter().enumerate() {
        let end = bounds.get(i + 1).map_or(src.len(), |b| b.0);
        let section = &src[*start..end];
        // Split long sections at blank lines, keeping byte ranges exact.
        let mut piece_start = 0;
        let mut words = 0;
        let mut pos = 0;
        for para in section.split_inclusive("\n\n") {
            let w = para.split_whitespace().count();
            if words > 0 && words + w > MAX_WORDS {
                push(&mut out, path, title, heading, src, start + piece_start..start + pos);
                piece_start = pos;
                words = 0;
            }
            words += w;
            pos += para.len();
        }
        push(&mut out, path, title, heading, src, start + piece_start..end);
    }
    out
}

fn push(out: &mut Vec<Chunk>, path: &NotePath, title: &str, heading: &Option<String>, src: &str, range: Range<usize>) {
    let plain = extract(&src[range.clone()]).plain;
    if plain.split_whitespace().count() < 3 {
        return;
    }
    let ctx = match heading {
        Some(h) if h != title => format!("{title} — {h}\n"),
        _ => format!("{title}\n"),
    };
    out.push(Chunk { path: path.clone(), heading: heading.clone(), range, text: format!("{ctx}{}", plain.trim()) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_heading_and_length() {
        let long: String = (0..60).map(|i| format!("Paragraph {i} has exactly eight words here.\n\n")).collect();
        let src = format!("# Note\nIntro text about ropes.\n## Details\n{long}## Tiny\nok\n");
        let chunks = chunk_note(&NotePath::new("Note.md"), &src);
        assert_eq!(chunks[0].heading.as_deref(), Some("Note"));
        assert!(chunks[0].text.contains("Intro text"));
        let details: Vec<_> = chunks.iter().filter(|c| c.heading.as_deref() == Some("Details")).collect();
        assert!(details.len() >= 2, "long section should be split");
        assert!(details.iter().all(|c| c.text.split_whitespace().count() <= MAX_WORDS + 10));
        assert!(chunks.iter().all(|c| c.heading.as_deref() != Some("Tiny")), "too-short passages are dropped");
        for c in &chunks {
            assert!(src.is_char_boundary(c.range.start) && src.is_char_boundary(c.range.end));
        }
    }
}
