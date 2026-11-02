// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Span citations that notice when the cited text changes (design §C.3).
//!
//! An [`Anchor`] records a passage of a note: its version, line range, the
//! quoted text and a little context on each side. [`resolve`] finds the
//! passage again in the note's current text. This is the exact-matching
//! version: an unchanged note, or the same quote found again (moved or not).
//! WS-C adds sentence widening, block ids, history-based mapping and fuzzy
//! re-anchoring (`Drifted`) behind the same functions.

use std::ops::Range;

use quire_vault::{ContentHash, NotePath};
use serde::{Deserialize, Serialize};

/// Longest quote kept whole; longer ones keep their start and end.
const MAX_QUOTE: usize = 400;
const QUOTE_KEEP: usize = 150;
const CONTEXT: usize = 32;
const ELLIPSIS: char = '…';

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub path: NotePath,
    /// Hash of the whole note when the anchor was made.
    pub version: ContentHash,
    /// 1-based inclusive line range.
    pub lines: (u32, u32),
    /// The cited text (at most 400 chars; longer quotes are `first 150…last 150`).
    pub quote: String,
    /// Up to 32 chars before and after the quote.
    pub prefix: String,
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// The nearest heading above the quote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AnchorStatus {
    /// The note is unchanged, or the quote is still on the same lines.
    Exact,
    /// The same text, now on other lines.
    Moved { lines: (u32, u32) },
    /// Similar text found (similarity 0.75 to 1).
    Drifted { lines: (u32, u32), similarity: f32 },
    /// Not found.
    Lost,
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn line_of(src: &str, byte: usize) -> u32 {
    u32::try_from(src[..floor_char(src, byte)].matches('\n').count() + 1).unwrap_or(u32::MAX)
}

/// The last `n` chars of `s`.
fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// An anchor for `byte_range` of `src` (the text of `path`).
pub fn make_anchor(path: &NotePath, src: &str, byte_range: Range<usize>) -> Anchor {
    let start = floor_char(src, byte_range.start);
    let end = floor_char(src, byte_range.end.max(start));
    let text = &src[start..end];
    let quote = if text.chars().count() > MAX_QUOTE {
        let head: String = text.chars().take(QUOTE_KEEP).collect();
        format!("{head}{ELLIPSIS}{}", tail(text, QUOTE_KEEP))
    } else {
        text.to_string()
    };
    let end_line_byte = if end > start && src[..end].ends_with('\n') { end - 1 } else { end };
    let heading = quire_markdown::outline(src).into_iter().rev().find(|h| h.offset <= start).map(|h| h.text);
    // A paragraph id at the end of the quote's last line.
    let eb = floor_char(src, end_line_byte);
    let line_start = src[..eb].rfind('\n').map_or(0, |i| i + 1);
    let line_end = src[eb..].find('\n').map_or(src.len(), |i| eb + i);
    let block_id = quire_markdown::callout::block_id(&src[line_start..line_end]).map(|(_, id)| id.to_string());
    Anchor {
        path: path.clone(),
        version: ContentHash::of(src),
        lines: (line_of(src, start), line_of(src, end_line_byte)),
        quote,
        prefix: tail(&src[..start], CONTEXT),
        suffix: src[end..].chars().take(CONTEXT).collect(),
        block_id,
        heading,
    }
}

/// The searchable part of a quote: all of it, or the kept start of a long one.
fn needle(quote: &str) -> &str {
    if quote.chars().count() > 2 * QUOTE_KEEP
        && let Some((head, _)) = quote.split_once(ELLIPSIS)
    {
        return head;
    }
    quote
}

/// Where `anchor` is in `current`. `old_text` (the note at `anchor.version`,
/// from history or the served cache) lets later versions map changed
/// passages; this version doesn't need it yet.
pub fn resolve(anchor: &Anchor, current: &str, old_text: Option<&str>) -> AnchorStatus {
    let _ = old_text;
    if ContentHash::of(current) == anchor.version {
        return AnchorStatus::Exact;
    }
    let n = needle(&anchor.quote);
    if n.is_empty() {
        return AnchorStatus::Lost;
    }
    let span = anchor.lines.1.saturating_sub(anchor.lines.0);
    let score = |at: usize| {
        let before = &current[..at];
        let after = &current[at + n.len()..];
        let context = u32::from(before.ends_with(&anchor.prefix)) + u32::from(after.starts_with(&anchor.suffix));
        (std::cmp::Reverse(context), line_of(current, at).abs_diff(anchor.lines.0))
    };
    let Some(best) = current.match_indices(n).map(|(i, _)| i).min_by_key(|&at| score(at)) else { return AnchorStatus::Lost };
    let start = line_of(current, best);
    if start == anchor.lines.0 { AnchorStatus::Exact } else { AnchorStatus::Moved { lines: (start, start + span) } }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "# Rope Notes\n## Undo\nRopes make undo cheap because subtrees are shared. ^undo\n\nMore text.\n";

    fn anchor_on(src: &str, quote: &str) -> Anchor {
        let at = src.find(quote).unwrap();
        make_anchor(&NotePath::new("Rope Notes.md"), src, at..at + quote.len())
    }

    #[test]
    fn makes_anchors_with_context() {
        let a = anchor_on(NOTE, "Ropes make undo cheap");
        assert_eq!(a.lines, (3, 3));
        assert_eq!(a.heading.as_deref(), Some("Undo"));
        assert_eq!(a.prefix, "# Rope Notes\n## Undo\n");
        assert_eq!(a.suffix, " because subtrees are shared. ^u");
        assert_eq!(a.block_id.as_deref(), Some("undo"));
        assert_eq!(a.version, ContentHash::of(NOTE));
        let long = "x".repeat(1000);
        let a = make_anchor(&NotePath::new("L.md"), &long, 0..1000);
        assert_eq!(a.quote.chars().count(), 301);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Anchor>(&json).unwrap(), a);
    }

    #[test]
    fn resolves_exact_moved_and_lost() {
        let a = anchor_on(NOTE, "subtrees are shared");
        assert_eq!(resolve(&a, NOTE, None), AnchorStatus::Exact);
        let edited_elsewhere = NOTE.replace("More text.", "More text, edited.");
        assert_eq!(resolve(&a, &edited_elsewhere, None), AnchorStatus::Exact);
        let moved = format!("Intro line\n\n{NOTE}");
        assert_eq!(resolve(&a, &moved, None), AnchorStatus::Moved { lines: (5, 5) });
        let gone = NOTE.replace("subtrees are shared", "nodes are copied");
        assert_eq!(resolve(&a, &gone, None), AnchorStatus::Lost);
        // Two copies: the one with matching context wins.
        let twice = format!("subtrees are shared\n{NOTE}");
        assert_eq!(resolve(&a, &twice, None), AnchorStatus::Moved { lines: (4, 4) });
        let s = serde_json::to_value(AnchorStatus::Moved { lines: (4, 4) }).unwrap();
        assert_eq!(s, serde_json::json!({"status": "moved", "lines": [4, 4]}));
    }
}
