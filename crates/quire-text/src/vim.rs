// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Vim keys (design §F.1): a pure state machine over a [`Buffer`].
//!
//! The editor feeds keys to [`VimState::handle`] with the current buffer and
//! selection and gets an [`Outcome`] back: at most one [`Transaction`] (so
//! every Normal-mode command is one undo step), a new selection, undo or
//! redo requests, a `:` command for the editor, or text for the clipboard.
//! Nothing here touches the document; the same keys on the same buffer
//! always give the same outcome, which is what the test table checks.
//!
//! Insert mode is the editor's own typing: keys come back as not
//! [`Outcome::consumed`]. [`Group`] marks where an insert session starts and
//! ends so the editor can make it one undo step.
//!
//! Supported: motions `h j k l w b e W B E ge 0 ^ $ gg G { } % f F t T ; ,
//! gj gk + - _ n N`, counts, operators `d c y > < gu gU g~` with text objects
//! (`iw aw iW aW i" a" i' a' i( a( i[ a[ i{ a{ i< a< ip ap it at`),
//! `x X s S r R o O a A i I p P J gJ u Ctrl-r . ~ D C Y v V gv`, `/ ? n N`,
//! `:w :q :wq :x :{n} :s :%s :d :y`, registers `"a`–`"z` (`"A` appends),
//! `"0`–`"9`, `"-`, `"_` and `"+` (the system clipboard). Not supported:
//! macros, marks, visual block and a vimrc.

mod ex;
mod motions;
mod parse;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ops::Range;

pub use ex::{Ex, Substitute, vim_to_regex};
pub use motions::FindKind;

use crate::{Buffer, Selection, Transaction};
use motions as m;
use parse::{Act, Cmd, Motion, ObjKind, Op, Parsed, Simple, Target, TextObj};

/// A key as the state machine sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A typed character (letters, digits, punctuation, space).
    Char(char),
    Esc,
    Enter,
    Backspace,
    Delete,
    Tab,
    /// Ctrl plus a lowercase letter or `[`.
    Ctrl(char),
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    /// Keys from Vim notation: `"d2w"`, `"ihello<Esc>"`, `"<C-r>"`, `"<CR>"`.
    pub fn parse_seq(s: &str) -> Vec<Self> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if c == '<'
                && let Some(end) = rest.find('>')
                && end > 1
            {
                let name = &rest[1..end];
                let key = match name {
                    "Esc" => Some(Self::Esc),
                    "CR" | "Enter" => Some(Self::Enter),
                    "BS" => Some(Self::Backspace),
                    "Del" => Some(Self::Delete),
                    "Tab" => Some(Self::Tab),
                    "Left" => Some(Self::Left),
                    "Right" => Some(Self::Right),
                    "Up" => Some(Self::Up),
                    "Down" => Some(Self::Down),
                    "Home" => Some(Self::Home),
                    "End" => Some(Self::End),
                    "PageUp" => Some(Self::PageUp),
                    "PageDown" => Some(Self::PageDown),
                    "lt" => Some(Self::Char('<')),
                    n if n.len() == 3 && n.starts_with("C-") => n[2..].chars().next().map(|c| Self::Ctrl(c.to_ascii_lowercase())),
                    _ => None,
                };
                if let Some(k) = key {
                    out.push(k);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
            out.push(Self::Char(c));
            rest = &rest[c.len_utf8()..];
        }
        out
    }
}

/// The editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
}

/// Where an insert session (one undo step) starts and ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Group {
    #[default]
    None,
    /// Remember the text before this outcome's edit: everything until
    /// [`Group::End`] becomes one undo step.
    Begin,
    /// The session is over (this outcome's edit belongs to it).
    End,
}

/// A put from the system clipboard (`"+p`). The editor reads the clipboard
/// and calls [`VimState::put_clipboard`] with its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paste {
    pub before: bool,
    pub count: usize,
}

/// What the editor should do after a key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// False: the editor handles the key itself (typing in Insert mode).
    pub consumed: bool,
    /// One edit, with the selection to set after it.
    pub edit: Option<Transaction>,
    /// Set this selection before applying [`Outcome::edit`]: it's where undo
    /// puts the cursor back (Vim returns to the line's start after `dd`).
    pub before: Option<Selection>,
    /// The new selection when there's no edit.
    pub selection: Option<Selection>,
    pub undo: usize,
    pub redo: usize,
    pub group: Group,
    pub ex: Option<Ex>,
    /// Put this on the system clipboard (a yank or delete into `"+`).
    pub clipboard: Option<String>,
    /// Read the system clipboard and put it (`"+p`).
    pub paste_clipboard: Option<Paste>,
}

/// Editor services the state machine can't compute from the text alone.
pub trait Host {
    /// `gj`/`gk`: the position one display row down (or up) from `pos` in
    /// wrapped text, or `None` to move by lines.
    fn display_row(&mut self, buf: &Buffer, pos: usize, down: bool) -> Option<usize> {
        let _ = (buf, pos, down);
        None
    }

    /// Where `pattern` next matches after (or before) `from`, wrapping
    /// around the buffer. The default finds the text as it is.
    fn search(&mut self, buf: &Buffer, pattern: &str, from: usize, backward: bool) -> Option<usize> {
        if pattern.is_empty() {
            return None;
        }
        let text = buf.to_string();
        let from_b = buf.char_to_byte(from);
        let hit = if backward {
            // Matches that start before `from` (they may run past it).
            let mut end = (from_b + pattern.len()).saturating_sub(1).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].rfind(pattern).or_else(|| text[from_b..].rfind(pattern).map(|i| i + from_b))
        } else {
            let after = (from_b + text[from_b..].chars().next().map_or(0, char::len_utf8)).min(text.len());
            text[after..].find(pattern).map(|i| i + after).or_else(|| text[..after].find(pattern))
        }?;
        Some(buf.byte_to_char(hit))
    }

    /// Lines a page down or up moves.
    fn page_lines(&self) -> usize {
        20
    }
}

/// A [`Host`] for plain text: lines, not display rows, and literal search.
pub struct PlainHost;

impl Host for PlainHost {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Register {
    text: String,
    linewise: bool,
}

/// A change `.` can repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recorded {
    cmd: Cmd,
    /// What was typed in the insert session the command started.
    insert: Option<String>,
    /// For a Visual-mode change: the selection's shape.
    visual: Option<(bool, usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertKind {
    Plain,
    Open { below: bool },
    Replace,
}

#[derive(Debug, Clone)]
struct InsertSession {
    base: Buffer,
    start: usize,
    count: usize,
    kind: InsertKind,
    rec: Option<Recorded>,
}

/// Edits made while carrying out one command, composed into one
/// [`Transaction`] against the buffer the command started from.
struct Scratch {
    orig: Buffer,
    work: Buffer,
    /// (start, end in `orig`, end in `work`) of the changed region.
    span: Option<(usize, usize, usize)>,
}

impl Scratch {
    fn new(buf: &Buffer) -> Self {
        Self { orig: buf.clone(), work: buf.clone(), span: None }
    }

    fn replace(&mut self, r: Range<usize>, text: &str) {
        let (a, b) = (r.start.min(self.work.len_chars()), r.end.min(self.work.len_chars()));
        if self.work.slice(a..b) == text {
            return;
        }
        let tx = Transaction::replace(a, b, text);
        let _ = tx.apply(&mut self.work);
        let delta = text.chars().count() as isize - (b - a) as isize;
        self.span = Some(match self.span {
            None => (a, b, (b as isize + delta) as usize),
            Some((s, oe, ce)) => {
                let end_before = ce.max(b);
                (s.min(a), oe + (end_before - ce), (end_before as isize + delta) as usize)
            }
        });
    }

    fn finish(self) -> Option<Transaction> {
        let (s, oe, ce) = self.span?;
        let new = self.work.slice(s..ce);
        (self.orig.slice(s..oe) != new).then(|| Transaction::replace(s, oe, new))
    }
}

/// What an operator works on: characters, or whole lines (first, last).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Span {
    Chars(Range<usize>),
    Lines(usize, usize),
}

/// How a motion's range is taken by an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// Where Vim puts the cursor after an undo or redo.
///
/// `before` became `after`; the cursor goes to the first changed line, in
/// the column it had before the change (`uh`, line and column) when that is
/// the line, else to its first non-blank.
pub fn undo_cursor(before: &Buffer, after: &Buffer, uh: (usize, usize)) -> usize {
    let (p, end_before, end_after) = diff_span(before, after);
    let mut l = if p == end_before && p == end_after {
        0
    } else {
        // Lines before the one holding the first difference are the same,
        // and so is that line when the difference starts at its line break.
        let l = after.char_to_line(p);
        let ends_here = |b: &Buffer| l < b.len_lines() && b.line_range(l).end == p;
        if ends_here(before) && ends_here(after) { l + 1 } else { l }
    }
    .min(m::last_line(after));
    if uh.0 + 1 == l && l > 0 {
        l -= 1;
    }
    if uh.0 == l { m::at_col(after, l, uh.1, false) } else { m::first_non_blank(after, l) }
}

/// The Vim state of one editor.
#[derive(Debug, Clone, Default)]
pub struct VimState {
    mode: Mode,
    keys: Vec<Key>,
    registers: HashMap<char, Register>,
    last_change: Option<Recorded>,
    last_find: Option<(FindKind, char)>,
    last_search: Option<(String, bool)>,
    /// The column `j`/`k` keep (`usize::MAX`: the end of the line).
    want_col: Option<usize>,
    anchor: usize,
    cursor: usize,
    last_visual: Option<(Mode, usize, usize)>,
    insert: Option<InsertSession>,
    replaced: Vec<Option<char>>,
    replace_typed: String,
    cmdline: Option<(char, String)>,
    /// Text `.` inserts when it repeats an insert session.
    replay: Option<String>,
    message: Option<String>,
    /// One level of indentation for `>` and `<`.
    pub indent: String,
}

fn line_range(buf: &Buffer, l0: usize, l1: usize) -> Range<usize> {
    m::line_start(buf, l0)..m::next_line_start(buf, l1)
}

fn toggle_case(s: &str, op: Op) -> String {
    match op {
        Op::Lower => s.to_lowercase(),
        Op::Upper => s.to_uppercase(),
        _ => s
            .chars()
            .flat_map(|c| -> Vec<char> {
                if c.is_lowercase() {
                    c.to_uppercase().collect()
                } else if c.is_uppercase() {
                    c.to_lowercase().collect()
                } else {
                    vec![c]
                }
            })
            .collect(),
    }
}

fn leading_ws(buf: &Buffer, line: usize) -> String {
    buf.slice(m::line_start(buf, line)..m::first_non_blank(buf, line))
}

/// The text typed in an insert session: what `cur` has that `base` didn't,
/// found by trimming the common start and end.
fn typed_text(base: &Buffer, cur: &Buffer) -> String {
    let (p, _, end) = diff_span(base, cur);
    cur.slice(p..end)
}

/// Bytes `x` and `y` have in common, from the start (or from the end with
/// `rev`), comparing whole chunks where they line up.
fn common_bytes<'a>(mut x: impl Iterator<Item = &'a str>, mut y: impl Iterator<Item = &'a str>, rev: bool) -> usize {
    let (mut xa, mut ya): (&[u8], &[u8]) = (b"", b"");
    let mut n = 0;
    loop {
        if xa.is_empty() {
            match x.next() {
                Some(c) => xa = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        if ya.is_empty() {
            match y.next() {
                Some(c) => ya = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        let k = xa.len().min(ya.len());
        let (xs, ys) = if rev { (&xa[xa.len() - k..], &ya[ya.len() - k..]) } else { (&xa[..k], &ya[..k]) };
        if xs != ys {
            let same = if rev {
                xs.iter().rev().zip(ys.iter().rev()).take_while(|(a, b)| a == b).count()
            } else {
                xs.iter().zip(ys).take_while(|(a, b)| a == b).count()
            };
            return n + same;
        }
        n += k;
        if rev {
            xa = &xa[..xa.len() - k];
            ya = &ya[..ya.len() - k];
        } else {
            xa = &xa[k..];
            ya = &ya[k..];
        }
    }
}

/// Where two texts differ: `(start, end in a, end in b)` in chars, after
/// trimming what they share at the start and the end.
pub fn diff_span(a: &Buffer, b: &Buffer) -> (usize, usize, usize) {
    let (ra, rb) = (a.rope(), b.rope());
    // The char holding the first differing byte starts the difference.
    let pb = common_bytes(ra.chunks(), rb.chunks(), false);
    let p = a.byte_to_char(pb.min(a.len_bytes()));
    let p_byte = a.char_to_byte(p);
    let room = a.len_bytes().min(b.len_bytes()) - p_byte;
    let sb = common_bytes(ra.chunks_at_byte(ra.len_bytes()).0.reversed(), rb.chunks_at_byte(rb.len_bytes()).0.reversed(), true).min(room);
    // The shared end starts at a char boundary at or after the cut.
    let boundary = |buf: &Buffer, at: usize| {
        let c = buf.byte_to_char(at);
        if buf.char_to_byte(c) < at { c + 1 } else { c }
    };
    (p, boundary(a, a.len_bytes() - sb), boundary(b, b.len_bytes() - sb))
}

impl VimState {
    pub fn new() -> Self {
        Self { indent: "  ".into(), ..Self::default() }
    }

    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The `:` or `/` line being typed, with its prompt character.
    pub fn cmdline(&self) -> Option<String> {
        self.cmdline.as_ref().map(|(c, s)| format!("{c}{s}"))
    }

    /// Keys of a command that isn't finished (`2d`, `"a`).
    pub fn pending(&self) -> String {
        self.keys
            .iter()
            .map(|k| match k {
                Key::Char(c) => c.to_string(),
                Key::Ctrl(c) => format!("^{}", c.to_ascii_uppercase()),
                _ => String::new(),
            })
            .collect()
    }

    /// What the status bar shows: `-- INSERT --`, `:s/a/b`, `2d`…
    pub fn status(&self) -> String {
        if let Some(c) = self.cmdline() {
            return c;
        }
        let mode = match self.mode {
            Mode::Normal => "",
            Mode::Insert => "-- INSERT --",
            Mode::Replace => "-- REPLACE --",
            Mode::Visual => "-- VISUAL --",
            Mode::VisualLine => "-- VISUAL LINE --",
        };
        let pending = self.pending();
        match (mode.is_empty(), pending.is_empty()) {
            (true, _) => pending,
            (false, true) => mode.to_string(),
            (false, false) => format!("{mode}  {pending}"),
        }
    }

    /// A message for the status bar (an error, "Pattern not found"), once.
    pub fn take_message(&mut self) -> Option<String> {
        self.message.take()
    }

    /// Whether the editor's own key handling applies (Insert mode).
    pub const fn typing(&self) -> bool {
        matches!(self.mode, Mode::Insert)
    }

    /// Back to Normal mode without finishing anything (the note was
    /// switched or Vim keys turned off).
    pub fn reset(&mut self) {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.last_visual = Some((self.mode, self.anchor, self.cursor));
        }
        self.mode = Mode::Normal;
        self.keys.clear();
        self.insert = None;
        self.cmdline = None;
        self.replaced.clear();
    }

    /// Where the cursor block is drawn for a selection the state machine set.
    pub fn cursor_pos(&self, buf: &Buffer, sel: Selection) -> usize {
        match self.mode {
            Mode::Visual | Mode::VisualLine if self.visual_selection(buf) == sel => self.cursor.min(buf.len_chars()),
            Mode::Visual | Mode::VisualLine if sel.head > sel.anchor => sel.head - 1,
            _ => sel.head.min(buf.len_chars()),
        }
    }

    /// The selection to show when the editor's selection doesn't fit the
    /// mode (a mouse drag starts Visual mode; the Normal-mode cursor can't
    /// sit past a line's last character). `None` when it fits.
    pub fn observe(&mut self, buf: &Buffer, sel: Selection) -> Option<Selection> {
        self.sync(buf, sel);
        let want = match self.mode {
            Mode::Normal if self.cmdline.is_none() => Selection::cursor(Self::clamp(buf, sel.head)),
            Mode::Visual | Mode::VisualLine => self.visual_selection(buf),
            _ => return None,
        };
        (want != sel).then_some(want)
    }

    fn clamp(buf: &Buffer, pos: usize) -> usize {
        let pos = pos.min(buf.len_chars());
        pos.min(m::line_last(buf, m::line_of(buf, pos)))
    }

    fn visual_selection(&self, buf: &Buffer) -> Selection {
        let len = buf.len_chars();
        let (a, c) = (self.anchor.min(len), self.cursor.min(len));
        if self.mode == Mode::VisualLine {
            let (la, lc) = (m::line_of(buf, a), m::line_of(buf, c));
            let r = line_range(buf, la.min(lc), la.max(lc));
            return if c >= a { Selection::new(r.start, r.end) } else { Selection::new(r.end, r.start) };
        }
        if c >= a { Selection::new(a, (c + 1).min(len)) } else { Selection::new((a + 1).min(len), c) }
    }

    /// Take in changes made outside: a selection made with the mouse.
    fn sync(&mut self, buf: &Buffer, sel: Selection) {
        let len = buf.len_chars();
        match self.mode {
            Mode::Normal if !sel.is_empty() && self.cmdline.is_none() => {
                self.mode = Mode::Visual;
                self.adopt(sel);
            }
            Mode::Visual | Mode::VisualLine if self.visual_selection(buf) != sel => {
                if sel.is_empty() {
                    self.mode = Mode::Normal;
                } else {
                    self.mode = Mode::Visual;
                    self.adopt(sel);
                }
            }
            _ => {}
        }
        self.anchor = self.anchor.min(len);
        self.cursor = self.cursor.min(len);
    }

    fn adopt(&mut self, sel: Selection) {
        if sel.head > sel.anchor {
            self.anchor = sel.anchor;
            self.cursor = sel.head - 1;
        } else {
            self.anchor = sel.anchor.saturating_sub(1);
            self.cursor = sel.head;
        }
    }

    /// Handle one key.
    pub fn handle(&mut self, key: Key, buf: &Buffer, sel: Selection, host: &mut dyn Host) -> Outcome {
        self.sync(buf, sel);
        if self.cmdline.is_some() {
            return self.cmdline_key(key, buf, sel, host);
        }
        match self.mode {
            Mode::Insert => match key {
                Key::Esc | Key::Ctrl('[' | 'c') => self.end_insert(buf, sel),
                _ => Outcome::default(),
            },
            Mode::Replace => self.replace_key(key, buf, sel),
            Mode::Normal | Mode::Visual | Mode::VisualLine => {
                self.keys.push(key);
                let visual = self.mode != Mode::Normal;
                let parsed = if visual { parse::parse_visual(&self.keys) } else { parse::parse_normal(&self.keys) };
                match parsed {
                    Parsed::Incomplete => Outcome { consumed: true, ..Outcome::default() },
                    Parsed::Invalid => {
                        self.keys.clear();
                        Outcome { consumed: true, ..Outcome::default() }
                    }
                    Parsed::Done(cmd) => {
                        self.keys.clear();
                        self.run(&cmd, buf, sel, host)
                    }
                }
            }
        }
    }

    /// Put text read from the system clipboard (after [`Outcome::paste_clipboard`]).
    pub fn put_clipboard(&mut self, text: &str, paste: Paste, buf: &Buffer, sel: Selection) -> Outcome {
        self.sync(buf, sel);
        let mut out = Outcome { consumed: true, ..Outcome::default() };
        if text.is_empty() {
            return out;
        }
        let mut sx = Scratch::new(buf);
        let pos = Self::clamp(buf, sel.head);
        let linewise = text.ends_with('\n');
        let pos = Self::put_text(&mut sx, pos, text, linewise, paste.before, paste.count);
        self.finish(sx, pos, &mut out);
        out
    }

    fn run(&mut self, cmd: &Cmd, buf: &Buffer, sel: Selection, host: &mut dyn Host) -> Outcome {
        let mut out = Outcome { consumed: true, ..Outcome::default() };
        let mut sx = Scratch::new(buf);
        let pos = match self.mode {
            Mode::Visual | Mode::VisualLine => self.cursor,
            _ => Self::clamp(buf, sel.head),
        };
        let pos = if self.mode == Mode::Normal {
            self.normal(cmd, &mut sx, pos, host, &mut out)
        } else {
            self.visual(cmd, &mut sx, pos, host, &mut out)
        };
        self.finish(sx, pos, &mut out);
        out
    }

    /// Turn the scratch edits and the new cursor into the outcome.
    fn finish(&mut self, sx: Scratch, pos: usize, out: &mut Outcome) {
        let buf = sx.work.clone();
        let sel = match self.mode {
            Mode::Normal => Selection::cursor(Self::clamp(&buf, pos)),
            Mode::Visual | Mode::VisualLine => {
                self.cursor = pos.min(buf.len_chars());
                self.visual_selection(&buf)
            }
            Mode::Insert | Mode::Replace => Selection::cursor(pos.min(buf.len_chars())),
        };
        match sx.finish() {
            Some(tx) => out.edit = Some(tx.with_selection(sel)),
            None => out.selection = Some(sel),
        }
    }

    // ------------------------------------------------------------ registers

    fn store(&mut self, reg: Option<char>, text: String, linewise: bool, yank: bool, out: &mut Outcome) {
        let r = Register { text, linewise };
        match reg {
            Some('_') => return,
            Some('+' | '*') => out.clipboard = Some(r.text.clone()),
            Some(c @ 'A'..='Z') => {
                let lower = c.to_ascii_lowercase();
                let merged = match self.registers.get(&lower) {
                    Some(old) => {
                        let mut t = old.text.clone();
                        if old.linewise && !t.ends_with('\n') {
                            t.push('\n');
                        }
                        if !old.linewise && r.linewise {
                            t.push('\n');
                        }
                        t.push_str(&r.text);
                        Register { text: t, linewise: old.linewise || r.linewise }
                    }
                    None => r,
                };
                self.registers.insert(lower, merged.clone());
                self.registers.insert('"', merged);
                return;
            }
            Some(c) if c != '"' => {
                self.registers.insert(c, r.clone());
            }
            _ if yank => {
                self.registers.insert('0', r.clone());
            }
            _ if r.linewise || r.text.contains('\n') => {
                for i in (1..9).rev() {
                    let from = char::from(b'0' + i);
                    if let Some(v) = self.registers.get(&from).cloned() {
                        self.registers.insert(char::from(b'0' + i + 1), v);
                    }
                }
                self.registers.insert('1', r.clone());
            }
            _ => {
                self.registers.insert('-', r.clone());
            }
        }
        self.registers.insert('"', r);
    }

    // ------------------------------------------------------------ motions

    fn motion(
        &mut self,
        buf: &Buffer,
        pos: usize,
        mo: &Motion,
        count: Option<usize>,
        op: bool,
        host: &mut dyn Host,
    ) -> Option<(usize, Kind)> {
        let n = count.unwrap_or(1);
        let line = m::line_of(buf, pos);
        let last = m::last_line(buf);
        let keeps_col =
            matches!(mo, Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown | Motion::DisplayUp | Motion::DisplayDown);
        if !keeps_col {
            self.want_col = None;
        }
        let vertical = |s: &mut Self, target: usize| -> usize {
            let col = s.want_col.unwrap_or_else(|| m::col_of(buf, pos));
            s.want_col = Some(col);
            if col == usize::MAX { m::line_last(buf, target) } else { m::at_col(buf, target, col, false) }
        };
        Some(match mo {
            Motion::Left => {
                let ls = m::line_start(buf, line);
                if pos <= ls {
                    return None;
                }
                (pos - n.min(pos - ls), Kind::Exclusive)
            }
            Motion::Right => {
                let limit = if op { m::line_end(buf, line) } else { m::line_last(buf, line) };
                if pos >= limit {
                    return None;
                }
                ((pos + n).min(limit), Kind::Exclusive)
            }
            Motion::Down | Motion::PageDown => {
                let n = if *mo == Motion::PageDown { n * host.page_lines() } else { n };
                if line >= last {
                    return None;
                }
                (vertical(self, (line + n).min(last)), Kind::Linewise)
            }
            Motion::Up | Motion::PageUp => {
                let n = if *mo == Motion::PageUp { n * host.page_lines() } else { n };
                if line == 0 {
                    return None;
                }
                (vertical(self, line.saturating_sub(n)), Kind::Linewise)
            }
            Motion::DisplayDown | Motion::DisplayUp => {
                let down = *mo == Motion::DisplayDown;
                let mut p = pos;
                for _ in 0..n {
                    if let Some(q) = host.display_row(buf, p, down) {
                        p = q;
                    } else {
                        let l = m::line_of(buf, p);
                        let t = if down { (l < last).then_some(l + 1) } else { l.checked_sub(1) };
                        let Some(t) = t else { break };
                        let col = self.want_col.unwrap_or_else(|| m::col_of(buf, pos));
                        self.want_col = Some(col);
                        p = m::at_col(buf, t, col, false);
                    }
                }
                if p == pos {
                    return None;
                }
                (p, Kind::Exclusive)
            }
            Motion::WordFwd(big) => {
                let t = m::word_fwd(buf, pos, n, *big, op);
                if t == pos {
                    return None;
                }
                (t, Kind::Exclusive)
            }
            Motion::WordBack(big) => {
                let t = m::word_back(buf, pos, n, *big);
                if t == pos {
                    return None;
                }
                (t, Kind::Exclusive)
            }
            Motion::WordEnd(big) => (m::word_end(buf, pos, n, *big, false), Kind::Inclusive),
            Motion::WordEndBack(big) => (m::word_end_back(buf, pos, n, *big), Kind::Inclusive),
            Motion::LineStart => (m::line_start(buf, line), Kind::Exclusive),
            Motion::FirstNonBlank => (m::first_non_blank(buf, line), Kind::Exclusive),
            Motion::LineEnd => {
                let l = (line + n - 1).min(last);
                self.want_col = Some(usize::MAX);
                if m::line_is_empty(buf, l) {
                    (m::line_start(buf, l), if l == line { Kind::Exclusive } else { Kind::Inclusive })
                } else {
                    (m::line_end(buf, l) - 1, Kind::Inclusive)
                }
            }
            Motion::NextLineFirst => {
                if line + n > last {
                    return None;
                }
                (m::first_non_blank(buf, line + n), Kind::Linewise)
            }
            Motion::PrevLineFirst => {
                if n > line {
                    return None;
                }
                (m::first_non_blank(buf, line - n), Kind::Linewise)
            }
            Motion::LineFirst => (m::first_non_blank(buf, (line + n - 1).min(last)), Kind::Linewise),
            Motion::GotoFirst => (m::first_non_blank(buf, count.map_or(0, |c| c - 1).min(last)), Kind::Linewise),
            Motion::GotoLast => (m::first_non_blank(buf, count.map_or(last, |c| c - 1).min(last)), Kind::Linewise),
            Motion::ParaFwd | Motion::ParaBack => {
                let (p, incl) = m::paragraph(buf, pos, n, *mo == Motion::ParaFwd)?;
                (p, if incl { Kind::Inclusive } else { Kind::Exclusive })
            }
            Motion::MatchPair => match count {
                Some(c) if c <= 100 => {
                    let l = ((c * (last + 1)).div_ceil(100)).saturating_sub(1);
                    (m::first_non_blank(buf, l), Kind::Linewise)
                }
                Some(_) => return None,
                None => (m::match_pair(buf, pos)?, Kind::Inclusive),
            },
            Motion::Find(k, c) => {
                self.last_find = Some((*k, *c));
                let t = m::find_char(buf, pos, n, *k, *c, false)?;
                (t, if k.forward() { Kind::Inclusive } else { Kind::Exclusive })
            }
            Motion::RepeatFind(rev) => {
                let (k, c) = self.last_find?;
                let k = if *rev { k.reversed() } else { k };
                let t = m::find_char(buf, pos, n, k, c, true)?;
                (t, if k.forward() { Kind::Inclusive } else { Kind::Exclusive })
            }
            Motion::SearchNext(rev) => {
                let Some((pat, back)) = self.last_search.clone() else {
                    self.message = Some("No previous search".into());
                    return None;
                };
                let backward = back != *rev;
                let mut p = pos;
                for _ in 0..n {
                    let Some(q) = host.search(buf, &pat, p, backward) else {
                        self.message = Some(format!("Pattern not found: {pat}"));
                        return None;
                    };
                    p = q;
                }
                (p, Kind::Exclusive)
            }
        })
    }

    /// What an operator works on, from the cursor and a motion's end.
    fn motion_span(buf: &Buffer, pos: usize, to: usize, kind: Kind) -> Span {
        let (a, b) = (pos.min(to), pos.max(to));
        match kind {
            Kind::Linewise => Span::Lines(m::line_of(buf, a), m::line_of(buf, b)),
            Kind::Inclusive => Span::Chars(a..(b + 1).min(buf.len_chars())),
            Kind::Exclusive => {
                let (la, lb) = (m::line_of(buf, a), m::line_of(buf, b));
                // Ending in column 0 of a later line: stop at the end of the line
                // before (and take whole lines when starting in the indent).
                if lb > la && b == m::line_start(buf, lb) {
                    if a <= m::first_non_blank(buf, la) {
                        return Span::Lines(la, lb - 1);
                    }
                    return Span::Chars(a..m::line_end(buf, lb - 1));
                }
                Span::Chars(a..b)
            }
        }
    }

    fn object_span(obj: m::Object) -> Span {
        match obj.lines {
            Some((a, b)) => Span::Lines(a, b),
            None => Span::Chars(obj.range),
        }
    }

    fn object(buf: &Buffer, pos: usize, o: TextObj, count: usize) -> Option<m::Object> {
        match o.kind {
            ObjKind::Word(big) => m::word_object(buf, pos, count, big, o.around),
            ObjKind::Quote(q) => m::quote_object(buf, pos, q, o.around),
            ObjKind::Bracket(a, b) => m::bracket_object(buf, pos, a, b, count, o.around),
            ObjKind::Paragraph => Some(m::paragraph_object(buf, pos, count, o.around)),
            ObjKind::Tag => m::tag_object(buf, pos, count, o.around),
        }
    }

    // ------------------------------------------------------------ operators

    /// Apply `op` to `span`. Returns the new cursor.
    fn apply_op(&mut self, op: Op, span: Span, times: usize, cmd: &Cmd, sx: &mut Scratch, pos: usize, out: &mut Outcome) -> usize {
        let buf = sx.work.clone();
        let (r, linewise, l0, l1) = match span {
            Span::Chars(r) => {
                let (a, b) = (m::line_of(&buf, r.start), m::line_of(&buf, r.end.saturating_sub(1).max(r.start)));
                (r, false, a, b)
            }
            Span::Lines(a, b) => (line_range(&buf, a, b), true, a, b),
        };
        let reg_text = || {
            if linewise {
                let mut t = buf.slice(m::line_start(&buf, l0)..m::line_end(&buf, l1));
                t.push('\n');
                t
            } else {
                buf.slice(r.clone())
            }
        };
        match op {
            Op::Delete => {
                self.store(cmd.reg, reg_text(), linewise, false, out);
                if linewise {
                    out.before = Some(Selection::cursor(m::line_start(&buf, m::line_of(&buf, pos))));
                    let mut range = line_range(&buf, l0, l1);
                    // The last line has no line break after it: take the one before.
                    if l1 == m::last_line(&buf) && l0 > 0 {
                        range.start -= 1;
                    }
                    sx.replace(range, "");
                    let l = l0.min(m::last_line(&sx.work));
                    m::first_non_blank(&sx.work, l)
                } else {
                    sx.replace(r.clone(), "");
                    r.start
                }
            }
            Op::Change => {
                self.store(cmd.reg, reg_text(), linewise, false, out);
                let at = if linewise {
                    let indent = leading_ws(&buf, l0);
                    let s = m::line_start(&buf, l0);
                    sx.replace(s..m::line_end(&buf, l1), &indent);
                    s + indent.chars().count()
                } else {
                    sx.replace(r.clone(), "");
                    r.start
                };
                self.begin_insert(sx, at, 1, InsertKind::Plain, Some(cmd.clone()), out)
            }
            Op::Yank => {
                self.store(cmd.reg, reg_text(), linewise, true, out);
                if linewise {
                    if m::line_of(&buf, pos) == l0 { pos } else { m::at_col(&buf, l0, m::col_of(&buf, pos), false) }
                } else {
                    r.start
                }
            }
            Op::Indent | Op::Outdent => {
                let unit = if self.indent.is_empty() { "\t".to_string() } else { self.indent.clone() };
                let width = unit.chars().count();
                for l in l0..=l1 {
                    let s = m::line_start(&sx.work, l);
                    if op == Op::Indent {
                        if !m::line_is_empty(&sx.work, l) {
                            sx.replace(s..s, &unit.repeat(times));
                        }
                    } else {
                        let mut e = s;
                        let mut cols = 0;
                        while cols < width * times {
                            match sx.work.char_at(e) {
                                Some(' ') => cols += 1,
                                Some('\t') => cols += width,
                                _ => break,
                            }
                            e += 1;
                        }
                        sx.replace(s..e, "");
                    }
                }
                m::first_non_blank(&sx.work, l0)
            }
            Op::Lower | Op::Upper | Op::ToggleCase => {
                let t = toggle_case(&buf.slice(r.clone()), op);
                sx.replace(r.clone(), &t);
                r.start
            }
        }
    }

    // ------------------------------------------------------------ insert sessions

    fn begin_insert(&mut self, sx: &mut Scratch, at: usize, count: usize, kind: InsertKind, cmd: Option<Cmd>, out: &mut Outcome) -> usize {
        let rec = cmd.map(|cmd| Recorded { cmd, insert: None, visual: None });
        if let Some(text) = self.replay.clone() {
            // `.`: type what was typed last time, without entering Insert mode.
            let end = Self::insert_text(sx, at, &text, count, kind);
            let ls = m::line_start(&sx.work, m::line_of(&sx.work, end));
            return if end > ls { end - 1 } else { end };
        }
        self.mode = if kind == InsertKind::Replace { Mode::Replace } else { Mode::Insert };
        self.insert = Some(InsertSession { base: sx.work.clone(), start: at, count, kind, rec });
        out.group = Group::Begin;
        at
    }

    /// Insert `text` at `at`, `count` times (each on a new line for `o`/`O`).
    /// Returns the position after the text.
    fn insert_text(sx: &mut Scratch, at: usize, text: &str, count: usize, kind: InsertKind) -> usize {
        if kind == InsertKind::Replace {
            let line_end = m::line_end(&sx.work, m::line_of(&sx.work, at));
            let n = text.chars().count();
            let end = (at + n).min(line_end);
            let t: String = text.repeat(count);
            sx.replace(at..end.max(at), &t);
            return at + t.chars().count();
        }
        let mut pos = at;
        for i in 0..count {
            if i > 0
                && let InsertKind::Open { .. } = kind
            {
                let l = m::line_of(&sx.work, pos);
                let indent = leading_ws(&sx.work, l);
                let e = m::line_end(&sx.work, l);
                sx.replace(e..e, &format!("\n{indent}"));
                pos = e + 1 + indent.chars().count();
            }
            sx.replace(pos..pos, text);
            pos += text.chars().count();
        }
        pos
    }

    fn end_insert(&mut self, buf: &Buffer, sel: Selection) -> Outcome {
        let mut out = Outcome { consumed: true, group: Group::End, ..Outcome::default() };
        let kind = self.mode;
        self.mode = Mode::Normal;
        let Some(s) = self.insert.take() else {
            out.selection = Some(Selection::cursor(Self::clamp(buf, sel.head)));
            return out;
        };
        let typed = if kind == Mode::Replace { std::mem::take(&mut self.replace_typed) } else { typed_text(&s.base, buf) };
        self.replaced.clear();
        let mut sx = Scratch::new(buf);
        let mut pos = sel.head.min(buf.len_chars());
        if s.count > 1 && !typed.is_empty() {
            pos = match s.kind {
                InsertKind::Open { .. } => {
                    let e = m::line_end(buf, m::line_of(buf, pos));
                    let indent = leading_ws(&s.base, m::line_of(&s.base, s.start.min(s.base.len_chars())));
                    let t = format!("\n{indent}{typed}").repeat(s.count - 1);
                    sx.replace(e..e, &t);
                    e + t.chars().count()
                }
                _ => Self::insert_text(&mut sx, pos, &typed, s.count - 1, InsertKind::Plain),
            };
        }
        let ls = m::line_start(&sx.work, m::line_of(&sx.work, pos));
        if pos > ls {
            pos -= 1;
        }
        if let Some(mut rec) = s.rec {
            rec.insert = Some(typed);
            self.last_change = Some(rec);
        }
        self.finish(sx, pos, &mut out);
        out
    }

    fn replace_key(&mut self, key: Key, buf: &Buffer, sel: Selection) -> Outcome {
        let mut out = Outcome { consumed: true, ..Outcome::default() };
        let pos = sel.head.min(buf.len_chars());
        let mut sx = Scratch::new(buf);
        let new_pos = match key {
            Key::Esc | Key::Ctrl('[' | 'c') => return self.end_insert(buf, sel),
            Key::Char(c) => {
                let le = m::line_end(buf, m::line_of(buf, pos));
                if pos < le {
                    self.replaced.push(buf.char_at(pos));
                    sx.replace(pos..pos + 1, &c.to_string());
                } else {
                    self.replaced.push(None);
                    sx.replace(pos..pos, &c.to_string());
                }
                self.replace_typed.push(c);
                pos + 1
            }
            Key::Enter => {
                self.replaced.push(None);
                sx.replace(pos..pos, "\n");
                self.replace_typed.push('\n');
                pos + 1
            }
            Key::Backspace => match self.replaced.pop() {
                Some(Some(orig)) => {
                    sx.replace(pos - 1..pos, &orig.to_string());
                    self.replace_typed.pop();
                    pos - 1
                }
                Some(None) => {
                    sx.replace(pos - 1..pos, "");
                    self.replace_typed.pop();
                    pos - 1
                }
                None => {
                    let ls = m::line_start(buf, m::line_of(buf, pos));
                    if pos > ls { pos - 1 } else { pos }
                }
            },
            _ => return Outcome::default(),
        };
        self.finish(sx, new_pos, &mut out);
        out
    }

    // ------------------------------------------------------------ the command line

    fn cmdline_key(&mut self, key: Key, buf: &Buffer, sel: Selection, host: &mut dyn Host) -> Outcome {
        let mut out = Outcome { consumed: true, ..Outcome::default() };
        let Some((prompt, mut text)) = self.cmdline.take() else { return out };
        match key {
            Key::Char(c) => text.push(c),
            Key::Tab => text.push('\t'),
            Key::Backspace if text.is_empty() => return out,
            Key::Backspace => {
                text.pop();
            }
            Key::Esc | Key::Ctrl('[' | 'c') => return out,
            Key::Enter => {
                let pos = Self::clamp(buf, sel.head);
                let mut sx = Scratch::new(buf);
                let pos =
                    if prompt == ':' { self.ex(&text, &mut sx, pos, &mut out) } else { self.search(&text, prompt == '?', buf, pos, host) };
                self.finish(sx, pos, &mut out);
                return out;
            }
            _ => {}
        }
        self.cmdline = Some((prompt, text));
        out
    }

    fn search(&mut self, pattern: &str, backward: bool, buf: &Buffer, pos: usize, host: &mut dyn Host) -> usize {
        let pat = if pattern.is_empty() {
            let Some((p, _)) = &self.last_search else {
                self.message = Some("No previous search".into());
                return pos;
            };
            p.clone()
        } else {
            pattern.to_string()
        };
        self.last_search = Some((pat, backward));
        self.motion(buf, pos, &Motion::SearchNext(false), None, false, host).map_or(pos, |(p, _)| p)
    }

    fn ex(&mut self, text: &str, sx: &mut Scratch, pos: usize, out: &mut Outcome) -> usize {
        let buf = sx.work.clone();
        let visual = self.last_visual.map(|(_, a, c)| {
            let (la, lc) = (m::line_of(&buf, a.min(buf.len_chars())), m::line_of(&buf, c.min(buf.len_chars())));
            (la.min(lc), la.max(lc))
        });
        let at = ex::Lines { cur: m::line_of(&buf, pos), last: m::last_line(&buf), visual };
        let cmd = Cmd { reg: None, count: None, act: Act::Do(Simple::Escape) };
        match ex::parse(text, &at) {
            ex::Parsed::Nothing => pos,
            ex::Parsed::Goto(l) => m::first_non_blank(&buf, l),
            ex::Parsed::DeleteLines(r) => self.apply_op(Op::Delete, Span::Lines(r.start, r.end - 1), 1, &cmd, sx, pos, out),
            ex::Parsed::YankLines(r) => {
                self.apply_op(Op::Yank, Span::Lines(r.start, r.end - 1), 1, &cmd, sx, pos, out);
                pos
            }
            ex::Parsed::Ex(e) => {
                out.ex = Some(e);
                pos
            }
            ex::Parsed::Error(e) => {
                self.message = Some(e);
                pos
            }
        }
    }

    // ------------------------------------------------------------ Normal mode

    fn normal(&mut self, cmd: &Cmd, sx: &mut Scratch, pos: usize, host: &mut dyn Host, out: &mut Outcome) -> usize {
        let n = cmd.n();
        let buf = sx.work.clone();
        let line = m::line_of(&buf, pos);
        let last = m::last_line(&buf);
        // A change `.` can repeat (insert sessions record theirs at Escape).
        let record = |s: &mut Self| {
            if s.replay.is_none() {
                s.last_change = Some(Recorded { cmd: cmd.clone(), insert: None, visual: None });
            }
        };
        match &cmd.act {
            Act::Move(mo) => self.motion(&buf, pos, mo, cmd.count, false, host).map_or(pos, |(p, _)| p),
            Act::Select(_) => pos,
            Act::Op(op, target) => {
                let span = match target {
                    Target::Lines => {
                        // Like Vim, `2dd` on the last line does nothing.
                        if n > 1 && line == last {
                            return pos;
                        }
                        Span::Lines(line, (line + n - 1).min(last))
                    }
                    Target::Object(o) => match Self::object(&buf, pos, *o, n) {
                        Some(obj) => Self::object_span(obj),
                        None => return pos,
                    },
                    // `cw` on a word changes to its end, not up to the next word.
                    Target::Motion(Motion::WordFwd(big)) if *op == Op::Change && m::cls(&buf, pos, *big) != 0 => {
                        let e = m::word_end(&buf, pos, n, *big, true);
                        Self::motion_span(&buf, pos, e, Kind::Inclusive)
                    }
                    Target::Motion(mo) => match self.motion(&buf, pos, mo, cmd.count, true, host) {
                        Some((to, kind)) => Self::motion_span(&buf, pos, to, kind),
                        None => return pos,
                    },
                };
                if *op != Op::Yank && *op != Op::Change {
                    record(self);
                }
                self.apply_op(*op, span, 1, cmd, sx, pos, out)
            }
            Act::Do(s) => self.simple(*s, cmd, sx, pos, host, out),
        }
    }

    fn simple(&mut self, s: Simple, cmd: &Cmd, sx: &mut Scratch, pos: usize, host: &mut dyn Host, out: &mut Outcome) -> usize {
        let n = cmd.n();
        let buf = sx.work.clone();
        let line = m::line_of(&buf, pos);
        let (ls, le) = (m::line_start(&buf, line), m::line_end(&buf, line));
        let last = m::last_line(&buf);
        let rec = |st: &mut Self| {
            if st.replay.is_none() {
                st.last_change = Some(Recorded { cmd: cmd.clone(), insert: None, visual: None });
            }
        };
        let as_op = |st: &mut Self, sx: &mut Scratch, op: Op, span: Span, out: &mut Outcome| st.apply_op(op, span, 1, cmd, sx, pos, out);
        match s {
            Simple::DelChar => {
                if ls == le {
                    return pos;
                }
                rec(self);
                as_op(self, sx, Op::Delete, Span::Chars(pos..(pos + n).min(le)), out)
            }
            Simple::DelCharBack => {
                if pos == ls {
                    return pos;
                }
                rec(self);
                as_op(self, sx, Op::Delete, Span::Chars(pos - n.min(pos - ls)..pos), out)
            }
            Simple::SubstChar => {
                let end = if ls == le { pos } else { (pos + n).min(le) };
                as_op(self, sx, Op::Change, Span::Chars(pos..end), out)
            }
            Simple::SubstLine => as_op(self, sx, Op::Change, Span::Lines(line, (line + n - 1).min(last)), out),
            Simple::ChangeEnd | Simple::DelEnd => {
                let end = m::line_end(&buf, (line + n - 1).min(last));
                if s == Simple::DelEnd {
                    if end == pos {
                        return pos;
                    }
                    rec(self);
                    as_op(self, sx, Op::Delete, Span::Chars(pos..end), out)
                } else {
                    as_op(self, sx, Op::Change, Span::Chars(pos..end), out)
                }
            }
            Simple::YankLine => as_op(self, sx, Op::Yank, Span::Lines(line, (line + n - 1).min(last)), out),
            Simple::ReplaceChar(c) => {
                if pos + n > le || ls == le {
                    return pos;
                }
                rec(self);
                if c == '\n' {
                    sx.replace(pos..pos + n, "\n");
                    return pos + 1;
                }
                sx.replace(pos..pos + n, &c.to_string().repeat(n));
                pos + n - 1
            }
            Simple::ReplaceMode => {
                self.replace_typed.clear();
                self.replaced.clear();
                if let Some(text) = self.replay.clone() {
                    let end = Self::insert_text(sx, pos, &text, n, InsertKind::Replace);
                    return end.saturating_sub(1).max(ls);
                }
                self.begin_insert(sx, pos, n, InsertKind::Replace, Some(cmd.clone()), out)
            }
            Simple::OpenBelow | Simple::OpenAbove => {
                let indent = leading_ws(&buf, line);
                let at = if s == Simple::OpenBelow {
                    sx.replace(le..le, &format!("\n{indent}"));
                    le + 1 + indent.chars().count()
                } else {
                    sx.replace(ls..ls, &format!("{indent}\n"));
                    ls + indent.chars().count()
                };
                self.begin_insert(sx, at, n, InsertKind::Open { below: s == Simple::OpenBelow }, Some(cmd.clone()), out)
            }
            Simple::Append => {
                let at = if ls == le { pos } else { (pos + 1).min(le) };
                self.begin_insert(sx, at, n, InsertKind::Plain, Some(cmd.clone()), out)
            }
            Simple::AppendEnd => self.begin_insert(sx, le, n, InsertKind::Plain, Some(cmd.clone()), out),
            Simple::Insert => self.begin_insert(sx, pos, n, InsertKind::Plain, Some(cmd.clone()), out),
            Simple::InsertStart => {
                let at = m::first_non_blank(&buf, line);
                self.begin_insert(sx, at, n, InsertKind::Plain, Some(cmd.clone()), out)
            }
            Simple::Put { before } => {
                let reg = cmd.reg.unwrap_or('"');
                if matches!(reg, '+' | '*') {
                    out.paste_clipboard = Some(Paste { before, count: n });
                    return pos;
                }
                let Some(r) = self.registers.get(&reg).cloned() else {
                    self.message = Some(format!("Nothing in register {reg}"));
                    return pos;
                };
                rec(self);
                Self::put_text(sx, pos, &r.text, r.linewise, before, n)
            }
            Simple::Join { spaces } => {
                if line >= last {
                    return pos;
                }
                rec(self);
                Self::join(sx, line, n.max(2) - 1, spaces)
            }
            Simple::Undo => {
                out.undo = n;
                pos
            }
            Simple::Redo => {
                out.redo = n;
                pos
            }
            Simple::Repeat => self.repeat(cmd.count, sx, pos, host, out),
            Simple::Visual | Simple::VisualLine => {
                self.mode = if s == Simple::Visual { Mode::Visual } else { Mode::VisualLine };
                self.anchor = pos;
                pos
            }
            Simple::Reselect => match self.last_visual {
                Some((mode, a, c)) => {
                    self.mode = mode;
                    self.anchor = a.min(buf.len_chars());
                    c.min(buf.len_chars())
                }
                None => pos,
            },
            Simple::ToggleCaseChar => {
                if ls == le {
                    return pos;
                }
                rec(self);
                let end = (pos + n).min(le);
                let t = toggle_case(&buf.slice(pos..end), Op::ToggleCase);
                sx.replace(pos..end, &t);
                end.min(m::line_last(&sx.work, line))
            }
            Simple::Cmdline(c) => {
                self.cmdline = Some((c, String::new()));
                pos
            }
            Simple::Escape | Simple::Swap | Simple::VisualOp(..) => pos,
            Simple::WriteQuit => {
                out.ex = Some(Ex::WriteQuit);
                pos
            }
            Simple::QuitNow => {
                out.ex = Some(Ex::Quit);
                pos
            }
        }
    }

    /// `J`: join `joins` following lines onto `line`.
    fn join(sx: &mut Scratch, line: usize, joins: usize, spaces: bool) -> usize {
        let mut at = m::line_end(&sx.work, line);
        for _ in 0..joins {
            let l = m::line_of(&sx.work, at);
            if l >= m::last_line(&sx.work) {
                break;
            }
            let e = m::line_end(&sx.work, l);
            let next = l + 1;
            let cut_end = if spaces { m::first_non_blank(&sx.work, next) } else { m::line_start(&sx.work, next) };
            let next_empty = cut_end == m::line_end(&sx.work, next);
            let ends_blank = e > m::line_start(&sx.work, l) && sx.work.char_at(e - 1).is_some_and(|c| c == ' ' || c == '\t');
            let sep = if !spaces || next_empty || ends_blank || sx.work.char_at(cut_end) == Some(')') || e == m::line_start(&sx.work, l) {
                ""
            } else {
                " "
            };
            sx.replace(e..cut_end, sep);
            // The cursor goes where the lines met (on the space, if one was put there).
            at = e;
        }
        at
    }

    fn put_text(sx: &mut Scratch, pos: usize, text: &str, linewise: bool, before: bool, n: usize) -> usize {
        let buf = sx.work.clone();
        let line = m::line_of(&buf, pos.min(buf.len_chars()));
        if linewise {
            let mut t = text.to_string();
            if !t.ends_with('\n') {
                t.push('\n');
            }
            let rep = t.repeat(n);
            if before {
                let at = m::line_start(&buf, line);
                sx.replace(at..at, &rep);
                return m::first_non_blank(&sx.work, line);
            }
            if line == m::last_line(&buf) {
                let at = buf.len_chars();
                let body = rep.trim_end_matches('\n');
                sx.replace(at..at, &format!("\n{body}"));
            } else {
                let at = m::next_line_start(&buf, line);
                sx.replace(at..at, &rep);
            }
            return m::first_non_blank(&sx.work, line + 1);
        }
        let rep = text.repeat(n);
        let (ls, le) = (m::line_start(&buf, line), m::line_end(&buf, line));
        let at = if before || ls == le { pos } else { (pos + 1).min(le) };
        sx.replace(at..at, &rep);
        if rep.contains('\n') { at } else { (at + rep.chars().count()).saturating_sub(1).max(at) }
    }

    fn repeat(&mut self, count: Option<usize>, sx: &mut Scratch, pos: usize, host: &mut dyn Host, out: &mut Outcome) -> usize {
        let Some(mut rec) = self.last_change.clone() else { return pos };
        if count.is_some() {
            rec.cmd.count = count;
            if let Some(last) = self.last_change.as_mut() {
                last.cmd.count = count;
            }
        }
        self.replay = Some(rec.insert.clone().unwrap_or_default());
        let p = if let Some((linewise, lines, chars)) = rec.visual {
            let buf = sx.work.clone();
            let line = m::line_of(&buf, pos);
            self.mode = if linewise { Mode::VisualLine } else { Mode::Visual };
            self.anchor = pos;
            self.cursor = if linewise || lines > 0 {
                let l = (line + lines).min(m::last_line(&buf));
                if linewise { m::line_start(&buf, l) } else { m::at_col(&buf, l, chars, false) }
            } else {
                (pos + chars).min(m::line_last(&buf, line))
            };
            let c = self.cursor;
            self.visual(&rec.cmd, sx, c, host, out)
        } else {
            self.normal(&rec.cmd, sx, pos, host, out)
        };
        self.replay = None;
        // A repeated change never leaves an insert session open.
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            self.mode = Mode::Normal;
            self.insert = None;
        }
        out.group = Group::None;
        p
    }

    // ------------------------------------------------------------ Visual mode

    fn visual(&mut self, cmd: &Cmd, sx: &mut Scratch, pos: usize, host: &mut dyn Host, out: &mut Outcome) -> usize {
        let buf = sx.work.clone();
        let len = buf.len_chars();
        let n = cmd.n();
        let lines = self.mode == Mode::VisualLine;
        let (a, c) = (self.anchor.min(len), pos.min(len));
        let (lo, hi) = (a.min(c), a.max(c));
        let (l0, l1) = (m::line_of(&buf, lo), m::line_of(&buf, hi));
        let char_range = lo..(hi + 1).min(len);
        let leave = |s: &mut Self| {
            s.last_visual = Some((s.mode, s.anchor, s.cursor));
            s.mode = Mode::Normal;
        };
        // What `.` needs to select the same shape again.
        let shape = (lines, l1 - l0, if l1 == l0 { hi - lo } else { m::col_of(&buf, hi) });
        let record = |s: &mut Self| {
            if s.replay.is_none() {
                s.last_change = Some(Recorded { cmd: cmd.clone(), insert: None, visual: Some(shape) });
            }
        };
        match &cmd.act {
            Act::Move(mo) => {
                let (p, _) = self.motion(&buf, c, mo, cmd.count, false, host).unwrap_or((c, Kind::Exclusive));
                if *mo == Motion::LineEnd && !lines {
                    return m::line_end(&buf, m::line_of(&buf, p)).min(len.saturating_sub(1)).max(p);
                }
                p
            }
            Act::Select(o) => {
                let Some(obj) = Self::object(&buf, c, *o, n) else { return c };
                if obj.range.is_empty() {
                    return c;
                }
                if let Some((a, b)) = obj.lines {
                    // Like Vim, the start keeps its column.
                    self.mode = Mode::VisualLine;
                    self.anchor = m::at_col(&buf, a, m::col_of(&buf, self.anchor.min(len)), false);
                    return m::line_start(&buf, b);
                }
                self.anchor = obj.range.start;
                obj.range.end - 1
            }
            Act::Op(..) => c,
            Act::Do(s) => match *s {
                Simple::VisualOp(op, force_lines) => {
                    let lw = lines || force_lines;
                    let span = if lw { Span::Lines(l0, l1) } else { Span::Chars(char_range) };
                    if op != Op::Yank {
                        record(self);
                    }
                    let times = if matches!(op, Op::Indent | Op::Outdent) { n } else { 1 };
                    let start = if lw { m::line_start(&buf, l0) } else { lo };
                    leave(self);
                    let p = self.apply_op(op, span, times, cmd, sx, start, out);
                    if op == Op::Yank { if lw { m::line_start(&buf, l0) } else { lo } } else { p }
                }
                Simple::ReplaceChar(ch) => {
                    record(self);
                    let range = if lines { line_range(&buf, l0, l1) } else { char_range };
                    let t: String = buf.slice(range.clone()).chars().map(|x| if x == '\n' { x } else { ch }).collect();
                    sx.replace(range.clone(), &t);
                    leave(self);
                    range.start
                }
                Simple::Join { spaces } => {
                    record(self);
                    leave(self);
                    Self::join(sx, l0, (l1 - l0).max(1), spaces)
                }
                Simple::Put { .. } => {
                    let reg = cmd.reg.unwrap_or('"');
                    let span = if lines { Span::Lines(l0, l1) } else { Span::Chars(char_range) };
                    leave(self);
                    if matches!(reg, '+' | '*') {
                        let p = self.apply_op(Op::Delete, span, 1, &Cmd { reg: Some('_'), ..cmd.clone() }, sx, lo, out);
                        out.paste_clipboard = Some(Paste { before: true, count: n });
                        return p;
                    }
                    let Some(r) = self.registers.get(&reg).cloned() else {
                        self.message = Some(format!("Nothing in register {reg}"));
                        return lo;
                    };
                    record(self);
                    // The replaced text goes to the unnamed register.
                    let del = Cmd { reg: None, ..cmd.clone() };
                    let p = self.apply_op(Op::Delete, span, 1, &del, sx, lo, out);
                    if !lines {
                        if r.linewise {
                            let body = r.text.trim_end_matches('\n');
                            sx.replace(p..p, &format!("\n{body}\n"));
                            return m::first_non_blank(&sx.work, m::line_of(&sx.work, p) + 1);
                        }
                        sx.replace(p..p, &r.text);
                        return (p + r.text.chars().count()).saturating_sub(1).max(p);
                    }
                    // Whole lines were replaced: the text goes where they were.
                    let body = r.text.trim_end_matches('\n');
                    if sx.work.len_chars() == 0 {
                        sx.replace(0..0, body);
                        0
                    } else if l1 == m::last_line(&buf) && l0 > 0 {
                        let e = sx.work.len_chars();
                        sx.replace(e..e, &format!("\n{body}"));
                        m::first_non_blank(&sx.work, l0)
                    } else {
                        let at = m::line_start(&sx.work, l0);
                        sx.replace(at..at, &format!("{body}\n"));
                        m::first_non_blank(&sx.work, l0)
                    }
                }
                Simple::Swap => {
                    let old = self.anchor;
                    self.anchor = c;
                    old
                }
                Simple::Visual | Simple::VisualLine => {
                    let want = if *s == Simple::Visual { Mode::Visual } else { Mode::VisualLine };
                    if self.mode == want {
                        leave(self);
                    } else {
                        self.mode = want;
                    }
                    c
                }
                Simple::Escape => {
                    leave(self);
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
                        self.anchor = la.min(len);
                        lc.min(len)
                    }
                    None => c,
                },
                _ => c,
            },
        }
    }
}
