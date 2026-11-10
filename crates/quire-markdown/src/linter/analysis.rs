// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the linter needs to know about a note: where each line starts, which
//! lines are code, math, HTML or front matter (never touched), and where
//! headings, list items and emphasis are. It all comes from the same
//! `CommonMark` parse the rest of Quire uses, so "inside a code block" means
//! the same thing everywhere.

use std::collections::HashSet;
use std::ops::Range;

use pulldown_cmark::{Event, MetadataBlockKind, Parser, Tag, TagEnd};

/// One line: bytes `start..end` are its text, `end..next` its line ending.
#[derive(Debug, Clone, Copy)]
pub(super) struct Line {
    pub start: usize,
    pub end: usize,
    pub next: usize,
}

pub(super) fn split_lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        let (end, next) = match rest.find('\n') {
            Some(i) => {
                let e = if i > 0 && rest.as_bytes()[i - 1] == b'\r' { i - 1 } else { i };
                (start + e, start + i + 1)
            }
            None => (text.len(), text.len()),
        };
        out.push(Line { start, end, next });
        start = next;
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Heading {
    pub first: usize,
    pub last: usize,
    pub level: u8,
    pub setext: bool,
    /// Not inside a block quote or a list item.
    pub top: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Item {
    pub line: usize,
    /// Byte offset of the marker.
    pub marker: usize,
    pub ordered: bool,
    pub last: usize,
    /// Index of the enclosing item, if any.
    pub parent: Option<usize>,
    /// Only items and lists around it (no block quote): safe to re-indent.
    pub plain: bool,
    /// The list this item belongs to sits next to another bullet list, so
    /// changing its marker could merge the two.
    pub list_adjacent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Heading,
    Code,
    List,
    Other,
}

/// A block directly in the document (not in a quote or an item).
#[derive(Debug, Clone, Copy)]
pub(super) struct Block {
    pub kind: Kind,
    pub first: usize,
    pub last: usize,
}

#[derive(Debug, Clone)]
pub(super) struct Emph {
    pub range: Range<usize>,
    pub strong: bool,
}

pub(super) struct Analysis {
    pub lines: Vec<Line>,
    /// Code, HTML, display math and front matter lines.
    pub protected: Vec<bool>,
    /// First and last line of well-formed front matter.
    pub front: Option<(usize, usize)>,
    pub headings: Vec<Heading>,
    pub items: Vec<Item>,
    pub blocks: Vec<Block>,
    /// Lines that end in a hard break (two or more spaces).
    pub hard_breaks: HashSet<usize>,
    pub emphasis: Vec<Emph>,
    /// Inline code, inline math, inline HTML and link destinations.
    pub inline_protected: Vec<Range<usize>>,
}

impl Analysis {
    pub(super) fn line_of(&self, byte: usize) -> usize {
        self.lines.partition_point(|l| l.next <= byte).min(self.lines.len().saturating_sub(1))
    }

    /// The line a range's last byte is on (a trailing newline belongs to its line).
    fn last_line(&self, text: &str, r: &Range<usize>) -> usize {
        let mut end = r.end.max(r.start + 1) - 1;
        // Ranges often end after the newline; a CR before it is part of the ending.
        while end > r.start && matches!(text.as_bytes().get(end), Some(b'\n' | b'\r')) {
            end -= 1;
        }
        self.line_of(end)
    }

    pub(super) fn in_inline_protected(&self, byte: usize) -> bool {
        let i = self.inline_protected.partition_point(|r| r.end <= byte);
        self.inline_protected.get(i).is_some_and(|r| r.start <= byte)
    }
}

pub(super) fn options() -> pulldown_cmark::Options {
    crate::options()
}

// Containers: (is_item_or_quote, last block kind inside it, last list index in it).
struct Container {
    item: Option<usize>,
    quote: bool,
    last_list: Option<(usize, bool)>,
    last_was_list: bool,
}

pub(super) fn analyze(text: &str) -> Analysis {
    let lines = split_lines(text);
    let mut a = Analysis {
        protected: vec![false; lines.len()],
        lines,
        front: None,
        headings: Vec::new(),
        items: Vec::new(),
        blocks: Vec::new(),
        hard_breaks: HashSet::new(),
        emphasis: Vec::new(),
        inline_protected: Vec::new(),
    };
    let protect = |a: &mut Analysis, r: &Range<usize>| {
        let (f, l) = (a.line_of(r.start), a.last_line(text, r));
        for p in &mut a.protected[f..=l.max(f).min(a.lines.len() - 1)] {
            *p = true;
        }
    };
    if a.lines.is_empty() {
        return a;
    }

    let mut stack = vec![Container { item: None, quote: false, last_list: None, last_was_list: false }];
    // For each list (in order): item indices that belong to it.
    let mut list_items: Vec<Vec<usize>> = Vec::new();
    let mut list_stack: Vec<usize> = Vec::new();
    let mut heading_start: Option<(Range<usize>, u8)> = None;

    let depth0 = |s: &[Container]| s.len() == 1;
    for (ev, range) in Parser::new_ext(text, options()).into_offset_iter() {
        match ev {
            Event::Start(tag) => {
                let top = depth0(&stack);
                let block = |kind: Kind, a: &mut Analysis, stack: &mut Vec<Container>| {
                    if top {
                        a.blocks.push(Block { kind, first: a.line_of(range.start), last: a.last_line(text, &range) });
                    }
                    if let Some(c) = stack.last_mut() {
                        c.last_was_list = false;
                    }
                };
                match tag {
                    Tag::MetadataBlock(MetadataBlockKind::YamlStyle) => {
                        let (f, l) = (a.line_of(range.start), a.last_line(text, &range));
                        if range.start == 0 {
                            a.front = Some((f, l));
                        }
                        protect(&mut a, &range);
                    }
                    Tag::MetadataBlock(_) | Tag::HtmlBlock => protect(&mut a, &range),
                    Tag::CodeBlock(_) => {
                        // Blank lines at the end of an unclosed fence are code too.
                        let (f, l) = (a.line_of(range.start), a.line_of(range.end.max(1) - 1));
                        for p in &mut a.protected[f..=l.max(f)] {
                            *p = true;
                        }
                        block(Kind::Code, &mut a, &mut stack);
                        if top && let Some(b) = a.blocks.last_mut() {
                            b.last = l.max(f);
                        }
                    }
                    Tag::Heading { level, .. } => {
                        heading_start = Some((range.clone(), level as u8));
                        block(Kind::Heading, &mut a, &mut stack);
                    }
                    Tag::List(first) => {
                        let bullet = first.is_none();
                        let idx = list_items.len();
                        list_items.push(Vec::new());
                        list_stack.push(idx);
                        // A bullet list right after another bullet list in the
                        // same container (nothing between them).
                        if let Some(c) = stack.last_mut() {
                            if let (true, Some((prev, prev_bullet))) = (c.last_was_list, c.last_list)
                                && prev_bullet
                                && bullet
                            {
                                // Mark both as adjacent through a sentinel: items of
                                // both lists get the flag below.
                                list_items[idx].push(usize::MAX);
                                list_items[prev].push(usize::MAX);
                            }
                            c.last_list = Some((idx, bullet));
                        }
                        block(Kind::List, &mut a, &mut stack);
                        if let Some(c) = stack.last_mut() {
                            c.last_was_list = true;
                        }
                    }
                    Tag::Item => {
                        // The parser's range starts after the parent's indent (or at the line break before
                        // a tab-indented item); the marker comes after the rest.
                        let marker = range.start + text[range.start..].len() - text[range.start..].trim_start().len();
                        let line = a.line_of(marker);
                        let parent = stack.iter().rev().find_map(|c| c.item);
                        let plain = !stack.iter().any(|c| c.quote);
                        let ordered = text[marker..].starts_with(|c: char| c.is_ascii_digit());
                        let idx = a.items.len();
                        a.items.push(Item { line, marker, ordered, last: a.last_line(text, &range), parent, plain, list_adjacent: false });
                        if let Some(l) = list_stack.last() {
                            list_items[*l].push(idx);
                        }
                        stack.push(Container { item: Some(idx), quote: false, last_list: None, last_was_list: false });
                    }
                    Tag::BlockQuote(_) => {
                        block(Kind::Other, &mut a, &mut stack);
                        stack.push(Container { item: None, quote: true, last_list: None, last_was_list: false });
                    }
                    Tag::Emphasis => a.emphasis.push(Emph { range: range.clone(), strong: false }),
                    Tag::Strong => a.emphasis.push(Emph { range: range.clone(), strong: true }),
                    Tag::Link { .. } => {
                        if let Some(off) = text.get(range.clone()).and_then(|r| r.rfind("](")) {
                            a.inline_protected.push(range.start + off + 1..range.end);
                        }
                    }
                    Tag::FootnoteDefinition(_) | Tag::DefinitionListDefinition => {
                        block(Kind::Other, &mut a, &mut stack);
                        // Lists inside keep their indentation: like a quote, not ours to move.
                        stack.push(Container { item: None, quote: true, last_list: None, last_was_list: false });
                    }
                    Tag::Paragraph | Tag::Table(_) | Tag::DefinitionList => {
                        block(Kind::Other, &mut a, &mut stack);
                    }
                    _ => {}
                }
            }
            Event::End(end) => match end {
                TagEnd::Item | TagEnd::BlockQuote(_) | TagEnd::FootnoteDefinition | TagEnd::DefinitionListDefinition => {
                    stack.pop();
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    if let Some(c) = stack.last_mut() {
                        c.last_was_list = true;
                    }
                }
                TagEnd::Heading(_) => {
                    if let Some((r, level)) = heading_start.take() {
                        let first = a.line_of(r.start);
                        let last = a.last_line(text, &range);
                        let setext = last > first;
                        a.headings.push(Heading { first, last, level, setext, top: depth0(&stack) });
                    }
                }
                _ => {}
            },
            Event::InlineMath(_) | Event::Code(_) | Event::InlineHtml(_) => a.inline_protected.push(range),
            Event::DisplayMath(_) | Event::Html(_) => protect(&mut a, &range),
            // The break is the trailing spaces before the newline; a backslash break has no
            // spaces and isn't ours to touch.
            Event::HardBreak if text.get(range.clone()).is_some_and(|t| t.starts_with("  ")) => {
                a.hard_breaks.insert(a.line_of(range.start));
            }
            _ => {}
        }
    }
    // Adjacent bullet lists: flag the items of both.
    for items in &list_items {
        if items.contains(&usize::MAX) {
            for &i in items.iter().filter(|i| **i != usize::MAX) {
                a.items[i].list_adjacent = true;
            }
        }
    }
    a.inline_protected.sort_by_key(|r| r.start);
    a
}

/// The whitespace-insensitive shape of a note, for checking that a rule
/// changed only whitespace and markup: the parser's events with positions
/// dropped, text with its whitespace removed (code, math and HTML kept
/// exactly) and front matter left out. `levels`: whether heading levels
/// count (they don't for the rules that change them on purpose).
pub(super) fn signature(text: &str, levels: bool) -> Vec<String> {
    // Line endings aren't part of the shape (the parser splits CRLF text differently).
    let text = &text.replace("\r\n", "\n");
    let mut out = Vec::new();
    let mut run = String::new();
    let mut in_code = false;
    let mut in_front = false;
    let flush = |run: &mut String, out: &mut Vec<String>| {
        if !run.is_empty() {
            out.push(format!("T:{run}"));
            run.clear();
        }
    };
    for ev in Parser::new_ext(text, options()) {
        match ev {
            Event::Start(Tag::MetadataBlock(_)) => in_front = true,
            Event::End(TagEnd::MetadataBlock(_)) => in_front = false,
            _ if in_front => {}
            Event::Text(t) if in_code => {
                flush(&mut run, &mut out);
                out.push(format!("X:{t}"));
            }
            Event::Text(t) => run.extend(t.chars().filter(|c| !c.is_whitespace())),
            Event::Start(Tag::CodeBlock(k)) => {
                flush(&mut run, &mut out);
                in_code = true;
                out.push(format!("{k:?}"));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                out.push("E:code".into());
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut run, &mut out);
                out.push(if levels { format!("H{}", level as u8) } else { "H".into() });
            }
            Event::End(TagEnd::Heading(level)) => {
                flush(&mut run, &mut out);
                out.push(if levels { format!("/H{}", level as u8) } else { "/H".into() });
            }
            other => {
                flush(&mut run, &mut out);
                out.push(format!("{other:?}"));
            }
        }
    }
    flush(&mut run, &mut out);
    out
}
