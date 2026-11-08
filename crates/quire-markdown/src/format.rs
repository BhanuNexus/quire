// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Formatting for the toolbar and the Format commands (0.11).
//!
//! Every function works on all selections at once (multiple cursors) and
//! returns one [`Transaction`] with the selections after it, so a button
//! press is one undo step. The toggles read the Markdown around each
//! selection: bolding bold text unbolds it, a caret inside `**word**`
//! unbolds the word, and a caret inside a plain word bolds the word.
//! Text is only ever wrapped or unwrapped, never dropped: the only
//! characters added or taken away are markers, and whole fence lines.
//! Which markers pair up is the Markdown parser's call, so stars in code,
//! math or `2*3` are never touched. Positions are chars, clamped to the
//! text.

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
    c.is_alphanumeric() || c == '\'' || c == '-' || c == '_'
}

/// Whether markers put at `a` and `b` would run into the same character
/// next to them (a backtick next to two more makes a fence, `~~` next to
/// `~~` makes `~~~~`).
fn glued_to(line: &[char], a: usize, b: usize, m: &str) -> bool {
    let c = m.chars().next();
    a.checked_sub(1).and_then(|i| line.get(i)).copied() == c || line.get(b).copied() == c
}

/// The marker to wrap `line[a..b]` in: italic is `_`, except next to a
/// letter, digit or `_`, where `_` wouldn't be emphasis (`snake_case`), so
/// it is `*`.
fn wrap_marker(kind: Inline, line: &[char], a: usize, b: usize) -> &'static str {
    let glued = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let edge = |i: usize| line.get(i) == Some(&'_');
    let inner = b > a && (edge(a) || edge(b - 1));
    if kind == Inline::Italic && (inner || glued(a.checked_sub(1).and_then(|i| line.get(i))) || glued(line.get(b))) {
        "*"
    } else {
        kind.marker()
    }
}

/// Code, math, strike and highlight markers can't touch the same character
/// (`` ` `` + `` `x` `` would make a fence, `$` + `$x$` display math).
fn clashes(kind: Inline, line: &[char], a: usize, b: usize) -> bool {
    if matches!(kind, Inline::Bold | Inline::Italic) {
        return false;
    }
    let c = kind.marker().chars().next();
    glued_to(line, a, b, kind.marker()) || (b > a && (line.get(a).copied() == c || line.get(b - 1).copied() == c))
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

/// What the Markdown parser found in the whole text, in chars: the
/// formatted spans (only these are ever unwrapped) and the verbatim ranges
/// (code, math, HTML, link destinations), where nothing is wrapped.
///
/// Pairing markers by hand got code spans, math, escapes and literal
/// stars (`2*3`) wrong and unwrapped spans that weren't there (0.11
/// review H3).
struct Marks {
    /// (kind, start, end, marker length), absolute chars.
    spans: Vec<(Inline, usize, usize, usize)>,
    verbatim: Vec<std::ops::Range<usize>>,
}

impl Marks {
    fn of(buf: &Buffer) -> Self {
        use pulldown_cmark::{Event, Parser, Tag};
        let text = buf.to_string();
        let parsed = crate::parse_source(&text);
        let mut verbatim = crate::scan::Verbatim::default();
        let mut raw: Vec<(Inline, std::ops::Range<usize>)> = Vec::new();
        for (ev, range) in Parser::new_ext(&parsed, crate::options()).into_offset_iter() {
            verbatim.event(&text, &ev, &range);
            let kind = match ev {
                Event::Start(Tag::Strong) => Inline::Bold,
                Event::Start(Tag::Emphasis) => Inline::Italic,
                Event::Start(Tag::Strikethrough) => Inline::Strike,
                Event::Code(_) => Inline::Code,
                Event::InlineMath(_) | Event::DisplayMath(_) => Inline::Math,
                _ => continue,
            };
            raw.push((kind, range));
        }
        let verbatim_bytes = verbatim.finish();
        for inline in crate::scan::scan_with(&text, &verbatim_bytes) {
            if let crate::scan::Inline::Highlight { range } = inline {
                raw.push((Inline::Highlight, range));
            }
        }
        let chars: Vec<char> = text.chars().collect();
        let mut spans = Vec::new();
        for (kind, r) in raw {
            let (st, en) = (buf.byte_to_char(r.start), buf.byte_to_char(r.end));
            if let Some(len) = marker_len(&chars, kind, st, en) {
                spans.push((kind, st, en, len));
            }
        }
        let verbatim = verbatim_bytes.into_iter().map(|r| buf.byte_to_char(r.start)..buf.byte_to_char(r.end)).collect();
        Self { spans, verbatim }
    }

    /// `pos` is strictly inside a verbatim range (code, math, …).
    fn in_verbatim(&self, pos: usize) -> bool {
        self.verbatim.iter().any(|r| r.start < pos && pos < r.end)
    }

    /// Spans of `kind` that lie on the line `ls..le`, in line columns.
    fn on_line(&self, kind: Inline, ls: usize, le: usize) -> Vec<Span> {
        self.spans
            .iter()
            .filter(|(k, st, en, _)| *k == kind && ls <= *st && *en <= le)
            .map(|&(_, st, en, len)| Span { open: st - ls, close: en - len - ls, len })
            .collect()
    }
}

/// The marker length of a parsed span at `st..en`, or `None` when its
/// ends aren't markers Quire can take away (or, for `*`, are inside a
/// word: `2*3 and 4*5` is read as literal stars).
fn marker_len(chars: &[char], kind: Inline, st: usize, en: usize) -> Option<usize> {
    let run = |c: char| chars.get(st..en).map_or(0, |s| s.iter().take_while(|x| **x == c).count());
    let len = match kind {
        Inline::Bold | Inline::Highlight => 2,
        Inline::Italic => 1,
        Inline::Strike => run('~').min(2),
        Inline::Code => run('`'),
        Inline::Math => run('$').min(2),
    };
    if len == 0 || en < st + 2 * len {
        return None;
    }
    let open = chars.get(st..st + len)?;
    let close = chars.get(en - len..en)?;
    let c = open[0];
    let ok_char = match kind {
        Inline::Bold | Inline::Italic => c == '*' || c == '_',
        _ => kind.marker().starts_with(c),
    };
    if !ok_char || open.iter().chain(close).any(|x| *x != c) {
        return None;
    }
    if c == '*' {
        let alnum = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|x| x.is_alphanumeric());
        let intraword = |a: usize, b: usize| alnum(a.checked_sub(1)) && alnum(Some(b));
        if intraword(st, st + len) && intraword(en - len, en) {
            return None;
        }
    }
    Some(len)
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
fn span_around(spans: &[Span], a: usize, b: usize) -> Option<Span> {
    spans.iter().copied().find(|s| {
        let (cs, ce) = s.content();
        (cs <= a && b <= ce) || (s.open == a && s.end() == b) || (a == b && s.open < a && a < s.end())
    })
}

/// Selections clamped to the text, with no end between a `\r` and its
/// `\n` (0.11 review L4: such positions made the toggles panic).
fn prep(buf: &Buffer, sels: &[Selection]) -> Vec<Selection> {
    let len = buf.len_chars();
    let fix = |p: usize| {
        let p = p.min(len);
        let l = buf.char_to_line(p);
        p.min(buf.line_range(l).end.max(buf.line_to_char(l)))
    };
    multi::normalize(sels.iter().map(|s| Selection::new(fix(s.anchor), fix(s.head))).collect())
}

/// Toggle inline formatting on every selection.
pub fn toggle_inline(buf: &Buffer, sels: &[Selection], kind: Inline) -> Option<Edit> {
    let mut plan = Plan::default();
    let m = kind.marker();
    let ml = m.chars().count();
    let marks = Marks::of(buf);
    let fences = Fences::of(buf);
    for sel in prep(buf, sels) {
        let (s, e) = (sel.start(), sel.end());
        let (l1, l2) = (buf.char_to_line(s), buf.char_to_line(e));
        if l1 != l2 && matches!(kind, Inline::Code | Inline::Math) {
            let fence = if kind == Inline::Code { "```" } else { "$$" };
            let (changes, a, h) = fence_block(buf, &fences, sel, fence);
            plan.push(changes, a, h);
            continue;
        }
        if l1 != l2 {
            inline_lines(buf, &marks, sel, kind, &mut plan);
            continue;
        }
        let ls = buf.line_to_char(l1);
        let line = chars_of(buf, l1);
        let (a, b) = ((s - ls).min(line.len()), (e - ls).min(line.len()));
        // Unwrap the span the selection is in (or is).
        if let Some(sp) = span_around(&marks.on_line(kind, ls, ls + line.len()), a, b) {
            let changes = vec![Change::new(ls + sp.open, ls + sp.open + sp.len, ""), Change::new(ls + sp.close, ls + sp.end(), "")];
            let (cs, ce) = sp.content();
            let (ns, ne) = if s == e { (s.clamp(ls + cs, ls + ce), s.clamp(ls + cs, ls + ce)) } else { (s.max(ls + cs), e.min(ls + ce)) };
            let (an, hd) = directed(sel, at(ns, Assoc::After), at(ne, Assoc::Before));
            plan.push(changes, an, hd);
            continue;
        }
        // Inside code or math, or on a fence line: markers there would be
        // literal text, or stop the fence being one.
        if marks.in_verbatim(s) || marks.in_verbatim(e) || fence_line(&buf.line(l1)).is_some() {
            plan.push(Vec::new(), at(sel.anchor, Assoc::After), at(sel.head, Assoc::After));
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
            if ws < we && clashes(kind, &line, ws, we) {
                plan.push(Vec::new(), at(s, Assoc::After), at(s, Assoc::After));
            } else if ws < we {
                let m = wrap_marker(kind, &line, ws, we);
                let changes = vec![Change::new(ls + ws, ls + ws, m), Change::new(ls + we, ls + we, m)];
                let t = if a == we { at(s, Assoc::Before) } else { at(s, Assoc::After) };
                plan.push(changes, t, t);
            } else if glued_to(&line, a, a, m) {
                plan.push(Vec::new(), at(s, Assoc::After), at(s, Assoc::After));
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
            if glued_to(&line, a, a, m) {
                plan.push(Vec::new(), at(sel.anchor, Assoc::After), at(sel.head, Assoc::After));
                continue;
            }
            let changes = vec![Change::new(s, s, m.repeat(2))];
            let t = plus(s, Assoc::Before, ml);
            plan.push(changes, t, t);
            continue;
        }
        if clashes(kind, &line, ta, tb) {
            plan.push(Vec::new(), at(sel.anchor, Assoc::After), at(sel.head, Assoc::After));
            continue;
        }
        let m = wrap_marker(kind, &line, ta, tb);
        let changes = vec![Change::new(ls + ta, ls + ta, m), Change::new(ls + tb, ls + tb, m)];
        let (an, hd) = directed(sel, at(ls + ta, Assoc::After), at(ls + tb, Assoc::Before));
        plan.push(changes, an, hd);
    }
    plan.finish()
}

/// Inline formatting over several lines: each line's text is wrapped on
/// its own (Markdown emphasis doesn't cross paragraphs). All wrapped
/// already: unwrap them all.
fn inline_lines(buf: &Buffer, marks: &Marks, sel: Selection, kind: Inline, plan: &mut Plan) {
    let m = kind.marker();
    let (s, e) = (sel.start(), sel.end());
    let mut segments = Vec::new();
    for l in buf.char_to_line(s)..=buf.char_to_line(e) {
        let ls = buf.line_to_char(l);
        let line = chars_of(buf, l);
        let mut a = s.saturating_sub(ls).min(line.len()).max(content_start(&line)).min(line.len());
        let mut b = e.saturating_sub(ls).min(line.len());
        while a < b && line[a].is_whitespace() {
            a += 1;
        }
        while b > a && line[b - 1].is_whitespace() {
            b -= 1;
        }
        // Code and math lines (fenced blocks too) are left as they are.
        if a < b && !marks.in_verbatim(ls + a) && !marks.in_verbatim(ls + b) && !clashes(kind, &line, a, b) {
            let m = if wrap_marker(kind, &line, a, b) == "*" { "*" } else { m };
            segments.push((ls, a, b, m, span_around(&marks.on_line(kind, ls, ls + line.len()), a, b)));
        }
    }
    if segments.is_empty() {
        return;
    }
    let unwrap = segments.iter().all(|x| x.4.is_some());
    let mut changes = Vec::new();
    for (ls, a, b, m, sp) in segments {
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

/// The line break to write after line `l`: its own, or else the one the
/// text's first line has (`\r\n` text stays `\r\n`).
fn line_break(buf: &Buffer, l: usize) -> &'static str {
    let crlf = |l: usize| buf.char_at(buf.line_range(l).end) == Some('\r');
    let own = l + 1 < buf.len_lines();
    if (own && crlf(l)) || (!own && buf.len_lines() > 1 && crlf(0)) { "\r\n" } else { "\n" }
}

/// Fenced blocks, paired from the top of the text: (opening line, closing
/// line, fence family). A fence line holds only the fence (```` ``` ```` or
/// `~~~`, three or more, with an optional info string on the opener) or
/// only `$$`; a line that merely starts with one (`$$a$$ is the sum`) is
/// text (0.11 review H3).
struct Fences(Vec<(usize, usize, &'static str)>);

/// A fence line: (family, char, run length, has an info string).
fn fence_line(line: &str) -> Option<(&'static str, char, usize, bool)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let t = line[indent..].trim_end();
    if t == "$$" {
        return Some(("$$", '$', 2, false));
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = t.chars().take_while(|x| *x == c).count();
    let info = t[run..].trim();
    if run < 3 || (c == '`' && info.contains('`')) {
        return None;
    }
    Some(("```", c, run, !info.is_empty()))
}

impl Fences {
    fn of(buf: &Buffer) -> Self {
        let mut out = Vec::new();
        let mut open: Option<(usize, &'static str, char, usize)> = None;
        for l in 0..buf.len_lines() {
            let Some((family, c, run, info)) = fence_line(&buf.line(l)) else { continue };
            match open {
                None => open = Some((l, family, c, run)),
                Some((ol, of, oc, orun)) if of == family && oc == c && run >= orun && !info => {
                    out.push((ol, l, family));
                    open = None;
                }
                Some(_) => {}
            }
        }
        Self(out)
    }

    /// The block of `family` that opens on `open` and closes on `close`.
    fn pair(&self, open: usize, close: usize, family: &str) -> bool {
        self.0.iter().any(|&(o, c, f)| o == open && c == close && f == family)
    }
}

/// Take a fenced block's opening and closing lines out, with their line
/// breaks (so `\r\n` stays `\r\n`).
fn unfence(buf: &Buffer, open: usize, close: usize) -> Vec<Change> {
    let line_with_break = |l: usize| {
        let s = buf.line_to_char(l);
        if l + 1 < buf.len_lines() { s..buf.line_to_char(l + 1) } else { s..buf.line_range(l).end }
    };
    let a = line_with_break(open);
    let mut z = line_with_break(close);
    if close + 1 >= buf.len_lines() && close > open + 1 {
        // The last line has no break of its own: take the one before it.
        z.start = buf.line_range(close - 1).end;
    }
    if z.start <= a.end {
        return vec![Change::new(a.start, z.end, "")];
    }
    vec![Change::new(a.start, a.end, ""), Change::new(z.start, z.end, "")]
}

/// Wrap whole lines in a fence (```` ``` ```` or `$$`), or take the fences
/// away when the lines are a fenced block or its inside.
fn fence_block(buf: &Buffer, fences: &Fences, sel: Selection, fence: &str) -> (Vec<Change>, Target, Target) {
    let lines = lines_of(buf, sel);
    let (first, last) = (*lines.start(), *lines.end());
    if first < last && fences.pair(first, last, fence) {
        // A whole block selected: remove its fences.
        let changes = unfence(buf, first, last);
        let (an, hd) = directed(sel, at(sel.start(), Assoc::After), at(sel.end(), Assoc::Before));
        return (changes, an, hd);
    }
    if first > 0 && fences.pair(first - 1, last + 1, fence) {
        let changes = unfence(buf, first - 1, last + 1);
        let (an, hd) = directed(sel, at(sel.start(), Assoc::After), at(sel.end(), Assoc::Before));
        return (changes, an, hd);
    }
    let nl = line_break(buf, last);
    let start = buf.line_to_char(first);
    let end = buf.line_range(last).end;
    let changes = vec![Change::new(start, start, format!("{fence}{nl}")), Change::new(end, end, format!("{nl}{fence}"))];
    if start == end {
        let t = plus(start, Assoc::Before, fence.chars().count() + nl.chars().count());
        return (changes, t, t);
    }
    // Inside the fences: past the opener at the line's start, before the
    // closer at its end.
    let inside = |p: usize| {
        let p = p.clamp(start, end);
        at(p, if p == end { Assoc::Before } else { Assoc::After })
    };
    (changes, inside(sel.anchor), inside(sel.head))
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
    let sels = &prep(buf, sels);
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
    let sels = &prep(buf, sels);
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
    let sels = &prep(buf, sels);
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
    let sels = &prep(buf, sels);
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
    for sel in prep(buf, sels) {
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
    for sel in prep(buf, sels) {
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
    for sel in prep(buf, sels) {
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
    for sel in prep(buf, sels) {
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
    let fences = Fences::of(buf);
    for sel in prep(buf, sels) {
        let (c, a, h) = fence_block(buf, &fences, sel, "```");
        plan.push(c, a, h);
    }
    plan.finish()
}

/// `$$` fences around the selected lines (or off), or an empty block.
pub fn toggle_math_block(buf: &Buffer, sels: &[Selection]) -> Option<Edit> {
    let mut plan = Plan::default();
    let fences = Fences::of(buf);
    for sel in prep(buf, sels) {
        let (c, a, h) = fence_block(buf, &fences, sel, "$$");
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
    let sels = prep(buf, sels);
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

    // 0.11 review H3 and L4/L5: toggles never drop or garble text.
    #[test]
    fn a_line_that_starts_with_a_fence_is_text() {
        let math = |b: &Buffer, s: &[Selection]| toggle_inline(b, s, Inline::Math);
        assert_eq!(
            check("«$$a+b$$ is the sum\nsome text\n$$c$$ another»\n", math),
            "$$\n«$$a+b$$ is the sum\nsome text\n$$c$$ another»\n$$\n"
        );
        let code = |b: &Buffer, s: &[Selection]| toggle_inline(b, s, Inline::Code);
        assert_eq!(check("«```js``` inline\nx\n```js``` too»", code), "```\n«```js``` inline\nx\n```js``` too»\n```");
    }

    #[test]
    fn a_code_block_between_two_blocks_stays_separate() {
        assert_eq!(
            check("```\nA\n```\n«plain»\n```python\nB\n```\n", toggle_code_block),
            "```\nA\n```\n```\n«plain»\n```\n```python\nB\n```\n"
        );
        assert_eq!(check("```python\nB\n```\n«x»", toggle_code_block), "```python\nB\n```\n```\n«x»\n```");
        assert_eq!(check("```python\n«B»\n```\n", toggle_code_block), "«B»\n", "inside its own block: the fences go");
        assert_eq!(check("~~~\n«B»\n~~~", toggle_code_block), "«B»");
    }

    #[test]
    fn emphasis_skips_code_math_and_literal_stars() {
        assert_eq!(check("`a**b` an|d **c** x", bold), "`a**b` **an|d** **c** x");
        assert_eq!(check("$a*b*c$ and word|", italic), "$a*b*c$ and _word|_");
        assert_eq!(check("$a*|b*c$ and word", italic), "(no change) $a*|b*c$ and word", "a caret in math: no change");
        assert_eq!(check("2*3 an|d 4*5", italic), "2*3 _an|d_ 4*5");
        assert_eq!(check("\\*a|b\\*", italic), "\\*_a|b_\\*");
        assert_eq!(check("`co|de` x", bold), "(no change) `co|de` x", "a caret in code: no change");
        assert_eq!(check("[t](u**r|l**)", bold), "(no change) [t](u**r|l**)", "link destinations are left alone");
        assert_eq!(check("`co|de`", |b, s| toggle_inline(b, s, Inline::Code)), "co|de");
        assert_eq!(check("$x|$", |b, s| toggle_inline(b, s, Inline::Math)), "x|");
        assert_eq!(check("~~x|~~", |b, s| toggle_inline(b, s, Inline::Strike)), "x|");
    }

    #[test]
    fn positions_off_the_text_are_clamped() {
        let buf = Buffer::from("a\r\nbc\r\n");
        let kinds = [Inline::Bold, Inline::Italic, Inline::Strike, Inline::Highlight, Inline::Code, Inline::Math];
        for p in 0..12 {
            for q in 0..12 {
                let sels = [Selection::new(p, q)];
                for k in kinds {
                    let _ = toggle_inline(&buf, &sels, k);
                }
                let _ = toggle_code_block(&buf, &sels);
                let _ = toggle_math_block(&buf, &sels);
                let _ = toggle_link(&buf, &sels);
                let _ = toggle_wikilink(&buf, &sels);
                let _ = set_heading(&buf, &sels, 2);
                let _ = toggle_list(&buf, &sels, List::Bullet);
                let _ = toggle_quote(&buf, &sels);
                let _ = toggle_task(&buf, &sels);
                let _ = toggle_callout(&buf, &sels, "note");
                let _ = insert_footnote(&buf, &sels);
                let _ = insert_rule(&buf, &sels);
            }
        }
    }

    #[test]
    fn fences_keep_crlf() {
        assert_eq!(check("```\r\n«a»\r\n```\r\nb\r\n", toggle_code_block), "«a»\r\nb\r\n");
        assert_eq!(check("x\r\n```\r\n«a»\r\n```", toggle_code_block), "x\r\n«a»");
        assert_eq!(check("«a»\r\nb\r\n", toggle_code_block), "```\r\n«a»\r\n```\r\nb\r\n");
        assert_eq!(check("```\n|\n```", toggle_code_block), "|");
    }

    #[test]
    fn footnotes_are_numbered_and_defined_at_the_end() {
        assert_eq!(check("Claim.|", insert_footnote), "Claim.[^1]\n\n[^1]: |");
        assert_eq!(check("A.| B.[^1]\n\n[^1]: x\n", insert_footnote), "A.[^2] B.[^1]\n\n[^1]: x\n[^2]: |");
        assert_eq!(check("A.| B.|", insert_footnote), "A.[^1]| B.[^2]|\n\n[^1]: \n[^2]: ");
    }
}
