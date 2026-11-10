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

/// Settings → Editing (the Settings dialog scrolls it).
pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    editing(ui, ws, pal);
    PanelOut::default()
}

fn editing(ui: &mut Ui, ws: &mut Workspace, pal: &Palette) {
    let e = &mut ws.settings.editing;
    ui.checkbox(&mut e.vim, RichText::new("Vim keys").strong());
    note(
        ui,
        pal,
        "Normal, Insert and Visual modes, operators with motions and text objects, counts, registers (\"+ is the clipboard), . to repeat, \
         u and Ctrl+R, / and ? to search, :w :q :s, macros (qa … q, @a, @@), marks (ma, `a, 'a, '', `.) and Visual-block mode \
         (Ctrl+V, with I, A, c, d, y, p, r and $). Not supported: a vimrc, :normal, Ctrl+A and Ctrl+X. \
         Vim keys work in the fast editor (not the classic one); embedded notes are edited without them.",
    );
    if e.vim && !cfg!(target_os = "macos") {
        ui.checkbox(&mut e.vim_ctrl_scroll, "Ctrl+B, F, D and U scroll, as in Vim");
        note(
            ui,
            pal,
            "In Normal and Visual mode: a page back and forward, half a page down and up. Off: those keys run Quire's commands (Bold, Find…).",
        );
    }

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
    note(
        ui,
        pal,
        "Either way, Ctrl+click or a middle click opens a note in a tab of its own, and a note that is already open is switched to, never opened twice.",
    );

    ui.add_space(10.0);
    ui.checkbox(&mut e.display_titles, "Show note titles instead of file names");
    note(ui, pal, "In the sidebar and on tabs: the title from front matter or the first heading. Links keep using file names.");
    ui.add_space(6.0);
    ui.checkbox(&mut e.two_pane, "Folders and notes side by side in the sidebar");
    ui.add_space(6.0);
    let root = e.new_note_folder == "root";
    ui.horizontal_wrapped(|ui| {
        ui.label("New notes (Ctrl+N) go");
        if ui.radio(!root, "in the open note's folder").clicked() {
            e.new_note_folder = "current".into();
        }
        if ui.radio(root, "at the top of the vault").clicked() {
            e.new_note_folder = "root".into();
        }
    });

    ui.add_space(10.0);
    date_format(ui, ws, pal);

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

/// Settings → Editing → Dates: how dates look in properties and views.
fn date_format(ui: &mut Ui, ws: &mut Workspace, pal: &Palette) {
    use quire_app::datefmt::{PRESETS, display, round_trips, valid};
    ui.label(RichText::new("Dates in properties and views").strong());
    let current = ws.settings.editing.date_format.clone();
    let label = |f: &str| PRESETS.iter().find(|(p, _)| *p == f).map_or_else(|| "Custom".to_string(), |(_, l)| (*l).to_string());
    egui::ComboBox::from_id_salt("date-format")
        .selected_text(format!("{} ({})", label(&current), display("2026-11-05", &current)))
        .show_ui(ui, |ui| {
            for (f, l) in PRESETS {
                let on = current == f;
                if ui.selectable_label(on, format!("{l}  ({})", display("2026-11-05", f))).clicked() {
                    ws.settings.editing.date_format = f.to_string();
                }
            }
        });
    ui.horizontal_wrapped(|ui| {
        ui.label("Custom format");
        let w = ui.available_width().min(160.0);
        ui.add(egui::TextEdit::singleline(&mut ws.settings.editing.date_format).hint_text("%d.%m.%Y").desired_width(w));
    });
    let f = ws.settings.editing.date_format.clone();
    if !f.trim().is_empty() && !valid(&f) {
        ui.label(
            RichText::new("That isn't a date format that shows the day, the month and the year, so dates stay as stored.")
                .size(12.0)
                .color(pal.warn),
        );
    } else if !f.trim().is_empty() && !round_trips(&f) {
        ui.label(
            RichText::new("Dates in this format can't be typed back; type them as 2026-11-05 to change one.").size(12.0).color(pal.warn),
        );
    }
    note(
        ui,
        pal,
        "Only what you see changes: the file keeps 2026-11-05, and your computer's region settings are not used. Month and day names are English. \
         Codes: %d day, %m month, %Y year, %b Nov, %B November, %a Thu, %A Thursday; %-d has no leading zero.",
    );
}
