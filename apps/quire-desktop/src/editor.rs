// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The editor adapter: keeps egui's `TextEdit` (which wants a `String`) in
//! sync with the rope-backed `Document`, and routes smart keys to the core.

use std::collections::HashMap;

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{Event, Key, Modifiers, TextBuffer, TextEdit};
use quire_app::text::Selection;
use quire_app::{DocId, Workspace};

use crate::highlight::HighlightCache;
use crate::theme::{FontSupport, Palette};

/// The widget-side copy of a document.
struct Mirror {
    text: String,
    rev: u64,
    sel: Selection,
}

#[derive(Default)]
pub struct EditorState {
    mirrors: HashMap<DocId, Mirror>,
    highlight: HashMap<DocId, HighlightCache>,
    /// Scroll the caret into view on the next frame (after jumps).
    pub scroll_to_caret: bool,
    pub request_focus: bool,
}

pub fn editor_id(doc: DocId) -> egui::Id {
    egui::Id::unique(("quire-editor", doc.0))
}

fn store_selection(ctx: &egui::Context, id: egui::Id, sel: Selection) {
    let mut st = TextEditState::load(ctx, id).unwrap_or_default();
    st.cursor.set_char_range(Some(CCursorRange { primary: CCursor::new(sel.head), secondary: CCursor::new(sel.anchor), h_pos: None }));
    st.store(ctx, id);
}

impl EditorState {
    pub fn forget(&mut self, docs: &[DocId]) {
        self.mirrors.retain(|k, _| docs.contains(k));
        self.highlight.retain(|k, _| docs.contains(k));
    }

    /// Intercept keys the core handles better than a plain text widget.
    /// Must run before the `TextEdit` is shown this frame.
    pub fn handle_smart_keys(&mut self, ctx: &egui::Context, ws: &mut Workspace) {
        let Some(doc) = ws.active_doc() else { return };
        let id = editor_id(doc.id);
        if !ctx.memory(|m| m.has_focus(id)) {
            return;
        }
        // Only act when the key is the sole text-changing event this frame,
        // otherwise ordering with typed characters could be lost.
        let (enter, tab, shift_tab, space, other_text) = ctx.input(|i| {
            let mut texts = 0;
            for e in &i.events {
                match e {
                    Event::Text(_) | Event::Paste(_) | Event::Cut | Event::Ime(_) => texts += 1,
                    Event::Key { key: Key::Backspace | Key::Delete, pressed: true, .. } => texts += 1,
                    _ => {}
                }
            }
            let m = i.modifiers;
            let only_space_text = texts == 1 && i.events.iter().any(|e| matches!(e, Event::Text(t) if t == " "));
            (
                i.key_pressed(Key::Enter) && m.is_none(),
                i.key_pressed(Key::Tab) && m.is_none(),
                i.key_pressed(Key::Tab) && m.shift_only(),
                i.key_pressed(Key::Space) && only_space_text && (m.is_none() || m.shift_only()),
                texts,
            )
        });
        if enter && other_text == 0 && ws.smart_enter() {
            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        }
        if tab && other_text == 0 && ws.smart_tab(false) {
            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
        }
        if shift_tab && other_text == 0 {
            ws.smart_tab(true);
            ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab));
        }
        if space && ws.smart_space() {
            ctx.input_mut(|i| {
                i.events.retain(|e| !matches!(e, Event::Text(t) if t == " ") && !matches!(e, Event::Key { key: Key::Space, .. }));
            });
        }
    }

    /// Draw the editor for the active document.
    pub fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, fonts: FontSupport) {
        let ctx = ui.ctx().clone();
        let Some(doc) = ws.active_doc() else { return };
        let doc_id = doc.id;
        let id = editor_id(doc_id);
        let plain_mode = doc.large;

        // Core → widget: the document changed (command, undo, reload) or the caret was moved.
        let fresh = !self.mirrors.contains_key(&doc_id);
        let mirror = self.mirrors.entry(doc_id).or_insert_with(|| Mirror {
            text: doc.text().to_string(),
            rev: doc.revision(),
            sel: doc.selection(),
        });
        if fresh {
            // A new widget has no cursor state; seed it from the document.
            store_selection(&ctx, id, mirror.sel);
            self.scroll_to_caret = mirror.sel.head > 0;
        }
        if mirror.rev != doc.revision() {
            mirror.text = doc.text().to_string();
            mirror.rev = doc.revision();
            mirror.sel = doc.selection();
            store_selection(&ctx, id, mirror.sel);
        } else if mirror.sel != doc.selection() {
            mirror.sel = doc.selection();
            store_selection(&ctx, id, mirror.sel);
            self.scroll_to_caret = mirror.sel.head > 0;
        }

        let size = ws.settings.font_size;
        let max_w = ws.settings.line_width as f32 * size * 0.56;
        let hl = self.highlight.entry(doc_id).or_default();
        let mut layouter = |ui: &egui::Ui, buf: &dyn TextBuffer, wrap: f32| {
            let mut job = hl.job(buf.as_str(), plain_mode, pal, size, fonts);
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };

        let scroll_to_caret = std::mem::take(&mut self.scroll_to_caret);
        let request_focus = std::mem::take(&mut self.request_focus);
        let mut output = None;
        let scroll_key =
            ws.active_doc().and_then(|d| d.path.as_ref()).map_or_else(|| format!("doc-{}", doc_id.0), |p| p.as_str().to_string());
        egui::ScrollArea::vertical().id_salt(("quire-scroll", scroll_key)).auto_shrink([false, false]).show(ui, |ui| {
            let avail = ui.available_width();
            let w = avail.min(max_w).max(120.0);
            let pad = ((avail - w) / 2.0).max(0.0);
            ui.horizontal_top(|ui| {
                ui.add_space(pad);
                let out = TextEdit::multiline(&mut mirror.text)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::symmetric(2, 28))
                    .desired_width(w)
                    .desired_rows(24)
                    .lock_focus(true)
                    .cursor_at_end(false)
                    .hint_text("Start writing…   # heading · - list · [[link]] · #tag · @tomorrow · 12 * 4 =")
                    .layouter(&mut layouter)
                    .show(ui);
                if scroll_to_caret && let Some(r) = out.cursor_range {
                    let rect = out.galley.pos_from_cursor(r.primary).translate(out.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect.expand(48.0), Some(egui::Align::Center));
                }
                output = Some(out);
            });
        });
        let Some(out) = output else { return };
        if request_focus {
            out.response.request_focus();
        }

        // Widget → core.
        let sel = out.cursor_range.map_or(mirror.sel, |r| Selection::new(r.secondary.index.into(), r.primary.index.into()));
        let ctrl_click = out.response.clicked() && ctx.input(|i| i.modifiers.command);
        if let Some(doc) = ws.active_doc_mut() {
            if out.response.changed() {
                doc.apply_widget_text(&mirror.text, sel);
                mirror.rev = doc.revision();
                mirror.sel = doc.selection();
            } else if sel != mirror.sel {
                doc.set_selection(sel);
                mirror.sel = sel;
            }
        }
        if ctrl_click && ws.follow_link_at_caret() {
            self.request_focus = true;
            self.scroll_to_caret = true;
        }
    }
}
