// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Formatting for the toolbar and the Format commands (0.11).
//!
//! Every function works on all selections at once (multiple cursors) and
//! returns one [`Transaction`] with the selections after it, so a button
//! press is one undo step. The toggles read the Markdown around each
//! selection: bolding bold text unbolds it, a caret inside `**word**`
//! unbolds the word, and a caret inside a plain word bolds the word.
//! Text is only ever wrapped or unwrapped, never dropped. Positions are
//! chars.

use quire_text::{Assoc, Buffer, Change, Selection, Transaction, multi};

/// The edit and the selections after it (primary not singled out).
pub type Edit = (Transaction, Vec<Selection>);

/// Inline formatting that wraps text in markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Strike,
    Highlight,
    Code,
    Math,
}

impl Inline {
    /// The marker Quire writes.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "_",
            Self::Strike => "~~",
            Self::Highlight => "==",
            Self::Code => "`",
            Self::Math => "$",
        }
    }
}

/// Line formatting (lists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Bullet,
    Numbered,
}

/// Where a selection end lands: `pos` in the old text mapped through the
/// edit (`assoc` decides the side of an insertion there), plus `offset`.
#[derive(Debug, Clone, Copy)]
struct Target {
    pos: usize,
    assoc: Assoc,
    offset: usize,
}

const fn at(pos: usize, assoc: Assoc) -> Target {
    Target { pos, assoc, offset: 0 }
}

const fn plus(pos: usize, assoc: Assoc, offset: usize) -> Target {
    Target { pos, assoc, offset }
}

/// Changes for all selections, and where each selection goes.
#[derive(Default)]
struct Plan {
    changes: Vec<Change>,
    sels: Vec<(Target, Target)>,
}

impl Plan {
    /// Add one selection's changes, unless they overlap changes already
    /// taken (two carets in the same word): then only the selection moves.
    fn push(&mut self, changes: Vec<Change>, anchor: Target, head: Target) {
        let clash = changes.iter().any(|c| {
            self.changes.iter().any(|o| {
                let same = o.start == c.start && o.end == c.end && o.text == c.text;
                !same && c.start < o.end.max(o.start + 1) && o.start < c.end.max(c.start + 1) && !(c.start == c.end && o.start == o.end)
            })
        });
        if !clash {
            for c in changes {
                if !self.changes.iter().any(|o| o.start == c.start && o.end == c.end && o.text == c.text) {
                    self.changes.push(c);
                }
            }
        }
        self.sels.push((anchor, head));
    }

    fn finish(self) -> Option<Edit> {
        let tx = Transaction::new(self.changes).ok()?;
        if tx.is_empty() {
            return None;
        }
        let map = |t: Target| tx.map_pos(t.pos, t.assoc) + t.offset;
        let sels = self.sels.into_iter().map(|(a, h)| Selection::new(map(a), map(h))).collect();
        Some((tx, multi::normalize(sels)))
    }
}

/// A selection keeps its direction: (anchor target, head target) from
/// (start target, end target).
const fn directed(sel: Selection, start: Target, end: Target) -> (Target, Target) {
    if sel.head >= sel.anchor { (start, end) } else { (end, start) }
}

fn chars_of(buf: &Buffer, line: usize) -> Vec<char> {
    buf.line(line).chars().collect()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-'
}

// ------------------------------------------------------------ inline spans

/// A marked span on a line: opening marker at `open..open + len`, closing
/// at `close..close + len` (line columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    open: usize,
    close: usize,
    len: usize,
}

impl Span {
    const fn content(&self) -> (usize, usize) {
        (self.open + self.len, self.close)
    }
    const fn end(&self) -> usize {
        self.close + self.len
    }
}

/// Spans of `kind` on a line. Bold and italic read runs of `*` and `_`:
/// a run of 2 or more holds bold, an odd run holds italic (`***` is both).
fn spans(line: &[char], kind: Inline) -> Vec<Span> {
    match kind {
        Inline::Bold | Inline::Italic => {
            let k = if kind == Inline::Bold { 2 } else { 1 };
            let mut out = Vec::new();
            for ch in ['*', '_'] {
                let mut tokens = Vec::new();
                let mut i = 0;
                while i < line.len() {
                    if line[i] != ch {
                        i += 1;
                        continue;
                    }
                    let start = i;
                    while i < line.len() && line[i] == ch {
                        i += 1;
                    }
                    let len = i - start;
                    let before = start.checked_sub(1).map(|j| line[j]);
                    let after = line.get(i).copied();
                    // `* item` at the start of a line is a bullet, `***` alone a rule.
                    let at_start = line[..start].iter().all(|c| *c == ' ' || *c == '\t');
                    if at_start && (after == Some(' ') || after.is_none()) {
                        continue;
                    }
                    // snake_case: underscores inside a word aren't emphasis.
                    if ch == '_' && before.is_some_and(char::is_alphanumeric) && after.is_some_and(char::is_alphanumeric) {
                        continue;
                    }
                    let holds = if k == 2 { len >= 2 } else { len % 2 == 1 };
                    if holds {
                        tokens.push((start, len));
                    }
                }
                for pair in tokens.chunks(2) {
                    if let [(os, ol), (cs, _)] = *pair {
                        out.push(Span { open: os + ol - k, close: cs, len: k });
                    }
                }
            }
            out.sort_by_key(|s| s.open);
            out
        }
        _ => {
            let m: Vec<char> = kind.marker().chars().collect();
            let mut found = Vec::new();
            let mut i = 0;
            while i + m.len() <= line.len() {
                if line[i..i + m.len()] == m[..] {
                    found.push(i);
                    i += m.len();
                } else {
                    i += 1;
                }
            }
            found.chunks(2).filter_map(|p| if let [o, c] = *p { Some(Span { open: o, close: c, len: m.len() }) } else { None }).collect()
        }
    }
}

/// Columns where a line's text starts after list, task, quote and heading markers.
fn content_start(line: &[char]) -> usize {
    let s: String = line.iter().collect();
    let mut col = 0;
    let mut rest = s.as_str();
    loop {
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let skipped = rest.len() - trimmed.len();
        let mut next = None;
        if let Some(r) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix('>')) {
            next = Some(r);
        } else if let Some(p) = crate::editing::parse_prefix(trimmed) {
            let bytes: usize = trimmed.chars().take(p.len).map(char::len_utf8).sum();
            next = Some(&trimmed[bytes..]);
        } else if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if hashes <= 6 && trimmed[hashes..].starts_with(' ') {
                next = Some(&trimmed[hashes + 1..]);
            }
        }
        match next {
            Some(r) if r.len() < trimmed.len() => {
                col += rest[..skipped].chars().count() + trimmed[..trimmed.len() - r.len()].chars().count();
                rest = r;
            }
            _ => return col,
        }
    }
}

/// One line's part of an inline toggle: the span around `a..b` (columns),
/// if there is one.
fn span_around(line: &[char], kind: Inline, a: usize, b: usize) -> Option<Span> {
    spans(line, kind).into_iter().find(|s| {
        let (cs, ce) = s.content();
        (cs <= a && b <= ce) || (s.open == a && s.end() == b) || (a == b && s.open < a && a < s.end())
    })
}

/// Toggle inline formatting on every selection.
pub fn toggle_inline(buf: &Buffer, sels: &[Selection], kind: Inline) -> Option<Edit> {
    let mut plan = Plan::default();
    let m = kind.marker();
    let ml = m.chars().count();
    for sel in multi::normalize(sels.to_vec()) {
        let (s, e) = (sel.start(), sel.end());
        let (l1, l2) = (buf.char_to_line(s), buf.char_to_line(e));
        if l1 != l2 && matches!(kind, Inline::Code | Inline::Math) {
            let fence = if kind == Inline::Code { "```" } else { "$$" };
            let (changes, a, h) = fence_block(buf, sel, fence);
            plan.push(changes, a, h);
            continue;
        }
        if l1 != l2 {
            inline_lines(buf, sel, kind, &mut plan);
            continue;
        }
        let ls = buf.line_to_char(l1);
        let line = chars_of(buf, l1);
        let (a, b) = (s - ls, e - ls);
        // Unwrap the span the selection is in (or is).
        if let Some(sp) = span_around(&line, kind, a, b) {
            let changes = vec![Change::new(ls + sp.open, ls + sp.open + sp.len, ""), Change::new(ls + sp.close, ls + sp.end(), "")];
            let (cs, ce) = sp.content();
            let (ns, ne) = if s == e { (s.clamp(ls + cs, ls + ce), s.clamp(ls + cs, ls + ce)) } else { (s.max(ls + cs), e.min(ls + ce)) };
            let (an, hd) = directed(sel, at(ns, Assoc::After), at(ne, Assoc::Before));
            plan.push(changes, an, hd);
            continue;
        }
        if s == e {
            // Between an empty pair (`****`): take the pair out.
            if a >= ml
                && line.get(a - ml..a).is_some_and(|x| x.iter().copied().eq(m.chars()))
                && line.get(a..a + ml).is_some_and(|x| x.iter().copied().eq(m.chars()))
            {
                plan.push(vec![Change::new(s - ml, s + ml, "")], at(s - ml, Assoc::Before), at(s - ml, Assoc::Before));
                continue;
            }
            // In a word: the word.
            let mut ws = a;
            while ws > 0 && is_word(line[ws - 1]) {
                ws -= 1;
            }
            let mut we = a;
            while we < line.len() && is_word(line[we]) {
                we += 1;
            }
            if ws < we {
                let changes = vec![Change::new(ls + ws, ls + ws, m), Change::new(ls + we, ls + we, m)];
                let t = if a == we { at(s, Assoc::Before) } else { at(s, Assoc::After) };
                plan.push(changes, t, t);
            } else {
                let changes = vec![Change::new(s, s, m.repeat(2))];
                let t = plus(s, Assoc::Before, ml);
                plan.push(changes, t, t);
            }
            continue;
        }
        // Wrap, leaving out spaces at the edges (`**word** ` not `**word **`).
        let mut ta = a;
        let mut tb = b;
        while ta < tb && line[ta].is_whitespace() {
            ta += 1;
        }
        while tb > ta && line[tb - 1].is_whitespace() {
            tb -= 1;
        }
        if ta == tb {
            let changes = vec![Change::new(s, s, m.repeat(2))];
            let t = plus(s, Assoc::Before, ml);
            plan.push(changes, t, t);
            continue;
        }
        let changes = vec![Change::new(ls + ta, ls + ta, m), Change::new(ls + tb, ls + tb, m)];
        let (an, hd) = directed(sel, at(ls + ta, Assoc::After), at(ls + tb, Assoc::Before));
        plan.push(changes, an, hd);
    }
    plan.finish()
}

/// Inline formatting over several lines: each line's text is wrapped on
/// its own (Markdown emphasis doesn't cross paragraphs). All wrapped
/// already: unwrap them all.
fn inline_lines(buf: &Buffer, sel: Selection, kind: Inline, plan: &mut Plan) {
    let m = kind.marker();
    let (s, e) = (sel.start(), sel.end());
    let mut segments = Vec::new();
    for l in buf.char_to_line(s)..=buf.char_to_line(e) {
        let ls = buf.line_to_char(l);
        let line = chars_of(buf, l);
        let mut a = s.saturating_sub(ls).min(line.len()).max(content_start(&line));
        let mut b = e.saturating_sub(ls).min(line.len());
        while a < b && line[a].is_whitespace() {
            a += 1;
        }
        while b > a && line[b - 1].is_whitespace() {
            b -= 1;
        }
        if a < b {
            segments.push((ls, a, b, span_around(&line, kind, a, b)));
        }
    }
    if segments.is_empty() {
        return;
    }
    let unwrap = segments.iter().all(|x| x.3.is_some());
    let mut changes = Vec::new();
    for (ls, a, b, sp) in segments {
        match sp {
            Some(sp) if unwrap => {
                changes.push(Change::new(ls + sp.open, ls + sp.open + sp.len, ""));
                changes.push(Change::new(ls + sp.close, ls + sp.end(), ""));
            }
            Some(_) => {}
            None => {
                changes.push(Change::new(ls + a, ls + a, m));
                changes.push(Change::new(ls + b, ls + b, m));
            }
        }
    }
    let (an, hd) = directed(sel, at(s, Assoc::After), at(e, Assoc::Before));
    plan.push(changes, an, hd);
}

/// The whole lines a selection touches (a selection ending at column 0
/// leaves that line out).
fn lines_of(buf: &Buffer, sel: Selection) -> std::ops::RangeInclusive<usize> {
    let first = buf.char_to_line(sel.start());
    let mut last = buf.char_to_line(sel.end());
    if last > first && sel.end() == buf.line_to_char(last) {
        last -= 1;
    }
    first..=last
}

/// Wrap whole lines in a fence (```` ``` ```` or `$$`), or take the fences
/// away when the lines are fenced already.
fn fence_block(buf: &Buffer, sel: Selection, fence: &str) -> (Vec<Change>, Target, Target) {
    let lines = lines_of(buf, sel);
    let (first, last) = (*lines.start(), *lines.end());
    let is_fence = |l: usize| buf.line(l).trim_start().starts_with(fence);
    if first < last && is_fence(first) && is_fence(last) {
        // Fences selected: remove both lines.
        let a = buf.line_to_char(first);
        let a_end = buf.line_to_char(first + 1);
        let z = buf.line_range(last);
        let z_start = if last > 0 { buf.line_range(last - 1).end } else { z.start };
        let changes = vec![Change::new(a, a_end, ""), Change::new(z_start, z.end, "")];
        return (changes, at(a_end, Assoc::After), at(z_start, Assoc::Before));
    }
    if first > 0 && last + 1 < buf.len_lines() && is_fence(first - 1) && is_fence(last + 1) && !is_fence(first) {
        let a = buf.line_to_char(first - 1);
        let changes =
            vec![Change::new(a, buf.line_to_char(first), ""), Change::new(buf.line_range(last).end, buf.line_range(last + 1).end, "")];
        let (an, hd) = directed(sel, at(sel.start(), Assoc::After), at(sel.end(), Assoc::Before));
        return (changes, an, hd);
    }
    let start = buf.line_to_char(first);
    let end = buf.line_range(last).end;
    let changes = vec![Change::new(start, start, format!("{fence}\n")), Change::new(end, end, format!("\n{fence}"))];
    if start == end {
        let t = plus(start, Assoc::Before, fence.chars().count() + 1);
        return (changes, t, t);
    }
    let (an, hd) = directed(sel, at(sel.start().max(start), Assoc::After), at(sel.end().min(end), Assoc::Before));
    (changes, an, hd)
}

// ------------------------------------------------------------ lines

/// Every line any selection touches, in order, once.
fn all_lines(buf: &Buffer, sels: &[Selection]) -> Vec<usize> {
    let mut v: Vec<usize> = sels.iter().flat_map(|s| lines_of(buf, *s)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Map every selection through line edits (prefixes at line starts).
fn line_edit(sels: &[Selection], changes: Vec<Change>) -> Option<Edit> {
    let mut plan = Plan { changes, sels: Vec::new() };
    for s in multi::normalize(sels.to_vec()) {
        plan.sels.push((at(s.anchor, Assoc::After), at(s.head, Assoc::After)));
    }
    plan.finish()
}

/// The lines to format: blank lines are left alone when there are others.
fn content_lines(buf: &Buffer, sels: &[Selection]) -> Vec<usize> {
    let lines = all_lines(buf, sels);
    let filled: Vec<usize> = lines.iter().copied().filter(|l| !buf.line(*l).trim().is_empty()).collect();
    if filled.is_empty() { lines } else { filled }
}

fn heading_level(line: &str) -> usize {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes > 0 && hashes <= 6 && (line[hashes..].starts_with(' ') || line.len() == hashes) { hashes } else { 0 }
}

/// Set the heading level of every selected line (0 makes paragraphs).
/// Lines all at `level` already go back to paragraphs.
pub fn set_heading(buf: &Buffer, sels: &[Selection], level: u8) -> Option<Edit> {
    let lines = content_lines(buf, sels);
    let level = usize::from(level.min(6));
    let all_there = lines.iter().all(|l| heading_level(&buf.line(*l)) == level);
    let target = if all_there { 0 } else { level };
    let mut changes = Vec::new();
    for l in lines {
        let text = buf.line(l);
        let have = heading_level(&text);
        let start = buf.line_to_char(l);
        let strip = if have > 0 { (have + 1).min(text.chars().count()) } else { 0 };
        let prefix = if target == 0 { String::new() } else { format!("{} ", "#".repeat(target)) };
        if have != target {
            changes.push(Change::new(start, start + strip, prefix));
        }
    }
    line_edit(sels, changes)
}

/// Bulleted or numbered list on every selected line, or back to plain
/// lines when they all are that list already. Tasks become bullets.
pub fn toggle_list(buf: &Buffer, sels: &[Selection], kind: List) -> Option<Edit> {
    use crate::editing::{Prefix, parse_prefix};
    let lines = content_lines(buf, sels);
    let is_kind = |l: usize| match parse_prefix(&buf.line(l)).map(|p| p.kind) {
        Some(Prefix::Bullet(_)) => kind == List::Bullet,
        Some(Prefix::Ordered { .. }) => kind == List::Numbered,
        _ => false,
    };
    let remove = lines.iter().all(|l| is_kind(*l));
    let mut changes = Vec::new();
    let mut n = 0u64;
    let mut prev: Option<usize> = None;
    for l in lines {
        let text = buf.line(l);
        let start = buf.line_to_char(l);
        let p = parse_prefix(&text).filter(|p| !matches!(p.kind, Prefix::Quote(_)));
        let indent_len = text.chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let marker_end = p.as_ref().map_or(indent_len, |p| p.len);
        if remove {
            changes.push(Change::new(start + indent_len, start + marker_end, ""));
            continue;
        }
        if prev.is_none_or(|q| q + 1 != l) {
            n = 0;
        }
        prev = Some(l);
        n += 1;
        let marker = match kind {
            List::Bullet => "- ".to_string(),
            List::Numbered => format!("{n}. "),
        };
        changes.push(Change::new(start + indent_len, start + marker_end, marker));
    }
    line_edit(sels, changes)
}

/// `- [ ]` on lines that aren't tasks; a task's box is ticked or cleared.
pub fn toggle_task(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut changes = Vec::new();
    for l in content_lines(buf, sels) {
        let start = buf.line_to_char(l);
        if let Some(tx) = crate::editing::toggle_task(buf, Selection::cursor(start)) {
            changes.extend(tx.changes().iter().cloned());
        }
    }
    line_edit(sels, changes)
}

/// `> ` on every selected line, or off when they all have it.
pub fn toggle_quote(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let lines = all_lines(buf, sels);
    let quoted = |l: usize| buf.line(l).starts_with('>');
    let remove = lines.iter().all(|l| quoted(*l));
    let changes = lines
        .into_iter()
        .map(|l| {
            let s = buf.line_to_char(l);
            let t = buf.line(l);
            if remove { Change::new(s, s + if t.starts_with("> ") { 2 } else { 1 }, "") } else { Change::new(s, s, "> ") }
        })
        .collect();
    line_edit(sels, changes)
}

/// A callout (`> [!note]`) around the selected lines, or the lines back
/// out of one when the selection starts at its first line.
pub fn toggle_callout(buf: &Buffer, sels: &[Selection], kind: &str) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let lines = lines_of(buf, sel);
        let (first, last) = (*lines.start(), *lines.end());
        // The callout the first line is in: its header is above, on quoted lines.
        let mut h = first;
        while h > 0 && buf.line(h).starts_with('>') && crate::callout::parse_header(&buf.line(h)).is_none() {
            h -= 1;
        }
        if crate::callout::parse_header(&buf.line(h)).is_some() {
            // Take the header line out and unquote the callout's lines.
            let mut changes = vec![Change::new(buf.line_to_char(h), buf.line_to_char(h + 1).min(buf.len_chars()), "")];
            let mut l = h + 1;
            while l < buf.len_lines() && buf.line(l).starts_with('>') {
                let s = buf.line_to_char(l);
                changes.push(Change::new(s, s + if buf.line(l).starts_with("> ") { 2 } else { 1 }, ""));
                l += 1;
            }
            plan.push(changes, at(sel.anchor, Assoc::After), at(sel.head, Assoc::After));
            continue;
        }
        let start = buf.line_to_char(first);
        let header = format!("> [!{kind}]\n");
        let mut changes = vec![];
        for l in lines {
            let s = buf.line_to_char(l);
            let quoted = buf.line(l).starts_with('>');
            let text = if l == first {
                format!("{header}{}", if quoted { ">" } else { "> " })
            } else {
                (if quoted { ">" } else { "> " }).to_string()
            };
            changes.push(Change::new(s, s, text));
        }
        let empty = first == last && buf.line(first).trim().is_empty();
        if empty {
            let t = plus(start, Assoc::Before, header.chars().count() + 2);
            plan.push(changes, t, t);
        } else {
            plan.push(changes, at(sel.anchor, Assoc::After), at(sel.head, Assoc::After));
        }
    }
    plan.finish()
}

// ------------------------------------------------------------ links and inserts

/// `[text](url)` spans on a line: (start, text start, text end, end) columns.
fn md_links(line: &[char]) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line[i] == '['
            && (i == 0 || line[i - 1] != '!')
            && line.get(i + 1) != Some(&'[')
            && let Some(close) = (i + 1..line.len()).find(|&j| line[j] == ']')
            && line.get(close + 1) == Some(&'(')
            && let Some(end) = (close + 2..line.len()).find(|&j| line[j] == ')')
        {
            out.push((i, i + 1, close, end + 1));
            i = end + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// `[[…]]` spans on a line (not embeds): (start, end) columns.
fn wikilinks(line: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < line.len() {
        if line[i] == '['
            && line[i + 1] == '['
            && (i == 0 || line[i - 1] != '!')
            && let Some(end) = (i + 2..line.len().saturating_sub(1)).find(|&j| line[j] == ']' && line[j + 1] == ']')
        {
            out.push((i, end + 2));
            i = end + 2;
            continue;
        }
        i += 1;
    }
    out
}

fn looks_like_url(t: &str) -> bool {
    let t = t.trim();
    (t.starts_with("http://") || t.starts_with("https://") || t.starts_with("www.") || t.starts_with("mailto:"))
        && !t.contains(char::is_whitespace)
}

/// A Markdown link: `[selection]()` with the caret in the parentheses,
/// `[](url)` for a selected URL, or the link back to its text when the
/// selection is in one.
pub fn toggle_link(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let (s, e) = (sel.start(), sel.end());
        let l = buf.char_to_line(s);
        let ls = buf.line_to_char(l);
        let line = chars_of(buf, l);
        let (a, b) = (s - ls, e.min(ls + line.len()) - ls);
        if let Some(&(st, ts, te, en)) = md_links(&line).iter().find(|(st, _, _, en)| *st <= a && b <= *en && (a < *en || a == b)) {
            let changes = vec![Change::new(ls + st, ls + ts, ""), Change::new(ls + te, ls + en, "")];
            plan.push(changes, at(ls + ts, Assoc::After), at(ls + te, Assoc::Before));
            continue;
        }
        let text = buf.slice(s..e);
        if !text.is_empty() && looks_like_url(&text) {
            let t = plus(s, Assoc::Before, 1);
            plan.push(vec![Change::new(s, e, format!("[]({})", text.trim()))], t, t);
        } else {
            let n = text.chars().count();
            let t = plus(s, Assoc::Before, n + 3);
            plan.push(vec![Change::new(s, e, format!("[{text}]()"))], t, t);
        }
    }
    plan.finish()
}

/// `[[selection]]`, or the link back to its text when the selection is in one.
pub fn toggle_wikilink(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let (s, e) = (sel.start(), sel.end());
        let l = buf.char_to_line(s);
        let ls = buf.line_to_char(l);
        let line = chars_of(buf, l);
        let (a, b) = (s - ls, e.min(ls + line.len()) - ls);
        if let Some(&(st, en)) =
            wikilinks(&line).iter().find(|(st, en)| *st <= a && b <= *en && (a < *en || a == b) && !(a == *st && b == *st))
        {
            let changes = vec![Change::new(ls + st, ls + st + 2, ""), Change::new(ls + en - 2, ls + en, "")];
            let (an, hd) = directed(sel, at(s.max(ls + st + 2), Assoc::After), at(e.min(ls + en - 2), Assoc::Before));
            plan.push(changes, an, hd);
            continue;
        }
        if s == e {
            let t = plus(s, Assoc::Before, 2);
            plan.push(vec![Change::new(s, s, "[["), Change::new(s, s, "]]")], t, t);
        } else {
            let (an, hd) = directed(sel, at(s, Assoc::After), at(e, Assoc::Before));
            plan.push(vec![Change::new(s, s, "[["), Change::new(e, e, "]]")], an, hd);
        }
    }
    plan.finish()
}

/// Insert a block on lines of its own after each selection's line (or on
/// the line itself when it's empty). `select` is the part of `block` to
/// select afterwards (char range in `block`).
fn insert_block(buf: &Buffer, sels: &[Selection], block: &str, select: std::ops::Range<usize>, gap_after: bool) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let l = buf.char_to_line(sel.end());
        let r = buf.line_range(l);
        let blank = buf.line(l).trim().is_empty();
        let next_filled = l + 1 < buf.len_lines() && !buf.line(l + 1).trim().is_empty();
        let (pos, lead) = if blank { (r.start, String::new()) } else { (r.end, "\n\n".to_string()) };
        let trail = if next_filled && gap_after { "\n" } else { "" };
        let text = format!("{lead}{block}{trail}");
        let base = lead.chars().count();
        let (from, to) = (plus(pos, Assoc::Before, base + select.start), plus(pos, Assoc::Before, base + select.end));
        let end = if blank { r.end } else { pos };
        plan.push(vec![Change::new(pos, end, text)], from, to);
    }
    plan.finish()
}

/// A two-column table with "Column 1" selected.
pub fn insert_table(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    insert_block(buf, sels, "| Column 1 | Column 2 |\n| --- | --- |\n|  |  |", 2..10, true)
}

/// A horizontal rule on its own line, with a blank line above so the line
/// before doesn't become a heading.
pub fn insert_rule(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    insert_block(buf, sels, "---", 3..3, false)
}

/// ```` ``` ```` fences around the selected lines (or off), or an empty block.
pub fn toggle_code_block(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let (c, a, h) = fence_block(buf, sel, "```");
        plan.push(c, a, h);
    }
    plan.finish()
}

/// `$$` fences around the selected lines (or off), or an empty block.
pub fn toggle_math_block(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut plan = Plan::default();
    for sel in multi::normalize(sels.to_vec()) {
        let (c, a, h) = fence_block(buf, sel, "$$");
        plan.push(c, a, h);
    }
    plan.finish()
}

/// The next free footnote number in the text.
fn next_footnote(buf: &Buffer) -> u64 {
    let text = buf.to_string();
    let mut max = 0;
    let mut rest = text.as_str();
    while let Some(i) = rest.find("[^") {
        rest = &rest[i + 2..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with(']') {
            max = max.max(digits.parse().unwrap_or(0));
        }
    }
    max + 1
}

/// A footnote reference after each selection (`[^1]`) and its definition
/// at the end of the note. With one caret, it moves to the definition.
pub fn insert_footnote(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let sels = multi::normalize(sels.to_vec());
    let first = next_footnote(buf);
    let len = buf.len_chars();
    let text = buf.to_string();
    let last_is_def = text.trim_end().lines().last().is_some_and(|l| l.starts_with("[^"));
    // A blank line before the first definition, unless the note ends with definitions.
    let mut lead = if text.is_empty() || (text.ends_with('\n') && (last_is_def || text.ends_with("\n\n"))) {
        ""
    } else if text.ends_with('\n') || last_is_def || text.ends_with("\n\n") {
        "\n"
    } else {
        "\n\n"
    };
    // References first, then definitions: inserts at the same place keep this order.
    let mut plan = Plan::default();
    let mut defs = Vec::new();
    for (i, sel) in sels.iter().enumerate() {
        let n = first + i as u64;
        let e = sel.end();
        let reference = format!("[^{n}]");
        let t = if sels.len() == 1 { at(len, Assoc::After) } else { plus(e, Assoc::Before, reference.chars().count()) };
        plan.changes.push(Change::new(e, e, reference));
        plan.sels.push((t, t));
        defs.push(Change::new(len, len, format!("{lead}[^{n}]: ")));
        lead = "\n";
    }
    plan.changes.extend(defs);
    plan.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` with `|` for carets and `[` `]` around selections (anchor at
    /// `[`): apply `f` and show the result the same way.
    fn check(text: &str, f: impl Fn(&Buffer, &[Selection]) -> Option<Edit>) -> String {
        let mut plain = String::new();
        let mut sels = Vec::new();
        let mut open = None;
        for c in text.chars() {
            let at = plain.chars().count();
            match c {
                '|' => sels.push(Selection::cursor(at)),
                '«' => open = Some(at),
                '»' => sels.push(Selection::new(open.take().expect("« first"), at)),
                c => plain.push(c),
            }
        }
        let mut buf = Buffer::from(plain.as_str());
        let Some((tx, after)) = f(&buf, &sels) else { return format!("(no change) {text}") };
        tx.apply(&mut buf).unwrap();
        let mut out: Vec<char> = buf.to_string().chars().collect();
        let mut marks: Vec<(usize, char)> = Vec::new();
        for s in after {
            if s.is_empty() {
                marks.push((s.head, '|'));
            } else {
                marks.push((s.start(), '«'));
                marks.push((s.end(), '»'));
            }
        }
        marks.sort_by_key(|m| std::cmp::Reverse(m.0));
        for (p, c) in marks {
            out.insert(p, c);
        }
        out.into_iter().collect()
    }

    fn bold(b: &Buffer, s: &[Selection]) -> Option<Edit> {
        toggle_inline(b, s, Inline::Bold)
    }
    fn italic(b: &Buffer, s: &[Selection]) -> Option<Edit> {
        toggle_inline(b, s, Inline::Italic)
    }

    #[test]
    fn bold_toggles_on_and_off() {
        assert_eq!(check("make «this» bold", bold), "make **«this»** bold");
        assert_eq!(check("make **«this»** bold", bold), "make «this» bold", "bolding bold text unbolds it");
        assert_eq!(check("make «**this**» bold", bold), "make «this» bold", "markers selected too");
        assert_eq!(check("make **th|is** bold", bold), "make th|is bold", "a caret in bold text");
        assert_eq!(check("make th|is bold", bold), "make **th|is** bold", "a caret in a word bolds the word");
        assert_eq!(check("make this| bold", bold), "make **this|** bold");
        assert_eq!(check("end |", bold), "end **|**");
        assert_eq!(check("end **|**", bold), "end |", "an empty pair goes away");
        assert_eq!(check("make «this »bold", bold), "make **«this»** bold", "spaces stay outside");
        assert_eq!(check("a **b «c» d** e", bold), "a b «c» d e", "part of a bold span unbolds the span");
    }

    #[test]
    fn bold_and_italic_share_stars() {
        assert_eq!(check("***«x»***", bold), "*«x»*");
        assert_eq!(check("***«x»***", italic), "**«x»**");
        assert_eq!(check("**«x»**", italic), "**_«x»_**");
        assert_eq!(check("*«x»*", italic), "«x»", "either marker counts");
        assert_eq!(check("__«x»__", bold), "«x»");
        assert_eq!(check("* item |one", bold), "* item **|one**", "a bullet isn't emphasis");
        assert_eq!(check("snake_case «x»", italic), "snake_case _«x»_");
    }

    #[test]
    fn every_cursor_is_formatted_in_one_edit() {
        assert_eq!(check("«one» two «three»", bold), "**«one»** two **«three»**");
        assert_eq!(check("o|ne t|wo", |b, s| toggle_inline(b, s, Inline::Code)), "`o|ne` `t|wo`");
        assert_eq!(check("**o|ne** **t|wo**", bold), "o|ne t|wo");
        assert_eq!(check("tw|o|s", bold), "**tw|o|s**", "two carets in one word wrap it once");
    }

    #[test]
    fn several_lines_are_wrapped_line_by_line() {
        assert_eq!(check("- «one\n- two»", bold), "- **«one**\n- **two»**");
        assert_eq!(check("- «**one**\n- **two**»", bold), "- «one\n- two»");
        assert_eq!(check("«a\nb»", |b, s| toggle_inline(b, s, Inline::Code)), "```\n«a\nb»\n```");
        assert_eq!(check("```\n«a\nb»\n```", |b, s| toggle_inline(b, s, Inline::Code)), "«a\nb»", "inside fences: the fences go");
        assert_eq!(check("«```\na\n```»", toggle_code_block), "«a»");
    }

    #[test]
    fn other_inline_marks() {
        assert_eq!(check("«x»", |b, s| toggle_inline(b, s, Inline::Strike)), "~~«x»~~");
        assert_eq!(check("==«x»==", |b, s| toggle_inline(b, s, Inline::Highlight)), "«x»");
        assert_eq!(check("«E=mc^2»", |b, s| toggle_inline(b, s, Inline::Math)), "$«E=mc^2»$");
    }

    #[test]
    fn headings_lists_and_quotes() {
        let h2 = |b: &Buffer, s: &[Selection]| set_heading(b, s, 2);
        assert_eq!(check("Ti|tle", h2), "## Ti|tle");
        assert_eq!(check("## Ti|tle", h2), "Ti|tle");
        assert_eq!(check("# Ti|tle", h2), "## Ti|tle");
        assert_eq!(check("«a\nb»", h2), "## «a\n## b»");
        let bullets = |b: &Buffer, s: &[Selection]| toggle_list(b, s, List::Bullet);
        let numbers = |b: &Buffer, s: &[Selection]| toggle_list(b, s, List::Numbered);
        assert_eq!(check("«a\n\nb»", bullets), "- «a\n\n- b»", "blank lines stay blank");
        assert_eq!(check("- «a\n- b»", bullets), "«a\nb»");
        assert_eq!(check("- «a\n- b»", numbers), "1. «a\n2. b»");
        assert_eq!(check("  - [ ] t|ask", numbers), "  1. t|ask");
        assert_eq!(check("|", bullets), "- |");
        assert_eq!(check("«a\nb»", toggle_quote), "> «a\n> b»");
        assert_eq!(check("> «a\n> b»", toggle_quote), "«a\nb»");
        assert_eq!(check("a|\nb|", toggle_task), "- [ ] a|\n- [ ] b|");
    }

    #[test]
    fn callouts() {
        let note = |b: &Buffer, s: &[Selection]| toggle_callout(b, s, "note");
        assert_eq!(check("«a\nb»", note), "> [!note]\n> «a\n> b»");
        assert_eq!(check("> [!note]\n> «a\n> b»", note), "«a\nb»");
        assert_eq!(check("|", note), "> [!note]\n> |");
    }

    #[test]
    fn links() {
        assert_eq!(check("see «docs»", toggle_link), "see [docs](|)");
        assert_eq!(check("«https://x.org»", toggle_link), "[|](https://x.org)");
        assert_eq!(check("see [do|cs](u)", toggle_link), "see «docs»", "a link back to its text");
        assert_eq!(check("«Note»", toggle_wikilink), "[[«Note»]]");
        assert_eq!(check("[[No|te]]", toggle_wikilink), "No|te");
        assert_eq!(check("|", toggle_wikilink), "[[|]]");
    }

    #[test]
    fn blocks_go_on_their_own_lines() {
        assert_eq!(check("text|", insert_rule), "text\n\n---|");
        assert_eq!(check("|", insert_rule), "---|");
        assert_eq!(check("a|\nb", insert_rule), "a\n\n---|\nb");
        assert_eq!(check("x|", insert_table), "x\n\n| «Column 1» | Column 2 |\n| --- | --- |\n|  |  |");
        assert_eq!(check("|", toggle_math_block), "$$\n|\n$$");
        assert_eq!(check("«a»", toggle_math_block), "$$\n«a»\n$$");
    }

    #[test]
    fn footnotes_are_numbered_and_defined_at_the_end() {
        assert_eq!(check("Claim.|", insert_footnote), "Claim.[^1]\n\n[^1]: |");
        assert_eq!(check("A.| B.[^1]\n\n[^1]: x\n", insert_footnote), "A.[^2] B.[^1]\n\n[^1]: x\n[^2]: |");
        assert_eq!(check("A.| B.|", insert_footnote), "A.[^1]| B.[^2]|\n\n[^1]: \n[^2]: ");
    }
}
