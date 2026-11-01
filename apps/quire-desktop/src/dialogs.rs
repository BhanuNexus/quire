// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings, version history and import dialogs.

use std::path::Path;

use crossbeam_channel::Receiver;
use egui::{RichText, Ui};
use quire_app::history_ops::{DiffKind, diff, diff_stats};
use quire_app::vault::Snapshot;
use quire_app::workspace::NoticeLevel;
use quire_app::{Theme, Workspace, ai};

use crate::theme::Palette;

fn modal_width(ctx: &egui::Context, want: f32) -> f32 {
    want.min(ctx.content_rect().width() - 48.0)
}

/// A modal on the panel colour, so text fields (drawn on the page colour) stand out.
fn modal(ctx: &egui::Context, name: &str, pal: &Palette) -> egui::Modal {
    egui::Modal::new(egui::Id::unique(name)).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel))
}

// ------------------------------------------------------------------ settings

#[derive(Default)]
pub(crate) struct SettingsState {
    key_input: String,
    test: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    tab: SettingsTab,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    #[default]
    Editor,
    Ai,
    About,
}

/// Returns true when the dialog should close.
pub(crate) fn settings(ctx: &egui::Context, st: &mut SettingsState, ws: &mut Workspace, pal: &Palette) -> bool {
    let mut close = false;
    if let Some(rx) = &st.test
        && let Ok(r) = rx.try_recv()
    {
        st.test_result = Some(r);
        st.test = None;
    }
    if st.test.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let before = ws.settings.clone();
    modal(ctx, "quire-settings", pal).show(ctx, |ui| {
        let w = modal_width(ctx, 560.0);
        ui.set_width(w);
        ui.set_max_width(w);
        ui.horizontal(|ui| {
            ui.heading("Settings");
            ui.add_space(12.0);
            ui.selectable_value(&mut st.tab, SettingsTab::Editor, "Editor");
            ui.selectable_value(&mut st.tab, SettingsTab::Ai, "AI & search");
            ui.selectable_value(&mut st.tab, SettingsTab::About, "About");
        });
        ui.separator();
        match st.tab {
            SettingsTab::Editor => editor_settings(ui, ws),
            SettingsTab::Ai => ai_settings(ui, st, ws, pal),
            SettingsTab::About => about(ui, pal),
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Done").clicked() {
                close = true;
            }
            ui.label(RichText::new("Changes apply immediately and are saved.").size(12.0).color(pal.muted));
        });
    });
    let s = &ws.settings;
    let ai_changed = (&s.ai_provider, &s.ai_base_url, &s.ai_model, &s.embeddings, &s.embed_model, &s.embed_base_url)
        != (&before.ai_provider, &before.ai_base_url, &before.ai_model, &before.embeddings, &before.embed_model, &before.embed_base_url);
    if ai_changed {
        ws.apply_ai_settings();
        st.test_result = None;
    }
    if ws.settings != before {
        let _ = ws.settings.save();
    }
    close
}

pub(crate) const SOURCE_URL: &str = "https://github.com/BhanuNexus/quire";

fn about(ui: &mut Ui, pal: &Palette) {
    ui.label(RichText::new(format!("Quire {}", env!("CARGO_PKG_VERSION"))).strong().size(16.0));
    ui.label(RichText::new("A fast, local-first Markdown notepad.").color(pal.muted));
    ui.add_space(8.0);
    ui.label(
        "Quire is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General \
         Public License, version 3 or (at your option) any later version. It comes with no warranty.",
    );
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.hyperlink_to("Source code", SOURCE_URL);
        ui.label("·");
        ui.hyperlink_to("Licence (AGPL-3.0)", format!("{SOURCE_URL}/blob/main/LICENSE"));
        ui.label("·");
        ui.hyperlink_to("Report a bug", format!("{SOURCE_URL}/issues/new/choose"));
        ui.label("·");
        ui.hyperlink_to("Discussions", format!("{SOURCE_URL}/discussions"));
    });
}

fn editor_settings(ui: &mut Ui, ws: &mut Workspace) {
    let s = &mut ws.settings;
    egui::Grid::new("settings-editor").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        ui.label("Theme");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.theme, Theme::System, "System");
            ui.selectable_value(&mut s.theme, Theme::Light, "Light");
            ui.selectable_value(&mut s.theme, Theme::Dark, "Dark");
        });
        ui.end_row();
        ui.label("Font size");
        ui.add(egui::Slider::new(&mut s.font_size, 11.0..=28.0).step_by(1.0));
        ui.end_row();
        ui.label("Line width");
        ui.add(egui::Slider::new(&mut s.line_width, 50..=160).suffix(" chars"));
        ui.end_row();
        ui.label("Editor");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.editor, "fast".to_string(), "Fast").on_hover_text("Virtualized; multi-cursor; handles huge files");
            ui.selectable_value(&mut s.editor, "classic".to_string(), "Classic");
        });
        ui.end_row();
        ui.label("Indent");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.indent_unit, "  ".to_string(), "2 spaces");
            ui.selectable_value(&mut s.indent_unit, "    ".to_string(), "4 spaces");
            ui.selectable_value(&mut s.indent_unit, "\t".to_string(), "Tab");
        });
        ui.end_row();
        ui.label("Autosave after");
        let mut secs = s.autosave_ms as f32 / 1000.0;
        if ui.add(egui::Slider::new(&mut secs, 0.3..=10.0).suffix(" s")).changed() {
            s.autosave_ms = (secs * 1000.0) as u64;
        }
        ui.end_row();
        ui.label("Show");
        ui.vertical(|ui| {
            ui.checkbox(&mut s.inline_images, "Images inline in the editor");
            ui.checkbox(&mut s.dates_as_links, "Insert dates as [[links]]");
            ui.checkbox(&mut s.inline_calculator, "Inline calculator (Ctrl+Enter)");
        });
        ui.end_row();
    });
}

fn ai_settings(ui: &mut Ui, st: &mut SettingsState, ws: &mut Workspace, pal: &Palette) {
    ui.label(
        RichText::new("Quire works offline. Nothing leaves this computer unless you pick a provider here.").size(12.5).color(pal.muted),
    );
    ui.add_space(6.0);
    let provider = ws.settings.ai_provider.clone();
    egui::Grid::new("settings-ai").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        ui.label("Assistant");
        ui.vertical(|ui| {
            let s = &mut ws.settings;
            ui.radio_value(&mut s.ai_provider, "local".to_string(), "Offline (built in)");
            ui.radio_value(&mut s.ai_provider, "openai".to_string(), "OpenAI-compatible (Ollama, LM Studio, OpenAI…)");
            ui.radio_value(&mut s.ai_provider, "anthropic".to_string(), "Anthropic (Claude)");
        });
        ui.end_row();
        if provider != "local" {
            let s = &mut ws.settings;
            ui.label("Server");
            ui.add(egui::TextEdit::singleline(&mut s.ai_base_url).hint_text(ai::default_base_url(&provider)).desired_width(320.0));
            ui.end_row();
            ui.label("Model");
            ui.add(egui::TextEdit::singleline(&mut s.ai_model).hint_text(ai::default_model(&provider)).desired_width(320.0));
            ui.end_row();
            ui.label("API key");
            ui.vertical(|ui| {
                let has = ai::has_api_key(&provider);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut st.key_input)
                            .password(true)
                            .hint_text(if has { "•••••• saved" } else { "paste key" })
                            .desired_width(220.0),
                    );
                    if ui.add_enabled(!st.key_input.trim().is_empty(), egui::Button::new("Save key")).clicked() {
                        match ws.save_api_key(&provider, &st.key_input) {
                            Ok(()) => ws.notify(NoticeLevel::Info, "Key saved"),
                            Err(e) => ws.notify(NoticeLevel::Error, e),
                        }
                        st.key_input.clear();
                    }
                    if has && ui.button("Remove").clicked() {
                        let _ = ws.save_api_key(&provider, "");
                    }
                });
                let note = if provider == "openai" { " · not needed for local servers" } else { "" };
                ui.label(RichText::new(format!("Stored in {}{note}", ai::secret_store_kind())).size(11.5).color(pal.muted));
            });
            ui.end_row();
        }
        ui.label("");
        ui.horizontal(|ui| {
            if ui.add_enabled(st.test.is_none(), egui::Button::new("Test connection")).clicked() {
                st.test = Some(ws.test_ai_connection());
                st.test_result = None;
            }
            if st.test.is_some() {
                ui.add(egui::Spinner::new());
            }
            match &st.test_result {
                Some(Ok(m)) => {
                    ui.label(RichText::new(format!("✓ {m}")).color(pal.accent));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(pal.danger));
                }
                None => {}
            }
        });
        ui.end_row();
        ui.label("Meaning search");
        ui.vertical(|ui| {
            let s = &mut ws.settings;
            ui.radio_value(&mut s.embeddings, "lsa".to_string(), "Built in (offline, instant)");
            ui.radio_value(&mut s.embeddings, "endpoint".to_string(), "Neural embeddings from an endpoint");
            if s.embeddings == "endpoint" {
                ui.horizontal(|ui| {
                    ui.label("Model");
                    ui.add(egui::TextEdit::singleline(&mut s.embed_model).desired_width(160.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Server");
                    ui.add(
                        egui::TextEdit::singleline(&mut s.embed_base_url)
                            .hint_text("same as assistant, or http://localhost:11434")
                            .desired_width(240.0),
                    );
                });
            }
        });
        ui.end_row();
    });
    if let Some(e) = ws.index().semantic_error() {
        ui.label(RichText::new(format!("Embeddings unavailable ({e}); using the built-in index.")).size(12.0).color(pal.warn));
    }
}

// ------------------------------------------------------------------ history

pub(crate) struct HistoryState {
    snapshots: Vec<Snapshot>,
    selected: usize,
    /// (index, text) of the loaded snapshot.
    loaded: Option<(usize, String)>,
}

impl HistoryState {
    pub(crate) fn new(ws: &Workspace) -> Self {
        let snapshots = ws.note_history();
        // The newest snapshot is usually the text you're looking at; start
        // on the one before it so the dialog opens on an actual change.
        let current = ws.active_doc().map(|d| d.text().to_string());
        let same = snapshots.first().and_then(|s| ws.snapshot_text(s)) == current;
        let selected = usize::from(same && snapshots.len() > 1);
        Self { snapshots, selected, loaded: None }
    }
}

pub(crate) fn history(ctx: &egui::Context, st: &mut HistoryState, ws: &mut Workspace, pal: &Palette) -> bool {
    let mut close = false;
    let mut restore = None;
    let current = ws.active_doc().map(|d| d.text().to_string()).unwrap_or_default();
    let title = ws.active_doc().map(quire_app::Document::title).unwrap_or_default();
    if st.loaded.as_ref().is_none_or(|(i, _)| *i != st.selected)
        && let Some(s) = st.snapshots.get(st.selected)
    {
        st.loaded = ws.snapshot_text(s).map(|t| (st.selected, t));
    }
    modal(ctx, "quire-history", pal).show(ctx, |ui| {
        let w = modal_width(ctx, 820.0);
        ui.set_width(w);
        ui.set_max_width(w);
        ui.heading(format!("Version history · {title}"));
        ui.label(RichText::new("Saved automatically in the vault's .quire folder. Restoring is undoable.").size(12.0).color(pal.muted));
        ui.separator();
        if st.snapshots.is_empty() {
            ui.label(RichText::new("No earlier versions yet. Versions are kept as you edit and save.").color(pal.muted));
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                egui::ScrollArea::vertical().id_salt("hist-list").max_height(420.0).show(ui, |ui| {
                    for (i, s) in st.snapshots.iter().enumerate() {
                        let label = chrono::DateTime::from_timestamp_millis(s.at_ms)
                            .map(|d| d.with_timezone(&chrono::Local).format("%b %-d · %H:%M:%S").to_string())
                            .unwrap_or_default();
                        if ui.selectable_label(i == st.selected, label).clicked() {
                            st.selected = i;
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                if let Some((_, old)) = &st.loaded {
                    let (added, removed) = diff_stats(old, &current);
                    ui.label(RichText::new(format!("Changes since this version: +{added} −{removed} lines")).size(12.0).color(pal.muted));
                    egui::ScrollArea::vertical().id_salt("hist-diff").max_height(390.0).auto_shrink([false, true]).show(ui, |ui| {
                        let lines = diff(old, &current, 3);
                        if lines.iter().all(|l| l.kind == DiffKind::Same) {
                            ui.label(RichText::new("Identical to the current text.").color(pal.muted));
                        }
                        for l in lines.iter().take(2000) {
                            let (prefix, color, bg) = match l.kind {
                                DiffKind::Same => (" ", pal.text, egui::Color32::TRANSPARENT),
                                DiffKind::Added => ("+", pal.text, pal.accent.gamma_multiply(0.18)),
                                DiffKind::Removed => ("−", pal.text, pal.danger.gamma_multiply(0.18)),
                                DiffKind::Gap => ("", pal.faint, egui::Color32::TRANSPARENT),
                            };
                            let text =
                                RichText::new(format!("{prefix} {}", l.text)).monospace().size(12.5).color(color).background_color(bg);
                            ui.add(egui::Label::new(text).truncate());
                        }
                    });
                    ui.label(RichText::new("+ lines are in your current text, − lines only in this version.").size(11.0).color(pal.faint));
                }
            });
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.add_enabled(st.loaded.is_some(), egui::Button::new("Restore this version")).clicked() {
                restore = st.snapshots.get(st.selected).cloned();
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
    });
    if let Some(s) = restore
        && ws.restore_snapshot(&s)
    {
        close = true;
    }
    close
}

// ------------------------------------------------------------------ import

#[derive(Default)]
pub(crate) struct ImportState {
    source: String,
    folder: String,
    plan: Option<Result<quire_import::ImportPlan, String>>,
    planned_for: String,
}

pub(crate) fn import(ctx: &egui::Context, st: &mut ImportState, ws: &mut Workspace, pal: &Palette) -> bool {
    let mut close = false;
    let mut apply = false;
    if st.folder.is_empty() && st.plan.is_none() {
        st.folder = "Imported".into();
    }
    modal(ctx, "quire-import", pal).show(ctx, |ui| {
        let w = modal_width(ctx, 600.0);
        ui.set_width(w);
        ui.set_max_width(w);
        ui.heading("Import notes");
        ui.label(
            RichText::new(
                "Logseq graph · Notion export (.zip or folder) · Bear (.bear2bk, TextBundles) · Evernote (.enex) · any Markdown folder",
            )
            .size(12.0)
            .color(pal.muted),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.source).hint_text("folder or file to import").desired_width(300.0));
            if ui.button("Folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                st.source = p.display().to_string();
            }
            if ui.button("File…").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("Exports", &["zip", "enex", "bear2bk", "md"]).pick_file()
            {
                st.source = p.display().to_string();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Into folder");
            ui.add(egui::TextEdit::singleline(&mut st.folder).hint_text("vault root").desired_width(180.0));
        });
        if !st.source.trim().is_empty() && st.planned_for != st.source {
            st.planned_for.clone_from(&st.source);
            st.plan = Some(quire_import::plan(Path::new(st.source.trim())));
        }
        ui.separator();
        match &st.plan {
            None => {
                ui.label(
                    RichText::new("Pick an export to see what will be imported. Nothing is written until you confirm.").color(pal.muted),
                );
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(pal.danger));
            }
            Some(Ok(plan)) => {
                ui.label(RichText::new(plan.summary()).strong());
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for n in plan.notes.iter().take(300) {
                        ui.add(egui::Label::new(RichText::new(n.path.as_str()).size(12.5)).truncate());
                    }
                    if plan.notes.len() > 300 {
                        ui.label(RichText::new(format!("… and {} more", plan.notes.len() - 300)).color(pal.muted));
                    }
                    for wmsg in &plan.warnings {
                        ui.label(RichText::new(format!("⚠ {wmsg}")).size(12.0).color(pal.warn));
                    }
                });
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            let ready = matches!(&st.plan, Some(Ok(p)) if !p.notes.is_empty());
            if ui.add_enabled(ready, egui::Button::new("Import")).clicked() {
                apply = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
            ui.label(RichText::new("Existing notes are never overwritten.").size(12.0).color(pal.muted));
        });
    });
    if apply
        && let Some(Ok(plan)) = &st.plan
        && let Some(vault) = ws.vault().cloned()
    {
        match quire_import::apply(plan, &vault, st.folder.trim()) {
            Ok(r) => {
                let renamed = if r.renamed.is_empty() { String::new() } else { format!(" ({} renamed to avoid clashes)", r.renamed.len()) };
                ws.notify(NoticeLevel::Info, format!("Imported {} notes and {} files{renamed}", r.notes, r.files));
                ws.reload_index();
                close = true;
            }
            Err(e) => ws.notify(NoticeLevel::Error, format!("Import failed: {e}")),
        }
    }
    close
}

// ------------------------------------------------------------------ projects

#[derive(Default)]
pub(crate) struct ProjectState {
    p: quire_app::projects::NewProject,
    focused: bool,
    error: Option<String>,
}

/// Returns (close, opened a note).
pub(crate) fn new_project(ctx: &egui::Context, st: &mut ProjectState, ws: &mut Workspace, pal: &Palette) -> (bool, bool) {
    let (mut close, mut create) = (false, false);
    if st.p.status.is_empty() {
        st.p.status = "planned".into();
    }
    modal(ctx, "quire-new-project", pal).show(ctx, |ui| {
        ui.set_width(modal_width(ctx, 440.0));
        ui.heading("New project");
        ui.label(
            RichText::new(
                "A note in Projects/ with status, due date and owner. It appears on the project board, which is created if needed.",
            )
            .size(12.0)
            .color(pal.muted),
        );
        ui.add_space(6.0);
        egui::Grid::new("np-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Name");
            let r = ui.add(egui::TextEdit::singleline(&mut st.p.name).hint_text("Website relaunch").desired_width(260.0));
            if !st.focused {
                r.request_focus();
                st.focused = true;
            }
            create |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
            ui.label("Status");
            ui.horizontal(|ui| {
                for s in quire_app::projects::STATUSES {
                    ui.selectable_value(&mut st.p.status, s.to_string(), s);
                }
            });
            ui.end_row();
            ui.label("Due");
            ui.add(egui::TextEdit::singleline(&mut st.p.due).hint_text("YYYY-MM-DD (optional)").desired_width(160.0));
            ui.end_row();
            ui.label("Owner");
            ui.add(egui::TextEdit::singleline(&mut st.p.owner).hint_text("optional").desired_width(160.0));
            ui.end_row();
        });
        if let Some(e) = &st.error {
            ui.label(RichText::new(e).color(pal.danger).size(12.5));
        }
        ui.label(RichText::new("Customise new projects by adding Templates/Project.md.").size(11.5).color(pal.faint));
        ui.separator();
        ui.horizontal(|ui| {
            create |= ui.add_enabled(!st.p.name.trim().is_empty(), egui::Button::new("Create project")).clicked();
            close |= ui.button("Cancel").clicked();
        });
    });
    if create && !st.p.name.trim().is_empty() {
        match ws.new_project(&st.p) {
            Ok(_) => return (true, true),
            Err(e) => st.error = Some(e),
        }
    }
    (close, false)
}

// ------------------------------------------------------------------ folders

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderMode {
    NewNote,
    Move,
}

pub(crate) struct FolderState {
    mode: FolderMode,
    input: String,
    folders: Vec<String>,
    focused: bool,
    error: Option<String>,
}

impl FolderState {
    pub(crate) fn new(ws: &Workspace, mode: FolderMode) -> Self {
        // New notes default to the current note's folder; moves start empty.
        let current = ws.active_doc().and_then(|d| d.path.as_ref()).map(|p| p.folder().to_string()).unwrap_or_default();
        let input = if mode == FolderMode::NewNote { current } else { String::new() };
        Self { mode, input, folders: ws.folders(), focused: false, error: None }
    }
}

/// Returns (close, opened/moved a note).
pub(crate) fn folder_picker(ctx: &egui::Context, st: &mut FolderState, ws: &mut Workspace, pal: &Palette) -> (bool, bool) {
    let (mut close, mut go) = (false, false);
    let title = ws.active_doc().map(quire_app::Document::title).unwrap_or_default();
    modal(ctx, "quire-folder-picker", pal).show(ctx, |ui| {
        ui.set_width(modal_width(ctx, 440.0));
        ui.heading(match st.mode {
            FolderMode::NewNote => "New note in folder".to_string(),
            FolderMode::Move => format!("Move “{title}”"),
        });
        ui.label(
            RichText::new("Pick a folder or type a new one, e.g. Work/Clients. Leave empty for the vault root.")
                .size(12.0)
                .color(pal.muted),
        );
        ui.add_space(6.0);
        let r = ui.add(egui::TextEdit::singleline(&mut st.input).hint_text("Folder").desired_width(f32::INFINITY));
        if !st.focused {
            r.request_focus();
            st.focused = true;
        }
        go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let q = st.input.trim().to_lowercase();
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for f in st.folders.iter().filter(|f| q.is_empty() || f.to_lowercase().contains(&q)) {
                if ui.selectable_label(st.input.trim() == f, f).clicked() {
                    st.input.clone_from(f);
                }
            }
        });
        if let Some(e) = &st.error {
            ui.label(RichText::new(e).color(pal.danger).size(12.5));
        }
        ui.separator();
        ui.horizontal(|ui| {
            let label = match st.mode {
                FolderMode::NewNote => "Create note",
                FolderMode::Move => "Move",
            };
            go |= ui.button(label).clicked();
            close |= ui.button("Cancel").clicked();
            if st.mode == FolderMode::Move {
                ui.label(RichText::new("Links to the note keep working.").size(12.0).color(pal.muted));
            }
        });
    });
    if go {
        let r = match st.mode {
            FolderMode::NewNote => ws.new_note_in(&st.input).map(|_| ()),
            FolderMode::Move => ws.move_active(&st.input),
        };
        match r {
            Ok(()) => return (true, true),
            Err(e) => st.error = Some(e),
        }
    }
    (close, false)
}

// ------------------------------------------------------------------ tour

/// (title, body, keys to try). Short on purpose: one idea per card.
const TOUR: [(&str, &str, &str); 5] = [
    (
        "Welcome to Quire",
        "Your notes are plain Markdown files in a folder you choose. Quire styles them as you type and saves automatically. Nothing leaves your computer unless you turn on an AI provider.",
        "Just start typing: the first line names the note",
    ),
    (
        "Find anything",
        "Jump to any note by name, run any command, or search every note by words and by meaning.",
        "Ctrl+P quick open  ·  Ctrl+K commands  ·  Ctrl+Shift+F search",
    ),
    (
        "Link your thinking",
        "Type [[ to link notes. The Insights panel shows backlinks, related notes and mentions you haven't linked yet. Renaming a note updates every link to it.",
        "[[  link  ·  Alt+Enter follow  ·  F2 or double-click a tab to rename",
    ),
    (
        "Organise without a database",
        "+ New → Project makes a project note with status and due date, and a board to drag projects across. Right-click any note or folder for more. Properties and views are still plain text.",
        "Ctrl+Alt+N new project  ·  right-click a note  ·  Ctrl+Alt+H version history",
    ),
    (
        "Your notes, your AI",
        "Ask questions of your notes with cited answers, offline. Connect Ollama or Claude in Settings if you want a model to write the answer. The shortcut sheet lists everything else.",
        "Ctrl+Shift+A ask  ·  Ctrl+, settings  ·  Ctrl+/ all shortcuts",
    ),
];

/// Returns true when the tour should close (finished or skipped).
pub(crate) fn tour(ctx: &egui::Context, step: &mut usize, pal: &Palette) -> bool {
    let mut close = false;
    let last = TOUR.len() - 1;
    *step = (*step).min(last);
    let (title, body, keys) = TOUR[*step];
    let (left, right) =
        ctx.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Enter)));
    modal(ctx, "quire-tour", pal).show(ctx, |ui| {
        ui.set_width(modal_width(ctx, 480.0));
        ui.add_space(4.0);
        ui.label(RichText::new(format!("{} of {}", *step + 1, TOUR.len())).size(11.0).color(pal.muted));
        ui.label(RichText::new(title).size(22.0).strong().color(pal.strong));
        ui.add_space(6.0);
        ui.label(RichText::new(body).size(15.0).color(pal.text));
        ui.add_space(10.0);
        egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(keys).monospace().size(12.5).color(pal.accent));
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            // Progress dots.
            for i in 0..TOUR.len() {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                let c = if i == *step { pal.accent } else { pal.border };
                ui.painter().circle_filled(r.center(), 3.5, c);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let next = if *step == last { "Start writing" } else { "Next" };
                if ui.button(RichText::new(next).strong()).clicked() || right {
                    if *step == last {
                        close = true;
                    } else {
                        *step += 1;
                    }
                }
                if *step > 0 && (ui.button("Back").clicked() || left) {
                    *step -= 1;
                }
                if *step < last && ui.button(RichText::new("Skip tour").color(pal.muted)).clicked() {
                    close = true;
                }
            });
        });
    });
    close
}

// ------------------------------------------------------------------ sync conflicts

/// Conflict copies left by sync tools, with a merge preview for each.
/// Returns (close, opened a note).
pub(crate) fn sync_conflicts(ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> (bool, bool) {
    let (mut close, mut opened) = (false, false);
    let copies = ws.conflict_copies();
    let mut action: Option<(usize, u8)> = None;
    modal(ctx, "quire-sync-conflicts", pal).show(ctx, |ui| {
        ui.set_width(modal_width(ctx, 620.0));
        ui.heading("Sync conflicts");
        ui.label(
            RichText::new(
                "Your sync tool saved these as separate copies because two devices changed the same note. \
                 Merging combines both, using version history to find what changed on each side.",
            )
            .size(12.5)
            .color(pal.muted),
        );
        ui.separator();
        if copies.is_empty() {
            ui.label(RichText::new("No conflicts left.").color(pal.muted));
        }
        egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
            for (i, c) in copies.iter().enumerate() {
                let preview = ws.preview_copy_merge(c);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(c.original.stem()).strong());
                    ui.label(RichText::new(format!("· {} copy · {}", c.tool, c.copy.file_name())).size(12.0).color(pal.muted));
                });
                let summary = match &preview {
                    Some(r) if r.is_clean() => ("Merges cleanly".to_string(), pal.accent),
                    Some(r) => (
                        format!(
                            "{} conflicting change{} will be marked for you to edit",
                            r.conflicts,
                            if r.conflicts == 1 { "" } else { "s" }
                        ),
                        pal.warn,
                    ),
                    None => ("Couldn't read one of the files".to_string(), pal.danger),
                };
                ui.label(RichText::new(summary.0).size(12.5).color(summary.1));
                ui.horizontal(|ui| {
                    if ui.add_enabled(preview.is_some(), egui::Button::new("Merge into original")).clicked() {
                        action = Some((i, 0));
                    }
                    if ui.button("Keep original").on_hover_text("Move the copy to the vault's trash").clicked() {
                        action = Some((i, 1));
                    }
                    if ui.button("Open both").clicked() {
                        action = Some((i, 2));
                    }
                });
                ui.separator();
            }
        });
        if ui.button("Close").clicked() {
            close = true;
        }
    });
    if let Some((i, what)) = action
        && let Some(c) = copies.get(i)
    {
        match what {
            0 => match ws.merge_conflict_copy(c) {
                Ok(0) => ws.notify(NoticeLevel::Info, format!("Merged the copy into “{}”", c.original.stem())),
                Ok(n) => ws.notify(
                    NoticeLevel::Warn,
                    format!("Merged into “{}” with {n} marked conflict{}", c.original.stem(), if n == 1 { "" } else { "s" }),
                ),
                Err(e) => ws.notify(NoticeLevel::Error, e),
            },
            1 => {
                if let Err(e) = ws.discard_conflict_copy(c) {
                    ws.notify(NoticeLevel::Error, e);
                }
            }
            _ => {
                ws.open_note(&c.copy);
                ws.open_note(&c.original);
                close = true;
                opened = true;
            }
        }
    }
    (close, opened)
}
