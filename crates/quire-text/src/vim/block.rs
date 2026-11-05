// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Columns for Visual-block mode (`Ctrl-v`).
//!
//! A block is a range of lines and a range of display columns: a tab
//! reaches the next tab stop and wide characters (CJK) take two columns,
//! as in Vim. A character the block's edge cuts through (a tab, a wide
//! character) is split into spaces when the block is edited.

use unicode_width::UnicodeWidthChar;

use super::motions as m;
use super::parse::{Act, Cmd, Motion, Op, Simple};
use super::{Group, Host, InsertKind, InsertSession, Mode, Outcome, Recorded, Register, Scratch, Shape, VimState, toggle_case};
use crate::{Buffer, Selection};

/// How many columns `c` takes when it starts at column `col`.
pub(super) fn width(c: char, col: usize, tabstop: usize) -> usize {
    if c == '\t' {
        let ts = tabstop.max(1);
        ts - col % ts
    } else {
        c.width().unwrap_or(1)
    }
}

/// Each char of `line` (without its line break): (char index, column, width).
pub(super) fn cells(buf: &Buffer, line: usize, tabstop: usize) -> Vec<(usize, usize, usize)> {
    let (s, e) = (m::line_start(buf, line), m::line_end(buf, line));
    let mut col = 0;
    let mut out = Vec::with_capacity(e - s);
    for (i, c) in buf.rope().slice(s..e).chars().enumerate() {
        let w = width(c, col, tabstop);
        out.push((s + i, col, w));
        col += w;
    }
    out
}

/// The columns `pos` covers: (first, last). The line's end is one column
/// past its last char.
pub(super) fn vcols(buf: &Buffer, pos: usize, tabstop: usize) -> (usize, usize) {
    let line = m::line_of(buf, pos);
    let cs = cells(buf, line, tabstop);
    if let Some((_, c, w)) = cs.iter().find(|(i, _, _)| *i == pos) {
        return (*c, c + w.max(&1) - 1);
    }
    let end = cs.last().map_or(0, |(_, c, w)| c + w);
    (end, end)
}

/// The char on `line` covering column `col`, or the line's last char (its
/// start when empty) when the line is shorter.
pub(super) fn at_vcol(buf: &Buffer, line: usize, col: usize, tabstop: usize) -> usize {
    let cs = cells(buf, line, tabstop);
    cs.iter().find(|(_, c, w)| col < c + w.max(&1)).or_else(|| cs.last()).map_or_else(|| m::line_start(buf, line), |(i, _, _)| *i)
}

/// The part of one line inside a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cut {
    /// Chars `start..end` are (at least partly) inside the block.
    pub start: usize,
    pub end: usize,
    /// Columns of the first char left of the block (it is cut through).
    pub pre: usize,
    /// Columns of the last char right of the block.
    pub post: usize,
    /// The line ends before the block's first column.
    pub short: bool,
    /// What the block holds on this line (cut chars become spaces).
    pub text: String,
    /// Display width of the whole line.
    pub width: usize,
}

/// The block's part of `line`: columns `left..=right` (`right` `None`: to the end).
pub(super) fn cut(buf: &Buffer, line: usize, left: usize, right: Option<usize>, tabstop: usize) -> Cut {
    let cs = cells(buf, line, tabstop);
    let width = cs.last().map_or(0, |(_, c, w)| c + w);
    let le = m::line_end(buf, line);
    let Some(first) = cs.iter().position(|(_, c, w)| c + w > left || (*w == 0 && *c >= left)) else {
        return Cut { start: le, end: le, pre: 0, post: 0, short: true, text: String::new(), width };
    };
    let last = right.map_or(cs.len(), |r| cs.iter().position(|(_, c, _)| *c > r).unwrap_or(cs.len()));
    let start = cs[first].0;
    let end = if last < cs.len() { cs[last].0 } else { le };
    let pre = left.saturating_sub(cs[first].1);
    let post = match (right, last.checked_sub(1).and_then(|i| cs.get(i))) {
        (Some(r), Some((_, c, w))) if last > first && c + w > r + 1 => c + w - r - 1,
        _ => 0,
    };
    let mut text = String::new();
    for (k, (i, c, w)) in cs[first..last.max(first)].iter().enumerate() {
        let lo = (*c).max(left);
        let hi = right.map_or(c + w, |r| (c + w).min(r + 1));
        let split = (k == 0 && pre > 0) || (k + first + 1 == last && post > 0);
        if split {
            text.push_str(&" ".repeat(hi.saturating_sub(lo)));
        } else if let Some(ch) = buf.char_at(*i) {
            text.push(ch);
        }
    }
    Cut { start, end, pre, post, short: false, text, width }
}

/// A block: lines `l0..=l1`, columns `left..=right` (`None`: to each line's end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Geom {
    pub l0: usize,
    pub l1: usize,
    pub left: usize,
    pub right: Option<usize>,
}

/// Where a block insert puts its cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Insert {
    /// `I`: before the block.
    Before,
    /// `A`: after it (padding short lines).
    After,
    /// `c`: where the block was.
    Change,
}

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

/// Display width of text that starts at column 0.
fn text_width(s: &str, tabstop: usize) -> usize {
    s.chars().fold(0, |col, c| col + width(c, col, tabstop))
}

impl VimState {
    pub(super) fn block_geom(&self, buf: &Buffer) -> Geom {
        let len = buf.len_chars();
        let (a, c) = (self.anchor.min(len), self.cursor.min(len));
        let (la, lc) = (m::line_of(buf, a), m::line_of(buf, c));
        let ((a0, a1), (c0, c1)) = (vcols(buf, a, self.tabstop), vcols(buf, c, self.tabstop));
        Geom { l0: la.min(lc), l1: la.max(lc), left: a0.min(c0), right: (!self.dollar).then(|| a1.max(c1)) }
    }

    /// The block as selections: the cursor's row, then the others.
    pub(super) fn block_rows(&self, buf: &Buffer) -> (Selection, Vec<Selection>) {
        let g = self.block_geom(buf);
        let cur_line = m::line_of(buf, self.cursor.min(buf.len_chars()));
        let mut main = Selection::cursor(self.cursor.min(buf.len_chars()));
        let mut rest = Vec::new();
        for l in g.l0..=g.l1 {
            let c = cut(buf, l, g.left, g.right, self.tabstop);
            if c.short {
                continue;
            }
            let s = Selection::new(c.start, c.end);
            if l == cur_line {
                main = s;
            } else {
                rest.push(s);
            }
        }
        (main, rest)
    }

    /// Delete (unless `keep`) the block, storing its rows in a register.
    /// Returns where each row was cut (`None` for short lines).
    fn block_cut(&mut self, sx: &mut Scratch, g: Geom, reg: Option<char>, keep: bool, out: &mut Outcome) -> Vec<Option<usize>> {
        let rows: Vec<String> = (g.l0..=g.l1).map(|l| cut(&sx.work, l, g.left, g.right, self.tabstop).text).collect();
        self.store_reg(reg, Register { text: rows.join("\n"), linewise: false, block: true }, keep, out);
        let mut at = Vec::new();
        for l in g.l0..=g.l1 {
            let c = cut(&sx.work, l, g.left, g.right, self.tabstop);
            if c.short {
                at.push(None);
                continue;
            }
            if !keep {
                sx.replace(c.start..c.end, &spaces(c.pre + c.post));
            }
            at.push(Some(c.start + c.pre));
        }
        at
    }

    /// The top-left corner of a block.
    fn block_corner(&self, buf: &Buffer, g: Geom) -> usize {
        at_vcol(buf, g.l0, g.left, self.tabstop)
    }

    /// Split a char cut by a block's edge into spaces; returns the position
    /// `cols` columns into it.
    fn split_at(&self, sx: &mut Scratch, pos: usize, cols: usize) -> usize {
        if cols == 0 {
            return pos;
        }
        let (c0, c1) = vcols(&sx.work, pos, self.tabstop);
        sx.replace(pos..pos + 1, &spaces(c1 + 1 - c0));
        pos + cols
    }

    /// Start typing at one cursor a row (`I`, `A`, `c`).
    fn block_insert(&mut self, sx: &mut Scratch, g: Geom, how: Insert, cmd: &Cmd, shape: Shape, out: &mut Outcome) -> usize {
        let mut cursors = Vec::new();
        let corner = self.block_corner(&sx.work, g);
        if how == Insert::Change {
            cursors = self.block_cut(sx, g, cmd.reg, false, out).into_iter().flatten().collect();
        } else {
            for l in g.l0..=g.l1 {
                let at = match (how, g.right) {
                    (Insert::After, None) => Some(m::line_end(&sx.work, l)),
                    (Insert::After, Some(r)) => {
                        let c = cut(&sx.work, l, r + 1, None, self.tabstop);
                        if c.short {
                            let e = m::line_end(&sx.work, l);
                            let pad = (r + 1).saturating_sub(c.width);
                            sx.replace(e..e, &spaces(pad));
                            Some(e + pad)
                        } else {
                            Some(self.split_at(sx, c.start, c.pre))
                        }
                    }
                    _ => {
                        // A line that ends right where the block starts gets the text too.
                        let c = cut(&sx.work, l, g.left, None, self.tabstop);
                        if c.short {
                            (c.width == g.left).then(|| m::line_end(&sx.work, l))
                        } else {
                            Some(self.split_at(sx, c.start, c.pre))
                        }
                    }
                };
                cursors.extend(at);
            }
        }
        self.last_visual = Some((Mode::VisualBlock, self.anchor, self.cursor));
        self.mode = Mode::Normal;
        let Some(&top) = cursors.first() else { return corner };
        let ret = match how {
            Insert::Before => Some(top),
            Insert::After => Some(corner),
            Insert::Change => None,
        };
        let rec = Recorded { cmd: cmd.clone(), insert: None, visual: Some(shape) };
        if let Some(text) = self.replay.clone() {
            let n = text.chars().count();
            // Bottom row first, so the positions above stay where they are.
            for at in cursors.iter().rev() {
                sx.replace(*at..*at, &text);
            }
            return ret.unwrap_or_else(|| (top + n).saturating_sub(1).max(top));
        }
        if self.replay.is_none() {
            self.last_change = Some(rec.clone());
        }
        self.mode = Mode::Insert;
        self.insert =
            Some(InsertSession { base: sx.work.clone(), start: top, count: 1, kind: InsertKind::Block { top, ret }, rec: Some(rec) });
        out.group = Group::Begin;
        out.extra = Some(cursors[1..].iter().map(|p| Selection::cursor(*p)).collect());
        top
    }

    /// `p` / `P` of a block register in Normal mode: row `i` goes on the
    /// `i`-th line from the cursor's, in the cursor's column.
    pub(super) fn put_block(&self, sx: &mut Scratch, pos: usize, text: &str, before: bool, n: usize) -> usize {
        let line = m::line_of(&sx.work, pos);
        let col = if before || m::line_is_empty(&sx.work, line) {
            vcols(&sx.work, pos, self.tabstop).0
        } else {
            vcols(&sx.work, pos, self.tabstop).1 + 1
        };
        let rows: Vec<String> = text.split('\n').map(|r| r.repeat(n.max(1))).collect();
        let wide = rows.iter().map(|r| text_width(r, self.tabstop)).max().unwrap_or(0);
        let mut first = None;
        for (i, row) in rows.iter().enumerate() {
            let l = line + i;
            if l > m::last_line(&sx.work) {
                let e = sx.work.len_chars();
                sx.replace(e..e, "\n");
            }
            let c = cut(&sx.work, l, col, None, self.tabstop);
            let at = if c.short {
                let e = m::line_end(&sx.work, l);
                let pad = col.saturating_sub(c.width);
                sx.replace(e..e, &spaces(pad));
                sx.replace(e + pad..e + pad, row);
                e + pad
            } else {
                let at = self.split_at(sx, c.start, c.pre);
                let w = text_width(row, self.tabstop);
                sx.replace(at..at, &format!("{row}{}", spaces(wide.saturating_sub(w))));
                at
            };
            first.get_or_insert(at);
        }
        first.unwrap_or(pos)
    }

    /// A key in Visual-block mode.
    pub(super) fn visual_block(&mut self, cmd: &Cmd, sx: &mut Scratch, c: usize, host: &mut dyn Host, out: &mut Outcome) -> usize {
        let buf = sx.work.clone();
        let ts = self.tabstop;
        let g = self.block_geom(&buf);
        let shape = Shape::Block(g.l1 - g.l0, g.right.map(|r| r.saturating_sub(g.left)));
        let leave = |s: &mut Self| {
            s.last_visual = Some((Mode::VisualBlock, s.anchor, s.cursor));
            s.mode = Mode::Normal;
            s.dollar = false;
        };
        let record = |s: &mut Self| {
            if s.replay.is_none() {
                s.last_change = Some(Recorded { cmd: cmd.clone(), insert: None, visual: Some(shape) });
            }
        };
        match &cmd.act {
            Act::Move(mo) => {
                let Some((p, _)) = self.motion(&buf, c, mo, cmd.count, false, host) else {
                    self.error = true;
                    return c;
                };
                if super::is_jump(mo) && p != c {
                    self.marks.set('\'', buf.line_col(c));
                }
                match mo {
                    Motion::LineEnd => {
                        self.dollar = true;
                        p
                    }
                    Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown | Motion::DisplayUp | Motion::DisplayDown => {
                        let l = m::line_of(&buf, p);
                        if self.dollar { m::line_last(&buf, l) } else { at_vcol(&buf, l, self.block_col, ts) }
                    }
                    _ => {
                        self.dollar = false;
                        self.block_col = vcols(&buf, p, ts).0;
                        p
                    }
                }
            }
            Act::Select(_) | Act::Op(..) => c,
            Act::Do(s) => match *s {
                // `Ctrl-v` again leaves block mode, as Escape does.
                Simple::Escape | Simple::VisualBlock => {
                    leave(self);
                    c
                }
                Simple::Visual | Simple::VisualLine => {
                    self.mode = if *s == Simple::Visual { Mode::Visual } else { Mode::VisualLine };
                    self.dollar = false;
                    c
                }
                Simple::Swap => {
                    let old = self.anchor;
                    self.anchor = c;
                    self.block_col = vcols(&buf, old, ts).0;
                    old
                }
                Simple::SwapSide => {
                    let a = self.anchor.min(buf.len_chars());
                    let (ac, cc) = (vcols(&buf, a, ts).0, vcols(&buf, c, ts).0);
                    self.anchor = at_vcol(&buf, m::line_of(&buf, a), cc, ts);
                    let to = at_vcol(&buf, m::line_of(&buf, c), ac, ts);
                    self.block_col = vcols(&buf, to, ts).0;
                    to
                }
                Simple::SetMark(ch) => {
                    self.marks.set(ch, buf.line_col(c));
                    c
                }
                Simple::Cmdline(_) => {
                    leave(self);
                    self.cmdline = Some((':', "'<,'>".into()));
                    c
                }
                Simple::Reselect => match self.last_visual {
                    Some((mode, la, lc)) => {
                        self.last_visual = Some((self.mode, self.anchor, c));
                        self.mode = mode;
                        self.anchor = la.min(buf.len_chars());
                        lc.min(buf.len_chars())
                    }
                    None => c,
                },
                Simple::VisualOp(Op::Indent, _) => {
                    record(self);
                    let unit = if self.indent.is_empty() { "\t".to_string() } else { self.indent.clone() };
                    for l in g.l0..=g.l1 {
                        let k = cut(&sx.work, l, g.left, None, ts);
                        if !k.short {
                            let at = self.split_at(sx, k.start, k.pre);
                            sx.replace(at..at, &unit.repeat(cmd.n()));
                        }
                    }
                    leave(self);
                    self.block_corner(&sx.work, g)
                }
                Simple::VisualOp(_, true) => {
                    // `X`, `Y`, `S`, `R`, `<`: whole lines, as in Visual-line mode.
                    self.mode = Mode::VisualLine;
                    self.dollar = false;
                    self.visual(cmd, sx, c, host, out)
                }
                Simple::BlockEol(op) => {
                    self.dollar = true;
                    let g = self.block_geom(&buf);
                    self.block_op(op, g, cmd, Shape::Block(g.l1 - g.l0, None), sx, out)
                }
                Simple::VisualOp(op, false) => self.block_op(op, g, cmd, shape, sx, out),
                Simple::BlockInsert { append } => {
                    let how = if append { Insert::After } else { Insert::Before };
                    self.block_insert(sx, g, how, cmd, shape, out)
                }
                Simple::ReplaceChar(ch) => {
                    record(self);
                    for l in g.l0..=g.l1 {
                        let k = cut(&sx.work, l, g.left, g.right, ts);
                        if k.short {
                            continue;
                        }
                        let cols = text_width(&k.text, ts);
                        let inner = if ch == '\t' || ch == '\n' { k.text.clone() } else { ch.to_string().repeat(cols) };
                        sx.replace(k.start..k.end, &format!("{}{inner}{}", spaces(k.pre), spaces(k.post)));
                    }
                    leave(self);
                    self.block_corner(&sx.work, g)
                }
                Simple::Join { spaces } => {
                    record(self);
                    leave(self);
                    Self::join(sx, g.l0, (g.l1 - g.l0).max(1), spaces)
                }
                Simple::Put { .. } => {
                    let reg = cmd.reg.unwrap_or('"');
                    let r = self.registers.get(&reg).cloned();
                    leave(self);
                    if matches!(reg, '+' | '*') {
                        self.block_cut(sx, g, Some('_'), false, out);
                        out.paste_clipboard = Some(super::Paste { before: true, count: cmd.n() });
                        return self.block_corner(&sx.work, g);
                    }
                    let Some(r) = r else {
                        self.message = Some(format!("Nothing in register {reg}"));
                        self.error = true;
                        return self.block_corner(&buf, g);
                    };
                    record(self);
                    let at = self.block_cut(sx, g, None, false, out);
                    let corner = self.block_corner(&sx.work, g);
                    if r.block {
                        return self.put_block(sx, corner, &r.text, true, 1);
                    }
                    if !r.linewise && !r.text.contains('\n') {
                        // One line of text goes on every row.
                        let mut shift = 0;
                        for p in at.into_iter().flatten() {
                            sx.replace(p + shift..p + shift, &r.text);
                            shift += r.text.chars().count();
                        }
                        return corner;
                    }
                    Self::put_text(sx, corner, &r.text, r.linewise, true, 1)
                }
                _ => c,
            },
        }
    }

    /// `d`, `y`, `c`, `~`, `u`, `U` on a block.
    fn block_op(&mut self, op: Op, g: Geom, cmd: &Cmd, shape: Shape, sx: &mut Scratch, out: &mut Outcome) -> usize {
        let ts = self.tabstop;
        if op != Op::Yank && self.replay.is_none() && op != Op::Change {
            self.last_change = Some(Recorded { cmd: cmd.clone(), insert: None, visual: Some(shape) });
        }
        let leave = |s: &mut Self| {
            s.last_visual = Some((Mode::VisualBlock, s.anchor, s.cursor));
            s.mode = Mode::Normal;
            s.dollar = false;
        };
        match op {
            Op::Delete | Op::Yank => {
                self.block_cut(sx, g, cmd.reg, op == Op::Yank, out);
                leave(self);
                self.block_corner(&sx.work, g)
            }
            Op::Change => self.block_insert(sx, g, Insert::Change, cmd, shape, out),
            Op::Lower | Op::Upper | Op::ToggleCase => {
                for l in g.l0..=g.l1 {
                    let k = cut(&sx.work, l, g.left, g.right, ts);
                    let (s, e) = (k.start + usize::from(k.pre > 0), k.end.saturating_sub(usize::from(k.post > 0)));
                    if !k.short && s < e {
                        let t = toggle_case(&sx.work.slice(s..e), op);
                        sx.replace(s..e, &t);
                    }
                }
                leave(self);
                self.block_corner(&sx.work, g)
            }
            Op::Indent | Op::Outdent => {
                leave(self);
                self.block_corner(&sx.work, g)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_tabs_and_wide_chars() {
        let b = Buffer::from("ab\tc\n日本語x");
        assert_eq!(vcols(&b, 2, 8), (2, 7), "a tab reaches the next stop");
        assert_eq!(vcols(&b, 3, 8), (8, 8));
        assert_eq!(vcols(&b, 6, 8), (2, 3), "wide chars take two columns");
        assert_eq!(at_vcol(&b, 1, 3, 8), 6);
        assert_eq!(at_vcol(&b, 1, 40, 8), 8, "past the end: the last char");
        let c = cut(&b, 0, 1, Some(3), 8);
        assert_eq!((c.start, c.end, c.pre, c.post, c.text.as_str()), (1, 3, 0, 4, "b  "));
        let c = cut(&b, 1, 1, Some(2), 8);
        assert_eq!((c.start, c.end, c.pre, c.post, c.text.as_str()), (5, 7, 1, 1, "  "));
        assert!(cut(&b, 0, 9, None, 8).short);
        assert_eq!(cut(&b, 1, 2, None, 8).text, "本語x");
    }
}
