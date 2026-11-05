// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Macros: keys recorded into a register (`qa…q`) are kept as text, as in
//! Vim, so `"ap` shows them and `"ayy` then `@a` runs an edited macro.
//! Escape is `^[`, Enter `^M`, Ctrl keys their control characters; keys
//! with no character (arrows, Delete) use private-use characters.

use super::{Ex, Host, Key, Mode, Outcome, Register, Scratch, VimState, diff_span, undo_cursor};
use crate::{Buffer, Selection, multi};

/// Macros run inside each other at most this deep.
const MAX_DEPTH: usize = 20;
/// A macro run (with its count and the macros it runs) stops after this many keys.
const MAX_KEYS: usize = 200_000;

const SPECIAL: [Key; 8] = [Key::Delete, Key::Left, Key::Right, Key::Up, Key::Down, Key::Home, Key::End, Key::PageUp];
const PRIVATE: u32 = 0xE000;

fn key_char(k: Key) -> char {
    match k {
        Key::Char(c) => c,
        // Ctrl-[ is Escape.
        Key::Esc | Key::Ctrl(_) if !matches!(k, Key::Ctrl(c) if c.is_ascii_lowercase()) => '\u{1b}',
        Key::Enter => '\r',
        Key::Backspace => '\u{8}',
        Key::Tab => '\t',
        Key::Ctrl(c) => char::from(c as u8 - b'a' + 1),
        Key::PageDown => char::from_u32(PRIVATE + SPECIAL.len() as u32).unwrap_or('\u{fffd}'),
        k => SPECIAL.iter().position(|s| *s == k).and_then(|i| char::from_u32(PRIVATE + i as u32)).unwrap_or('\u{fffd}'),
    }
}

fn char_key(c: char) -> Key {
    match c {
        '\u{1b}' => Key::Esc,
        '\r' => Key::Enter,
        '\u{8}' => Key::Backspace,
        '\t' => Key::Tab,
        '\n' => Key::Ctrl('j'),
        '\u{1}'..='\u{1a}' => Key::Ctrl(char::from(c as u8 - 1 + b'a')),
        _ => match (c as u32).checked_sub(PRIVATE).map(|i| i as usize) {
            Some(i) if i < SPECIAL.len() => SPECIAL[i],
            Some(i) if i == SPECIAL.len() => Key::PageDown,
            _ => Key::Char(c),
        },
    }
}

/// Keys as register text.
pub(super) fn to_text(keys: &[Key]) -> String {
    keys.iter().map(|k| key_char(*k)).collect()
}

/// Register text as keys.
pub(super) fn to_keys(text: &str) -> Vec<Key> {
    text.chars().map(char_key).collect()
}

/// The editor's typing, inside a macro: Insert-mode keys at every cursor.
pub(super) fn type_key(k: Key, buf: &mut Buffer, sel: &mut Selection, extra: &mut Vec<Selection>) {
    let mut all = extra.clone();
    all.push(*sel);
    all = multi::normalize(all);
    let main = all.iter().position(|s| s.head == sel.head).unwrap_or(0);
    let done = match k {
        Key::Char(c) => Some(multi::insert(&all, &c.to_string())),
        Key::Enter => Some(multi::insert(&all, "\n")),
        Key::Tab => Some(multi::insert(&all, "\t")),
        Key::Backspace => multi::delete_backward(buf, &all, false),
        Key::Delete => multi::delete_forward(buf, &all, false),
        Key::Left | Key::Right => {
            let h = if k == Key::Left { sel.head.saturating_sub(1) } else { (sel.head + 1).min(buf.len_chars()) };
            *sel = Selection::cursor(h);
            extra.clear();
            None
        }
        _ => None,
    };
    if let Some((tx, sels)) = done
        && tx.apply(buf).is_ok()
    {
        *sel = sels.get(main).copied().unwrap_or(*sel);
        *extra = sels.into_iter().enumerate().filter(|(i, _)| *i != main).map(|(_, s)| s).collect();
    }
}

impl VimState {
    /// `q{reg}` … `q` is over: keep the keys in the register.
    pub(super) fn store_macro(&mut self, reg: char, keys: &[Key]) {
        let text = to_text(keys);
        if reg.is_ascii_uppercase() {
            let lower = reg.to_ascii_lowercase();
            let mut r = self.registers.get(&lower).cloned().unwrap_or_default();
            r.text.push_str(&text);
            self.registers.insert(lower, r);
        } else {
            self.registers.insert(if reg == '"' { '"' } else { reg }, Register { text, linewise: false, block: false });
        }
    }

    /// `@{reg}`, `count` times: every key as if typed, on a copy of the
    /// note; the note then gets the result as one edit (one undo step).
    /// Stops at the first command that fails, as in Vim.
    pub(super) fn run_macro(
        &mut self,
        reg: char,
        count: usize,
        sx: &mut Scratch,
        pos: usize,
        host: &mut dyn Host,
        out: &mut Outcome,
    ) -> usize {
        let reg = if reg == '@' {
            let Some(r) = self.last_macro else {
                self.message = Some("E748: No previously used register".into());
                self.error = true;
                return pos;
            };
            r
        } else {
            reg.to_ascii_lowercase()
        };
        let Some(text) = self.registers.get(&reg).map(|r| r.text.clone()).filter(|t| !t.is_empty()) else {
            self.message = Some(format!("Nothing in register {reg}"));
            self.error = true;
            return pos;
        };
        self.last_macro = Some(reg);
        if self.depth >= MAX_DEPTH {
            self.message = Some("Macros nested too deep".into());
            self.error = true;
            return pos;
        }
        let keys = to_keys(&text);
        let start = sx.work.clone();
        let mut buf = start.clone();
        let mut sel = Selection::cursor(pos);
        let mut extra: Vec<Selection> = Vec::new();
        let mut undo: Vec<(Buffer, (usize, usize))> = Vec::new();
        let mut redo: Vec<(Buffer, (usize, usize))> = Vec::new();
        let mut budget = MAX_KEYS;
        self.depth += 1;
        'run: for _ in 0..count.max(1) {
            for k in &keys {
                if budget == 0 {
                    self.message = Some("The macro was stopped: too many keys".into());
                    break 'run;
                }
                budget -= 1;
                self.error = false;
                let o = self.handle(*k, &buf, sel, host);
                if !o.consumed {
                    type_key(*k, &mut buf, &mut sel, &mut extra);
                    continue;
                }
                if let Some(b) = o.before {
                    sel = b;
                }
                if let Some(tx) = &o.edit {
                    undo.push((buf.clone(), buf.line_col(sel.head)));
                    redo.clear();
                    if tx.apply(&mut buf).is_ok() {
                        sel = tx.selection().unwrap_or(sel).clamp(buf.len_chars());
                    }
                } else if let Some(s) = o.selection {
                    sel = s;
                }
                if let Some(e) = o.extra {
                    extra = e;
                }
                for _ in 0..o.undo {
                    if let Some((text, uh)) = undo.pop() {
                        redo.push((buf.clone(), uh));
                        sel = Selection::cursor(undo_cursor(&buf, &text, uh));
                        buf = text;
                    }
                }
                for _ in 0..o.redo {
                    if let Some((text, _)) = redo.pop() {
                        let uh = buf.line_col(sel.head);
                        undo.push((buf.clone(), uh));
                        sel = Selection::cursor(undo_cursor(&buf, &text, uh));
                        buf = text;
                    }
                }
                if o.undo + o.redo > 0
                    && let Some(s) = self.observe(&buf, sel)
                {
                    sel = s;
                }
                if let Some(c) = o.clipboard {
                    out.clipboard = Some(c);
                }
                if o.paste_clipboard.is_some() {
                    self.message = Some("A macro can't put from the clipboard".into());
                    self.error = true;
                }
                match o.ex {
                    Some(Ex::Substitute(s)) => {
                        if let Some(t) = host.substitute(&buf, &s) {
                            undo.push((buf.clone(), buf.line_col(sel.head)));
                            let line = s.lines.end.saturating_sub(1);
                            buf = Buffer::from(t.as_str());
                            sel = Selection::cursor(super::motions::first_non_blank(&buf, line.min(super::motions::last_line(&buf))));
                        } else {
                            self.message = Some(format!("Pattern not found: {}", s.pattern));
                            self.error = true;
                        }
                    }
                    Some(e) => out.ex = Some(e),
                    None => {}
                }
                if self.error {
                    break 'run;
                }
            }
        }
        self.depth -= 1;
        // The marks already followed each step.
        self.seen = Some(buf.clone());
        let (s, oe, ne) = diff_span(&start, &buf);
        if (s, oe, ne) != (s, s, s) {
            sx.replace(s..oe, &buf.slice(s..ne));
            sx.marks_done = true;
        }
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            out.group = super::Group::Begin;
        }
        if !extra.is_empty() || self.mode == Mode::VisualBlock {
            out.extra = Some(extra);
        }
        if self.mode.visual() {
            self.cursor = sel.head.min(buf.len_chars());
            return self.cursor;
        }
        sel.head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_register_text() {
        let keys = Key::parse_seq("ihi<CR><Esc><C-r><Left><PageDown><Del>x<Tab><BS>");
        let text = to_text(&keys);
        assert_eq!(&text[..4], "ihi\r");
        assert_eq!(to_keys(&text), keys);
        // A yanked line ends with a line break, which runs as Ctrl-J (down).
        assert_eq!(to_keys("dd\n"), vec![Key::Char('d'), Key::Char('d'), Key::Ctrl('j')]);
    }
}
