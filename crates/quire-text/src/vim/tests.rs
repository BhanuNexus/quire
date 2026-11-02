// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Vim test table: every case in `testdata/vim/cases.tsv` (text, keys,
//! and what Vim 9.1 made of them, recorded by `record.py`) run through the
//! state machine with a small stand-in for the editor.

use super::*;

/// The editor around the state machine: applies outcomes, types in Insert
/// mode, and keeps undo steps the way Quire's editor groups them.
struct Sim {
    vim: VimState,
    buf: Buffer,
    sel: Selection,
    /// Text before each change, and the cursor (line, column) before it.
    undo: Vec<(String, (usize, usize))>,
    redo: Vec<(String, (usize, usize))>,
    group: Option<(String, Selection)>,
    clipboard: String,
    ex: Vec<Ex>,
}

impl Sim {
    fn new(text_with_cursor: &str) -> Self {
        let (text, cur) = split(text_with_cursor);
        let mut vim = VimState::new();
        vim.indent = "  ".into();
        Self {
            vim,
            buf: Buffer::from(text.as_str()),
            sel: Selection::cursor(cur),
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
            clipboard: String::new(),
            ex: Vec::new(),
        }
    }

    fn snapshot(&self) -> (String, Selection) {
        (self.buf.to_string(), self.sel)
    }

    fn line_col(&self, pos: usize) -> (usize, usize) {
        self.buf.line_col(pos)
    }

    /// Undo or redo to `text`, placing the cursor as the editor does.
    fn restore(&mut self, text: &str, uh: (usize, usize)) {
        let after = Buffer::from(text);
        let cur = undo_cursor(&self.buf, &after, uh);
        self.buf = after;
        self.sel = Selection::cursor(cur);
    }

    fn push_undo(&mut self, before: (String, Selection)) {
        let uh = Buffer::from(before.0.as_str()).line_col(before.1.head);
        self.undo.push((before.0, uh));
        self.redo.clear();
    }

    fn apply(&mut self, tx: &Transaction) {
        tx.apply(&mut self.buf).expect("the state machine's edits fit the buffer");
        self.sel = tx.selection().expect("edits carry a selection").clamp(self.buf.len_chars());
    }

    /// Typing in Insert mode, as the editor does it.
    fn type_key(&mut self, k: Key) {
        let p = self.sel.head;
        let tx = match k {
            Key::Char(c) => Transaction::insert(p, c.to_string()).with_selection(Selection::cursor(p + 1)),
            Key::Tab => Transaction::insert(p, "\t").with_selection(Selection::cursor(p + 1)),
            Key::Enter => Transaction::insert(p, "\n").with_selection(Selection::cursor(p + 1)),
            Key::Backspace if p > 0 => Transaction::delete(p - 1, p).with_selection(Selection::cursor(p - 1)),
            Key::Left => {
                self.sel = Selection::cursor(p.saturating_sub(1));
                return;
            }
            _ => return,
        };
        if self.group.is_none() {
            self.push_undo(self.snapshot());
        }
        self.apply(&tx);
    }

    fn press(&mut self, k: Key) {
        let out = self.vim.handle(k, &self.buf, self.sel, &mut PlainHost);
        if !out.consumed {
            assert!(self.vim.typing(), "only Insert mode passes keys on ({k:?})");
            self.type_key(k);
            return;
        }
        self.outcome(out);
    }

    fn outcome(&mut self, out: Outcome) {
        if out.group == Group::Begin {
            self.group = Some(self.snapshot());
        }
        if let Some(tx) = &out.edit {
            if let Some(b) = out.before {
                self.sel = b;
            }
            if self.group.is_none() {
                self.push_undo(self.snapshot());
            }
            self.apply(tx);
        } else if let Some(s) = out.selection {
            self.sel = s;
        }
        if out.group == Group::End
            && let Some(before) = self.group.take()
            && before.0 != self.buf.to_string()
        {
            self.push_undo(before);
        }
        for _ in 0..out.undo {
            if let Some((text, uh)) = self.undo.pop() {
                self.redo.push((self.buf.to_string(), uh));
                self.restore(&text, uh);
            }
        }
        for _ in 0..out.redo {
            if let Some((text, _)) = self.redo.pop() {
                // The editor knows only where the cursor is now.
                let uh = self.line_col(self.sel.head);
                self.undo.push((self.buf.to_string(), uh));
                self.restore(&text, uh);
            }
        }
        if out.undo + out.redo > 0
            && let Some(s) = self.vim.observe(&self.buf, self.sel)
        {
            self.sel = s;
        }
        if let Some(c) = out.clipboard {
            self.clipboard = c;
        }
        if let Some(p) = out.paste_clipboard {
            let text = self.clipboard.clone();
            let o = self.vim.put_clipboard(&text, p, &self.buf, self.sel);
            self.outcome(o);
        }
        if let Some(e) = out.ex {
            if let Ex::Substitute(s) = &e {
                self.substitute(s);
            }
            self.ex.push(e);
        }
    }

    /// `:s` with a literal pattern (the editor uses regular expressions).
    fn substitute(&mut self, s: &Substitute) {
        let mut lines: Vec<String> = self.buf.to_string().split('\n').map(str::to_string).collect();
        for l in s.lines.clone() {
            if let Some(line) = lines.get_mut(l) {
                *line = if s.global { line.replace(&s.pattern, &s.replacement) } else { line.replacen(&s.pattern, &s.replacement, 1) };
            }
        }
        self.push_undo(self.snapshot());
        self.buf = Buffer::from(lines.join("\n").as_str());
    }

    fn run(&mut self, keys: &str) -> String {
        for k in Key::parse_seq(keys) {
            self.press(k);
        }
        if let Some(s) = self.vim.observe(&self.buf, self.sel) {
            self.sel = s;
        }
        self.show()
    }

    /// The text with `|` at the cursor (the start of a Visual selection's cursor).
    fn show(&self) -> String {
        let text = self.buf.to_string();
        let cur = self.vim.cursor_pos(&self.buf, self.sel).min(self.buf.len_chars());
        let b = self.buf.char_to_byte(cur);
        format!("{}|{}", &text[..b], &text[b..])
    }
}

fn split(s: &str) -> (String, usize) {
    let b = s.find('|').expect("a cursor");
    let text = format!("{}{}", &s[..b], &s[b + 1..]);
    (text, s[..b].chars().count())
}

fn unescape(s: &str) -> String {
    s.replace("\\\\", "\0").replace("\\n", "\n").replace("\\t", "\t").replace('\0', "\\")
}

/// A table case: the keys, then Escape (as `record.py` types it before dumping).
fn run(text: &str, keys: &str) -> String {
    Sim::new(text).run(&format!("{keys}<Esc>"))
}

#[test]
fn the_vim_table() {
    let table = include_str!("../../testdata/vim/cases.tsv");
    let mut failures = Vec::new();
    let mut n = 0;
    for line in table.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 3, "bad row: {line}");
        let (text, keys, want) = (unescape(cols[0]), cols[1], unescape(cols[2]));
        let got = run(&text, keys);
        n += 1;
        if got != want {
            failures.push(format!("{:?} + {keys}\n   want {want:?}\n    got {got:?}", cols[0]));
        }
    }
    assert!(n >= 300, "the table has {n} cases");
    assert!(failures.is_empty(), "{} of {n} cases differ from Vim:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn modes_and_status() {
    let mut s = Sim::new("|one two");
    assert_eq!(s.vim.status(), "");
    s.run("2d");
    assert_eq!(s.vim.status(), "2d");
    s.run("<Esc>v");
    assert_eq!((s.vim.mode(), s.vim.status().as_str()), (Mode::Visual, "-- VISUAL --"));
    s.run("<Esc>V");
    assert_eq!(s.vim.status(), "-- VISUAL LINE --");
    s.run("<Esc>i");
    assert_eq!(s.vim.status(), "-- INSERT --");
    assert!(s.vim.typing());
    s.run("<Esc>R");
    assert_eq!(s.vim.status(), "-- REPLACE --");
    s.run("<Esc>:s/x");
    assert_eq!(s.vim.status(), ":s/x");
    s.run("<Esc>/on");
    assert_eq!(s.vim.status(), "/on");
    s.run("<Esc>");
    assert_eq!((s.vim.mode(), s.vim.status().as_str()), (Mode::Normal, ""));
}

#[test]
fn insert_sessions_are_one_undo_step() {
    let mut s = Sim::new("|one");
    s.run("ccnew line<Esc>");
    assert_eq!(s.show(), "new lin|e");
    assert_eq!(s.undo.len(), 1, "the change and the typing undo together");
    assert_eq!(s.run("u"), "|one");
    // Begin comes with the edit that opens the session.
    let mut v = VimState::new();
    let b = Buffer::from("one");
    let out = v.handle(Key::Char('o'), &b, Selection::cursor(0), &mut PlainHost);
    assert_eq!((out.group, out.edit.is_some()), (Group::Begin, true));
    let out = v.handle(Key::Char('x'), &b, Selection::cursor(4), &mut PlainHost);
    assert!(!out.consumed);
    let out = v.handle(Key::Esc, &b, Selection::cursor(4), &mut PlainHost);
    assert_eq!(out.group, Group::End);
}

#[test]
fn ex_commands_reach_the_editor() {
    let mut s = Sim::new("|a\nb\nc");
    for (keys, ex) in [
        (":w<CR>", Ex::Write),
        (":q<CR>", Ex::Quit),
        (":wq<CR>", Ex::WriteQuit),
        (":x<CR>", Ex::WriteQuit),
        ("ZZ", Ex::WriteQuit),
        ("ZQ", Ex::Quit),
        (":wa<CR>", Ex::WriteAll),
        (":qa<CR>", Ex::QuitAll),
        (":noh<CR>", Ex::NoHighlight),
    ] {
        s.run(keys);
        assert_eq!(s.ex.pop(), Some(ex), "{keys}");
    }
    assert_eq!(s.run(":%s/b/X/g<CR>"), "|a\nX\nc");
    assert_eq!(s.run(":2,3s/X/Y/<CR>"), "|a\nY\nc");
    s.run(":frob<CR>");
    assert_eq!(s.vim.take_message().as_deref(), Some("Not an editor command: frob"));
    s.run("Vj:s/a/b/<CR>");
    let Some(Ex::Substitute(sub)) = s.ex.pop() else { panic!("a substitute") };
    assert_eq!(sub.lines, 0..2, "'<,'> is the visual selection");
}

#[test]
fn the_clipboard_register() {
    let mut s = Sim::new("|one two");
    s.run("\"+yiw");
    assert_eq!(s.clipboard, "one");
    assert_eq!(s.run("w\"+P"), "one on|etwo");
    s.clipboard = "line\n".into();
    assert_eq!(s.run("\"+p"), "one onetwo\n|line");
    assert_eq!(s.run("u"), "one on|etwo", "undo returns the cursor to where the put started");
    s.run("\"+dd");
    assert_eq!(s.clipboard, "one onetwo\n");
}

#[test]
fn search_uses_the_host() {
    struct Rows;
    impl Host for Rows {
        fn display_row(&mut self, _: &Buffer, pos: usize, down: bool) -> Option<usize> {
            Some(if down { pos + 10 } else { pos.saturating_sub(10) })
        }
        fn search(&mut self, _: &Buffer, pattern: &str, from: usize, _: bool) -> Option<usize> {
            (pattern == "x").then_some(from + 3)
        }
    }
    let b = Buffer::from("0123456789012345678901234567890");
    let mut v = VimState::new();
    let out = v.handle(Key::Char('g'), &b, Selection::cursor(2), &mut Rows);
    assert!(out.consumed && out.selection.is_none() && out.edit.is_none(), "waiting for the next key");
    let out = v.handle(Key::Char('j'), &b, Selection::cursor(2), &mut Rows);
    assert_eq!(out.selection, Some(Selection::cursor(12)), "gj moves a display row");
    for k in Key::parse_seq("/x") {
        v.handle(k, &b, Selection::cursor(12), &mut Rows);
    }
    let out = v.handle(Key::Enter, &b, Selection::cursor(12), &mut Rows);
    assert_eq!(out.selection, Some(Selection::cursor(15)));
    let out = v.handle(Key::Char('n'), &b, Selection::cursor(15), &mut Rows);
    assert_eq!(out.selection, Some(Selection::cursor(18)));
    for k in Key::parse_seq("/nothing<CR>") {
        v.handle(k, &b, Selection::cursor(18), &mut Rows);
    }
    assert_eq!(v.take_message().as_deref(), Some("Pattern not found: nothing"));
}

#[test]
fn a_mouse_selection_is_visual_mode() {
    let mut v = VimState::new();
    let b = Buffer::from("one two\nthree");
    assert_eq!(v.observe(&b, Selection::new(0, 3)), None);
    assert_eq!(v.mode(), Mode::Visual);
    let out = v.handle(Key::Char('d'), &b, Selection::new(0, 3), &mut PlainHost);
    assert_eq!(out.edit.map(|t| t.changes()[0].clone()), Some(crate::Change::new(0, 3, "")));
    assert_eq!(v.mode(), Mode::Normal);
    // The Normal-mode cursor never rests on a line break.
    assert_eq!(v.observe(&b, Selection::cursor(7)), Some(Selection::cursor(6)));
    assert_eq!(v.observe(&b, Selection::cursor(6)), None);
}

#[test]
fn keys_from_notation() {
    assert_eq!(
        Key::parse_seq("a<Esc><C-r><CR><lt>x"),
        vec![Key::Char('a'), Key::Esc, Key::Ctrl('r'), Key::Enter, Key::Char('<'), Key::Char('x')]
    );
    assert_eq!(Key::parse_seq("<nope>"), "<nope>".chars().map(Key::Char).collect::<Vec<_>>());
}

#[test]
fn counts_are_capped_and_nonsense_is_dropped() {
    let mut s = Sim::new("|ab");
    assert_eq!(s.run("99999999999x"), "|");
    let mut s = Sim::new("|ab");
    assert_eq!(s.run("dqx"), "|b", "an invalid command is dropped whole");
    assert_eq!(s.vim.pending(), "");
}
