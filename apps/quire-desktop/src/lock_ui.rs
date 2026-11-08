// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Locked folders in the app (docs/design/0.11-secure.md): the dialogs to
//! lock, unlock, change the passphrase and stop locking a folder; the
//! recovery key, shown once; the lock screen a locked note opens to (a
//! surface for `.qenc` files); and the bits the sidebar and search show.

use egui::{RichText, TextEdit};
use quire_app::vault::NotePath;
use quire_app::vault::lock;
use quire_app::{Command, Workspace};

use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;

const WHAT_IT_PROTECTS: &str = "Notes and files in the folder are stored encrypted. Someone with this disk, your cloud folder or a backup sees only ciphertext. \
While the folder is unlocked, anyone using this computer can read it. File names stay visible. Agents never see a locked folder.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lock,
    Unlock,
    Change,
    Stop,
    /// Show the recovery key again (asks for the passphrase).
    ShowKey,
}

/// The dialog for every locked-folder action.
pub(crate) struct LockDialog {
    mode: Mode,
    folder: String,
    pass: String,
    pass2: String,
    current: String,
    remember: bool,
    use_recovery: bool,
    recovery: String,
    error: Option<String>,
    started: bool,
    focused: bool,
    /// After locking: (folder, recovery key), until the person confirms.
    shown_key: Option<(String, String)>,
    saved: bool,
    /// The key is shown again on request, not right after locking.
    shown_again: bool,
}

impl LockDialog {
    fn new(mode: Mode, folder: String) -> Self {
        Self {
            mode,
            folder,
            pass: String::new(),
            pass2: String::new(),
            current: String::new(),
            remember: false,
            use_recovery: false,
            recovery: String::new(),
            error: None,
            started: false,
            focused: false,
            shown_key: None,
            saved: false,
            shown_again: false,
        }
    }

    /// The folder to start with: the active note's, or the first that fits.
    fn initial(ws: &Workspace, locked: bool) -> String {
        let here = ws.active_doc().and_then(|d| d.path.clone()).map(|p| p.folder().to_string()).filter(|f| !f.is_empty());
        if locked {
            let folders = ws.locked_folders();
            here.and_then(|h| folders.iter().find(|f| h == f.folder.path || h.starts_with(&format!("{}/", f.folder.path))))
                .or_else(|| folders.first())
                .map(|f| f.folder.path.clone())
                .unwrap_or_default()
        } else {
            here.unwrap_or_default()
        }
    }

    pub(crate) fn make_lock(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let mut d = Self::new(Mode::Lock, Self::initial(ws, false));
        if let Some(k) = ws.take_recovery_key() {
            d.shown_key = Some(k);
        }
        Box::new(d)
    }

    pub(crate) fn make_unlock(ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self::new(Mode::Unlock, Self::initial(ws, true)))
    }

    pub(crate) fn make_change(ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self::new(Mode::Change, Self::initial(ws, true)))
    }

    pub(crate) fn make_stop(ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self::new(Mode::Stop, Self::initial(ws, true)))
    }

    pub(crate) fn make_show_key(ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self::new(Mode::ShowKey, Self::initial(ws, true)))
    }

    #[cfg(test)]
    pub(crate) fn sample(i: usize) -> Self {
        let modes = [Mode::Lock, Mode::Unlock, Mode::Change, Mode::Stop, Mode::Lock, Mode::ShowKey, Mode::Unlock];
        let mut d = Self::new(modes[i % modes.len()], "Journal".into());
        if i == 4 {
            d.shown_key = Some(("Journal".into(), "58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E".into()));
        }
        if i == 1 {
            d.error = Some("Wrong passphrase.".into());
        }
        if i == 6 {
            d.use_recovery = true;
        }
        d
    }

    fn title(&self) -> &'static str {
        match self.mode {
            Mode::Lock => "Lock a folder",
            Mode::Unlock => "Unlock a folder",
            Mode::Change => "Change a locked folder's passphrase",
            Mode::Stop => "Stop locking a folder",
            Mode::ShowKey => "Show a locked folder's recovery key",
        }
    }

    fn folder_picker(&mut self, ui: &mut egui::Ui, ws: &Workspace, w: f32) {
        let choices: Vec<String> = if self.mode == Mode::Lock {
            let locked: Vec<String> = ws.locked_folders().into_iter().map(|f| f.folder.path).collect();
            let mut v: Vec<String> = ws.vault().map(quire_app::vault::Vault::folders).unwrap_or_default();
            // Top-level folders too (a folder holding only subfolders).
            let tops: Vec<String> = v.iter().filter_map(|f| f.split('/').next().map(str::to_string)).collect();
            v.extend(tops);
            v.retain(|f| !locked.iter().any(|l| f == l || f.starts_with(&format!("{l}/"))));
            // Locked folders with plain files left: running it again finishes.
            if let Some(vault) = ws.vault() {
                v.extend(lock::plain_left(vault).into_iter().map(|p| p.folder));
            }
            v.sort_by_key(|f| f.to_lowercase());
            v.dedup();
            v
        } else {
            ws.locked_folders().into_iter().map(|f| f.folder.path).collect()
        };
        if choices.is_empty() {
            let msg = if self.mode == Mode::Lock {
                "No folders yet: make a folder with a note in it first."
            } else {
                "This vault has no locked folders."
            };
            ui.label(msg);
            return;
        }
        if !choices.contains(&self.folder) {
            self.folder.clone_from(&choices[0]);
        }
        egui::ComboBox::from_id_salt("quire-lock-folder").width(w.min(320.0)).selected_text(format!("{}/", self.folder)).show_ui(
            ui,
            |ui| {
                for c in &choices {
                    ui.selectable_value(&mut self.folder, c.clone(), format!("{c}/"));
                }
            },
        );
    }

    fn remember_box(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        if quire_app::secure_state::system_keychain() {
            ui.checkbox(&mut self.remember, "Unlock automatically on this computer").on_hover_text(
                "Keeps the folder's key in the system keychain. Anyone who can sign in to this computer account can then open it.",
            );
        } else {
            ui.label(
                RichText::new("This computer has no system keychain Quire can use, so the folder can't unlock by itself.")
                    .size(11.5)
                    .color(pal.faint),
            );
        }
    }

    fn recovery_view(&mut self, ui: &mut egui::Ui, w: f32, pal: &Palette, res: &mut OverlayResult) {
        let Some((folder, key)) = self.shown_key.clone() else { return };
        let text = if self.shown_again {
            ui.heading(format!("{folder}/: recovery key"));
            "It opens the folder if you forget the passphrase, or if its key file is lost. Anyone who has it can read the folder: store it somewhere safe, away from this computer."
        } else {
            ui.heading(format!("{folder}/ is locked"));
            "This is its recovery key. It opens the folder if you forget the passphrase, or if its key file is lost. Store it somewhere safe, away from this computer. \
             (While the folder is unlocked, “Show a locked folder's recovery key…” shows it again, after asking for the passphrase.)"
        };
        ui.add(egui::Label::new(RichText::new(text).color(pal.muted)).wrap());
        ui.add_space(8.0);
        egui::Frame::new().fill(pal.code_bg).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(w - 24.0);
            ui.add(egui::Label::new(RichText::new(&key).monospace().size(14.0).color(pal.strong)).wrap());
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(key.clone());
            }
            ui.checkbox(&mut self.saved, "I've stored the recovery key");
        });
        ui.add_space(8.0);
        if ui.add_enabled(self.saved, egui::Button::new("Done")).clicked() {
            self.shown_key = None;
            res.close = true;
        }
    }
}

impl OverlayView for LockDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        // A conversion this dialog started has finished.
        if self.started && ws.lock_job().is_none() {
            self.started = false;
            if let Some(k) = ws.take_recovery_key() {
                self.shown_key = Some(k);
            } else if self.mode != Mode::Lock || (ws.is_unlocked(&self.folder) && conflicts(ws, &self.folder) == 0) {
                res.close = true;
                return res;
            } else if conflicts(ws, &self.folder) == 0 {
                self.error = Some("The conversion stopped; see the message below the editor. Run it again to finish.".into());
            }
        }
        if self.started {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        egui::Modal::new(egui::Id::unique("quire-lock-dialog")).show(ctx, |ui| {
            let w = 500.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            if self.shown_key.is_some() {
                self.recovery_view(ui, w, pal, &mut res);
                return;
            }
            ui.heading(self.title());
            if self.mode == Mode::Lock {
                ui.add(egui::Label::new(RichText::new(WHAT_IT_PROTECTS).color(pal.muted)).wrap());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Folder");
                self.folder_picker(ui, ws, w);
            });
            ui.add_space(6.0);
            let mut go = false;
            let field = |ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str| -> egui::Response {
                ui.label(label);
                ui.add(TextEdit::singleline(text).password(true).hint_text(hint).desired_width(w.min(360.0)))
            };
            let first = match self.mode {
                Mode::Lock => {
                    let r = field(ui, "New passphrase (8 characters or more)", &mut self.pass, "");
                    let r2 = field(ui, "Type it again", &mut self.pass2, "");
                    go |= r2.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    self.remember_box(ui, pal);
                    r
                }
                Mode::Unlock if self.use_recovery => {
                    ui.label("Recovery key");
                    let r = ui.add(TextEdit::singleline(&mut self.recovery).hint_text("XXXX-XXXX-…").desired_width(w.min(420.0)));
                    if key_missing(ws, &self.folder) {
                        ui.add(
                            egui::Label::new(
                                RichText::new("The folder's key file is missing: a new passphrase makes a new one, once the recovery key has opened the folder's files.")
                                    .size(11.5)
                                    .color(pal.warn),
                            )
                            .wrap(),
                        );
                        field(ui, "New passphrase (8 characters or more)", &mut self.pass, "");
                    } else {
                        field(ui, "New passphrase (optional)", &mut self.pass, "keep the current one");
                    }
                    if !self.pass.is_empty() {
                        field(ui, "Type it again", &mut self.pass2, "");
                    }
                    r
                }
                Mode::Unlock | Mode::Stop => {
                    let r = field(ui, "Passphrase", &mut self.pass, "");
                    go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if self.mode == Mode::Unlock {
                        self.remember_box(ui, pal);
                        if ui.link("Forgot it? Use the recovery key").clicked() {
                            self.use_recovery = true;
                        }
                    } else {
                        ui.add(
                            egui::Label::new(
                                RichText::new("Its notes are decrypted back to plain files. The encrypted copies go to the trash.")
                                    .color(pal.muted),
                            )
                            .wrap(),
                        );
                    }
                    r
                }
                Mode::Change => {
                    let r = field(ui, "Current passphrase or recovery key", &mut self.current, "");
                    field(ui, "New passphrase (8 characters or more)", &mut self.pass, "");
                    let r2 = field(ui, "Type it again", &mut self.pass2, "");
                    go |= r2.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add(egui::Label::new(RichText::new(OLD_KEY_FILES).size(11.5).color(pal.faint)).wrap());
                    r
                }
                Mode::ShowKey => {
                    let r = field(ui, "Passphrase", &mut self.pass, "");
                    go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add(
                        egui::Label::new(RichText::new("Asked again so that nobody at an unlocked computer can take the key away.").size(11.5).color(pal.faint))
                            .wrap(),
                    );
                    r
                }
            };
            if self.mode == Mode::Lock && !self.started {
                self.conflicts_view(ui, ws, pal);
            }
            if !self.focused {
                first.request_focus();
                self.focused = true;
            }
            if let Some(e) = &self.error {
                ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if self.started {
                    ui.spinner();
                    ui.label(ws.lock_job().unwrap_or("Working…").to_string());
                    return;
                }
                let label = match self.mode {
                    Mode::Lock => "Lock folder",
                    Mode::Unlock => "Unlock",
                    Mode::Change => "Change passphrase",
                    Mode::Stop => "Decrypt folder",
                    Mode::ShowKey => "Show recovery key",
                };
                if (ui.button(label).clicked() || go) && !self.folder.is_empty() {
                    self.error = self.act(ws).err();
                    if self.error.is_none() && !self.started && self.shown_key.is_none() {
                        res.close = true;
                    }
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
        });
        res
    }
}

impl LockDialog {
    fn act(&mut self, ws: &mut Workspace) -> Result<(), String> {
        let folder = self.folder.clone();
        match self.mode {
            Mode::Lock => {
                if self.pass != self.pass2 {
                    return Err("The passphrases differ.".into());
                }
                if self.pass.chars().count() < lock::MIN_PASSPHRASE && !ws.locked_folders().iter().any(|f| f.folder.path == folder) {
                    return Err("The passphrase must be at least 8 characters.".into());
                }
                ws.start_locking(&folder, &self.pass, self.remember)?;
                self.started = true;
            }
            Mode::Unlock if self.use_recovery => {
                if !self.pass.is_empty() && self.pass != self.pass2 {
                    return Err("The new passphrases differ.".into());
                }
                if self.pass.is_empty() && key_missing(ws, &folder) {
                    return Err("The key file is missing: choose a new passphrase to make a new one.".into());
                }
                let new = Some(self.pass.as_str()).filter(|p| !p.is_empty());
                ws.unlock_with_recovery_key(&folder, &self.recovery, new)?;
            }
            Mode::Unlock => ws.unlock_folder(&folder, &self.pass, self.remember)?,
            Mode::Change => {
                if self.pass != self.pass2 {
                    return Err("The new passphrases differ.".into());
                }
                ws.change_folder_passphrase(&folder, &self.current, &self.pass)?;
            }
            Mode::Stop => {
                let pass = Some(self.pass.as_str()).filter(|p| !p.is_empty());
                ws.start_unlocking_for_good(&folder, pass)?;
                self.started = true;
            }
            Mode::ShowKey => {
                let key = ws.folder_recovery_key(&folder, &self.pass)?;
                self.pass.clear();
                self.shown_key = Some((folder, key));
                self.shown_again = true;
            }
        }
        Ok(())
    }

    /// Plain files next to their encrypted versions in the chosen folder:
    /// both are kept, the plain one encrypted under a new name.
    fn conflicts_view(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
        let n = conflicts(ws, &self.folder);
        if n == 0 {
            return;
        }
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "{n} plain file{} here sit{} next to an encrypted version with other content. Quire shows the encrypted one and never saves over either. \
                     Keep both: each plain one is encrypted under a new name (“… (plain copy)”), for you to compare.",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "s" } else { "" }
                ))
                .size(12.0)
                .color(pal.warn),
            )
            .wrap(),
        );
        let unlocked = ws.is_unlocked(&self.folder);
        if ui.add_enabled(unlocked, egui::Button::new("Keep both")).on_disabled_hover_text("Unlock the folder first.").clicked()
            && let Err(e) = ws.keep_both_copies(&self.folder)
        {
            self.error = Some(e);
        }
    }
}

/// What "Change passphrase" doesn't change (security review S-M8).
const OLD_KEY_FILES: &str = "Only the key file changes: the folder's key stays the same, so the recovery key keeps working, and so does an old copy of the key file \
(in a backup, git history or a cloud folder's version history) with the old passphrase. If the passphrase leaked, also choose “Change the folder's key” below.";

/// How many plain files sit next to an encrypted version in `folder`.
fn conflicts(ws: &Workspace, folder: &str) -> usize {
    ws.vault().map_or(0, |v| lock::plain_left(v).into_iter().filter(|l| l.folder == folder).map(|l| l.conflicts.len()).sum())
}

/// Whether `folder` is remembered as locked but its key file is missing.
fn key_missing(ws: &Workspace, folder: &str) -> bool {
    ws.locked_folders().iter().any(|f| f.folder.path == folder && f.key_missing)
}

/// What a `.qenc` file opens to: the folder's lock screen while it is
/// locked; the note itself once it is unlocked.
pub(crate) struct LockedSurface {
    path: NotePath,
    pass: String,
    remember: bool,
    error: Option<String>,
    focused: bool,
    done: bool,
}

impl LockedSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self { path: path.clone(), pass: String::new(), remember: false, error: None, focused: false, done: false }
    }
}

impl Surface for LockedSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn title(&self) -> String {
        format!("🔒 {}", lock::plain_name(&self.path).map_or_else(|| self.path.file_name().to_string(), |p| p.stem().to_string()))
    }

    fn done(&self) -> bool {
        self.done
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let mut out = PanelOut::default();
        let plain = lock::plain_name(&self.path).unwrap_or_else(|| self.path.clone());
        let Some(folder) = ws.locked_folder_of(&plain).map(|f| f.path) else {
            ui.label(RichText::new("This encrypted file isn't in a locked folder Quire knows (its key file is missing).").color(pal.muted));
            return out;
        };
        if ws.is_unlocked(&folder) {
            if plain.is_markdown() {
                if ws.open_note(&plain).is_some() {
                    out.jumped = true;
                }
                self.done = true;
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.3);
                    ui.label(RichText::new(format!("🔓 {}", plain.file_name())).size(20.0).color(pal.strong));
                    ui.label(RichText::new("An encrypted file. Quire doesn't show encrypted pictures and files yet.").color(pal.muted));
                });
            }
            return out;
        }
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.22);
            ui.label(RichText::new("🔒").size(40.0));
            ui.label(RichText::new("Locked").size(22.0).color(pal.strong));
            ui.label(RichText::new(format!("{} is in {folder}/, a locked folder.", plain.stem())).color(pal.muted));
            ui.add_space(12.0);
            let r = ui.add(TextEdit::singleline(&mut self.pass).password(true).hint_text("Passphrase").desired_width(260.0));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if quire_app::secure_state::system_keychain() {
                ui.checkbox(&mut self.remember, "Unlock automatically on this computer");
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(pal.danger));
            }
            ui.add_space(6.0);
            if ui.button("Unlock").clicked() || enter {
                match ws.unlock_folder(&folder, &self.pass, self.remember) {
                    Ok(()) => {
                        self.pass.clear();
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.link("Use the recovery key…").clicked() {
                out.run = Some(Command::UnlockFolder);
            }
        });
        out
    }
}

/// A locked folder's badge in the Files tab: 🔒 locked, 🔓 unlocked.
pub(crate) fn folder_badge(ws: &Workspace, folder: &str) -> Option<&'static str> {
    ws.is_locked_folder(folder).then(|| if ws.is_unlocked(folder) { "🔓" } else { "🔒" })
}

/// Search results from unlocked locked folders, and a line for each folder
/// that is locked (not searched). Returns a note to open.
pub(crate) fn search_section(ui: &mut egui::Ui, ws: &Workspace, query: &str, pal: &Palette) -> Option<NotePath> {
    if query.trim().is_empty() {
        return None;
    }
    let mut open = None;
    let hits = ws.locked_search(query, 20);
    for h in &hits {
        let r = ui
            .vertical(|ui| {
                let t = ui
                    .add(egui::Label::new(RichText::new(format!("🔓 {}", h.title)).strong().color(pal.strong)).sense(egui::Sense::click()));
                ui.add(egui::Label::new(RichText::new(&h.snippet).color(pal.muted)).wrap());
                ui.add_space(6.0);
                t
            })
            .inner;
        if r.clicked() {
            open = Some(h.path.clone());
        }
    }
    for f in ws.locked_folders().into_iter().filter(|f| !f.unlocked) {
        ui.label(RichText::new(format!("🔒 {}/ is locked and wasn't searched", f.folder.path)).size(11.5).color(pal.faint));
    }
    open
}

/// Notes of unlocked locked folders for quick open: (label, folder, path).
pub(crate) fn quick_open(ws: &Workspace, query: &str) -> Vec<(String, String, NotePath)> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    ws.locked_quick_open(query, 10).into_iter().map(|h| (format!("🔓 {}", h.path.stem()), h.path.folder().to_string(), h.path)).collect()
}

/// Whether quick open should list this file (a `.qenc` of an unlocked
/// folder is listed as its note instead).
pub(crate) fn list_file(ws: &Workspace, p: &NotePath) -> bool {
    !(lock::is_encrypted_name(p.as_str()) && ws.locked_folder_cached(p).is_some_and(|f| ws.is_unlocked(&f)))
}

/// Asked when the window is closed while notes in locked folders have
/// edits that can't be saved (their folder was locked, here or on another
/// device, and Quire has no key): unlock to save them, or quit without them.
pub(crate) struct QuitDialog {
    notes: Vec<String>,
}

impl QuitDialog {
    pub(crate) const fn new(notes: Vec<String>) -> Self {
        Self { notes }
    }
}

impl OverlayView for QuitDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(egui::Id::unique("quire-quit-locked")).show(ctx, |ui| {
            ui.set_width(460.0_f32.min(ctx.content_rect().width() - 48.0));
            ui.heading("Edits that can't be saved");
            let n = self.notes.len();
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{n} note{} in a locked folder {} edits that weren't saved: the folder was locked, and Quire has no key to encrypt them. \
                         Unlock the folder to save them. Quitting now loses them (nothing is written in plain text).",
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "has" } else { "have" }
                    ))
                    .color(pal.muted),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            for t in self.notes.iter().take(8) {
                ui.label(RichText::new(format!("• {t}")).color(pal.strong));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Unlock…").strong()).clicked() {
                    res.close = true;
                    res.run = Some(Command::UnlockFolder);
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
                if ui.button(RichText::new("Quit without them").color(pal.danger)).clicked() {
                    ws.confirm_quit();
                    res.close = true;
                }
            });
        });
        res
    }
}
