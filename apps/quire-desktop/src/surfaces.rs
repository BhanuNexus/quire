// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Surfaces: editors for files that aren't Markdown notes (a canvas, a PDF,
//! an Obsidian `.base`). The app keeps open surfaces beside the note tabs,
//! shows their tabs in the tab bar, and gives the active one the central
//! panel. `registry::open_surface` picks the surface for a file.

use egui::RichText;
use quire_app::Workspace;
use quire_app::vault::NotePath;

use crate::panels::PanelOut;
use crate::theme::Palette;

/// An editor for one file.
pub(crate) trait Surface {
    /// The file it shows.
    fn path(&self) -> &NotePath;
    /// Tab title.
    fn title(&self) -> String {
        self.path().file_name().to_string()
    }
    /// Draw into the central panel.
    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut;
}

/// For a file Quire can't edit yet: say so, and offer the system's viewer.
pub(crate) struct SystemViewer {
    path: NotePath,
    what: &'static str,
}

impl SystemViewer {
    pub(crate) fn new(path: &NotePath, what: &'static str) -> Self {
        Self { path: path.clone(), what }
    }
}

impl Surface for SystemViewer {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new(self.path.file_name()).size(20.0).color(pal.strong));
            ui.label(RichText::new(format!("{} isn't available in this version of Quire.", self.what)).color(pal.muted));
            if let Some(abs) = ws.vault().and_then(|v| v.storage().absolute(&self.path))
                && ui.button("Open in system viewer").clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(format!("file://{}", abs.display())));
            }
        });
        PanelOut::default()
    }
}

/// Open surfaces and which one has the central panel.
#[derive(Default)]
pub(crate) struct Surfaces {
    pub open: Vec<Box<dyn Surface>>,
    /// Index into `open`; `None` shows the notes.
    pub active: Option<usize>,
}

impl Surfaces {
    /// Show `s` (or the already open surface for the same file).
    #[cfg_attr(not(test), expect(dead_code, reason = "WS-E and WS-G open surfaces from the file tree and links"))]
    pub(crate) fn open(&mut self, s: Box<dyn Surface>) {
        let i = self.open.iter().position(|o| o.path() == s.path()).unwrap_or_else(|| {
            self.open.push(s);
            self.open.len() - 1
        });
        self.active = Some(i);
    }

    /// Tabs for open surfaces, drawn after the note tabs. A click makes a
    /// surface active; clicking a note tab makes notes active again.
    pub(crate) fn tab_strip(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        if self.open.is_empty() {
            return;
        }
        let mut close = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Files").size(11.0).color(pal.muted));
            for (i, s) in self.open.iter().enumerate() {
                let active = self.active == Some(i);
                if ui.selectable_label(active, s.title()).clicked() {
                    self.active = Some(i);
                }
                if active && ui.small_button("×").clicked() {
                    close = Some(i);
                }
            }
        });
        if let Some(i) = close {
            self.open.remove(i);
            self.active = None;
        }
    }

    /// Draw the active surface; false when notes have the central panel.
    pub(crate) fn show_active(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> Option<PanelOut> {
        let s = self.open.get_mut(self.active?)?;
        Some(s.show(ui, ws, pal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_without_an_editor_open_a_placeholder_surface() {
        assert!(crate::registry::open_surface(&"Note.md".into()).is_none());
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(quire_app::vault::Vault::in_memory(&[]));
        let mut s = Surfaces::default();
        for p in ["board.canvas", "papers/rope.pdf", "Tasks.base", "board.canvas"] {
            s.open(crate::registry::open_surface(&p.into()).expect("registered"));
        }
        assert_eq!(s.open.len(), 3, "the same file opens once");
        assert_eq!(s.active, Some(0));
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                s.tab_strip(ui, &crate::theme::LIGHT);
                assert!(s.show_active(ui, &mut ws, &crate::theme::LIGHT).is_some());
            });
            out.textures_delta.clear();
        }
        s.active = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| assert!(s.show_active(ui, &mut ws, &crate::theme::LIGHT).is_none()));
        out.textures_delta.clear();
        assert_eq!(s.open[1].title(), "rope.pdf");
    }
}
