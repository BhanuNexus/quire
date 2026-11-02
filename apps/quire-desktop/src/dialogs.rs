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

/// Height for a modal's scrolling body: `want`, or less on a short window so
/// the `chrome` around it (title, notes, buttons) still fits on screen.
fn modal_body_height(ctx: &egui::Context, want: f32, chrome: f32) -> f32 {
    want.min(ctx.content_rect().height() - chrome).max(120.0)
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
    sync: SyncForm,
}

impl SettingsState {
    pub(crate) fn sync_tab(_ws: &Workspace) -> Self {
        Self { tab: SettingsTab::Sync, ..Default::default() }
    }

    #[cfg(test)]
    pub(crate) fn every_tab() -> Vec<Self> {
        [SettingsTab::Editor, SettingsTab::Ai, SettingsTab::Sync, SettingsTab::About]
            .into_iter()
            .map(|tab| Self { tab, ..Default::default() })
            .collect()
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    #[default]
    Editor,
    Ai,
    Sync,
    About,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    #[default]
    Create,
    Join,
}

/// How a new sync vault reaches the other devices.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SyncVia {
    /// A folder Dropbox, iCloud Drive and the like already sync. Free.
    #[default]
    Folder,
    Server,
}

#[derive(Default)]
struct SyncForm {
    mode: SyncMode,
    via: SyncVia,
    folder: String,
    suggested: Option<Vec<(String, std::path::PathBuf)>>,
    server: String,
    token: String,
    device: String,
    pass: String,
    pass2: String,
    secret: String,
    spaces: Option<Result<Vec<quire_app::vault_sync::SyncVaultInfo>, String>>,
    chosen: Option<usize>,
    error: Option<String>,
    /// Shown once after creating a vault.
    recovery: Option<String>,
    show_key: bool,
    confirm_stop: bool,
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
            ui.selectable_value(&mut st.tab, SettingsTab::Sync, "Sync");
            ui.selectable_value(&mut st.tab, SettingsTab::About, "About");
        });
        ui.separator();
        match st.tab {
            SettingsTab::Editor => editor_settings(ui, ws),
            SettingsTab::Ai => ai_settings(ui, st, ws, pal),
            SettingsTab::Sync => sync_settings(ui, &mut st.sync, ws, pal),
            SettingsTab::About => about(ui, ws, pal),
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

// ------------------------------------------------------------------ sync

fn sync_settings(ui: &mut Ui, f: &mut SyncForm, ws: &mut Workspace, pal: &Palette) {
    use quire_app::vault_sync::SyncStatus;
    if ws.vault_root().is_none() {
        ui.label(RichText::new("Open a notes folder first; sync works per folder.").color(pal.muted));
        return;
    }
    if let Some(rk) = f.recovery.clone() {
        ui.label(RichText::new("Sync is on. Save your recovery key").strong());
        ui.label(
            RichText::new(
                "If you forget the passphrase, this key is the only way to get your synced notes onto a new device. \
                 Nobody can reset it for you. Keep it in a password manager or on paper.",
            )
            .size(12.5)
            .color(pal.muted),
        );
        ui.add_space(6.0);
        egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(&rk).monospace().size(14.0).color(pal.strong)).wrap());
        });
        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(rk.clone());
            }
            if ui.button("I've saved it").clicked() {
                f.recovery = None;
            }
        });
        return;
    }

    if let Some(cfg) = ws.sync.config.clone() {
        let (label, detail, color) = match &ws.sync.status {
            SyncStatus::Off => ("Off".to_string(), String::new(), pal.muted),
            SyncStatus::Syncing => ("Syncing…".to_string(), String::new(), pal.accent),
            SyncStatus::Synced(at) => ("Up to date".to_string(), format!("checked {}s ago", at.elapsed().as_secs()), pal.accent),
            SyncStatus::Offline(e) => ("Offline, will keep trying".to_string(), e.clone(), pal.warn),
            SyncStatus::Waiting(e) => ("Waiting for files".to_string(), e.clone(), pal.warn),
            SyncStatus::Error(e) => ("Needs attention".to_string(), e.clone(), pal.danger),
        };
        egui::Grid::new("sync-status").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.label("Status");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(label).color(color).strong());
                if !detail.is_empty() {
                    ui.label(RichText::new(detail).size(12.0).color(pal.muted));
                }
            });
            ui.end_row();
            if let Some(dir) = &cfg.folder {
                ui.label("Sync folder");
                ui.add(egui::Label::new(dir.display().to_string()).wrap());
            } else {
                ui.label("Server");
                ui.label(&cfg.server);
            }
            ui.end_row();
            ui.label("This device");
            ui.label(&cfg.device_name);
            ui.end_row();
            ui.label("Vault id");
            ui.label(RichText::new(&cfg.space.0[..12.min(cfg.space.0.len())]).monospace());
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Sync now").clicked() {
                ws.sync_nudge();
            }
            if ui.button(if f.show_key { "Hide recovery key" } else { "Show recovery key" }).clicked() {
                f.show_key = !f.show_key;
            }
            let stop = if f.confirm_stop { "Click again to stop syncing" } else { "Stop syncing on this device" };
            if ui.button(RichText::new(stop).color(pal.danger)).clicked() {
                if f.confirm_stop {
                    if let Err(e) = ws.stop_sync() {
                        f.error = Some(e);
                    }
                    f.confirm_stop = false;
                } else {
                    f.confirm_stop = true;
                }
            }
        });
        if f.show_key
            && let Some(k) = ws.sync_recovery_key()
        {
            ui.add(egui::Label::new(RichText::new(k).monospace()).wrap());
        }
        ui.label(
            RichText::new(if cfg.folder.is_some() {
                "Stopping leaves your notes here as they are, and the sync folder for your other devices."
            } else {
                "Stopping leaves your notes here as they are, and the server copy for your other devices."
            })
            .size(12.0)
            .color(pal.muted),
        );
        if let Some(e) = &f.error {
            ui.label(RichText::new(e).color(pal.danger));
        }
        return;
    }

    ui.label(
        RichText::new(
            "Keep this notes folder the same on all your devices. Notes are encrypted on this device with your passphrase \
             before they go anywhere, so nothing in between can read them or their names.",
        )
        .size(12.5)
        .color(pal.muted),
    );
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(&mut f.via, SyncVia::Folder, "Through a folder you already sync (free)");
        ui.selectable_value(&mut f.via, SyncVia::Server, "Through a Quire Sync server");
    });
    match f.via {
        SyncVia::Folder => {
            ui.label(
                RichText::new(
                    "Pick a folder that Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing keeps on all your devices. \
                     Quire keeps an encrypted “Quire Sync” folder inside it.",
                )
                .size(12.0)
                .color(pal.muted),
            );
        }
        SyncVia::Server => {
            ui.hyperlink_to(RichText::new("Run your own server").size(12.5), format!("{SOURCE_URL}/blob/main/docs/self-hosting.md"));
        }
    }
    ui.add_space(6.0);
    if f.device.is_empty() {
        f.device = quire_app::vault_sync::default_device_name();
    }
    let suggested = f.suggested.get_or_insert_with(quire_app::vault_sync::suggested_sync_folders).clone();
    egui::Grid::new("sync-setup").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        match f.via {
            SyncVia::Folder => {
                ui.label("Sync folder");
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::TextEdit::singleline(&mut f.folder).hint_text("Choose a synced folder").desired_width(250.0))
                            .changed()
                        {
                            f.spaces = None;
                        }
                        if ui.button("Choose…").clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_folder()
                        {
                            f.folder = p.display().to_string();
                            f.spaces = None;
                        }
                    });
                    if !suggested.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("On this computer:").size(12.0).color(pal.muted));
                            for (label, p) in &suggested {
                                if ui.small_button(label).on_hover_text(p.display().to_string()).clicked() {
                                    f.folder = p.display().to_string();
                                    f.spaces = None;
                                }
                            }
                        });
                    }
                });
                ui.end_row();
            }
            SyncVia::Server => {
                ui.label("Server");
                ui.add(egui::TextEdit::singleline(&mut f.server).hint_text("https://sync.example.com").desired_width(320.0));
                ui.end_row();
                ui.label("Server token");
                ui.add(egui::TextEdit::singleline(&mut f.token).password(true).desired_width(320.0));
                ui.end_row();
            }
        }
        ui.label("This device");
        ui.add(egui::TextEdit::singleline(&mut f.device).desired_width(320.0));
        ui.end_row();
        ui.label("");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut f.mode, SyncMode::Create, "Start a new sync vault");
            ui.selectable_value(&mut f.mode, SyncMode::Join, "Join from another device");
        });
        ui.end_row();
        match f.mode {
            SyncMode::Create => {
                ui.label("Passphrase");
                ui.add(egui::TextEdit::singleline(&mut f.pass).password(true).desired_width(320.0));
                ui.end_row();
                ui.label("Again");
                ui.add(egui::TextEdit::singleline(&mut f.pass2).password(true).desired_width(320.0));
                ui.end_row();
            }
            SyncMode::Join => {
                ui.label("Vault");
                ui.vertical(|ui| {
                    let find = if f.via == SyncVia::Folder { "Find vaults in the folder" } else { "Find vaults on the server" };
                    if ui.button(find).clicked() {
                        f.spaces = Some(Workspace::sync_list(&sync_target(f)));
                        f.chosen = f.spaces.as_ref().and_then(|s| s.as_ref().ok()).filter(|l| l.len() == 1).map(|_| 0);
                    }
                    match &f.spaces {
                        Some(Ok(list)) if list.is_empty() => {
                            let none = if f.via == SyncVia::Folder {
                                "No Quire vaults in this folder yet. Start one from another device, and let the folder finish syncing."
                            } else {
                                "No vaults on this server yet. Start one from another device."
                            };
                            ui.label(RichText::new(none).color(pal.muted));
                        }
                        Some(Ok(list)) => {
                            for (i, s) in list.iter().enumerate() {
                                let when = chrono::DateTime::from_timestamp(s.created, 0)
                                    .map(|d| d.format("%Y-%m-%d").to_string())
                                    .unwrap_or_default();
                                let text = format!("Started {when} · {} · {}", s.detail, &s.space.0[..8.min(s.space.0.len())]);
                                ui.radio_value(&mut f.chosen, Some(i), text);
                            }
                        }
                        Some(Err(e)) => {
                            ui.label(RichText::new(e).color(pal.danger));
                        }
                        None => {}
                    }
                });
                ui.end_row();
                ui.label("Passphrase");
                ui.add(egui::TextEdit::singleline(&mut f.secret).password(true).hint_text("or recovery key").desired_width(320.0));
                ui.end_row();
            }
        }
    });
    ui.add_space(6.0);
    let reachable = match f.via {
        SyncVia::Folder => !f.folder.trim().is_empty(),
        SyncVia::Server => !f.server.trim().is_empty() && !f.token.trim().is_empty(),
    };
    let ready = reachable && !f.device.trim().is_empty();
    match f.mode {
        SyncMode::Create => {
            if ui.add_enabled(ready && !f.pass.is_empty(), egui::Button::new("Start syncing")).clicked() {
                f.error = None;
                if f.pass == f.pass2 {
                    match ws.sync_create(&sync_target(f), &f.pass, &f.device) {
                        Ok(rk) => {
                            f.recovery = Some(rk.to_string());
                            f.pass.clear();
                            f.pass2.clear();
                        }
                        Err(e) => f.error = Some(e),
                    }
                } else {
                    f.error = Some("The passphrases don't match".into());
                }
            }
            ui.label(
                RichText::new("You'll type the passphrase once on each new device. It never leaves your devices.")
                    .size(12.0)
                    .color(pal.muted),
            );
        }
        SyncMode::Join => {
            let chosen = f.chosen.and_then(|i| f.spaces.as_ref()?.as_ref().ok()?.get(i).cloned());
            if ui.add_enabled(ready && chosen.is_some() && !f.secret.is_empty(), egui::Button::new("Join and sync")).clicked()
                && let Some(s) = chosen
            {
                f.error = None;
                match ws.sync_join(&sync_target(f), &s.space, &f.secret, &f.device) {
                    Ok(()) => f.secret.clear(),
                    Err(e) => f.error = Some(e),
                }
            }
            ui.label(
                RichText::new("Notes already in this folder are kept and synced too. Same-named notes are kept side by side.")
                    .size(12.0)
                    .color(pal.muted),
            );
        }
    }
    if let Some(e) = &f.error {
        ui.label(RichText::new(e).color(pal.danger));
    }
}

fn sync_target(f: &SyncForm) -> quire_app::vault_sync::SyncTarget {
    use quire_app::vault_sync::SyncTarget;
    match f.via {
        SyncVia::Folder => SyncTarget::Folder(std::path::PathBuf::from(f.folder.trim())),
        SyncVia::Server => SyncTarget::Server { url: f.server.trim().to_string(), token: f.token.trim().to_string() },
    }
}

/// The math font's licence, in builds that include it.
const MATH_FONT_NOTICE: &[(&str, &str)] = &[
    #[cfg(feature = "typeset")]
    ("STIX Two Math", quire_typeset::fonts::STIX_TWO_MATH_LICENSE),
];

fn about(ui: &mut Ui, ws: &mut Workspace, pal: &Palette) {
    let edition = if crate::typeset_ui::AVAILABLE { "" } else { " Lite" };
    ui.label(RichText::new(format!("Quire{edition} {}", env!("CARGO_PKG_VERSION"))).strong().size(16.0));
    if !crate::typeset_ui::AVAILABLE {
        ui.label(
            RichText::new(
                "This is the smaller edition: math, diagrams and PDF export come with the full download. Everything else is the \
                 same, and switching keeps your notes and settings.",
            )
            .size(12.0)
            .color(pal.muted),
        );
        ui.hyperlink_to("Get the full edition", format!("{SOURCE_URL}/releases/latest"));
    }
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
    ui.add_space(12.0);
    ui.separator();
    ui.collapsing("Third-party notices", |ui| {
        ui.label(
            RichText::new(
                "Quire includes the Inter and JetBrains Mono fonts (and STIX Two Math in the full edition), under the SIL Open Font License 1.1, and English \
                 word lists for spell checking from SCOWL by Kevin Atkinson and the sources it draws on, including the UK \
                 Advanced Cryptics Dictionary. Their notices follow.",
            )
            .size(12.0)
            .color(pal.muted),
        );
        let notices: Vec<(&str, &str)> = quire_spell::DICTIONARY_NOTICES
            .iter()
            .copied()
            .chain([
                ("Inter", include_str!("../fonts/Inter-OFL.txt")),
                ("JetBrains Mono", include_str!("../fonts/JetBrainsMono-OFL.txt")),
            ])
            .chain(MATH_FONT_NOTICE.iter().copied())
            .collect();
        for (name, text) in notices {
            ui.collapsing(name, |ui| {
                egui::ScrollArea::vertical().id_salt(("notice", name)).max_height(220.0).show(ui, |ui| {
                    ui.label(RichText::new(text).monospace().size(11.0));
                });
            });
        }
    });
    ui.separator();
    ui.label(RichText::new("Updates").strong());
    ui.checkbox(&mut ws.settings.check_updates, "Check for a new version once a day");
    ui.label(
        RichText::new("Asks GitHub for the latest release. Nothing about you or your notes is sent, and nothing is installed for you.")
            .size(12.0)
            .color(pal.muted),
    );
    ui.horizontal(|ui| {
        let checking = ws.updates.checking();
        if ui.add_enabled(!checking, egui::Button::new(if checking { "Checking…" } else { "Check now" })).clicked() {
            ws.check_for_updates(true);
        }
        if let Some(r) = ws.updates.available.clone() {
            ui.hyperlink_to(format!("Get Quire {}", r.version), r.url);
        } else if let Some(s) = &ws.updates.status {
            ui.add(egui::Label::new(RichText::new(s).size(12.5).color(pal.muted)).wrap());
        }
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
            ui.checkbox(&mut s.live_preview, "Hide Markdown syntax except on the line you're editing");
            ui.checkbox(&mut s.dates_as_links, "Insert dates as [[links]]");
            ui.checkbox(&mut s.inline_calculator, "Inline calculator (Ctrl+Enter)");
        });
        ui.end_row();
        ui.label("Spelling");
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.spell_check, "Underline misspelled words");
            ui.add_enabled_ui(s.spell_check, |ui| {
                egui::ComboBox::from_id_salt("spell-lang").selected_text(s.spell_language.replace('_', "-")).show_ui(ui, |ui| {
                    for lang in crate::spell_ui::languages() {
                        let label = lang.replace('_', "-");
                        ui.selectable_value(&mut s.spell_language, lang, label);
                    }
                });
            });
        });
        ui.end_row();
        ui.label("Quick capture");
        ui.vertical(|ui| {
            ui.checkbox(&mut s.capture_shortcut, format!("{} adds a thought to today's note from any app", crate::capture_key::label()));
            if s.capture_shortcut
                && let Some(why) = crate::capture_key::unavailable()
            {
                ui.label(
                    RichText::new(format!("The shortcut isn't available here ({why}). Quick capture is still in the command palette."))
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
            }
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
    /// Showing versions from every device (the sync log) instead of this one's.
    all_devices: bool,
    sync_rx: Option<Receiver<Result<Vec<quire_app::vault_sync::SyncVersion>, String>>>,
    /// Newest first.
    sync: Option<Result<Vec<quire_app::vault_sync::SyncVersion>, String>>,
    sync_selected: usize,
    diffed: Option<Diffed>,
}

/// The diff on screen, kept until the texts it compares change.
struct Diffed {
    key: u64,
    lines: Vec<quire_app::history_ops::DiffLine>,
    stats: (usize, usize),
}

impl HistoryState {
    /// The All devices view, already showing `versions` (newest first).
    #[cfg(test)]
    pub(crate) fn all_devices(ws: &Workspace, versions: Result<Vec<quire_app::vault_sync::SyncVersion>, String>) -> Self {
        Self { all_devices: true, sync: Some(versions), ..Self::new(ws) }
    }

    pub(crate) fn new(ws: &Workspace) -> Self {
        let snapshots = ws.note_history();
        // The newest snapshot is usually the text you're looking at; start
        // on the one before it so the dialog opens on an actual change.
        let current = ws.active_doc().map(|d| d.text().to_string());
        let same = snapshots.first().and_then(|s| ws.snapshot_text(s)) == current;
        let selected = usize::from(same && snapshots.len() > 1);
        Self { snapshots, selected, loaded: None, all_devices: false, sync_rx: None, sync: None, sync_selected: 0, diffed: None }
    }
}

pub(crate) fn history(ctx: &egui::Context, st: &mut HistoryState, ws: &mut Workspace, pal: &Palette) -> bool {
    let mut close = false;
    let mut restore = None;
    let mut restore_sync: Option<(String, String)> = None;
    let current = ws.active_doc().map(|d| d.text().to_string()).unwrap_or_default();
    let title = ws.active_doc().map(quire_app::Document::title).unwrap_or_default();
    let syncing = ws.sync.config.is_some();
    if let Some(rx) = &st.sync_rx {
        match rx.try_recv() {
            Ok(r) => {
                st.sync = Some(r.map(|mut v| {
                    v.reverse();
                    v
                }));
                st.sync_rx = None;
            }
            Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
        }
    }
    if st.all_devices
        && st.sync.is_none()
        && st.sync_rx.is_none()
        && let Some(p) = ws.active_doc().and_then(|d| d.path.clone())
    {
        st.sync_rx = ws.fetch_sync_history(&p);
    }
    if st.loaded.as_ref().is_none_or(|(i, _)| *i != st.selected)
        && let Some(s) = st.snapshots.get(st.selected)
    {
        st.loaded = ws.snapshot_text(s).map(|t| (st.selected, t));
    }
    let when = |ms: i64, fmt: &str| {
        chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format(fmt).to_string()).unwrap_or_default()
    };
    // The text being compared: a local snapshot, or a version from the sync log.
    let shown: Option<String> = if st.all_devices {
        match &st.sync {
            Some(Ok(v)) => v.get(st.sync_selected).filter(|x| !x.deleted).map(|x| x.text.clone()),
            _ => None,
        }
    } else {
        st.loaded.as_ref().map(|(_, t)| t.clone())
    };
    modal(ctx, "quire-history", pal).show(ctx, |ui| {
        let w = modal_width(ctx, 820.0);
        // The lists scroll inside a body of fixed height that leaves room for
        // the title and buttons on small windows. It must be fixed: a modal
        // lays out in last frame's size, so anything sized by the space left
        // (like the divider between the lists) would grow it every repaint.
        let body_h = modal_body_height(ctx, 440.0, 170.0);
        ui.set_width(w);
        ui.set_max_width(w);
        ui.horizontal(|ui| {
            ui.heading(format!("Version history · {title}"));
            if syncing {
                ui.add_space(12.0);
                ui.selectable_value(&mut st.all_devices, false, "This device");
                ui.selectable_value(&mut st.all_devices, true, "All devices");
            }
        });
        let note = if st.all_devices {
            "Every version Quire Sync holds, from every device, including ones that lost a race. Restoring is undoable."
        } else {
            "Saved automatically in the vault's .quire folder. Restoring is undoable."
        };
        ui.label(RichText::new(note).size(12.0).color(pal.muted));
        ui.separator();
        if !st.all_devices && st.snapshots.is_empty() {
            ui.label(RichText::new("No earlier versions yet. Versions are kept as you edit and save.").color(pal.muted));
        }
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), body_h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.set_height(body_h);
            ui.vertical(|ui| {
                ui.set_width(200.0);
                egui::ScrollArea::vertical().id_salt("hist-list").max_height(body_h).show(ui, |ui| {
                    if st.all_devices {
                        match &st.sync {
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(RichText::new("Fetching…").color(pal.muted));
                                });
                            }
                            Some(Err(e)) => {
                                ui.label(RichText::new(e).color(pal.danger).size(12.5));
                            }
                            Some(Ok(v)) if v.is_empty() => {
                                ui.label(RichText::new("This note hasn't synced yet.").color(pal.muted));
                            }
                            Some(Ok(v)) => {
                                for (i, x) in v.iter().enumerate() {
                                    let date = if x.at > 0 { when(x.at, "%b %-d · %H:%M:%S") } else { "Earlier".to_string() };
                                    let device = if x.deleted { format!("{} · deleted", x.device) } else { x.device.clone() };
                                    let r = crate::theme::row(ui, i == st.sync_selected, 40.0, pal, |painter, rect| {
                                        let left = rect.left() + 8.0;
                                        painter.text(
                                            egui::pos2(left, rect.top() + 12.0),
                                            egui::Align2::LEFT_CENTER,
                                            &date,
                                            egui::FontId::proportional(13.0),
                                            pal.text,
                                        );
                                        painter.text(
                                            egui::pos2(left, rect.top() + 28.0),
                                            egui::Align2::LEFT_CENTER,
                                            &device,
                                            egui::FontId::proportional(11.5),
                                            pal.muted,
                                        );
                                    });
                                    if r.clicked() {
                                        st.sync_selected = i;
                                    }
                                }
                            }
                        }
                    } else {
                        for (i, s) in st.snapshots.iter().enumerate() {
                            let label = when(s.at_ms, "%b %-d · %H:%M:%S");
                            if ui.selectable_label(i == st.selected, label).clicked() {
                                st.selected = i;
                            }
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                if st.all_devices
                    && let Some(Ok(v)) = &st.sync
                    && v.get(st.sync_selected).is_some_and(|x| x.deleted)
                {
                    ui.label(RichText::new("This version deleted the note.").color(pal.muted));
                }
                if let Some(old) = &shown {
                    // Diffing the whole note is too slow to redo every frame.
                    let key = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        (old, &current).hash(&mut h);
                        h.finish()
                    };
                    if st.diffed.as_ref().is_none_or(|d| d.key != key) {
                        st.diffed = Some(Diffed { key, lines: diff(old, &current, 3), stats: diff_stats(old, &current) });
                    }
                    let d = st.diffed.as_ref().expect("just set");
                    let (added, removed) = d.stats;
                    ui.label(RichText::new(format!("Changes since this version: +{added} −{removed} lines")).size(12.0).color(pal.muted));
                    if d.lines.iter().all(|l| l.kind == DiffKind::Same) {
                        ui.label(RichText::new("Identical to the current text.").color(pal.muted));
                    }
                    // Every line, however long the diff, but only the visible rows are drawn.
                    let font = egui::FontId::monospace(12.5);
                    let row_h = 19.0;
                    egui::ScrollArea::vertical().id_salt("hist-diff").max_height(body_h - 44.0).auto_shrink([false, true]).show_rows(
                        ui,
                        row_h,
                        d.lines.len(),
                        |ui, range| {
                            for l in &d.lines[range] {
                                let (prefix, color, bg) = match l.kind {
                                    DiffKind::Same => (" ", pal.text, egui::Color32::TRANSPARENT),
                                    DiffKind::Added => ("+", pal.text, pal.accent.gamma_multiply(0.18)),
                                    DiffKind::Removed => ("−", pal.text, pal.danger.gamma_multiply(0.18)),
                                    DiffKind::Gap => ("", pal.faint, egui::Color32::TRANSPARENT),
                                };
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                                let painter = ui.painter_at(rect);
                                painter.rect_filled(rect.shrink2(egui::vec2(0.0, 1.0)), 3.0, bg);
                                painter.text(
                                    egui::pos2(rect.left() + 4.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    format!("{prefix} {}", l.text),
                                    font.clone(),
                                    color,
                                );
                            }
                        },
                    );
                    ui.label(RichText::new("+ lines are in your current text, − lines only in this version.").size(11.0).color(pal.faint));
                }
            });
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.add_enabled(shown.is_some(), egui::Button::new("Restore this version")).clicked() {
                if st.all_devices {
                    if let (Some(text), Some(Ok(v))) = (&shown, &st.sync)
                        && let Some(x) = v.get(st.sync_selected)
                    {
                        let what = if x.at > 0 {
                            format!("the version from {} on {}", when(x.at, "%b %-d, %H:%M"), x.device)
                        } else {
                            format!("an earlier version from {}", x.device)
                        };
                        restore_sync = Some((text.clone(), what));
                    }
                } else {
                    restore = st.snapshots.get(st.selected).cloned();
                }
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
    if let Some((text, what)) = restore_sync
        && ws.restore_text(&text, &what)
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

// ------------------------------------------------------------------ publish

pub(crate) struct PublishState {
    title: String,
    out: String,
    all: bool,
    result: Option<Result<(quire_app::publish::PublishReport, std::path::PathBuf), String>>,
}

impl PublishState {
    pub(crate) fn new(ws: &Workspace) -> Self {
        let out = ws.vault_root().and_then(|r| r.parent().map(|p| p.join(format!("{} site", ws.vault_name()))));
        Self { title: ws.vault_name(), out: out.map(|p| p.display().to_string()).unwrap_or_default(), all: false, result: None }
    }
}

/// Returns true when the dialog should close.
pub(crate) fn publish(ctx: &egui::Context, st: &mut PublishState, ws: &mut Workspace, pal: &Palette) -> bool {
    use quire_app::publish::Selection;
    let mut close = false;
    modal(ctx, "quire-publish", pal).show(ctx, |ui| {
        ui.set_width(modal_width(ctx, 500.0));
        ui.heading("Publish as a website");
        ui.label(
            RichText::new(
                "Turns notes into a folder of web pages with links, backlinks, tags and search. Put it on GitHub Pages, \
                 Netlify, Cloudflare Pages or any web server. Links to notes you don't publish become plain text.",
            )
            .size(12.0)
            .color(pal.muted),
        );
        ui.add_space(6.0);
        egui::Grid::new("pub-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Site title");
            ui.add(egui::TextEdit::singleline(&mut st.title).desired_width(300.0));
            ui.end_row();
            ui.label("Notes");
            ui.vertical(|ui| {
                ui.radio_value(&mut st.all, false, "Only notes with publish: true");
                ui.radio_value(&mut st.all, true, "All notes (except publish: false and templates)");
            });
            ui.end_row();
            ui.label("Into folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut st.out).desired_width(230.0));
                if ui.button("Choose…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    st.out = p.display().to_string();
                }
            });
            ui.end_row();
        });
        match &st.result {
            Some(Ok((r, index))) => {
                let more = if r.private_links > 0 {
                    format!(", {} links to unpublished notes left as text", r.private_links)
                } else {
                    String::new()
                };
                ui.label(RichText::new(format!("Published {} pages and {} files{more}.", r.pages, r.files)).color(pal.accent));
                ui.hyperlink_to("Open the site", format!("file://{}", index.display()));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(pal.danger).size(12.5));
            }
            None => {}
        }
        ui.separator();
        ui.horizontal(|ui| {
            let ready = !st.out.trim().is_empty() && !st.title.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Publish")).clicked() {
                let out = std::path::PathBuf::from(st.out.trim());
                let which = if st.all { Selection::All } else { Selection::Marked };
                st.result = Some(ws.publish_site(&out, st.title.trim(), which).map(|r| (r, out.join("index.html"))));
            }
            close |= ui.button("Close").clicked();
        });
    });
    close
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
#[cfg(test)]
pub(crate) const TOUR_STEPS: usize = TOUR.len();

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
pub(crate) fn tour(ctx: &egui::Context, step: &mut usize, ws: &mut Workspace, pal: &Palette) -> bool {
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
        if *step == last {
            ui.add_space(8.0);
            if ui.checkbox(&mut ws.settings.check_updates, "Tell me when a new version is out (checks GitHub once a day)").changed() {
                let _ = ws.settings.save();
            }
        }
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
