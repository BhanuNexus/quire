// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Editing: Vim keys, tab mode, display titles, the homepage
//! and the file browser layout (`[editing]` in settings.toml).

use egui::{RichText, Ui};
use quire_app::Workspace;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

pub(crate) const fn available(_ws: &Workspace) -> bool {
    true
}

fn note(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(pal.muted)).wrap());
}

pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let max_h = crate::theme_ui::tab_height(ui);
    egui::ScrollArea::vertical()
        .id_salt("quire-settings-editing")
        .max_height(max_h)
        .auto_shrink([false, true])
        .show(ui, |ui| editing(ui, ws, pal));
    PanelOut::default()
}

fn editing(ui: &mut Ui, ws: &mut Workspace, pal: &Palette) {
    let e = &mut ws.settings.editing;
    ui.checkbox(&mut e.vim, RichText::new("Vim keys").strong());
    note(
        ui,
        pal,
        "Normal, Insert and Visual modes, operators with motions and text objects, counts, registers (\"+ is the clipboard), . to repeat, \
         u and Ctrl+R, / and ? to search, :w :q :s. Not supported: macros, marks, visual block mode and a vimrc. \
         Vim keys work in the fast editor (not the classic one); embedded notes are edited without them.",
    );

    ui.add_space(10.0);
    ui.label(RichText::new("Opening notes").strong());
    let reuse = e.tab_mode != "new";
    if ui.radio(reuse, "Reuse a preview tab").clicked() {
        e.tab_mode = "reuse".into();
    }
    note(ui, pal, "A note opened with one click replaces the tab in italics until you edit it or double-click it.");
    if ui.radio(!reuse, "Open every note in a new tab").clicked() {
        e.tab_mode = "new".into();
    }
    note(ui, pal, "Either way, Ctrl+click or a middle click opens a note in a tab of its own.");

    ui.add_space(10.0);
    ui.checkbox(&mut e.display_titles, "Show note titles instead of file names");
    note(ui, pal, "In the sidebar and on tabs: the title from front matter or the first heading. Links keep using file names.");
    ui.add_space(6.0);
    ui.checkbox(&mut e.two_pane, "Folders and notes side by side in the sidebar");

    ui.add_space(10.0);
    ui.label(RichText::new("Homepage").strong());
    note(ui, pal, "Opens when Quire starts without notes from last time, and with Go: Open homepage note.");
    let active = ws.active_doc().and_then(|d| d.path.clone());
    ui.horizontal_wrapped(|ui| {
        let e = &mut ws.settings.editing;
        let w = ui.available_width().min(200.0);
        ui.add(egui::TextEdit::singleline(&mut e.homepage).hint_text("Note name").desired_width(w));
        if let Some(p) = &active
            && ui.small_button("This note").clicked()
        {
            e.homepage = p.without_ext().to_string();
        }
        if !e.homepage.is_empty() && ui.small_button("None").clicked() {
            e.homepage.clear();
        }
    });
    let home = ws.settings.editing.homepage.trim().trim_end_matches(".md").to_string();
    if !home.is_empty() && ws.index().read().resolve(&home, None).is_none() {
        ui.label(RichText::new(format!("No note called “{home}” in this vault")).size(12.0).color(pal.warn));
    }
}
