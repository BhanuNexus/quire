// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What Vim keys do to documents and the workspace: `:` commands, undo and
//! redo with Vim's cursor, one undo step per insert session, and search.
//! The state machine itself is `quire_text::vim`; the editor view feeds it.

use quire_text::vim::{Ex, Host, Substitute, diff_span, undo_cursor, vim_to_regex};
use quire_text::{Buffer, Change, EditKind, Selection, Transaction};

use crate::document::Document;
use crate::find::{FindOptions, compile};
use crate::workspace::Workspace;

/// Notes bigger than this keep an insert session as the editor recorded it
/// (several undo steps) rather than walking back through them.
const COLLAPSE_MAX_CHARS: usize = 2_000_000;

/// Search with Vim patterns (as regular expressions; plain text when the
/// pattern isn't a valid one). `from` is a char index; the search wraps.
pub(super) fn search(buf: &Buffer, pattern: &str, from: usize, backward: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let opts = FindOptions { case_sensitive: true, whole_word: false, regex: true };
    let re = compile(&vim_to_regex(pattern), opts).or_else(|_| compile(pattern, FindOptions { regex: false, ..opts })).ok()?;
    let text = buf.to_string();
    let from_b = buf.char_to_byte(from);
    let starts: Vec<usize> = re.find_iter(&text).filter(|m| !m.is_empty()).map(|m| m.start()).collect();
    let hit = if backward {
        starts.iter().rev().find(|s| **s < from_b).or_else(|| starts.last())
    } else {
        starts.iter().find(|s| **s > from_b).or_else(|| starts.first())
    }?;
    Some(buf.byte_to_char(*hit))
}

/// A [`Host`] for the editor: search with regular expressions, display
/// rows from the editor's layout (`rows`), and its page height.
pub struct VimHost<'a> {
    pub rows: Option<&'a mut dyn FnMut(usize, bool) -> Option<usize>>,
    pub page: usize,
}

impl Host for VimHost<'_> {
    fn display_row(&mut self, _buf: &Buffer, pos: usize, down: bool) -> Option<usize> {
        self.rows.as_mut().and_then(|f| f(pos, down))
    }

    fn search(&mut self, buf: &Buffer, pattern: &str, from: usize, backward: bool) -> Option<usize> {
        search(buf, pattern, from, backward)
    }

    fn page_lines(&self) -> usize {
        self.page.max(1)
    }
}

/// `u` and Ctrl-R, `n` times: the cursor goes where Vim puts it (the first
/// changed line). Returns a message when there is nothing to undo or redo.
pub fn vim_undo(doc: &mut Document, n: usize, redo: bool) -> Option<String> {
    for i in 0..n {
        let before = doc.buffer().clone();
        let cursor_before = doc.selection().head;
        let done = if redo { doc.redo() } else { doc.undo() };
        if !done {
            return (i == 0).then(|| if redo { "Already at newest change" } else { "Already at oldest change" }.to_string());
        }
        // Undo restores the cursor from before the change; redo leaves it
        // after, so the cursor from before the redo stands in.
        let uh = if redo { before.line_col(cursor_before) } else { doc.buffer().line_col(doc.selection().head) };
        let cur = undo_cursor(&before, doc.buffer(), uh);
        doc.set_selection(Selection::cursor(cur));
    }
    None
}

/// Make an insert session one undo step.
///
/// Undo back to `base` (the text when the session began, at revision
/// `base_rev`) and apply the session's net change as a single edit. Leaves
/// the history alone if it can't get back.
pub fn collapse_insert(doc: &mut Document, base: &Buffer, base_rev: u64) {
    if doc.revision() <= base_rev + 1 || doc.buffer().len_chars() > COLLAPSE_MAX_CHARS {
        return;
    }
    let target = doc.buffer().clone();
    let sel = doc.selection();
    let max = doc.revision() - base_rev;
    let mut undone = 0;
    while undone < max && doc.buffer().rope() != base.rope() && doc.can_undo() {
        doc.undo();
        undone += 1;
    }
    if doc.buffer().rope() != base.rope() {
        for _ in 0..undone {
            doc.redo();
        }
        doc.set_selection(sel);
        return;
    }
    let (s, old_end, new_end) = diff_span(base, &target);
    if s == old_end && s == new_end {
        doc.set_selection(sel);
        return;
    }
    let _ = doc.apply(Transaction::replace(s, old_end, target.slice(s..new_end)).with_selection(sel), EditKind::Other);
}

/// `:s` on the active note: Vim's pattern and replacement as regular expressions.
fn substitute(ws: &mut Workspace, s: &Substitute) {
    let opts = FindOptions { case_sensitive: !s.ignore_case, whole_word: false, regex: true };
    let re = match compile(&s.regex_pattern(), opts) {
        Ok(re) => re,
        Err(e) => {
            super::vim_say(ws, format!("Bad pattern: {}", e.lines().last().unwrap_or("invalid")));
            return;
        }
    };
    let rep = s.regex_replacement();
    let Some(doc) = ws.active_doc_mut() else { return };
    let buf = doc.buffer().clone();
    let mut changes = Vec::new();
    let mut count = 0;
    let mut last_line = None;
    for l in s.lines.clone().filter(|l| *l < buf.len_lines()) {
        let r = buf.line_range(l);
        let line = buf.slice(r.clone());
        // `replacen` with 0 replaces every match.
        let n = usize::from(!s.global);
        let hits = re.find_iter(&line).take(if s.global { usize::MAX } else { 1 }).count();
        if hits == 0 {
            continue;
        }
        let new = re.replacen(&line, n, rep.as_str()).into_owned();
        count += hits;
        last_line = Some(l);
        if new != line {
            changes.push(Change::new(r.start, r.end, new));
        }
    }
    let Some(l) = last_line else {
        super::vim_say(ws, format!("Pattern not found: {}", s.pattern));
        return;
    };
    if let Ok(tx) = Transaction::new(changes) {
        let mut after = buf.clone();
        let _ = tx.apply(&mut after);
        let line_start = after.line_to_char(l);
        let indent = after.line(l).chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let _ = doc.apply(tx.with_selection(Selection::cursor(line_start + indent)), EditKind::Other);
    }
    let lines = s.lines.clone().filter(|l| *l < buf.len_lines()).count();
    if count > 2 || lines > 2 {
        super::vim_say(ws, format!("{count} substitution{} on the note", if count == 1 { "" } else { "s" }));
    }
}

/// A `:` command from the Vim command line.
pub fn vim_ex(ws: &mut Workspace, ex: &Ex) {
    let active = ws.active_doc().map(|d| (d.id, d.path.is_some()));
    let save = |ws: &mut Workspace| match active {
        Some((id, true)) => {
            ws.save(id);
        }
        Some((_, false)) => super::vim_say(ws, "The scratchpad isn't saved to the vault"),
        None => {}
    };
    let close_all = |ws: &mut Workspace| {
        let ids: Vec<_> = ws.docs().iter().map(|d| d.id).collect();
        for id in ids {
            ws.close(id);
        }
    };
    match ex {
        Ex::Write => save(ws),
        Ex::Quit => {
            if let Some((id, _)) = active {
                ws.close(id);
            }
        }
        Ex::WriteQuit => {
            save(ws);
            if let Some((id, _)) = active {
                ws.close(id);
            }
        }
        Ex::WriteAll => ws.save_all(),
        Ex::QuitAll => close_all(ws),
        Ex::WriteQuitAll => {
            ws.save_all();
            close_all(ws);
        }
        Ex::Substitute(s) => substitute(ws, s),
        Ex::NoHighlight => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::ws_with;
    use super::*;
    use crate::document::DocId;

    #[test]
    fn regex_search_wraps_and_falls_back_to_text() {
        let b = Buffer::from("one two\nthree two\n(x");
        assert_eq!(search(&b, "two", 0, false), Some(4));
        assert_eq!(search(&b, "two", 4, false), Some(14));
        assert_eq!(search(&b, "two", 14, false), Some(4), "wraps");
        assert_eq!(search(&b, "t\\(wo\\|h\\)", 5, false), Some(8));
        assert_eq!(search(&b, "two", 14, true), Some(4));
        assert_eq!(search(&b, "(x", 0, false), Some(18), "not a valid pattern: plain text");
        assert_eq!(search(&b, "nope", 0, false), None);
    }

    #[test]
    fn an_insert_session_is_one_undo_step() {
        let mut d = Document::new(DocId(1), None, "abc", 1 << 20);
        let base = d.buffer().clone();
        let rev = d.revision();
        for (i, c) in ["x", "y", "z"].iter().enumerate() {
            d.apply(Transaction::insert(1 + i, *c).with_selection(Selection::cursor(2 + i)), EditKind::Other).unwrap();
        }
        collapse_insert(&mut d, &base, rev);
        assert_eq!(&*d.text(), "axyzbc");
        assert!(d.undo());
        assert_eq!(&*d.text(), "abc");
        assert!(!d.can_undo(), "one step");
        assert!(d.redo());
        assert_eq!(&*d.text(), "axyzbc");
    }

    #[test]
    fn undo_puts_the_cursor_on_the_change() {
        let mut d = Document::new(DocId(1), None, "one\ntwo\nthree", 1 << 20);
        d.set_selection(Selection::cursor(9));
        d.apply(Transaction::delete(8, 13).with_selection(Selection::cursor(8)), EditKind::Other).unwrap();
        d.set_selection(Selection::cursor(0));
        assert_eq!(vim_undo(&mut d, 1, false), None);
        assert_eq!((&*d.text(), d.selection().head), ("one\ntwo\nthree", 9));
        assert_eq!(vim_undo(&mut d, 1, false).as_deref(), Some("Already at oldest change"));
        assert_eq!(vim_undo(&mut d, 1, true), None);
        assert_eq!(&*d.text(), "one\ntwo\n");
    }

    #[test]
    fn ex_commands_act_on_the_workspace() {
        let mut ws = ws_with(&[("A.md", "a a\nb a\nc"), ("B.md", "b")]);
        ws.open_note(&"A.md".into());
        let sub =
            |lines, g| Ex::Substitute(Substitute { lines, pattern: "a".into(), replacement: "<&>".into(), global: g, ignore_case: false });
        vim_ex(&mut ws, &sub(0..1, false));
        assert_eq!(&*ws.active_doc().unwrap().text(), "<a> a\nb a\nc");
        vim_ex(&mut ws, &sub(0..3, true));
        assert_eq!(&*ws.active_doc().unwrap().text(), "<<a>> <a>\nb <a>\nc");
        vim_ex(&mut ws, &Ex::Write);
        assert_eq!(ws.vault().unwrap().read(&"A.md".into()).unwrap(), "<<a>> <a>\nb <a>\nc");
        vim_ex(&mut ws, &sub(2..3, true));
        assert!(ws.features.nav.vim_message.as_ref().is_some_and(|(m, _)| m == "Pattern not found: a"));
        ws.open_note(&"B.md".into());
        vim_ex(&mut ws, &Ex::Quit);
        assert_eq!(ws.docs().len(), 1);
        vim_ex(&mut ws, &Ex::QuitAll);
        assert!(ws.docs().is_empty());
    }
}
