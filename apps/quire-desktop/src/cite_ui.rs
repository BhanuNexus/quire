// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Citations in the desktop (design §G.3): the "Insert citation" picker,
//! the "Import bibliography" dialog with its dry run, and Settings →
//! Citations (library files, the PDF export's style, a watched file).
//!
//! Owned by WS-G. Registered in `registry.rs` (`ui_target`, `SETTINGS_TABS`,
//! `layout_samples`).

use std::sync::Arc;

use egui::{Key, RichText, TextEdit};
use quire_app::Workspace;
use quire_app::citations::{self, Library, Reference};
use quire_app::markdown::cite::Formatter as _;
use quire_app::workspace::NoticeLevel;

use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

fn year(r: &Reference) -> &str {
    if r.year.is_empty() { "n.d." } else { &r.year }
}

/// "Insert citation…": find a work and put `[@key]` at the caret.
pub(crate) struct CitePicker {
    query: String,
    locator: String,
    pick: usize,
    focused: bool,
    /// For layout samples: a library of its own.
    library: Option<Arc<Library>>,
}

impl CitePicker {
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        // On a citation: start from its key.
        let query = citations::at_caret(ws).and_then(|(c, _)| c.items.first().map(|i| i.key.clone())).unwrap_or_default();
        Box::new(Self { query, locator: String::new(), pick: 0, focused: false, library: None })
    }

    #[cfg(test)]
    pub(crate) fn sample(with_library: bool) -> Self {
        let library = with_library.then(|| {
            let refs = citations::parse_bibtex(
                "@article{lovelace1843, author = {Lovelace, Ada}, title = {Notes on the Analytical Engine}, journal = {Scientific Memoirs}, year = 1843}\n\
                 @book{knuth1984, author = {Knuth, Donald E.}, title = {Literate Programming}, publisher = {CSLI}, year = 1984}",
            )
            .unwrap_or_default();
            Arc::new(Library::new(refs, Vec::new(), Vec::new()))
        });
        Self { query: String::new(), locator: "p. 4".into(), pick: 0, focused: true, library }
    }

    /// Insert at the caret: inside an open `[@…]` add `; @key`, else `[@key]`.
    fn insert(ws: &mut Workspace, key: &str, locator: &str) {
        let loc = if locator.trim().is_empty() { String::new() } else { format!(", {}", locator.trim()) };
        let inside = citations::at_caret(ws).is_some_and(|(c, _)| !c.in_text)
            && ws.active_doc().is_some_and(|d| {
                let b = d.buffer().char_to_byte(d.selection().head);
                d.text()[b..].starts_with(']')
            });
        if inside {
            ws.insert_text(&format!("; @{key}{loc}"));
        } else {
            ws.insert_text(&format!("[@{key}{loc}]"));
        }
    }
}

impl OverlayView for CitePicker {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let lib = self.library.clone().unwrap_or_else(|| Arc::clone(&ws.features.citations.library));
        egui::Modal::new(egui::Id::unique("quire-cite-picker")).show(ctx, |ui| {
            let w = 540.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("Insert citation");
            if lib.is_empty() {
                ui.label(RichText::new("There's no bibliography in this vault yet. Import a BibTeX file or Zotero's CSL-JSON export, and its works can be cited as [@key].").color(pal.muted));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Import a bibliography…").clicked() {
                        res.then = Some(crate::overlays::Overlay::Feature(ImportDialog::make(ws)));
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                });
                return;
            }
            let r = ui.add(TextEdit::singleline(&mut self.query).hint_text("Author, title, year or key").desired_width(w));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            let hits = lib.search(&self.query, 12);
            if r.changed() {
                self.pick = 0;
            }
            let (down, up, enter) = ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
            if down {
                self.pick = (self.pick + 1).min(hits.len().saturating_sub(1));
            }
            if up {
                self.pick = self.pick.saturating_sub(1);
            }
            ui.add_space(4.0);
            let mut chosen = None;
            egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                if hits.is_empty() {
                    ui.label(RichText::new("No work matches.").color(pal.muted));
                }
                for (i, h) in hits.iter().enumerate() {
                    let text = RichText::new(format!("{} {} — {}", h.short_authors(), year(h), h.title));
                    let resp = ui.selectable_label(i == self.pick, text).on_hover_text(format!("@{}", h.key));
                    if resp.clicked() {
                        self.pick = i;
                    }
                    if resp.double_clicked() {
                        chosen = Some(h.key.clone());
                    }
                    if i == self.pick && (down || up) {
                        resp.scroll_to_me(None);
                    }
                }
            });
            if let Some(h) = hits.get(self.pick) {
                ui.add_space(4.0);
                egui::Frame::NONE.fill(pal.code_bg).corner_radius(6.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                    ui.set_width(w - 16.0);
                    ui.label(RichText::new(lib.reference(&h.key).replace(['*', '\\'], "")).size(12.5).color(pal.text));
                    ui.label(RichText::new(format!("[@{}]", h.key)).monospace().size(11.5).color(pal.muted));
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Page or place:");
                ui.add(TextEdit::singleline(&mut self.locator).hint_text("p. 4").desired_width(90.0));
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can = hits.get(self.pick).is_some() && ws.active_doc().is_some();
                if ui.add_enabled(can, egui::Button::new("Insert")).clicked() || (enter && can) {
                    chosen = hits.get(self.pick).map(|h| h.key.clone());
                }
                if let Some(h) = hits.get(self.pick)
                    && ui.button(if h.pdf_name().is_some() { "Open PDF" } else { "Show entry" }).clicked()
                {
                    let r = (*h).clone();
                    citations::open_reference(ws, &r);
                    res.close = true;
                    res.jumped = h.pdf_name().is_none();
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
            if let Some(key) = chosen {
                Self::insert(ws, &key, &self.locator);
                res.close = true;
                res.jumped = true;
            }
        });
        res
    }
}

/// "Import bibliography…": choose a file, see what would change, import.
#[derive(Default)]
pub(crate) struct ImportDialog {
    error: Option<String>,
    watch: bool,
    chosen: Option<std::path::PathBuf>,
    /// For layout samples.
    sample_plan: Option<citations::PendingImport>,
}

impl ImportDialog {
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        ws.features.citations.import = None;
        Box::<Self>::default()
    }

    #[cfg(test)]
    pub(crate) fn sample(with_plan: bool) -> Self {
        let sample_plan = with_plan.then(|| {
            let added = citations::parse_any(
                r#"[{"id":"turing1936","type":"article-journal","title":"On Computable Numbers","author":[{"family":"Turing","given":"Alan"}],"issued":{"date-parts":[[1936]]}},
                    {"id":"hopper1952","type":"paper-conference","title":"The Education of a Computer","author":[{"family":"Hopper","given":"Grace"}],"issued":{"date-parts":[[1952]]}}]"#,
            )
            .unwrap_or_default();
            citations::PendingImport {
                from: "My Library.json".into(),
                target: "references.bib".into(),
                plan: citations::ImportPlan { added, updated: Vec::new(), unchanged: 41 },
            }
        });
        Self { error: None, watch: true, chosen: None, sample_plan }
    }
}

impl OverlayView for ImportDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let pending = self.sample_plan.clone().or_else(|| ws.features.citations.import.clone());
        let target = ws.features.citations.prefs.import_target();
        egui::Modal::new(egui::Id::unique("quire-import-bibliography")).show(ctx, |ui| {
            let w = 520.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("Import bibliography");
            ui.label(
                RichText::new(format!(
                    "A BibTeX file (.bib) or CSL-JSON (Zotero: Export → CSL JSON). Works are added to {target} by their key; a key that's already there is updated. You see what changes first."
                ))
                .color(pal.muted),
            );
            ui.add_space(8.0);
            if ui.button("Choose a file…").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("Bibliography", &["bib", "json"]).pick_file()
            {
                match citations::prepare_import(ws, &p) {
                    Ok(()) => {
                        self.error = None;
                        self.chosen = Some(p);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(pal.danger));
            }
            if let Some(p) = &pending {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{}: {}", p.from, p.plan.summary())).color(pal.strong));
                egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                    for r in p.plan.added.iter().take(50) {
                        ui.label(RichText::new(format!("+ {} — {} {}", r.key, r.short_authors(), year(r))).size(12.5).color(pal.added));
                    }
                    for r in p.plan.updated.iter().take(50) {
                        ui.label(RichText::new(format!("~ {} — {} {}", r.key, r.short_authors(), year(r))).size(12.5).color(pal.warn));
                    }
                    let more = p.plan.added.len().saturating_sub(50) + p.plan.updated.len().saturating_sub(50);
                    if more > 0 {
                        ui.label(RichText::new(format!("and {more} more")).size(12.0).color(pal.muted));
                    }
                });
                if self.chosen.is_some() || self.sample_plan.is_some() {
                    ui.checkbox(&mut self.watch, "Keep watching this file and merge its changes (for a Better BibTeX auto-export)");
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can = pending.as_ref().is_some_and(|p| !p.plan.is_empty());
                if ui.add_enabled(can, egui::Button::new("Import")).clicked() {
                    match citations::confirm_import(ws) {
                        Ok(msg) => {
                            if self.watch
                                && let Some(path) = self.chosen.clone()
                            {
                                let w = path.display().to_string();
                                if let Err(e) = citations::save_prefs(ws, |c| c.watch = w) {
                                    ws.notify(NoticeLevel::Warn, e);
                                }
                            }
                            ws.notify(NoticeLevel::Info, msg);
                            res.close = true;
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                if pending.as_ref().is_some_and(|p| p.plan.is_empty()) {
                    ui.label(RichText::new("Nothing new to import.").color(pal.muted));
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
        });
        res
    }

    fn closed(&mut self, ws: &mut Workspace) {
        ws.features.citations.import = None;
    }
}

/// Settings → Citations is offered once a vault is open.
pub(crate) fn settings_available(ws: &Workspace) -> bool {
    ws.vault().is_some()
}

/// Settings → Citations.
pub(crate) fn settings_tab(ui: &mut egui::Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let prefs = ws.features.citations.prefs.clone();
    let lib = Arc::clone(&ws.features.citations.library);
    ui.add(egui::Label::new(RichText::new("Cite works as [@key] in notes. Preview, HTML export and publishing show them as (Author Year) with a list of references; the PDF export uses the style below.").color(pal.muted)).wrap());
    ui.add_space(8.0);
    ui.label(RichText::new("LIBRARY").size(11.0).color(pal.muted).strong());
    let files = prefs.library.list();
    if files.is_empty() {
        ui.label(format!("{} (the default)", citations::DEFAULT_LIBRARY));
    }
    let mut remove = None;
    for (i, f) in files.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(f);
            if ui.small_button("Remove").on_hover_text("Stop using this file (it isn't deleted)").clicked() {
                remove = Some(i);
            }
        });
    }
    let id = egui::Id::unique("quire-citations-add-file");
    let mut add = ui.data_mut(|d| d.get_temp::<String>(id).unwrap_or_default());
    ui.horizontal_wrapped(|ui| {
        let w = (ui.available_width() - 60.0).clamp(80.0, 300.0);
        ui.add(TextEdit::singleline(&mut add).hint_text("Another .bib, e.g. refs/thesis.bib").desired_width(w));
        if ui
            .add_enabled(
                std::path::Path::new(add.trim()).extension().is_some_and(|e| e.eq_ignore_ascii_case("bib")),
                egui::Button::new("Add"),
            )
            .clicked()
        {
            let mut v = files.clone();
            if v.is_empty() {
                v.push(citations::DEFAULT_LIBRARY.into());
            }
            v.push(add.trim().to_string());
            if let Err(e) = citations::save_prefs(ws, |c| c.library = citations::Files::Many(v)) {
                ws.notify(NoticeLevel::Error, e);
            }
            add.clear();
        }
    });
    ui.data_mut(|d| d.insert_temp(id, add));
    if let Some(i) = remove {
        let mut v = files.clone();
        v.remove(i);
        let new = if v.is_empty() { citations::Files::None } else { citations::Files::Many(v) };
        if let Err(e) = citations::save_prefs(ws, |c| c.library = new) {
            ws.notify(NoticeLevel::Error, e);
        }
    }
    ui.label(RichText::new(format!("{} works", lib.refs.len())).color(pal.muted));
    for p in &lib.problems {
        ui.label(RichText::new(p).color(pal.warn));
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Import…").clicked() {
            out.run = Some(quire_app::Command::ImportBibliography);
        }
        if ui.button("Read again").clicked() {
            citations::reload(ws);
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new("STYLE FOR PDF EXPORT").size(11.0).color(pal.muted).strong());
    if !crate::typeset_ui::AVAILABLE {
        ui.add(egui::Label::new(RichText::new("PDF export comes with the full edition of Quire.").color(pal.muted)).wrap());
    }
    ui.horizontal_wrapped(|ui| {
        for (sid, label) in citations::STYLES {
            if ui.radio(prefs.style() == sid, label).clicked() && prefs.style() != sid {
                let s = sid.to_string();
                if let Err(e) = citations::save_prefs(ws, |c| c.style = s) {
                    ws.notify(NoticeLevel::Error, e);
                }
            }
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new("WATCHED FILE").size(11.0).color(pal.muted).strong());
    if prefs.watch.trim().is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new("None. Import a file and tick “Keep watching” to follow a Zotero (Better BibTeX) auto-export.")
                    .color(pal.muted),
            )
            .wrap(),
        );
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(&prefs.watch).wrap());
            if ui.small_button("Stop watching").clicked()
                && let Err(e) = citations::save_prefs(ws, |c| c.watch = String::new())
            {
                ws.notify(NoticeLevel::Error, e);
            }
        });
        ui.add(
            egui::Label::new(
                RichText::new("Checked every few seconds while Quire runs; new and changed works are merged into the library.")
                    .size(12.0)
                    .color(pal.muted),
            )
            .wrap(),
        );
    }
    ui.add(
        egui::Label::new(
            RichText::new("These settings are kept in the vault (.quire/shared/vault.toml), so they travel with it.")
                .size(12.0)
                .color(pal.faint),
        )
        .wrap(),
    );
    out
}
