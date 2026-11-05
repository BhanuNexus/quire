// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Vim keys in the fast editor: egui events in, the state machine's
//! outcomes (`quire_text::vim`) applied to the note.
//!
//! Insert mode is the editor's own typing (smart Enter, lists, IME and
//! multiple cursors keep working); only Escape goes to Vim there. In Normal
//! and Visual mode every key goes to Vim and the input method is off. An
//! insert session becomes one undo step when it ends.
//!
//! Visual-block mode shows its rows as the editor's extra selections, and
//! a block insert (`I`, `A`, `c`) types at one cursor a row. Ctrl+V reaches
//! the editor as a paste (the system takes the shortcut), so in Normal and
//! Visual mode a paste starts Visual-block mode. With an empty clipboard the
//! system passes nothing on; Vim's other key for it, Ctrl+Q, quits the app.

use egui::{Event, Key, Modifiers, Ui, ViewportCommand};
use quire_app::document::DocId;
use quire_app::text::vim::{self, Ex, Key as VKey, Mode, Outcome, Paste};
use quire_app::text::{Buffer, EditKind, Selection};
use quire_app::{Workspace, nav};

use super::{Cx, DocView, FastEditor};

/// What the adapter keeps between frames.
#[derive(Default)]
pub(crate) struct VimView {
    /// The note when an insert session began: (text, revision, note).
    base: Option<(Buffer, u64, DocId)>,
    /// `"+p` is waiting for the clipboard (asked with `RequestPaste`).
    paste: Option<(DocId, Paste)>,
    /// A `:` command to carry out after drawing (it may close the note).
    ex: Option<Ex>,
}

impl VimView {
    pub(crate) const fn take_ex(&mut self) -> Option<Ex> {
        self.ex.take()
    }
}

/// The Vim key for a key event, if Vim has a use for it.
fn vim_key(key: Key, m: Modifiers) -> Option<VKey> {
    if m.ctrl && !m.alt {
        return Some(match key {
            Key::OpenBracket => VKey::Ctrl('['),
            // Ctrl+C, V and X stay copy, paste and cut.
            Key::C | Key::V | Key::X => return None,
            k => {
                let name = k.name();
                let c = name.chars().next().filter(|c| name.len() == 1 && c.is_ascii_alphabetic())?;
                VKey::Ctrl(c.to_ascii_lowercase())
            }
        });
    }
    if m.command || m.alt {
        return None;
    }
    Some(match key {
        Key::Escape => VKey::Esc,
        Key::Enter => VKey::Enter,
        Key::Backspace => VKey::Backspace,
        Key::Delete => VKey::Delete,
        Key::Tab => VKey::Tab,
        Key::ArrowLeft => VKey::Left,
        Key::ArrowRight => VKey::Right,
        Key::ArrowUp => VKey::Up,
        Key::ArrowDown => VKey::Down,
        Key::Home => VKey::Home,
        Key::End => VKey::End,
        Key::PageUp => VKey::PageUp,
        Key::PageDown => VKey::PageDown,
        _ => return None,
    })
}

impl FastEditor {
    /// Fit the selection to the mode: a mouse selection is Visual mode, and
    /// the Normal-mode cursor never rests past a line's last character.
    pub(super) fn vim_observe(ws: &mut Workspace) {
        let Some(doc) = ws.active_doc() else { return };
        let (buf, sel) = (doc.buffer().clone(), doc.selection());
        if let Some(s) = ws.features.nav.vim.observe(&buf, sel)
            && let Some(d) = ws.active_doc_mut()
        {
            d.set_selection(s);
        }
    }

    /// Give one event to Vim. `None`: the editor handles it as usual
    /// (typing in Insert mode); otherwise whether the caret moved and the
    /// text changed.
    pub(super) fn vim_event(
        &mut self,
        ev: &Event,
        ui: &Ui,
        view: &mut DocView,
        ws: &mut Workspace,
        cx: &Cx<'_>,
        page: usize,
    ) -> Option<(bool, bool)> {
        let typing = ws.features.nav.vim.typing();
        let keys: Vec<VKey> = match ev {
            Event::Text(t) | Event::Ime(egui::ImeEvent::Commit(t)) if typing => {
                // A macro being recorded keeps what the editor types.
                ws.features.nav.vim.typed(t);
                return None;
            }
            Event::Text(t) => t.chars().map(VKey::Char).collect(),
            // Ctrl+V (taken by the system as paste): Visual-block mode.
            Event::Paste(_) if !typing && self.vim.paste.is_none() => vec![VKey::Ctrl('v')],
            Event::Key { key, pressed: true, modifiers, .. } => match vim_key(*key, *modifiers) {
                Some(k) => vec![k],
                None if typing => return None,
                // Letters come as text; other keys do nothing in Normal mode.
                None => return Some((false, false)),
            },
            Event::Paste(text) if !typing => return Some(self.vim_paste(text, ui, view, ws)),
            // Key releases, and no input method outside Insert mode.
            Event::Key { .. } | Event::Ime(_) if !typing => return Some((false, false)),
            _ => return None,
        };
        let mut moved = false;
        let mut edited = false;
        for k in keys {
            if k == VKey::Esc && ws.features.nav.vim.mode() == Mode::Normal && !view.extra.is_empty() {
                // Escape clears extra cursors.
                view.extra.clear();
            }
            let mut machine = std::mem::take(&mut ws.features.nav.vim);
            let out = {
                let Some(doc) = ws.active_doc() else {
                    ws.features.nav.vim = machine;
                    return Some((moved, edited));
                };
                let (buf, sel) = (doc.buffer().clone(), doc.selection());
                let mut rows = |pos: usize, down: bool| -> Option<usize> {
                    let (to, _) = self.vertical(ui, view, doc, cx, pos, if down { 1 } else { -1 }, None);
                    (to != pos).then_some(to)
                };
                let mut host = nav::VimHost { rows: Some(&mut rows), page };
                machine.handle(k, &buf, sel, &mut host)
            };
            let message = machine.take_message();
            ws.features.nav.vim = machine;
            if let Some(m) = message {
                nav::vim_say(ws, m);
            }
            if !out.consumed {
                // Insert mode: the editor types it.
                return (moved || edited).then_some((moved, edited));
            }
            let (m, e) = self.apply_vim(out, ui, view, ws);
            moved |= m;
            edited |= e;
        }
        Some((moved, edited))
    }

    /// Text from the clipboard: the answer to `"+p`, or Ctrl+V in Normal
    /// mode (put before the cursor). In Visual mode it replaces the selection.
    fn vim_paste(&mut self, text: &str, ui: &Ui, view: &mut DocView, ws: &mut Workspace) -> (bool, bool) {
        let Some(doc) = ws.active_doc() else { return (false, false) };
        let (id, buf, sel) = (doc.id, doc.buffer().clone(), doc.selection());
        let paste = match self.vim.paste.take() {
            Some((pid, p)) if pid == id => p,
            _ if matches!(ws.features.nav.vim.mode(), Mode::Visual | Mode::VisualLine | Mode::VisualBlock) => {
                let tx = quire_app::text::Transaction::replace(sel.start(), sel.end(), text)
                    .with_selection(Selection::cursor((sel.start() + text.chars().count()).saturating_sub(1)));
                if let Some(d) = ws.active_doc_mut()
                    && d.apply(tx, EditKind::Other).is_ok()
                {
                    view.own_rev = d.revision();
                }
                ws.features.nav.vim.reset();
                return (true, true);
            }
            _ => Paste { before: true, count: 1 },
        };
        let out = ws.features.nav.vim.put_clipboard(text, paste, &buf, sel);
        self.apply_vim(out, ui, view, ws)
    }

    /// Carry out what Vim asked for. Returns (moved, edited).
    fn apply_vim(&mut self, out: Outcome, ui: &Ui, view: &mut DocView, ws: &mut Workspace) -> (bool, bool) {
        let ctx = ui.ctx();
        let Some(doc) = ws.active_doc_mut() else { return (false, false) };
        let id = doc.id;
        let rev = doc.revision();
        if out.group == vim::Group::Begin {
            self.vim.base = Some((doc.buffer().clone(), rev, id));
        }
        if let Some(b) = out.before {
            doc.set_selection(b);
        }
        if let Some(tx) = out.edit {
            if doc.apply(tx, EditKind::Other).is_ok() {
                view.own_rev = doc.revision();
            }
        } else if let Some(s) = out.selection {
            doc.set_selection(s);
        }
        if let Some(extra) = out.extra {
            // A Visual block's other rows, or one cursor a row to type at.
            view.extra = extra;
        }
        if out.group == vim::Group::End
            && let Some((base, base_rev, bid)) = self.vim.base.take()
            && bid == id
        {
            nav::collapse_insert(doc, &base, base_rev);
            view.own_rev = doc.revision();
        }
        let undone = if out.undo > 0 { nav::vim_undo(doc, out.undo, false) } else { None };
        let message = if out.redo > 0 { nav::vim_undo(doc, out.redo, true) } else { undone };
        let edited = doc.revision() != rev;
        if let Some(c) = out.clipboard {
            ctx.copy_text(c);
        }
        if let Some(p) = out.paste_clipboard {
            self.vim.paste = Some((id, p));
            ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
        }
        if let Some(ex) = out.ex {
            self.vim.ex = Some(ex);
        }
        if let Some(m) = message {
            nav::vim_say(ws, m);
        }
        view.preferred_x = None;
        (true, edited)
    }
}

#[cfg(test)]
mod tests {
    use egui::{Pos2, RawInput, Rect, Vec2};
    use quire_app::Settings;
    use quire_app::vault::Vault;

    use super::*;

    /// egui events for keys in Vim notation, as a keyboard sends them.
    fn events(keys: &str) -> Vec<Event> {
        let key = |k: Key, modifiers: Modifiers| Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers };
        VKey::parse_seq(keys)
            .into_iter()
            .map(|k| match k {
                VKey::Char(c) => Event::Text(c.to_string()),
                VKey::Esc => key(Key::Escape, Modifiers::NONE),
                VKey::Enter => key(Key::Enter, Modifiers::NONE),
                VKey::Backspace => key(Key::Backspace, Modifiers::NONE),
                VKey::Ctrl(c) => key(Key::from_name(&c.to_ascii_uppercase().to_string()).expect("a letter"), Modifiers::CTRL),
                other => panic!("not used here: {other:?}"),
            })
            .collect()
    }

    struct Editor {
        ctx: egui::Context,
        fast: FastEditor,
        ws: Workspace,
    }

    impl Editor {
        fn new(text: &str) -> Self {
            let mut ws = Workspace::new(Settings::default());
            ws.attach_vault(Vault::in_memory(&[("A.md", text)]));
            ws.settings.editing.vim = true;
            ws.open_note(&"A.md".into());
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            let fast = FastEditor { request_focus: true, ..FastEditor::default() };
            let mut e = Self { ctx, fast, ws };
            // egui lets the editor keep Escape (its focus filter) only from
            // the frame after it took focus.
            for _ in 0..3 {
                e.keys("");
            }
            e
        }

        /// One frame with these keys.
        fn keys(&mut self, keys: &str) -> String {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
                events: events(keys),
                ..Default::default()
            };
            let (fast, ws) = (&mut self.fast, &mut self.ws);
            let mut out = self.ctx.run_ui(input, |ui| fast.show(ui, ws, &crate::theme::LIGHT, crate::theme::FontSupport::default()));
            out.textures_delta.clear();
            self.ws.tick(std::time::Instant::now());
            self.ws.active_doc().map(|d| d.text().to_string()).unwrap_or_default()
        }

        /// Ctrl+V as the system sends it: a paste of the clipboard.
        fn paste(&mut self, text: &str) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
                events: vec![Event::Paste(text.into())],
                ..Default::default()
            };
            let (fast, ws) = (&mut self.fast, &mut self.ws);
            let mut out = self.ctx.run_ui(input, |ui| fast.show(ui, ws, &crate::theme::LIGHT, crate::theme::FontSupport::default()));
            out.textures_delta.clear();
        }

        fn caret(&self) -> usize {
            self.ws.active_doc().map_or(0, |d| d.selection().head)
        }
    }

    #[test]
    fn vim_keys_edit_the_note() {
        let mut e = Editor::new("one two\nthree\n");
        assert_eq!(e.keys("wcwTWO<Esc>"), "one TWO\nthree\n");
        assert_eq!(e.caret(), 6, "Escape steps back onto the last letter typed");
        assert_eq!(e.keys("u"), "one two\nthree\n", "the change and its typing are one step");
        assert_eq!(e.keys("<C-r>"), "one TWO\nthree\n");
        assert_eq!(e.keys("jA!<CR>four<Esc>"), "one TWO\nthree!\nfour\n", "Insert mode is the editor's typing");
        assert_eq!(e.keys("u"), "one TWO\nthree\n");
        assert_eq!(e.keys("ggddp"), "three\none TWO\n");
        assert_eq!(e.keys("Vd"), "three\n");
        assert_eq!(e.ws.features.nav.vim.mode(), Mode::Normal);
        // :s goes through the workspace.
        assert_eq!(e.keys(":%s/r/R/<CR>"), "thRee\n");
        // Vim keys off: the editor types again.
        e.ws.settings.editing.vim = false;
        assert_eq!(e.keys("x"), "xthRee\n");
    }

    #[test]
    fn block_mode_types_on_every_row() {
        let mut e = Editor::new("abc\ndef\nghi\n");
        let id = e.ws.active_doc().map(|d| d.id).expect("a note");
        e.paste("x");
        e.keys("jj");
        assert_eq!(e.ws.features.nav.vim.mode(), Mode::VisualBlock);
        assert!(e.fast.has_extra_cursors(id), "the other rows are selections");
        assert_eq!(e.keys("I-"), "-abc\n-def\n-ghi\n", "one cursor a row");
        assert_eq!(e.keys("<Esc>"), "-abc\n-def\n-ghi\n");
        assert!(!e.fast.has_extra_cursors(id));
        assert_eq!(e.keys("u"), "abc\ndef\nghi\n", "the block insert is one undo step");
        // Again, from Normal mode: Ctrl+V comes as a paste.
        e.paste("x");
        assert_eq!(e.ws.features.nav.vim.mode(), Mode::VisualBlock);
        assert_eq!(e.keys("jd"), "bc\nef\nghi\n");
    }

    #[test]
    fn a_macro_records_typing_and_undoes_in_one_step() {
        let mut e = Editor::new("a\nb\nc\n");
        assert_eq!(e.keys("qaA!<Esc>jq"), "a!\nb\nc\n");
        assert_eq!(e.keys("@a"), "a!\nb!\nc\n", "the typed ! was recorded");
        assert_eq!(e.keys("u"), "a!\nb\nc\n");
        // Marks: set, edit above, jump back.
        assert_eq!(e.keys("ggjjmaggOtop<Esc>'ax"), "top\na!\nb\n\n", "the mark moved down with its line");
    }

    #[test]
    fn insert_mode_keeps_smart_typing() {
        let mut e = Editor::new("- item");
        assert_eq!(e.keys("A<CR>next<Esc>"), "- item\n- next", "Enter continues the list in Insert mode");
    }

    #[test]
    fn keys_for_vim() {
        assert_eq!(vim_key(Key::Escape, Modifiers::NONE), Some(VKey::Esc));
        assert_eq!(vim_key(Key::R, Modifiers::CTRL), Some(VKey::Ctrl('r')));
        assert_eq!(vim_key(Key::OpenBracket, Modifiers::CTRL), Some(VKey::Ctrl('[')));
        assert_eq!(vim_key(Key::C, Modifiers::CTRL), None, "Ctrl+C stays copy");
        assert_eq!(vim_key(Key::A, Modifiers::NONE), None, "letters come as text");
        assert_eq!(vim_key(Key::ArrowDown, Modifiers::SHIFT), Some(VKey::Down));
        assert_eq!(vim_key(Key::S, Modifiers::ALT), None);
    }
}
