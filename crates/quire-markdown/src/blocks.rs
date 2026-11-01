// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fast block scan and windowed highlighting for very large documents.
//!
//! Highlighting a 10 MB note takes ~400 ms, far over a frame. The editor only
//! needs styles for what is on screen, but Markdown is contextual: a line
//! inside a fenced code block looks like prose on its own. So we scan once for
//! the spans that change meaning far away (fenced code, front matter), then
//! highlight the visible window widened to a safe block boundary.

use std::ops::Range;

use crate::syntax::{StyleRun, highlight};

/// Byte ranges of fenced code blocks and front matter, sorted. Each range
/// starts at a line start and ends after its closing line (or at EOF).
pub fn verbatim_blocks(src: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let bytes = src.as_bytes();
    // Front matter.
    if src.starts_with("---\n") || src.starts_with("---\r\n") {
        let mut p = src.find('\n').map_or(src.len(), |i| i + 1);
        let mut closed = None;
        while p < src.len() {
            let end = src[p..].find('\n').map_or(src.len(), |i| p + i + 1);
            let line = src[p..end].trim_end();
            if line == "---" || line == "..." {
                closed = Some(end);
                break;
            }
            p = end;
        }
        if let Some(end) = closed {
            out.push(0..end);
            pos = end;
        }
    }
    let mut open: Option<(usize, u8, usize)> = None; // (start, fence char, fence len)
    while pos < src.len() {
        let end = memchr_newline(bytes, pos).map_or(src.len(), |i| i + 1);
        let line = &src[pos..end];
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent <= 3 {
            let rest = &line.as_bytes()[indent..];
            let ch = rest.first().copied().unwrap_or(0);
            if ch == b'`' || ch == b'~' {
                let n = rest.iter().take_while(|&&b| b == ch).count();
                if n >= 3 {
                    match open {
                        None => {
                            // Backtick fences can't have backticks in the info string.
                            if !(ch == b'`' && rest[n..].contains(&b'`')) {
                                open = Some((pos, ch, n));
                            }
                        }
                        Some((start, c, len)) if c == ch && n >= len && rest[n..].iter().all(|b| b.is_ascii_whitespace()) => {
                            out.push(start..end);
                            open = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        pos = end;
    }
    if let Some((start, _, _)) = open {
        out.push(start..src.len());
    }
    out
}

fn memchr_newline(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..].iter().position(|&b| b == b'\n').map(|i| from + i)
}

fn containing(blocks: &[Range<usize>], pos: usize) -> Option<&Range<usize>> {
    let i = blocks.partition_point(|b| b.end <= pos);
    blocks.get(i).filter(|b| b.start <= pos)
}

/// How far to look for a paragraph break before giving up.
const SEARCH_LIMIT: usize = 64 * 1024;

fn floor_boundary(src: &str, mut i: usize) -> usize {
    while !src.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Style runs covering `window` (absolute byte offsets), computed from a
/// slice widened to block boundaries. `blocks` comes from [`verbatim_blocks`].
pub fn highlight_window(src: &str, window: Range<usize>, blocks: &[Range<usize>]) -> Vec<StyleRun> {
    let mut start = floor_boundary(src, window.start.min(src.len()));
    let mut end = floor_boundary(src, window.end.min(src.len()));
    if let Some(b) = containing(blocks, start) {
        start = b.start;
    } else {
        let lo = floor_boundary(src, start.saturating_sub(SEARCH_LIMIT));
        start = src[lo..start].rfind("\n\n").map_or(lo, |i| lo + i + 2);
        // Never start inside a block we skipped over.
        if let Some(b) = containing(blocks, start) {
            start = b.start;
        }
    }
    if let Some(b) = containing(blocks, end.saturating_sub(1).max(start)) {
        end = end.max(b.end);
    } else {
        let hi = floor_boundary(src, (end + SEARCH_LIMIT).min(src.len()));
        end = src[end..hi].find("\n\n").map_or(hi, |i| end + i + 1);
    }
    let end = end.min(src.len());
    highlight(&src[start..end])
        .into_iter()
        .map(|mut r| {
            r.range.start += start;
            r.range.end += start;
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ntitle: x\n---\n# Title\n\nSome **bold** text\nand more.\n\n```rust\nlet a = 1;\n\n# not a heading\n```\n\n- [ ] task with `code`\n- [x] done\n\n> quote *em*\n\n## Next [[Link]] #tag\n";

    fn restrict(runs: &[StyleRun], w: &Range<usize>) -> Vec<StyleRun> {
        runs.iter()
            .filter(|r| r.range.end > w.start && r.range.start < w.end)
            .map(|r| StyleRun { range: r.range.start.max(w.start)..r.range.end.min(w.end), style: r.style })
            .collect()
    }

    #[test]
    fn finds_fences_and_front_matter() {
        let b = verbatim_blocks(DOC);
        assert_eq!(b.len(), 2);
        assert_eq!(&DOC[b[0].clone()], "---\ntitle: x\n---\n");
        assert!(DOC[b[1].clone()].starts_with("```rust") && DOC[b[1].clone()].ends_with("```\n"));
        assert_eq!(verbatim_blocks("```\nopen forever\n# x").len(), 1);
    }

    #[test]
    fn window_matches_full_highlight() {
        let full = highlight(DOC);
        let blocks = verbatim_blocks(DOC);
        // Every line-aligned window must agree with the full parse.
        let starts: Vec<usize> = std::iter::once(0).chain(DOC.match_indices('\n').map(|(i, _)| i + 1)).filter(|&i| i < DOC.len()).collect();
        for (i, &s) in starts.iter().enumerate() {
            let e = starts.get(i + 2).copied().unwrap_or(DOC.len());
            let w = s..e;
            assert_eq!(
                restrict(&highlight_window(DOC, w.clone(), &blocks), &w),
                restrict(&full, &w),
                "window {:?} = {:?}",
                w,
                &DOC[w.clone()]
            );
        }
    }
}
