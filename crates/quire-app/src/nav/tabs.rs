// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tab modes and where you were in each note (design §F.3).
//!
//! In reuse mode (the default) a note opened with a single click takes over
//! the "preview" tab, shown in italics, instead of adding a tab; editing it,
//! double-clicking it or opening it with Ctrl+click or a middle click keeps
//! it. In new mode every note gets its own tab.

use std::time::{Duration, Instant};

use quire_text::Selection;

use crate::document::DocId;
use crate::session::{MAX_POSITIONS, Position, Session, stash_positions, trim_positions};
use crate::workspace::Workspace;

/// How often changed positions are handed to the session save.
pub(super) const STASH_EVERY: Duration = Duration::from_secs(2);

/// Switching notes (or turning Vim keys off) returns Vim to Normal mode.
pub(super) fn track_vim_doc(ws: &mut Workspace) {
    let active = ws.active_doc().map(|d| d.id);
    let st = &mut ws.features.nav;
    if !ws.settings.editing.vim {
        if st.vim.mode() != quire_text::vim::Mode::Normal || st.vim.cmdline().is_some() {
            st.vim.reset();
        }
        return;
    }
    if st.vim_doc != active {
        st.vim.reset();
        st.vim_doc = active;
    }
    // `>` and `<` shift by the editor's indent.
    if st.vim.indent != ws.settings.indent_unit {
        st.vim.indent.clone_from(&ws.settings.indent_unit);
    }
}

/// Keep the preview tab: newly opened notes replace it in reuse mode.
pub(super) fn track_tabs(ws: &mut Workspace) -> bool {
    let ids: Vec<DocId> = ws.docs.iter().map(|d| d.id).collect();
    let reuse = ws.settings.editing.tab_mode != "new";
    let st = &mut ws.features.nav;
    if let Some(p) = st.preview {
        // Gone, or edited (which keeps it).
        match ws.docs.iter().find(|d| d.id == p) {
            Some(d) if d.revision() == st.preview_rev && !d.is_dirty() => {}
            _ => st.preview = None,
        }
    }
    let new: Vec<DocId> = ids.iter().copied().filter(|id| !st.known.contains(id)).collect();
    st.known = ids;
    let pinned = std::mem::take(&mut st.open_pinned);
    if new.is_empty() {
        return false;
    }
    if !reuse {
        st.preview = None;
        return false;
    }
    let mut changed = false;
    for id in new {
        let Some(doc) = ws.docs.iter().find(|d| d.id == id) else { continue };
        if pinned || doc.path.is_none() || doc.auto_name || doc.is_dirty() {
            continue;
        }
        let rev = doc.revision();
        if let Some(old) = ws.features.nav.preview.filter(|o| *o != id) {
            replace_tab(ws, old, id);
            changed = true;
        }
        ws.features.nav.preview = Some(id);
        ws.features.nav.preview_rev = rev;
    }
    changed
}

/// Close the preview tab `old` and put `new` where it was.
fn replace_tab(ws: &mut Workspace, old: DocId, new: DocId) {
    let Some(old_pos) = ws.docs.iter().position(|d| d.id == old) else { return };
    let was_active = ws.active_doc().map(|d| d.id);
    ws.close(old);
    let Some(i) = ws.docs.iter().position(|d| d.id == new) else { return };
    let doc = ws.docs.remove(i);
    let at = old_pos.min(ws.docs.len());
    ws.docs.insert(at, doc);
    ws.features.nav.known.retain(|k| *k != old);
    // Whatever was active stays active.
    let active = if was_active == Some(old) { Some(new) } else { was_active };
    ws.active = active.and_then(|a| ws.docs.iter().position(|d| d.id == a)).or(Some(at));
}

/// Keep a tab: it stops being the preview tab.
pub fn pin(ws: &mut Workspace, id: DocId) {
    if ws.features.nav.preview == Some(id) {
        ws.features.nav.preview = None;
    }
}

pub(super) fn load_positions(ws: &mut Workspace) {
    if let (Some(p), Some(root)) = (Session::path(), ws.vault_root()) {
        ws.features.nav.positions = crate::session::load_positions(&p, root);
    }
}

/// Remember where you are in a note (the editor calls this as you move).
pub fn remember_position(ws: &mut Workspace, id: DocId, caret: usize, scroll: f32) {
    let Some(path) = ws.docs.iter().find(|d| d.id == id).and_then(|d| d.path.as_ref()) else { return };
    let st = &mut ws.features.nav;
    let key = path.as_str();
    match st.positions.get_mut(key) {
        Some(p) if p.caret == caret && (p.scroll - scroll).abs() < 1.0 => {}
        Some(p) => {
            *p = Position { caret, scroll, at: crate::history_ops::now_ms() };
            st.positions_dirty = true;
        }
        None => {
            st.positions.insert(key.to_string(), Position { caret, scroll, at: crate::history_ops::now_ms() });
            st.positions_dirty = true;
        }
    }
}

/// When a note is shown for the first time: put the caret where it was last
/// time (unless something already placed it) and return the scroll offset to
/// restore, if the caret is where it was.
pub fn restore_position(ws: &mut Workspace, id: DocId) -> Option<f32> {
    let doc = ws.docs.iter_mut().find(|d| d.id == id)?;
    let pos = *ws.features.nav.positions.get(doc.path.as_ref()?.as_str())?;
    let len = doc.buffer().len_chars();
    if doc.revision() == 0 && doc.selection() == Selection::cursor(0) && pos.caret > 0 && pos.caret <= len {
        doc.set_selection(Selection::cursor(pos.caret));
    }
    (doc.selection().head == pos.caret).then_some(pos.scroll)
}

/// Hand changed positions to the session save now and then.
pub(super) fn track_positions(ws: &mut Workspace, now: Instant) {
    let st = &mut ws.features.nav;
    if !st.positions_dirty || st.last_stash.is_some_and(|t| now.duration_since(t) < STASH_EVERY) {
        return;
    }
    trim_positions(&mut st.positions, MAX_POSITIONS);
    st.positions_dirty = false;
    st.last_stash = Some(now);
    if !cfg!(test) {
        stash_positions(ws.vault_root(), &ws.features.nav.positions);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::ws_with;
    use super::*;

    fn titles(ws: &Workspace) -> Vec<String> {
        ws.docs().iter().map(crate::Document::title).collect()
    }

    #[test]
    fn reuse_mode_replaces_the_preview_tab() {
        let mut ws = ws_with(&[("A.md", "a"), ("B.md", "b"), ("C.md", "c"), ("D.md", "d")]);
        ws.tick(Instant::now());
        ws.open_note(&"A.md".into());
        ws.tick(Instant::now());
        assert_eq!(ws.features.nav.preview, ws.active_doc().map(|d| d.id));
        ws.open_note(&"B.md".into());
        ws.tick(Instant::now());
        assert_eq!(titles(&ws), ["B"], "B took A's tab");
        assert_eq!(ws.active_doc().map(crate::Document::title).as_deref(), Some("B"));
        // Editing keeps a tab.
        let id = ws.active_doc().unwrap().id;
        ws.insert_text("more ");
        ws.tick(Instant::now());
        assert_eq!(ws.features.nav.preview, None);
        ws.open_note(&"C.md".into());
        ws.tick(Instant::now());
        assert_eq!(titles(&ws), ["B", "C"]);
        // Ctrl+click or a middle click: its own tab.
        ws.features.nav.open_pinned = true;
        ws.open_note(&"D.md".into());
        ws.tick(Instant::now());
        assert_eq!(titles(&ws), ["B", "C", "D"]);
        // Opening an open note switches to it.
        ws.open_note(&"B.md".into());
        ws.tick(Instant::now());
        assert_eq!(ws.active_doc().map(|d| d.id), Some(id));
        assert_eq!(titles(&ws), ["B", "C", "D"]);
    }

    #[test]
    fn new_mode_keeps_every_tab() {
        let mut ws = ws_with(&[("A.md", "a"), ("B.md", "b")]);
        ws.settings.editing.tab_mode = "new".into();
        ws.tick(Instant::now());
        ws.open_note(&"A.md".into());
        ws.tick(Instant::now());
        ws.open_note(&"B.md".into());
        ws.tick(Instant::now());
        assert_eq!(titles(&ws), ["A", "B"]);
        assert_eq!(ws.features.nav.preview, None);
    }

    #[test]
    fn notes_reopen_where_you_left_them() {
        let mut ws = ws_with(&[("A.md", "line one\nline two\nline three")]);
        ws.tick(Instant::now());
        let id = ws.open_note(&"A.md".into()).unwrap();
        ws.active_doc_mut().unwrap().set_selection(Selection::cursor(12));
        remember_position(&mut ws, id, 12, 340.0);
        ws.close(id);
        let id = ws.open_note(&"A.md".into()).unwrap();
        assert_eq!(restore_position(&mut ws, id), Some(340.0));
        assert_eq!(ws.active_doc().unwrap().selection(), Selection::cursor(12));
        // A jump that already placed the caret wins.
        ws.close(id);
        let id = ws.open_note(&"A.md".into()).unwrap();
        ws.jump_to_byte(3);
        assert_eq!(restore_position(&mut ws, id), None);
        assert_eq!(ws.active_doc().unwrap().selection(), Selection::cursor(3));
    }

    #[test]
    fn switching_notes_leaves_insert_mode() {
        let mut ws = ws_with(&[("A.md", "a"), ("B.md", "b")]);
        ws.settings.editing.vim = true;
        ws.open_note(&"A.md".into());
        ws.tick(Instant::now());
        let b = ws.active_doc().unwrap().buffer().clone();
        let _ = ws.features.nav.vim.handle(quire_text::vim::Key::Char('i'), &b, Selection::cursor(0), &mut quire_text::vim::PlainHost);
        assert!(ws.features.nav.vim.typing());
        ws.features.nav.open_pinned = true;
        ws.open_note(&"B.md".into());
        ws.tick(Instant::now());
        assert!(!ws.features.nav.vim.typing());
    }
}
