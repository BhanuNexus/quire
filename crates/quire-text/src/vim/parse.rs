// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turning the keys typed so far into a command (`"a2d3w` → delete six
//! words into register a). The parser is re-run on the pending keys after
//! every key, so there is no hidden state between keys.

use super::Key;
use super::motions::FindKind;

/// Commands longer than this are nonsense; the keys are dropped.
const MAX_PENDING: usize = 24;
/// Counts are capped so `99999999x` can't stall the editor.
pub(super) const MAX_COUNT: usize = 9_999;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Motion {
    Left,
    Right,
    Up,
    Down,
    DisplayUp,
    DisplayDown,
    WordFwd(bool),
    WordBack(bool),
    WordEnd(bool),
    WordEndBack(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    /// `+` / Enter: first non-blank of a following line.
    NextLineFirst,
    /// `-`: first non-blank of a preceding line.
    PrevLineFirst,
    /// `_`: first non-blank, `count - 1` lines down (linewise).
    LineFirst,
    GotoFirst,
    GotoLast,
    ParaFwd,
    ParaBack,
    MatchPair,
    Find(FindKind, char),
    RepeatFind(bool),
    SearchNext(bool),
    PageDown,
    PageUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObjKind {
    Word(bool),
    Quote(char),
    Bracket(char, char),
    Paragraph,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TextObj {
    pub around: bool,
    pub kind: ObjKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Lower,
    Upper,
    ToggleCase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    Motion(Motion),
    Object(TextObj),
    /// `dd`, `cc`, `>>`…: `count` whole lines.
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Simple {
    DelChar,
    DelCharBack,
    SubstChar,
    SubstLine,
    ReplaceChar(char),
    ReplaceMode,
    OpenBelow,
    OpenAbove,
    Append,
    AppendEnd,
    Insert,
    InsertStart,
    Put {
        before: bool,
    },
    Join {
        spaces: bool,
    },
    Undo,
    Redo,
    Repeat,
    Visual,
    VisualLine,
    Reselect,
    ToggleCaseChar,
    DelEnd,
    ChangeEnd,
    YankLine,
    Cmdline(char),
    Escape,
    /// `ZZ` and `ZQ`.
    WriteQuit,
    QuitNow,
    /// Visual mode: `o` swaps the selection's ends.
    Swap,
    /// Visual mode: an operator acts on the selection.
    VisualOp(Op, bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Act {
    Move(Motion),
    Op(Op, Target),
    Do(Simple),
    /// Visual mode: extend the selection to a text object.
    Select(TextObj),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cmd {
    pub reg: Option<char>,
    /// The count before the command times the count after an operator.
    pub count: Option<usize>,
    pub act: Act,
}

impl Cmd {
    pub(super) fn n(&self) -> usize {
        self.count.unwrap_or(1)
    }
}

pub(super) enum Parsed {
    Incomplete,
    Invalid,
    Done(Cmd),
}

fn reg_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '"' | '+' | '*' | '_' | '-')
}

struct Cursor<'a> {
    keys: &'a [Key],
    i: usize,
}

impl Cursor<'_> {
    fn next(&mut self) -> Option<Key> {
        let k = self.keys.get(self.i).copied();
        self.i += usize::from(k.is_some());
        k
    }

    fn peek(&self) -> Option<Key> {
        self.keys.get(self.i).copied()
    }

    /// Digits as a count; `0` alone is not a count (it's a motion).
    fn count(&mut self) -> Option<usize> {
        let mut n: Option<usize> = None;
        while let Some(Key::Char(c)) = self.peek() {
            match (c.to_digit(10), n) {
                (Some(0), None) | (None, _) => break,
                (Some(d), _) => {
                    n = Some((n.unwrap_or(0) * 10 + d as usize).min(MAX_COUNT));
                    self.i += 1;
                }
            }
        }
        n
    }
}

/// `None`: more keys are needed.
type Step<T> = Result<Option<T>, ()>;

fn motion_key(c: &mut Cursor<'_>, k: Key) -> Step<Motion> {
    let m = match k {
        Key::Char('h') | Key::Left | Key::Backspace => Motion::Left,
        Key::Char('l' | ' ') | Key::Right => Motion::Right,
        Key::Char('j') | Key::Down | Key::Ctrl('n' | 'j') => Motion::Down,
        Key::Char('k') | Key::Up | Key::Ctrl('p') => Motion::Up,
        Key::Char('w') => Motion::WordFwd(false),
        Key::Char('W') => Motion::WordFwd(true),
        Key::Char('b') => Motion::WordBack(false),
        Key::Char('B') => Motion::WordBack(true),
        Key::Char('e') => Motion::WordEnd(false),
        Key::Char('E') => Motion::WordEnd(true),
        Key::Char('0') | Key::Home => Motion::LineStart,
        Key::Char('^') => Motion::FirstNonBlank,
        Key::Char('$') | Key::End => Motion::LineEnd,
        Key::Char('+') | Key::Enter => Motion::NextLineFirst,
        Key::Char('-') => Motion::PrevLineFirst,
        Key::Char('_') => Motion::LineFirst,
        Key::Char('G') => Motion::GotoLast,
        Key::Char('}') => Motion::ParaFwd,
        Key::Char('{') => Motion::ParaBack,
        Key::Char('%') => Motion::MatchPair,
        Key::Char(';') => Motion::RepeatFind(false),
        Key::Char(',') => Motion::RepeatFind(true),
        Key::Char('n') => Motion::SearchNext(false),
        Key::Char('N') => Motion::SearchNext(true),
        Key::PageDown | Key::Ctrl('f') => Motion::PageDown,
        Key::PageUp | Key::Ctrl('b') => Motion::PageUp,
        Key::Char(f @ ('f' | 'F' | 't' | 'T')) => {
            let kind = match f {
                'f' => FindKind::Forward,
                'F' => FindKind::Backward,
                't' => FindKind::Till,
                _ => FindKind::TillBack,
            };
            return match c.next() {
                None => Ok(None),
                Some(Key::Char(ch)) => Ok(Some(Motion::Find(kind, ch))),
                Some(Key::Tab) => Ok(Some(Motion::Find(kind, '\t'))),
                Some(_) => Err(()),
            };
        }
        Key::Char('g') => {
            return match c.next() {
                None => Ok(None),
                Some(Key::Char('g')) => Ok(Some(Motion::GotoFirst)),
                Some(Key::Char('j') | Key::Down) => Ok(Some(Motion::DisplayDown)),
                Some(Key::Char('k') | Key::Up) => Ok(Some(Motion::DisplayUp)),
                Some(Key::Char('e')) => Ok(Some(Motion::WordEndBack(false))),
                Some(Key::Char('E')) => Ok(Some(Motion::WordEndBack(true))),
                Some(Key::Char('_')) => Ok(Some(Motion::LineEnd)),
                Some(Key::Char('0')) => Ok(Some(Motion::LineStart)),
                _ => Err(()),
            };
        }
        _ => return Err(()),
    };
    Ok(Some(m))
}

fn object(c: &mut Cursor<'_>, around: bool) -> Step<TextObj> {
    let kind = match c.next() {
        None => return Ok(None),
        Some(Key::Char('w')) => ObjKind::Word(false),
        Some(Key::Char('W')) => ObjKind::Word(true),
        Some(Key::Char(q @ ('"' | '\'' | '`'))) => ObjKind::Quote(q),
        Some(Key::Char('(' | ')' | 'b')) => ObjKind::Bracket('(', ')'),
        Some(Key::Char('[' | ']')) => ObjKind::Bracket('[', ']'),
        Some(Key::Char('{' | '}' | 'B')) => ObjKind::Bracket('{', '}'),
        Some(Key::Char('<' | '>')) => ObjKind::Bracket('<', '>'),
        Some(Key::Char('p')) => ObjKind::Paragraph,
        Some(Key::Char('t')) => ObjKind::Tag,
        Some(_) => return Err(()),
    };
    Ok(Some(TextObj { around, kind }))
}

/// The operator a key starts, if any (`g` operators need the next key).
fn operator(c: &mut Cursor<'_>, k: Key) -> Step<(Op, Vec<Key>)> {
    Ok(Some(match k {
        Key::Char('d') => (Op::Delete, vec![k]),
        Key::Char('c') => (Op::Change, vec![k]),
        Key::Char('y') => (Op::Yank, vec![k]),
        Key::Char('>') => (Op::Indent, vec![k]),
        Key::Char('<') => (Op::Outdent, vec![k]),
        Key::Char('g') => match c.peek() {
            None => return Ok(None),
            Some(k2 @ Key::Char('u')) => {
                c.i += 1;
                (Op::Lower, vec![k, k2])
            }
            Some(k2 @ Key::Char('U')) => {
                c.i += 1;
                (Op::Upper, vec![k, k2])
            }
            Some(k2 @ Key::Char('~')) => {
                c.i += 1;
                (Op::ToggleCase, vec![k, k2])
            }
            Some(_) => return Err(()),
        },
        _ => return Err(()),
    }))
}

fn prefix(c: &mut Cursor<'_>) -> Step<(Option<char>, Option<usize>)> {
    let mut reg = None;
    let mut count = c.count();
    if c.peek() == Some(Key::Char('"')) {
        c.i += 1;
        match c.next() {
            None => return Ok(None),
            Some(Key::Char(r)) if reg_ok(r) => reg = Some(r),
            Some(_) => return Err(()),
        }
        if let Some(n) = c.count() {
            count = Some((count.unwrap_or(1) * n).min(MAX_COUNT));
        }
    }
    Ok(Some((reg, count)))
}

fn simple(c: &mut Cursor<'_>, k: Key) -> Step<Simple> {
    Ok(Some(match k {
        Key::Char('x') | Key::Delete => Simple::DelChar,
        Key::Char('X') => Simple::DelCharBack,
        Key::Char('s') => Simple::SubstChar,
        Key::Char('S') => Simple::SubstLine,
        Key::Char('r') => match c.next() {
            None => return Ok(None),
            Some(Key::Char(ch)) => Simple::ReplaceChar(ch),
            Some(Key::Enter) => Simple::ReplaceChar('\n'),
            Some(Key::Tab) => Simple::ReplaceChar('\t'),
            Some(_) => return Err(()),
        },
        Key::Char('R') => Simple::ReplaceMode,
        Key::Char('o') => Simple::OpenBelow,
        Key::Char('O') => Simple::OpenAbove,
        Key::Char('a') => Simple::Append,
        Key::Char('A') => Simple::AppendEnd,
        Key::Char('i') => Simple::Insert,
        Key::Char('I') => Simple::InsertStart,
        Key::Char('p') => Simple::Put { before: false },
        Key::Char('P') => Simple::Put { before: true },
        Key::Char('J') => Simple::Join { spaces: true },
        Key::Char('u') => Simple::Undo,
        Key::Ctrl('r') => Simple::Redo,
        Key::Char('.') => Simple::Repeat,
        Key::Char('v') => Simple::Visual,
        Key::Char('V') => Simple::VisualLine,
        Key::Char('~') => Simple::ToggleCaseChar,
        Key::Char('D') => Simple::DelEnd,
        Key::Char('C') => Simple::ChangeEnd,
        Key::Char('Y') => Simple::YankLine,
        Key::Char(ch @ (':' | '/' | '?')) => Simple::Cmdline(ch),
        Key::Esc | Key::Ctrl('[' | 'c') => Simple::Escape,
        Key::Char('Z') => match c.next() {
            None => return Ok(None),
            Some(Key::Char('Z')) => Simple::WriteQuit,
            Some(Key::Char('Q')) => Simple::QuitNow,
            Some(_) => return Err(()),
        },
        Key::Char('g') => match c.next() {
            None => return Ok(None),
            Some(Key::Char('v')) => Simple::Reselect,
            Some(Key::Char('J')) => Simple::Join { spaces: false },
            Some(_) => return Err(()),
        },
        _ => return Err(()),
    }))
}

/// Parse the pending keys of a Normal-mode command.
pub(super) fn parse_normal(keys: &[Key]) -> Parsed {
    if keys.len() > MAX_PENDING {
        return Parsed::Invalid;
    }
    let mut c = Cursor { keys, i: 0 };
    let (reg, count) = match prefix(&mut c) {
        Ok(Some(p)) => p,
        Ok(None) => return Parsed::Incomplete,
        Err(()) => return Parsed::Invalid,
    };
    let Some(k) = c.next() else { return Parsed::Incomplete };
    let done = |act: Act| Parsed::Done(Cmd { reg, count, act });
    // Simple commands first (`g` ones only if no operator or motion takes it).
    let start = c.i;
    if !matches!(k, Key::Char('g')) || matches!(c.peek(), Some(Key::Char('v' | 'J'))) {
        match simple(&mut c, k) {
            Ok(Some(s)) => return done(Act::Do(s)),
            Ok(None) => return Parsed::Incomplete,
            Err(()) => c.i = start,
        }
    }
    match operator(&mut c, k) {
        Ok(Some((op, op_keys))) => {
            let n2 = c.count();
            let count = match (count, n2) {
                (None, None) => None,
                (a, b) => Some((a.unwrap_or(1) * b.unwrap_or(1)).min(MAX_COUNT)),
            };
            let Some(t) = c.next() else { return Parsed::Incomplete };
            let target = match t {
                // `dd`, `>>`, `gUU`, `gUgU`.
                _ if t == op_keys[op_keys.len() - 1] => Target::Lines,
                Key::Char('g') if op_keys.len() == 2 && c.peek() == Some(op_keys[1]) => Target::Lines,
                Key::Char(ch @ ('i' | 'a')) => match object(&mut c, ch == 'a') {
                    Ok(Some(o)) => Target::Object(o),
                    Ok(None) => return Parsed::Incomplete,
                    Err(()) => return Parsed::Invalid,
                },
                _ => match motion_key(&mut c, t) {
                    Ok(Some(m)) => Target::Motion(m),
                    Ok(None) => return Parsed::Incomplete,
                    Err(()) => return Parsed::Invalid,
                },
            };
            return Parsed::Done(Cmd { reg, count, act: Act::Op(op, target) });
        }
        Ok(None) => return Parsed::Incomplete,
        Err(()) => c.i = start,
    }
    match motion_key(&mut c, k) {
        Ok(Some(m)) => done(Act::Move(m)),
        Ok(None) => Parsed::Incomplete,
        Err(()) => Parsed::Invalid,
    }
}

/// Parse the pending keys of a Visual-mode command.
pub(super) fn parse_visual(keys: &[Key]) -> Parsed {
    if keys.len() > MAX_PENDING {
        return Parsed::Invalid;
    }
    let mut c = Cursor { keys, i: 0 };
    let (reg, count) = match prefix(&mut c) {
        Ok(Some(p)) => p,
        Ok(None) => return Parsed::Incomplete,
        Err(()) => return Parsed::Invalid,
    };
    let Some(k) = c.next() else { return Parsed::Incomplete };
    let done = |act: Act| Parsed::Done(Cmd { reg, count, act });
    let vop = |op: Op, lines: bool| done(Act::Do(Simple::VisualOp(op, lines)));
    match k {
        Key::Char('d' | 'x') | Key::Delete => vop(Op::Delete, false),
        Key::Char('X' | 'D') => vop(Op::Delete, true),
        Key::Char('c' | 's') => vop(Op::Change, false),
        Key::Char('C' | 'S' | 'R') => vop(Op::Change, true),
        Key::Char('y') => vop(Op::Yank, false),
        Key::Char('Y') => vop(Op::Yank, true),
        Key::Char('>') => vop(Op::Indent, true),
        Key::Char('<') => vop(Op::Outdent, true),
        Key::Char('~') => vop(Op::ToggleCase, false),
        Key::Char('u') => vop(Op::Lower, false),
        Key::Char('U') => vop(Op::Upper, false),
        Key::Char('J') => done(Act::Do(Simple::Join { spaces: true })),
        Key::Char('p') => done(Act::Do(Simple::Put { before: false })),
        Key::Char('P') => done(Act::Do(Simple::Put { before: true })),
        Key::Char('o' | 'O') => done(Act::Do(Simple::Swap)),
        Key::Char('v') => done(Act::Do(Simple::Visual)),
        Key::Char('V') => done(Act::Do(Simple::VisualLine)),
        Key::Char(':') => done(Act::Do(Simple::Cmdline(':'))),
        Key::Esc | Key::Ctrl('[' | 'c') => done(Act::Do(Simple::Escape)),
        Key::Char('r') => match c.next() {
            None => Parsed::Incomplete,
            Some(Key::Char(ch)) => done(Act::Do(Simple::ReplaceChar(ch))),
            Some(_) => Parsed::Invalid,
        },
        Key::Char(ch @ ('i' | 'a')) => match object(&mut c, ch == 'a') {
            Ok(Some(o)) => done(Act::Select(o)),
            Ok(None) => Parsed::Incomplete,
            Err(()) => Parsed::Invalid,
        },
        Key::Char('g') => match c.peek() {
            None => Parsed::Incomplete,
            Some(Key::Char('u')) => vop(Op::Lower, false),
            Some(Key::Char('U')) => vop(Op::Upper, false),
            Some(Key::Char('~')) => vop(Op::ToggleCase, false),
            Some(Key::Char('J')) => done(Act::Do(Simple::Join { spaces: false })),
            Some(Key::Char('v')) => done(Act::Do(Simple::Reselect)),
            _ => match motion_key(&mut c, k) {
                Ok(Some(m)) => done(Act::Move(m)),
                Ok(None) => Parsed::Incomplete,
                Err(()) => Parsed::Invalid,
            },
        },
        _ => match motion_key(&mut c, k) {
            Ok(Some(m)) => done(Act::Move(m)),
            Ok(None) => Parsed::Incomplete,
            Err(()) => Parsed::Invalid,
        },
    }
}
