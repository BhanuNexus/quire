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
    /// It has done its job and its tab goes away (a locked note's lock
    /// screen, once the note is open).
    fn done(&self) -> bool {
        false
    }
    /// Once a frame while another tab is shown: work that mustn't wait for
    /// the tab to be shown again (a drawing's autosave, 0.13).
    fn tick(&mut self, ws: &mut Workspace) {
        let _ = ws;
    }
    /// Its tab is closed, or the app quits: write what isn't written yet.
    fn close(&mut self, ws: &mut Workspace) {
        let _ = ws;
    }
    /// Edits that are not written yet (after [`Self::close`] failed to
    /// write them): its tab stays open rather than losing them.
    fn unsaved(&self) -> bool {
        false
    }
    /// The person chose to lose the unsaved edits: forget them, so nothing
    /// writes them later (its drop too).
    fn discard(&mut self) {}
    /// A name to offer for "Save a copy as…": beside the file, `(copy)`
    /// before its extension.
    fn suggested_copy(&self) -> NotePath {
        let path = self.path();
        let ext = path.file_name().rsplit_once('.').map_or(String::new(), |(_, e)| format!(".{e}"));
        NotePath::join(path.folder(), &format!("{} (copy){ext}", path.stem()))
    }
    /// Write the edits to a new file, `to`, instead (when the file itself
    /// can't be written). `Err` says why not.
    fn save_copy_as(&mut self, ws: &mut Workspace, to: &NotePath) -> Result<(), String> {
        let _ = (ws, to);
        Err("This can't be saved as a copy".into())
    }
}

/// For a file Quire can't edit yet: say so, and offer the system's viewer.
#[expect(dead_code, reason = "every registered file type has an editor now; kept for the next one")]
pub(crate) struct SystemViewer {
    path: NotePath,
    what: &'static str,
}

#[expect(dead_code, reason = "every registered file type has an editor now; kept for the next one")]
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
    /// A tab whose changes can't be written, and the person closing it.
    stuck: Option<Stuck>,
}

/// The "can't be saved" window of one tab.
struct Stuck {
    path: NotePath,
    /// The name typed for "Save a copy".
    copy: String,
    error: Option<String>,
    /// "Close without saving" was pressed: asking once more.
    confirm: bool,
}

impl Surfaces {
    /// What is still unsaved, by title: asked about at quit.
    pub(crate) fn unsaved_titles(&self) -> Vec<String> {
        self.open.iter().filter(|s| s.unsaved()).map(|s| s.title()).collect()
    }

    /// Show `s` (or the already open surface for the same file).
    pub(crate) fn open(&mut self, s: Box<dyn Surface>) {
        let i = self.open.iter().position(|o| o.path() == s.path()).unwrap_or_else(|| {
            self.open.push(s);
            self.open.len() - 1
        });
        self.active = Some(i);
    }

    /// Tabs for open surfaces, drawn after the note tabs. A click makes a
    /// surface active; clicking a note tab makes notes active again.
    pub(crate) fn tab_strip(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
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
            self.open[i].close(ws);
            if self.open[i].unsaved() {
                // The write failed (the person was told why): the tab stays,
                // with a way out that says what is lost or kept.
                let path = self.open[i].path().clone();
                let copy = self.open[i].suggested_copy().to_string();
                self.stuck = Some(Stuck { path, copy, error: None, confirm: false });
            } else {
                self.open.remove(i);
                self.active = None;
            }
        }
        self.stuck_window(ui.ctx(), ws, pal);
    }

    /// The window for a tab whose changes can't be written.
    fn stuck_window(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) {
        let Some(st) = &mut self.stuck else { return };
        let Some(i) = self.open.iter().position(|o| *o.path() == st.path) else {
            self.stuck = None;
            return;
        };
        let title = self.open[i].title();
        let (mut keep, mut save_copy, mut discard) = (false, false, false);
        egui::Modal::new(egui::Id::unique("quire-surface-stuck")).show(ctx, |ui| {
            ui.set_width(440.0_f32.min(ctx.content_rect().width() - 48.0));
            ui.heading(format!("“{title}” can't be saved"));
            if st.confirm {
                ui.add(
                    egui::Label::new(
                        RichText::new("Close it and lose the changes that were never written? This can't be undone.").color(pal.muted),
                    )
                    .wrap(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Close without saving").color(pal.danger)).clicked() {
                        discard = true;
                    }
                    if ui.button("Back").clicked() {
                        st.confirm = false;
                    }
                });
                return;
            }
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Quire couldn't write its changes (the message shown earlier says why). Save them as a copy under a name you can write to, \
                         or close it and lose them.",
                    )
                    .color(pal.muted),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            ui.add(egui::TextEdit::singleline(&mut st.copy).desired_width(f32::INFINITY));
            if let Some(e) = &st.error {
                ui.label(RichText::new(e).color(pal.danger));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                save_copy = ui.button(RichText::new("Save a copy as…").strong()).clicked();
                if ui.button("Keep open").clicked() {
                    keep = true;
                }
                if ui.button(RichText::new("Close without saving").color(pal.danger)).clicked() {
                    st.confirm = true;
                }
            });
        });
        if keep {
            self.stuck = None;
        } else if save_copy {
            let to = NotePath::new(st.copy.trim());
            match self.open[i].save_copy_as(ws, &to) {
                Ok(()) => {
                    ws.notify(quire_app::workspace::NoticeLevel::Info, format!("Saved the changes of “{title}” as {}", to.file_name()));
                    discard = true;
                }
                Err(e) => st.error = Some(e),
            }
        }
        if discard {
            self.open[i].discard();
            self.open.remove(i);
            self.active = None;
            self.stuck = None;
        }
    }

    /// The app is quitting: every surface writes what isn't written yet.
    pub(crate) fn close_all(&mut self, ws: &mut Workspace) {
        for s in &mut self.open {
            s.close(ws);
        }
    }

    /// Once a frame: every surface not shown does its background work; one
    /// done goes away, and of two open on the same file (a drawing that
    /// followed a rename to a file open in another tab) the shown one stays.
    pub(crate) fn tick_hidden(&mut self, ws: &mut Workspace) {
        for (i, s) in self.open.iter_mut().enumerate() {
            if self.active != Some(i) {
                s.tick(ws);
            }
        }
        let mut i = 0;
        while i < self.open.len() {
            let dup =
                self.open.iter().enumerate().any(|(j, o)| {
                    j != i && o.path() == self.open[i].path() && (self.active == Some(j) || (j < i && self.active != Some(i)))
                });
            if (self.open[i].done() && self.active != Some(i)) || dup {
                self.open[i].close(ws);
                self.open.remove(i);
                self.active = match self.active {
                    Some(a) if a > i => Some(a - 1),
                    a => a,
                };
            } else {
                i += 1;
            }
        }
    }

    /// Draw the active surface; false when notes have the central panel.
    pub(crate) fn show_active(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> Option<PanelOut> {
        let i = self.active?;
        let s = self.open.get_mut(i)?;
        let out = s.show(ui, ws, pal);
        if s.done() {
            s.close(ws);
            self.open.remove(i);
            self.active = None;
        }
        Some(out)
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
                s.tab_strip(ui, &mut ws, &crate::theme::LIGHT);
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
