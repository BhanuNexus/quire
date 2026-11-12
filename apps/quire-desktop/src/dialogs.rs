// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings, version history and import dialogs.

use crossbeam_channel::Receiver;
use egui::{RichText, Ui};
use quire_app::history_ops::{DiffKind, diff, diff_stats};
use quire_app::vault::Snapshot;
use quire_app::workspace::NoticeLevel;
use quire_app::{Command, Theme, Workspace, ai};

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
    /// Show `tab` even if its feature isn't offered (layout tests).
    #[cfg(test)]
    force_tab: bool,
    /// State of feature tabs (`registry::SETTINGS_TABS`).
    features: crate::registry::FeatureUi,
}

impl SettingsState {
    pub(crate) fn sync_tab(_ws: &Workspace) -> Self {
        Self { tab: SettingsTab::Sync, ..Default::default() }
    }

    /// Open on a feature's tab (`registry::SETTINGS_TABS` id).
    pub(crate) fn feature_tab(id: &'static str) -> Self {
        Self { tab: SettingsTab::Feature(id), ..Default::default() }
    }

    /// Every tab, built in and registered (offered or not), for `layout_check`.
    #[cfg(test)]
    pub(crate) fn every_tab() -> Vec<(&'static str, Self)> {
        let builtin = [
            ("settings: editor", SettingsTab::Editor),
            ("settings: ai", SettingsTab::Ai),
            ("settings: sync", SettingsTab::Sync),
            ("settings: about", SettingsTab::About),
        ];
        let mut all: Vec<(&'static str, Self)> = builtin
            .into_iter()
            .chain(crate::registry::SETTINGS_TABS.iter().map(|t| ("settings: feature tab", SettingsTab::Feature(t.id))))
            .map(|(name, tab)| (name, Self { tab, force_tab: true, ..Default::default() }))
            .collect();
        // 0.12: the S3 and WebDAV forms, with a long error and a test result.
        for (name, via) in [("settings: sync s3", SyncVia::S3), ("settings: sync webdav", SyncVia::WebDav)] {
            let mut sync = SyncForm { via, ..Default::default() };
            sync.test_result = Some(Err("The storage refused the access key: SignatureDoesNotMatch (The request signature we calculated does not match the signature you provided.)".into()));
            sync.mode = SyncMode::Join;
            all.push((name, Self { tab: SettingsTab::Sync, force_tab: true, sync, ..Default::default() }));
        }
        // 0.13: Devices, removing one (with a long name), and the upgrade warning.
        for step in 0..3 {
            let sync = SyncForm { devices: DevicesPanel::sample(step), ..Default::default() };
            all.push(("settings: sync devices", Self { tab: SettingsTab::Sync, force_tab: true, sync, ..Default::default() }));
        }
        all
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsTab {
    #[default]
    Editor,
    Ai,
    Sync,
    About,
    /// A tab registered in `registry::SETTINGS_TABS`, by id. A registered
    /// `"ai"` tab replaces the built-in AI tab.
    Feature(&'static str),
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
    /// 0.12: AWS S3, Cloudflare R2, Backblaze B2, Wasabi, MinIO…
    S3,
    /// 0.12: Nextcloud, ownCloud, Fastmail, Synology, rclone…
    WebDav,
}

/// S3 providers: a starting endpoint and region.
const S3_PRESETS: &[(&str, &str, &str)] = &[
    ("AWS", "https://s3.us-east-1.amazonaws.com", "us-east-1"),
    ("Cloudflare R2", "https://ACCOUNT_ID.r2.cloudflarestorage.com", "auto"),
    ("Backblaze B2", "https://s3.us-west-004.backblazeb2.com", "us-west-004"),
    ("Wasabi", "https://s3.us-east-1.wasabisys.com", "us-east-1"),
    ("MinIO", "http://127.0.0.1:9000", "us-east-1"),
];

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
    /// "Also sync vault settings…" turned on: the sync server is being asked.
    shared_pending: Option<(bool, Receiver<Result<(), String>>)>,
    // 0.12: S3-compatible storage and WebDAV.
    s3_endpoint: String,
    s3_region: String,
    s3_bucket: String,
    s3_prefix: String,
    s3_access_key: String,
    s3_secret: String,
    dav_url: String,
    dav_user: String,
    dav_password: String,
    /// `http://` to a computer on the network.
    allow_http: bool,
    /// "Test connection" running, and its last result.
    test: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    /// "Update credentials" open, for a vault set up with S3 or WebDAV.
    edit_credentials: bool,
    /// New credentials being checked and saved.
    saving_credentials: Option<Receiver<Result<String, String>>>,
    /// 0.13: Settings → Sync → Devices and the sync format.
    devices: DevicesPanel,
}

/// Settings → Sync → Devices and the sync format (0.13).
#[derive(Default)]
struct DevicesPanel {
    loading: Option<Receiver<Result<quire_app::vault_sync::SyncDevices, String>>>,
    list: Option<Result<quire_app::vault_sync::SyncDevices, String>>,
    /// The device "Remove this device" was clicked for: its id and label.
    removing: Option<(String, String)>,
    pass: String,
    pass2: String,
    removal: Option<Receiver<Result<quire_app::vault_sync::DeviceRemoved, String>>>,
    removed: Option<Result<quire_app::vault_sync::DeviceRemoved, String>>,
    upgrade_info: Option<Receiver<Result<quire_app::vault_sync::UpgradeInfo, String>>>,
    /// "Upgrade sync format…" clicked: what it would mean.
    upgrade: Option<Result<quire_app::vault_sync::UpgradeInfo, String>>,
    upgrading: Option<Receiver<Result<(), String>>>,
    /// Layout checks: show the panel with sample devices and no vault.
    sample: bool,
}

impl SyncForm {
    fn remote_secret(&self, s3: bool) -> quire_app::vault_sync::RemoteSecret {
        use quire_app::vault_sync::RemoteSecret;
        if s3 {
            RemoteSecret::S3 {
                access_key: self.s3_access_key.trim().to_string(),
                secret_key: self.s3_secret.trim().to_string(),
                session_token: None,
            }
        } else {
            RemoteSecret::Webdav { user: self.dav_user.trim().to_string(), password: self.dav_password.clone() }
        }
    }

    fn clear_secrets(&mut self) {
        self.s3_secret.clear();
        self.dav_password.clear();
        self.token.clear();
    }

    /// After sync was set up: secrets and the last test go.
    fn done_setting_up(&mut self) {
        self.clear_secrets();
        self.test_result = None;
    }

    /// Start "Test connection" for `target` in the background.
    fn start_test(&mut self, target: quire_app::vault_sync::SyncTarget) {
        self.test_result = None;
        self.test = Some(Workspace::sync_test_in_background(target));
    }

    /// The test's result, once it's in; asks for repaints meanwhile.
    fn poll_test(&mut self, ui: &Ui) {
        if let Some(rx) = &self.test {
            match rx.try_recv() {
                Ok(r) => {
                    self.test_result = Some(r);
                    self.test = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(100)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.test = None,
            }
        }
    }

    fn show_test(&self, ui: &mut Ui, pal: &Palette) {
        if self.test.is_some() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("Testing…").size(12.0).color(pal.muted));
            });
        }
        match &self.test_result {
            Some(Ok(m)) => {
                ui.add(egui::Label::new(RichText::new(m).size(12.5).color(pal.accent)).wrap());
            }
            Some(Err(e)) => {
                ui.add(egui::Label::new(RichText::new(e).size(12.5).color(pal.danger)).wrap());
            }
            None => {}
        }
    }
}

/// Returns whether the dialog should close, and a command a tab asked for.
pub(crate) fn settings(ctx: &egui::Context, st: &mut SettingsState, ws: &mut Workspace, pal: &Palette) -> (bool, Option<Command>) {
    let mut close = false;
    let mut run = None;
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
        let w = modal_width(ctx, 720.0);
        // Every tab gets the same fixed-height body, scrolled when it needs
        // more: the dialog keeps its size between tabs and on small windows.
        let body_h = modal_body_height(ctx, 500.0, 110.0);
        ui.set_width(w);
        ui.set_max_width(w);
        let feature_tabs = crate::registry::offered(crate::registry::SETTINGS_TABS, ws);
        let ai_replaced = feature_tabs.iter().any(|t| t.id == "ai");
        #[cfg(test)]
        let forced = st.force_tab;
        #[cfg(not(test))]
        let forced = false;
        if let SettingsTab::Feature(id) = st.tab
            && !forced
            && !feature_tabs.iter().any(|t| t.id == id)
        {
            st.tab = SettingsTab::Editor;
        }
        if st.tab == SettingsTab::Ai && ai_replaced {
            st.tab = SettingsTab::Feature("ai");
        }
        ui.heading("Settings");
        ui.separator();
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), body_h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.set_height(body_h);
            // Scrolls on a short window: there are more tabs than 480 points hold.
            egui::ScrollArea::vertical().id_salt("quire-settings-tabs").max_height(body_h).auto_shrink([true, false]).show(ui, |ui| {
                // The scroll area keeps the parent's (sideways) layout: stack the tabs.
                ui.vertical(|ui| {
                    ui.set_width(132.0);
                    let mut tab = |ui: &mut Ui, value: SettingsTab, label: String| {
                        let r = crate::theme::row(ui, st.tab == value, 28.0, pal, |painter, rect| {
                            painter.text(
                                egui::pos2(rect.left() + 8.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                label,
                                egui::FontId::proportional(13.5),
                                if st.tab == value { pal.strong } else { pal.text },
                            );
                        });
                        if r.clicked() {
                            st.tab = value;
                        }
                    };
                    tab(ui, SettingsTab::Editor, "Editor".into());
                    if !ai_replaced {
                        tab(ui, SettingsTab::Ai, "AI & search".into());
                    }
                    tab(ui, SettingsTab::Sync, "Sync".into());
                    for t in &feature_tabs {
                        tab(ui, SettingsTab::Feature(t.id), (t.label)(ws));
                    }
                    tab(ui, SettingsTab::About, "About".into());
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                egui::ScrollArea::vertical().id_salt(("quire-settings-body", st.tab)).max_height(body_h).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.set_max_width(ui.available_width());
                        match st.tab {
                            SettingsTab::Editor => editor_settings(ui, ws),
                            SettingsTab::Ai => ai_settings(ui, st, ws, pal),
                            SettingsTab::Sync => sync_settings(ui, &mut st.sync, ws, pal),
                            SettingsTab::About => about(ui, ws, pal),
                            SettingsTab::Feature(id) => {
                                if let Some(t) = crate::registry::SETTINGS_TABS.iter().find(|t| t.id == id) {
                                    let out = (t.show)(ui, ws, &mut st.features, pal);
                                    if let Some(c) = out.run {
                                        run = Some(c);
                                    }
                                }
                            }
                        }
                    },
                );
            });
        });
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
    (close, run)
}

pub(crate) const SOURCE_URL: &str = "https://github.com/BhanuNexus/quire";

// ------------------------------------------------------------------ sync

fn sync_settings(ui: &mut Ui, f: &mut SyncForm, ws: &mut Workspace, pal: &Palette) {
    use quire_app::vault_sync::SyncStatus;
    if f.devices.sample {
        sync_devices(ui, &mut f.devices, ws, true, pal);
        return;
    }
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
            SyncStatus::Elsewhere(e) => ("Synced by another Quire".to_string(), e.clone(), pal.muted),
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
            if cfg.remote.is_some() {
                ui.label("Storage");
                ui.add(egui::Label::new(cfg.transport_label()).wrap());
            } else if let Some(dir) = &cfg.folder {
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
        if let Some(profile) = cfg.remote.clone() {
            remote_configured(ui, f, ws, &cfg, &profile, pal);
        }
        ui.add_space(4.0);
        let mut shared = f.shared_pending.as_ref().map_or(cfg.shared_state, |(on, _)| *on);
        if let Some((on, rx)) = &f.shared_pending {
            match rx.try_recv() {
                Ok(r) => {
                    f.error = r.and_then(|()| ws.sync_set_shared_state(*on)).err();
                    f.shared_pending = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(100)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => f.shared_pending = None,
            }
        }
        ui.horizontal_wrapped(|ui| {
            let r = ui
                .add_enabled(f.shared_pending.is_none(), egui::Checkbox::new(&mut shared, "Also sync vault settings and review history"))
                .on_hover_text("Agent permissions, suggestions and their history travel with the vault.");
            if r.changed() {
                // Turning it on asks the sync server first, off the UI thread.
                match ws.sync_share_vault_settings_in_background(shared) {
                    Ok(rx) => {
                        f.error = None;
                        f.shared_pending = Some((shared, rx));
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            if f.shared_pending.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
        ui.label(
            RichText::new(if cfg.remote.is_some() {
                "Settings travel with the notes, to every device of this vault."
            } else if cfg.folder.is_some() {
                "Devices still on Quire 0.8 keep syncing notes; settings reach devices on 0.9 or later."
            } else {
                "Every device needs Quire 0.9 or later."
            })
            .size(12.0)
            .color(pal.muted),
        );
        ui.label(
            RichText::new(if cfg.remote.is_some() {
                "Stopping leaves your notes here as they are, and the storage copy for your other devices."
            } else if cfg.folder.is_some() {
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
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Devices and sync format").id_salt("sync-devices").show(ui, |ui| {
            sync_devices(ui, &mut f.devices, ws, cfg.server.is_empty(), pal);
        });
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
        let before = f.via;
        ui.selectable_value(&mut f.via, SyncVia::Folder, "A synced folder (free)");
        ui.selectable_value(&mut f.via, SyncVia::Server, "Quire Sync server");
        ui.selectable_value(&mut f.via, SyncVia::S3, "S3-compatible storage");
        ui.selectable_value(&mut f.via, SyncVia::WebDav, "WebDAV");
        if f.via != before {
            f.spaces = None;
            f.test_result = None;
        }
    });
    f.poll_test(ui);
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
        SyncVia::S3 | SyncVia::WebDav => {
            ui.label(
                RichText::new(if f.via == SyncVia::S3 {
                    "AWS S3, Cloudflare R2, Backblaze B2, Wasabi, MinIO or any S3-compatible bucket. The storage sees only \
                     encrypted files with random names. Use a key that can reach only this bucket."
                } else {
                    "Nextcloud, ownCloud, Fastmail, Synology, rclone or any WebDAV folder. The server sees only encrypted \
                     files with random names. Use an app password if the server has them."
                })
                .size(12.0)
                .color(pal.muted),
            );
            ui.hyperlink_to(
                RichText::new("Setup, permissions and costs per provider").size(12.5),
                format!("{SOURCE_URL}/blob/main/docs/sync-backends.md"),
            );
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
            SyncVia::S3 => s3_fields(ui, f, pal),
            SyncVia::WebDav => dav_fields(ui, f),
        }
        if matches!(f.via, SyncVia::S3 | SyncVia::WebDav) {
            ui.label("");
            ui.vertical(|ui| {
                ui.checkbox(&mut f.allow_http, "Allow http:// (a server on your own network only)");
                if ui.add_enabled(f.test.is_none() && remote_ready(f), egui::Button::new("Test connection")).clicked() {
                    let t = sync_target(f);
                    f.start_test(t);
                }
                f.show_test(ui, pal);
            });
            ui.end_row();
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
                    let find = match f.via {
                        SyncVia::Folder => "Find vaults in the folder",
                        SyncVia::Server => "Find vaults on the server",
                        SyncVia::S3 | SyncVia::WebDav => "Find vaults in the storage",
                    };
                    if ui.button(find).clicked() {
                        f.spaces = Some(Workspace::sync_list(&sync_target(f)));
                        f.chosen = f.spaces.as_ref().and_then(|s| s.as_ref().ok()).filter(|l| l.len() == 1).map(|_| 0);
                    }
                    match &f.spaces {
                        Some(Ok(list)) if list.is_empty() => {
                            let none = match f.via {
                                SyncVia::Folder => {
                                    "No Quire vaults in this folder yet. Start one from another device, and let the folder finish syncing."
                                }
                                SyncVia::Server => "No vaults on this server yet. Start one from another device.",
                                SyncVia::S3 | SyncVia::WebDav => "No Quire vaults in this storage yet. Start one from another device.",
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
        SyncVia::S3 | SyncVia::WebDav => remote_ready(f),
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
                            f.done_setting_up();
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
                    Ok(()) => {
                        f.secret.clear();
                        f.done_setting_up();
                    }
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
    use quire_app::vault_sync::{RemoteProfile, SyncTarget, s3_virtual_hosted_default};
    match f.via {
        SyncVia::Folder => SyncTarget::Folder(std::path::PathBuf::from(f.folder.trim())),
        SyncVia::Server => SyncTarget::Server { url: f.server.trim().to_string(), token: f.token.trim().to_string() },
        SyncVia::S3 => SyncTarget::Remote {
            profile: RemoteProfile::S3 {
                endpoint: f.s3_endpoint.trim().to_string(),
                region: f.s3_region.trim().to_string(),
                bucket: f.s3_bucket.trim().to_string(),
                prefix: f.s3_prefix.trim().to_string(),
                virtual_hosted: s3_virtual_hosted_default(f.s3_endpoint.trim(), f.s3_bucket.trim()),
                allow_http: f.allow_http,
            },
            secret: f.remote_secret(true),
        },
        SyncVia::WebDav => SyncTarget::Remote {
            profile: RemoteProfile::Webdav { url: f.dav_url.trim().to_string(), allow_http: f.allow_http },
            secret: f.remote_secret(false),
        },
    }
}

/// Every S3 or WebDAV field filled in.
fn remote_ready(f: &SyncForm) -> bool {
    match f.via {
        SyncVia::S3 => [&f.s3_endpoint, &f.s3_region, &f.s3_bucket, &f.s3_access_key, &f.s3_secret].iter().all(|s| !s.trim().is_empty()),
        SyncVia::WebDav => [&f.dav_url, &f.dav_user].iter().all(|s| !s.trim().is_empty()) && !f.dav_password.is_empty(),
        _ => false,
    }
}

fn s3_fields(ui: &mut Ui, f: &mut SyncForm, pal: &Palette) {
    ui.label("Provider");
    ui.horizontal_wrapped(|ui| {
        for (name, endpoint, region) in S3_PRESETS {
            if ui.small_button(*name).on_hover_text(*endpoint).clicked() {
                f.s3_endpoint = (*endpoint).to_string();
                f.s3_region = (*region).to_string();
                f.allow_http = endpoint.starts_with("http://");
                f.test_result = None;
            }
        }
    });
    ui.end_row();
    ui.label("Endpoint");
    ui.vertical(|ui| {
        ui.add(egui::TextEdit::singleline(&mut f.s3_endpoint).hint_text("https://s3.us-east-1.amazonaws.com").desired_width(320.0));
        if f.s3_endpoint.contains("ACCOUNT_ID") {
            ui.label(RichText::new("Put your Cloudflare account ID in the address.").size(12.0).color(pal.muted));
        }
    });
    ui.end_row();
    ui.label("Region");
    ui.add(egui::TextEdit::singleline(&mut f.s3_region).hint_text("us-east-1 (R2: auto)").desired_width(320.0));
    ui.end_row();
    ui.label("Bucket");
    ui.add(egui::TextEdit::singleline(&mut f.s3_bucket).hint_text("my-notes").desired_width(320.0));
    ui.end_row();
    ui.label("Folder in bucket");
    ui.add(egui::TextEdit::singleline(&mut f.s3_prefix).hint_text("optional").desired_width(320.0));
    ui.end_row();
    s3_secret_fields(ui, f);
}

fn s3_secret_fields(ui: &mut Ui, f: &mut SyncForm) {
    ui.label("Access key ID");
    ui.add(egui::TextEdit::singleline(&mut f.s3_access_key).desired_width(320.0));
    ui.end_row();
    ui.label("Secret access key");
    ui.add(egui::TextEdit::singleline(&mut f.s3_secret).password(true).desired_width(320.0));
    ui.end_row();
}

fn dav_fields(ui: &mut Ui, f: &mut SyncForm) {
    ui.label("WebDAV folder");
    ui.add(
        egui::TextEdit::singleline(&mut f.dav_url)
            .hint_text("https://cloud.example.com/remote.php/dav/files/me/Quire")
            .desired_width(320.0),
    );
    ui.end_row();
    dav_secret_fields(ui, f);
}

fn dav_secret_fields(ui: &mut Ui, f: &mut SyncForm) {
    ui.label("User name");
    ui.add(egui::TextEdit::singleline(&mut f.dav_user).desired_width(320.0));
    ui.end_row();
    ui.label("Password");
    ui.add(egui::TextEdit::singleline(&mut f.dav_password).password(true).hint_text("an app password").desired_width(320.0));
    ui.end_row();
}

/// A vault set up with S3 or WebDAV: test the connection, or enter new
/// credentials (they expired, or were rotated).
fn remote_configured(
    ui: &mut Ui,
    f: &mut SyncForm,
    ws: &mut Workspace,
    cfg: &quire_app::vault_sync::SyncConfig,
    profile: &quire_app::vault_sync::RemoteProfile,
    pal: &Palette,
) {
    use quire_app::vault_sync::{RemoteProfile, sync_target_of};
    let s3 = matches!(profile, RemoteProfile::S3 { .. });
    f.poll_test(ui);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(f.test.is_none(), egui::Button::new("Test connection")).clicked() {
            match sync_target_of(cfg) {
                Ok(t) => f.start_test(t),
                Err(e) => f.test_result = Some(Err(e)),
            }
        }
        let label = if f.edit_credentials { "Cancel" } else { "Update credentials…" };
        if ui.button(label).clicked() {
            f.edit_credentials = !f.edit_credentials;
            f.clear_secrets();
        }
    });
    if f.edit_credentials {
        egui::Grid::new("sync-credentials").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            if s3 {
                s3_secret_fields(ui, f);
            } else {
                dav_secret_fields(ui, f);
            }
        });
        let ready = if s3 {
            !f.s3_access_key.trim().is_empty() && !f.s3_secret.trim().is_empty()
        } else {
            !f.dav_user.trim().is_empty() && !f.dav_password.is_empty()
        };
        if ui.add_enabled(ready && f.saving_credentials.is_none(), egui::Button::new("Check and save")).clicked() {
            match ws.sync_set_remote_secret_in_background(f.remote_secret(s3)) {
                Ok(rx) => {
                    f.test_result = None;
                    f.saving_credentials = Some(rx);
                }
                Err(e) => f.test_result = Some(Err(e)),
            }
        }
    }
    if let Some(rx) = &f.saving_credentials {
        match rx.try_recv() {
            Ok(Ok(m)) => {
                f.test_result = Some(Ok(format!("{m} Saved; syncing again.")));
                f.edit_credentials = false;
                f.saving_credentials = None;
                f.clear_secrets();
                ws.start_sync();
            }
            Ok(Err(e)) => {
                f.test_result = Some(Err(e));
                f.saving_credentials = None;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                ui.add(egui::Spinner::new().size(12.0));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => f.saving_credentials = None,
        }
    }
    f.show_test(ui, pal);
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

/// Which versions the history dialog lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HistoryView {
    /// Quire's own history on this device.
    #[default]
    Local,
    /// Every device's, from the sync log.
    AllDevices,
    /// The note's git commits.
    Git,
}

pub(crate) struct HistoryState {
    snapshots: Vec<Snapshot>,
    selected: usize,
    /// (index, text) of the loaded snapshot.
    loaded: Option<(usize, String)>,
    /// Who made each version (read as rows are drawn): `None` inside means
    /// the person typing, or a version from before 0.9.
    provenance: std::collections::HashMap<usize, Option<quire_app::agent::Provenance>>,
    view: HistoryView,
    /// The Git tab's state (the Git panel's drawing code).
    git: crate::git_ui::GitUi,
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
        Self { view: HistoryView::AllDevices, sync: Some(versions), ..Self::new(ws) }
    }

    pub(crate) fn new(ws: &Workspace) -> Self {
        let snapshots = ws.note_history();
        // The newest snapshot is usually the text you're looking at; start
        // on the one before it so the dialog opens on an actual change.
        let current = ws.active_doc().map(|d| d.text().to_string());
        let same = snapshots.first().and_then(|s| ws.snapshot_text(s)) == current;
        let selected = usize::from(same && snapshots.len() > 1);
        Self {
            snapshots,
            selected,
            loaded: None,
            provenance: std::collections::HashMap::new(),
            view: HistoryView::Local,
            git: crate::git_ui::GitUi::default(),
            sync_rx: None,
            sync: None,
            sync_selected: 0,
            diffed: None,
        }
    }
}

/// Returns whether the dialog should close, and a command to run after.
pub(crate) fn history(ctx: &egui::Context, st: &mut HistoryState, ws: &mut Workspace, pal: &Palette) -> (bool, Option<Command>) {
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
    if st.view == HistoryView::AllDevices
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
    let shown: Option<String> = if st.view == HistoryView::AllDevices {
        match &st.sync {
            Some(Ok(v)) => v.get(st.sync_selected).filter(|x| !x.deleted).map(|x| x.text.clone()),
            _ => None,
        }
    } else if st.view == HistoryView::Local {
        st.loaded.as_ref().map(|(_, t)| t.clone())
    } else {
        None
    };
    let git_repo = ws.features.git.repo;
    let mut run: Option<Command> = None;
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
            if syncing || git_repo {
                ui.add_space(12.0);
                ui.selectable_value(&mut st.view, HistoryView::Local, "This device");
                if syncing {
                    ui.selectable_value(&mut st.view, HistoryView::AllDevices, "All devices");
                }
                if git_repo {
                    ui.selectable_value(&mut st.view, HistoryView::Git, "Git");
                }
            }
        });
        let note = match st.view {
            HistoryView::AllDevices => {
                "Every version Quire Sync holds, from every device, including ones that lost a race. Restoring is undoable."
            }
            HistoryView::Local => "Saved automatically in the vault's .quire folder. Restoring is undoable.",
            HistoryView::Git => "The commits that changed this note.",
        };
        ui.label(RichText::new(note).size(12.0).color(pal.muted));
        ui.separator();
        if st.view == HistoryView::Git {
            egui::ScrollArea::vertical().id_salt("hist-git").max_height(body_h).auto_shrink([false, false]).show(ui, |ui| {
                crate::git_ui::history(ui, ws, &mut st.git, pal);
            });
            ui.separator();
            if ui.button("Close").clicked() {
                close = true;
            }
            return;
        }
        if st.view == HistoryView::Local && st.snapshots.is_empty() {
            ui.label(RichText::new("No earlier versions yet. Versions are kept as you edit and save.").color(pal.muted));
        }
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), body_h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.set_height(body_h);
            ui.vertical(|ui| {
                ui.set_width(200.0);
                egui::ScrollArea::vertical().id_salt("hist-list").max_height(body_h).show(ui, |ui| {
                    if st.view == HistoryView::AllDevices {
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
                            let prov = st.provenance.entry(i).or_insert_with(|| ws.snapshot_provenance(s));
                            let who = prov.as_ref().map(provenance_words);
                            let r =
                                crate::theme::row(ui, i == st.selected, if who.is_some() { 40.0 } else { 26.0 }, pal, |painter, rect| {
                                    let left = rect.left() + 8.0;
                                    let y = if who.is_some() { rect.top() + 12.0 } else { rect.center().y };
                                    painter.text(
                                        egui::pos2(left, y),
                                        egui::Align2::LEFT_CENTER,
                                        &label,
                                        egui::FontId::proportional(13.0),
                                        pal.text,
                                    );
                                    if let Some(w) = &who {
                                        let mut job = egui::text::LayoutJob::simple_singleline(
                                            w.clone(),
                                            egui::FontId::proportional(11.5),
                                            pal.muted,
                                        );
                                        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 16.0);
                                        let galley = painter.layout_job(job);
                                        painter.galley(egui::pos2(left, rect.top() + 28.0 - galley.size().y / 2.0), galley, pal.muted);
                                    }
                                });
                            if r.clicked() {
                                st.selected = i;
                            }
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                if st.view == HistoryView::Local
                    && let Some(Some(p)) = st.provenance.get(&st.selected)
                {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(provenance_words(p)).size(12.5).color(pal.text));
                        if p.actor.kind != quire_app::agent::ActorKind::Human || p.accepted_from.is_some() {
                            if p.trust == quire_app::agent::Trust::Untrusted {
                                ui.label(RichText::new("· based on untrusted content").size(12.0).color(pal.danger));
                            }
                            if ui.small_button("Agent activity").on_hover_text("What this agent did, and undo it").clicked() {
                                run = Some(Command::ShowAgentActivity);
                            }
                        }
                    });
                }
                if st.view == HistoryView::AllDevices
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
                if st.view == HistoryView::AllDevices {
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
    (close || run.is_some(), run)
}

/// Who made a version, in a few words.
fn provenance_words(p: &quire_app::agent::Provenance) -> String {
    use quire_app::agent::{ActorKind, Op};
    if let Some(from) = &p.accepted_from {
        return format!("You accepted {}'s suggestion", from.label);
    }
    match (p.actor.kind, p.op) {
        (_, Op::Revert) => "An agent's change, undone".into(),
        (ActorKind::Agent, _) => format!("Written by {}", p.actor.label),
        (ActorKind::Import, _) => format!("Imported ({})", p.actor.label),
        (ActorKind::Job, _) => format!("Quire ({})", p.actor.label),
        (ActorKind::Human, _) => "You".into(),
    }
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
        "Ask questions of your notes with cited answers, offline. Settings → AI is where you choose a provider, or no AI. The shortcut sheet lists everything else.",
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

impl DevicesPanel {
    /// For layout checks: the list (0), removing a device (1), the upgrade warning (2).
    #[cfg(test)]
    fn sample(step: usize) -> Self {
        use quire_app::vault_sync::{SyncDevice, SyncDevices, UpgradeInfo};
        let dev = |id: &str, label: &str, quire: Option<&str>, this: bool, removed: bool| SyncDevice {
            id: id.repeat(32 / id.len()),
            label: label.into(),
            quire: quire.map(str::to_string),
            this,
            removed,
            has_key: quire.is_some(),
        };
        let devices = vec![
            dev("1a2b", "Asha's laptop (1a2b1a2b)", Some("0.13.0"), true, false),
            dev("9f8e", "The very long name of the old family desktop in the study (9f8e9f8e)", None, false, false),
            dev("c0de", "Phone (c0dec0de)", Some("0.13.0"), false, false),
            dev("dead", "Lost laptop (deaddead)", Some("0.13.0"), false, true),
        ];
        let mut p = Self { sample: true, list: Some(Ok(SyncDevices { devices, upgraded: step != 2, key_epoch: 1 })), ..Default::default() };
        match step {
            1 => p.removing = Some(("c0de".repeat(8), "Phone (c0dec0de)".into())),
            2 => {
                p.upgrade = Some(Ok(UpgradeInfo {
                    upgraded: false,
                    old_devices: vec!["The very long name of the old family desktop in the study (9f8e9f8e)".into()],
                }));
            }
            _ => {}
        }
        p
    }
}

/// Repaint until `rx` answers; then its value.
fn poll_rx<T>(ui: &Ui, rx: &mut Option<Receiver<T>>) -> Option<T> {
    let r = rx.as_ref()?.try_recv();
    match r {
        Ok(v) => {
            *rx = None;
            Some(v)
        }
        Err(crossbeam_channel::TryRecvError::Empty) => {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
            None
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            *rx = None;
            None
        }
    }
}

/// Settings → Sync → Devices and sync format (0.13): the vault's devices,
/// "Remove this device", "Upgrade sync format…" and "Compact now".
fn sync_devices(ui: &mut Ui, p: &mut DevicesPanel, ws: &Workspace, compacts_here: bool, pal: &Palette) {
    if p.list.is_none() && p.loading.is_none() && !p.sample {
        match ws.sync_devices_in_background() {
            Ok(rx) => p.loading = Some(rx),
            Err(e) => p.list = Some(Err(e)),
        }
    }
    if let Some(r) = poll_rx(ui, &mut p.loading) {
        p.list = Some(r);
    }
    if let Some(r) = poll_rx(ui, &mut p.removal) {
        p.removed = Some(r);
        p.removing = None;
        p.list = None;
    }
    if let Some(r) = poll_rx(ui, &mut p.upgrade_info) {
        p.upgrade = Some(r);
    }
    if let Some(r) = poll_rx(ui, &mut p.upgrading) {
        p.upgrade = r.err().map(Err);
        p.list = None;
    }
    let small = |t: &str| RichText::new(t.to_string()).size(12.0).color(pal.muted);
    let upgraded = p.list.as_ref().and_then(|l| l.as_ref().ok()).is_some_and(|l| l.upgraded);
    match &p.list {
        None => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(small("Looking up the vault's devices…"));
            });
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
        }
        Some(Ok(list)) => {
            let mut clicked = None;
            for d in &list.devices {
                ui.horizontal(|ui| {
                    // The name and notes wrap in what's left beside the button.
                    let w = (ui.available_width() - 160.0).max(160.0);
                    ui.vertical(|ui| {
                        ui.set_width(w);
                        ui.add(egui::Label::new(RichText::new(&d.label).strong()).wrap());
                        let mut notes = Vec::new();
                        if d.this {
                            notes.push("this device".to_string());
                        }
                        if d.removed {
                            notes.push("removed".into());
                        }
                        notes.push(
                            d.quire
                                .as_ref()
                                .map_or_else(|| "Quire 0.12 or older, or not synced since updating".into(), |v| format!("Quire {v}")),
                        );
                        ui.add(egui::Label::new(small(&notes.join(" · "))).wrap());
                    });
                    if !d.this && !d.removed {
                        let b = ui
                            .add_enabled(
                                upgraded && p.removal.is_none(),
                                egui::Button::new(RichText::new("Remove this device").color(pal.danger)),
                            )
                            .on_disabled_hover_text("Upgrade the sync format first.");
                        if b.clicked() {
                            clicked = Some((d.id.clone(), d.label.clone()));
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if let Some(c) = clicked {
                p.removing = Some(c);
                p.removed = None;
                p.pass.clear();
                p.pass2.clear();
            }
        }
    }
    if let Some((id, label)) = p.removing.clone() {
        ui.add_space(6.0);
        egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(format!("Remove “{label}”?")).strong()).wrap());
            ui.add(
                egui::Label::new(small(
                    "The other devices get a new vault key at their next sync. The removed device keeps the notes it has, \
                     but can't read anything written from now on, and what it writes is ignored. \
                     Devices that join later use the passphrase below: choose a new one if whoever has that device knows the current one.",
                ))
                .wrap(),
            );
            ui.add(
                egui::TextEdit::singleline(&mut p.pass).password(true).hint_text("Passphrase (8 characters or more)").desired_width(280.0),
            );
            ui.add(egui::TextEdit::singleline(&mut p.pass2).password(true).hint_text("Again").desired_width(280.0));
            let ok = p.pass.chars().count() >= 8 && p.pass == p.pass2;
            ui.horizontal(|ui| {
                if ui.add_enabled(ok && p.removal.is_none(), egui::Button::new(RichText::new("Remove").color(pal.danger))).clicked() {
                    match ws.sync_remove_device_in_background(id.clone(), std::mem::take(&mut p.pass)) {
                        Ok(rx) => p.removal = Some(rx),
                        Err(e) => p.removed = Some(Err(e)),
                    }
                    p.pass2.clear();
                }
                if ui.button("Cancel").clicked() {
                    p.removing = None;
                    p.pass.clear();
                    p.pass2.clear();
                }
                if p.removal.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        });
    }
    match &p.removed {
        Some(Ok(r)) => {
            ui.add_space(6.0);
            ui.label(RichText::new("Removed. The vault's new recovery key:").strong());
            egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&r.recovery_key).monospace().color(pal.strong)).wrap());
            });
            ui.add(egui::Label::new(small("Save it: the old recovery key opens only what was written before.")).wrap());
            if !r.left_out.is_empty() {
                ui.add(egui::Label::new(small(&format!("Not given the new key (they must join again): {}", r.left_out.join(", ")))).wrap());
            }
            ui.add(egui::Label::new(small(&r.advice)).wrap());
            let mut done = false;
            ui.horizontal(|ui| {
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(r.recovery_key.clone());
                }
                done = ui.button("I've saved it").clicked();
            });
            if done {
                p.removed = None;
            }
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
        }
        None => {}
    }
    ui.add_space(8.0);
    if upgraded {
        ui.add(
            egui::Label::new(small(
                "Sync format: 0.13. Revisions are authenticated, devices can be removed, and old history is compacted.",
            ))
            .wrap(),
        );
        ui.horizontal_wrapped(|ui| {
            if compacts_here {
                if ui.button("Compact now").on_hover_text("Fold old revisions into a snapshot; the last 90 days of history stay.").clicked()
                    && let Err(e) = ws.sync_compact()
                {
                    p.list = Some(Err(e));
                }
            } else {
                ui.add(egui::Label::new(small("The sync server compacts its own history.")).wrap());
            }
        });
    } else if p.list.as_ref().is_some_and(Result::is_ok) {
        ui.add(egui::Label::new(small("Sync format: before 0.13. Upgrading authenticates every revision's header and lets you remove a device and compact old history.")).wrap());
        match &p.upgrade {
            None => {
                if ui.add_enabled(p.upgrade_info.is_none(), egui::Button::new("Upgrade sync format…")).clicked() {
                    match ws.sync_upgrade_info_in_background() {
                        Ok(rx) => p.upgrade_info = Some(rx),
                        Err(e) => p.upgrade = Some(Err(e)),
                    }
                }
            }
            Some(Err(e)) => {
                ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
                if ui.button("Close").clicked() {
                    p.upgrade = None;
                }
            }
            Some(Ok(info)) => {
                let info = info.clone();
                egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new("Every device needs Quire 0.13 afterwards.").strong()).wrap());
                    ui.add(
                        egui::Label::new(small("Devices on Quire 0.12 or older stop syncing and say “update Quire” until they are updated. This can't be undone."))
                            .wrap(),
                    );
                    if !info.old_devices.is_empty() {
                        ui.label(RichText::new("Not on 0.13 yet, as far as this vault knows:").color(pal.warn));
                        for d in &info.old_devices {
                            ui.add(egui::Label::new(small(&format!("• {d}"))).wrap());
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(p.upgrading.is_none(), egui::Button::new("Upgrade")).clicked() {
                            match ws.sync_upgrade_in_background() {
                                Ok(rx) => p.upgrading = Some(rx),
                                Err(e) => p.upgrade = Some(Err(e)),
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            p.upgrade = None;
                        }
                        if p.upgrading.is_some() {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                    });
                });
            }
        }
    }
    if ui.small_button("Refresh").clicked() && !p.sample {
        p.list = None;
    }
}
