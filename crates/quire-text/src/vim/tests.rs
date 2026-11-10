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
    /// The editor's other cursors (a Visual block's rows).
    extra: Vec<Selection>,
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
            extra: Vec::new(),
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

    /// Typing in Insert mode, as the editor does it (at every cursor).
    fn type_key(&mut self, k: Key) {
        let before = self.snapshot();
        macros::type_key(k, &mut self.buf, &mut self.sel, &mut self.extra);
        if self.group.is_none() && before.0 != self.buf.to_string() {
            self.push_undo(before);
        }
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
        if let Some(e) = out.extra {
            self.extra = e;
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
    /// The cursor goes to the first non-blank of the last line changed.
    fn substitute(&mut self, s: &Substitute) {
        let Some(text) = PlainHost.substitute(&self.buf, s) else { return };
        let last = s.lines.clone().rev().find(|l| *l < self.buf.len_lines() && self.buf.line(*l).contains(&s.pattern));
        self.push_undo(self.snapshot());
        self.buf = Buffer::from(text.as_str());
        if let Some(l) = last {
            self.sel = Selection::cursor(motions::first_non_blank(&self.buf, l));
        }
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
    assert_eq!(s.run(":%s/b/X/g<CR>"), "a\n|X\nc");
    assert_eq!(s.run(":2,3s/X/Y/<CR>"), "a\n|Y\nc");
    s.run(":frob<CR>");
    assert_eq!(s.vim.take_message().as_deref(), Some("Not an editor command: frob"));
    s.run("ggVj:s/a/b/<CR>");
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

#[test]
fn diff_span_matches_a_char_by_char_diff() {
    fn naive(a: &str, b: &str) -> (usize, usize, usize) {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let p = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let max = a.len().min(b.len()) - p;
        let s = a.iter().rev().zip(b.iter().rev()).take(max).take_while(|(x, y)| x == y).count();
        (p, a.len() - s, b.len() - s)
    }
    // A small generator, so the cases are the same every run.
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        usize::try_from(seed % n as u64).unwrap_or(0)
    };
    let pieces = ["a", "b", "é", "€", "😀", "\n", "line of text ", "日本"];
    for round in 0..300 {
        // Some texts span many rope chunks.
        let len = if round % 10 == 0 { 3000 } else { 1 + next(40) };
        let a: String = (0..len).map(|_| pieces[next(pieces.len())]).collect();
        let at = a.char_indices().map(|(i, _)| i).nth(next(a.chars().count().max(1))).unwrap_or(a.len());
        let cut = a[at..].char_indices().map(|(i, _)| i).nth(next(4)).map_or(a.len(), |i| at + i);
        let b = format!("{}{}{}", &a[..at], (0..next(4)).map(|_| pieces[next(pieces.len())]).collect::<String>(), &a[cut..]);
        for (x, y) in [(&a, &b), (&b, &a), (&a, &a)] {
            assert_eq!(diff_span(&Buffer::from(x.as_str()), &Buffer::from(y.as_str())), naive(x, y), "{x:?} → {y:?}");
        }
    }
    assert_eq!(diff_span(&Buffer::from(""), &Buffer::from("€")), (0, 0, 1));
    assert_eq!(diff_span(&Buffer::from("a€b"), &Buffer::from("a😀b")), (1, 2, 2));
}

#[test]
fn a_macro_run_is_one_undo_step() {
    let mut s = Sim::new("|a\nb\nc\nd");
    s.run("qaA!<Esc>jq");
    assert_eq!(s.vim.recording(), None);
    let before = s.undo.len();
    assert_eq!(s.run("2@a"), "a!\nb!\nc!\n|d");
    assert_eq!(s.undo.len(), before + 1, "two runs of three keys, one step");
    assert_eq!(s.run("u"), "a!\n|b\nc\nd");
    // The macro is text in its register.
    assert_eq!(s.run("\"ap"), "a!\nbA!\u{1b}|j\nc\nd");
}

#[test]
fn recording_shows_in_the_status_bar() {
    let mut s = Sim::new("|one");
    s.run("qq");
    assert_eq!(s.vim.status(), "recording @q");
    s.run("i");
    assert_eq!(s.vim.status(), "-- INSERT --  recording @q");
    s.run("x<Esc>q");
    assert_eq!(s.vim.status(), "");
    // Text typed by the editor itself is recorded too.
    let mut v = VimState::new();
    let b = Buffer::from("one");
    for k in Key::parse_seq("qai") {
        v.handle(k, &b, Selection::cursor(0), &mut PlainHost);
    }
    v.typed("hi");
    v.handle(Key::Esc, &b, Selection::cursor(0), &mut PlainHost);
    v.handle(Key::Char('q'), &b, Selection::cursor(0), &mut PlainHost);
    assert_eq!(v.registers.get(&'a').map(|r| r.text.as_str()), Some("ihi\u{1b}"));
}

#[test]
fn a_runaway_macro_stops() {
    let mut s = Sim::new("|ab");
    // `@a` inside `a`: stops at the nesting limit, the text is still there.
    s.vim.registers.insert('a', Register { text: "@a".into(), ..Register::default() });
    s.run("@a");
    assert_eq!(s.buf.to_string(), "ab");
    assert!(s.vim.take_message().is_some_and(|m| m.contains("deep")));
}

#[test]
fn marks_travel_with_their_note() {
    let mut s = Sim::new("one\n|two\nthree");
    s.run("ma");
    let kept = s.vim.swap_marks(Marks::default());
    assert_eq!(kept.get('a'), Some((1, 0)));
    s.run("gg");
    assert_eq!(s.run("'a"), "|one\ntwo\nthree", "no mark a here");
    s.vim.swap_marks(kept);
    assert_eq!(s.run("'a"), "one\n|two\nthree");
}

#[test]
fn a_block_is_one_selection_a_row() {
    let mut s = Sim::new("|abc\ndef\nghi");
    s.run("<C-v>jjl");
    assert_eq!(s.vim.mode(), Mode::VisualBlock);
    assert_eq!(s.vim.status(), "-- VISUAL BLOCK --");
    assert_eq!(s.sel, Selection::new(8, 10), "the cursor's row");
    assert_eq!(s.extra, vec![Selection::new(0, 2), Selection::new(4, 6)]);
    // `I` types at one cursor a row; Escape clears them; one undo step.
    s.vim.handle(Key::Esc, &s.buf.clone(), s.sel, &mut PlainHost);
    let mut s = Sim::new("|abc\ndef\nghi");
    s.run("<C-v>jjI");
    assert_eq!(s.extra.len(), 2);
    s.run("--<Esc>");
    assert!(s.extra.is_empty());
    assert_eq!(s.show(), "|--abc\n--def\n--ghi");
    assert_eq!(s.run("u"), "|abc\ndef\nghi");
}

/// 0.12: Ctrl+D and Ctrl+U move half a page (the host's page is 20 lines
/// here), or `count` lines; Ctrl+F and Ctrl+B a page.
#[test]
fn half_and_whole_pages() {
    let text = (0..60).fold(String::new(), |mut t, i| {
        t.push_str("line ");
        t.push_str(&i.to_string());
        t.push('\n');
        t
    });
    let at = |s: &str| s[..s.find('|').unwrap()].matches('\n').count();
    let start = format!("|{text}");
    assert_eq!(at(&run(&start, "<C-d>")), 10);
    assert_eq!(at(&run(&start, "<C-d><C-d>")), 20);
    assert_eq!(at(&run(&start, "3<C-d>")), 3);
    assert_eq!(at(&run(&start, "<C-f>")), 20);
    assert_eq!(at(&run(&start, "<C-f><C-u>")), 10);
    assert_eq!(at(&run(&start, "<C-f><C-b>")), 0);
    let last = at(&run(&start, "G"));
    assert_eq!(at(&run(&start, "G<C-u>")), last - 10);
}
