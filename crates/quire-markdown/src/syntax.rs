// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live-highlighting: turn Markdown source into flat, non-overlapping
//! [`StyleRun`]s that a UI can paint directly.
//!
//! Spans from the `CommonMark` parser nest (a link inside bold inside a
//! heading); a sweep over span boundaries flattens them into runs whose
//! [`Style`] is the union of every span covering that byte range.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::scan::{Inline, scan_with, verbatim_ranges};

/// Combined style for a run of text. Plain data so frontends map it to fonts/colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Style {
    /// 1–6 for headings, 0 otherwise.
    pub heading: u8,
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub code: bool,
    pub code_block: bool,
    pub link: bool,
    pub wikilink: bool,
    pub tag: bool,
    pub highlight: bool,
    pub quote: bool,
    pub task_done: bool,
    pub math: bool,
    pub front_matter: bool,
    /// Syntax punctuation (`#`, `**`, `](url)`, list bullets…) — usually dimmed.
    pub marker: bool,
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRun {
    pub range: Range<usize>,
    pub style: Style,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Heading(u8),
    Strong,
    Emphasis,
    Strike,
    Code,
    CodeBlock,
    Link,
    WikiLink,
    Tag,
    Highlight,
    Quote,
    TaskDone,
    Math,
    FrontMatter,
    Marker,
}

impl Kind {
    fn slot(self) -> usize {
        match self {
            Self::Heading(_) => 0,
            Self::Strong => 1,
            Self::Emphasis => 2,
            Self::Strike => 3,
            Self::Code => 4,
            Self::CodeBlock => 5,
            Self::Link => 6,
            Self::WikiLink => 7,
            Self::Tag => 8,
            Self::Highlight => 9,
            Self::Quote => 10,
            Self::TaskDone => 11,
            Self::Math => 12,
            Self::FrontMatter => 13,
            Self::Marker => 14,
        }
    }
}

const SLOTS: usize = 15;

struct Collector<'a> {
    src: &'a str,
    spans: Vec<(Range<usize>, Kind)>,
}

impl Collector<'_> {
    fn push(&mut self, r: Range<usize>, k: Kind) {
        if r.start < r.end && r.end <= self.src.len() {
            self.spans.push((r, k));
        }
    }

    /// Mark a literal prefix (e.g. `**`) at `at` and the matching suffix before `end`.
    fn mark_delims(&mut self, range: &Range<usize>, open: usize, close: usize) {
        self.push(range.start..range.start + open, Kind::Marker);
        if range.end >= close {
            self.push(range.end - close..range.end, Kind::Marker);
        }
    }
}

/// Compute style runs for `src`. Unstyled gaps are omitted.
pub fn highlight(src: &str) -> Vec<StyleRun> {
    let mut c = Collector { src, spans: Vec::with_capacity(src.len() / 16) };
    let mut item_stack: Vec<Range<usize>> = Vec::new();

    for (ev, range) in Parser::new_ext(src, crate::options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                let lvl = level as u8;
                c.push(range.clone(), Kind::Heading(lvl));
                let text = &src[range.clone()];
                let hashes = text.bytes().take_while(|&b| b == b'#').count();
                if hashes > 0 {
                    let ws = text[hashes..].bytes().take_while(|&b| b == b' ' || b == b'\t').count();
                    c.push(range.start..range.start + hashes + ws, Kind::Marker);
                } else if let Some(nl) = text.trim_end().rfind('\n') {
                    // Setext underline.
                    c.push(range.start + nl..range.end, Kind::Marker);
                }
            }
            Event::Start(Tag::Strong) => {
                c.push(range.clone(), Kind::Strong);
                c.mark_delims(&range, 2, 2);
            }
            Event::Start(Tag::Emphasis) => {
                c.push(range.clone(), Kind::Emphasis);
                c.mark_delims(&range, 1, 1);
            }
            Event::Start(Tag::Strikethrough) => {
                c.push(range.clone(), Kind::Strike);
                let n = src[range.clone()].bytes().take_while(|&b| b == b'~').count();
                c.mark_delims(&range, n, n);
            }
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                c.push(range.clone(), Kind::Link);
                let text = &src[range.clone()];
                if let Some(off) = text.rfind("](") {
                    let open = if text.starts_with('!') { 2 } else { 1 };
                    c.push(range.start..range.start + open, Kind::Marker);
                    c.push(range.start + off..range.end, Kind::Marker);
                } else if text.starts_with('<') {
                    c.mark_delims(&range, 1, 1);
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                c.push(range.clone(), Kind::CodeBlock);
                if matches!(kind, CodeBlockKind::Fenced(_)) {
                    let text = &src[range.clone()];
                    let first = text.find('\n').map_or(text.len(), |n| n + 1);
                    c.push(range.start..range.start + first, Kind::Marker);
                    let trimmed = text.trim_end_matches(['\n', '\r']);
                    if let Some(last_nl) = trimmed.rfind('\n') {
                        let tail = trimmed[last_nl + 1..].trim_start();
                        if tail.starts_with("```") || tail.starts_with("~~~") {
                            c.push(range.start + last_nl + 1..range.start + trimmed.len(), Kind::Marker);
                        }
                    }
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                c.push(range.clone(), Kind::Quote);
                // Mark each line's leading `>` run.
                let mut line_start = range.start;
                for line in src[range.clone()].split_inclusive('\n') {
                    let lead: usize = line.bytes().take_while(|&b| b == b'>' || b == b' ').count();
                    if line.trim_start().starts_with('>') {
                        c.push(line_start..line_start + lead, Kind::Marker);
                    }
                    line_start += line.len();
                }
            }
            Event::Start(Tag::Item) => {
                let text = &src[range.clone()];
                let indent = text.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
                let rest = &text[indent..];
                let marker_len = if rest.starts_with(['-', '*', '+']) {
                    1
                } else {
                    let d = rest.bytes().take_while(u8::is_ascii_digit).count();
                    if d > 0 && rest[d..].starts_with(['.', ')']) { d + 1 } else { 0 }
                };
                if marker_len > 0 {
                    c.push(range.start + indent..range.start + indent + marker_len, Kind::Marker);
                }
                item_stack.push(range);
            }
            Event::End(TagEnd::Item) => {
                item_stack.pop();
            }
            Event::TaskListMarker(done) => {
                c.push(range.clone(), Kind::Marker);
                if done && let Some(item) = item_stack.last() {
                    // Strike the item's own first line, not nested children.
                    let line_end = src[range.end..item.end].find('\n').map_or(item.end, |n| range.end + n);
                    c.push(range.end..line_end, Kind::TaskDone);
                }
            }
            Event::Code(_) => {
                c.push(range.clone(), Kind::Code);
                let n = src[range.clone()].bytes().take_while(|&b| b == b'`').count();
                c.mark_delims(&range, n, n);
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => c.push(range, Kind::Math),
            Event::Start(Tag::MetadataBlock(_)) => c.push(range, Kind::FrontMatter),
            Event::Rule | Event::InlineHtml(_) | Event::Html(_) => c.push(range, Kind::Marker),
            Event::FootnoteReference(_) => c.push(range, Kind::Link),
            _ => {}
        }
    }

    let verbatim = verbatim_ranges(src);
    for inline in scan_with(src, &verbatim) {
        match inline {
            Inline::WikiLink { range, link } => {
                c.push(range.clone(), Kind::WikiLink);
                let open = if link.embed { 3 } else { 2 };
                c.mark_delims(&range, open, 2);
                if link.alias.is_some()
                    && let Some(bar) = src[range.clone()].find('|')
                {
                    // Hide the target, show the alias.
                    c.push(range.start + open..range.start + bar + 1, Kind::Marker);
                }
            }
            Inline::Tag { range, .. } => c.push(range, Kind::Tag),
            Inline::Highlight { range } => {
                c.push(range.clone(), Kind::Highlight);
                c.mark_delims(&range, 2, 2);
            }
        }
    }

    flatten(c.spans)
}

fn flatten(spans: Vec<(Range<usize>, Kind)>) -> Vec<StyleRun> {
    if spans.is_empty() {
        return Vec::new();
    }
    // (position, is_start, kind). Ends sort before starts at the same position.
    let mut events: Vec<(usize, bool, Kind)> = Vec::with_capacity(spans.len() * 2);
    for (r, k) in spans {
        events.push((r.start, true, k));
        events.push((r.end, false, k));
    }
    events.sort_by_key(|&(pos, is_start, _)| (pos, is_start));

    let mut counts = [0u32; SLOTS];
    let mut heading_levels: Vec<u8> = Vec::new();
    let mut runs: Vec<StyleRun> = Vec::new();
    let mut last = events[0].0;

    for (pos, is_start, kind) in events {
        if pos > last {
            let style = style_from(&counts, &heading_levels);
            if !style.is_plain() {
                match runs.last_mut() {
                    Some(prev) if prev.range.end == last && prev.style == style => prev.range.end = pos,
                    _ => runs.push(StyleRun { range: last..pos, style }),
                }
            }
            last = pos;
        }
        let slot = kind.slot();
        if is_start {
            counts[slot] += 1;
            if let Kind::Heading(l) = kind {
                heading_levels.push(l);
            }
        } else {
            counts[slot] = counts[slot].saturating_sub(1);
            if let Kind::Heading(_) = kind {
                heading_levels.pop();
            }
        }
    }
    runs
}

fn style_from(c: &[u32; SLOTS], headings: &[u8]) -> Style {
    Style {
        heading: headings.last().copied().unwrap_or(0),
        strong: c[1] > 0,
        emphasis: c[2] > 0,
        strike: c[3] > 0,
        code: c[4] > 0,
        code_block: c[5] > 0,
        link: c[6] > 0,
        wikilink: c[7] > 0,
        tag: c[8] > 0,
        highlight: c[9] > 0,
        quote: c[10] > 0,
        task_done: c[11] > 0,
        math: c[12] > 0,
        front_matter: c[13] > 0,
        marker: c[14] > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_at(runs: &[StyleRun], pos: usize) -> Style {
        runs.iter().find(|r| r.range.contains(&pos)).map(|r| r.style).unwrap_or_default()
    }

    #[test]
    fn heading_and_markers() {
        let src = "## Hello **world**\n";
        let runs = highlight(src);
        let h = style_at(&runs, 3);
        assert_eq!(h.heading, 2);
        assert!(!h.marker);
        assert!(style_at(&runs, 0).marker);
        let w = style_at(&runs, src.find("world").unwrap());
        assert!(w.strong && w.heading == 2);
        assert!(style_at(&runs, src.find("**").unwrap()).marker);
    }

    #[test]
    fn runs_are_sorted_and_disjoint() {
        let src = "# T\n\n- [x] done *it*\n- [ ] todo `code` [[Link|alias]] #tag\n\n> quote ==hi==\n\n```rust\nfn x() {}\n```\n";
        let runs = highlight(src);
        for w in runs.windows(2) {
            assert!(w[0].range.end <= w[1].range.start, "{w:?}");
        }
        assert!(style_at(&runs, src.find("done").unwrap()).task_done);
        assert!(!style_at(&runs, src.find("todo").unwrap()).task_done);
        assert!(style_at(&runs, src.find("alias").unwrap()).wikilink);
        assert!(style_at(&runs, src.find("Link").unwrap()).marker);
        assert!(style_at(&runs, src.find("tag").unwrap()).tag);
        assert!(style_at(&runs, src.find("hi=").unwrap()).highlight);
        assert!(style_at(&runs, src.find("fn x").unwrap()).code_block);
        assert!(style_at(&runs, src.find("quote").unwrap()).quote);
    }
}
