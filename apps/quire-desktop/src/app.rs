// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The egui application: wires panels, editor and overlays to the workspace.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Key, KeyboardShortcut, Modifiers};
use egui_commonmark::CommonMarkCache;
use quire_app::workspace::{NoticeLevel, UiRequest};
use quire_app::{COMMANDS, Command, Session, Settings, Theme, Workspace};

use crate::editor::EditorState;
use crate::overlays::{self, Mode, Overlay};
use crate::panels::{self, PanelOut, SidebarState, SidebarTab, WelcomeAction};
use crate::theme::{self, DARK, FontSupport, LIGHT, Palette};
use crate::view::FastEditor;

const SAMPLE: &[(&str, &str)] = &[
    ("Welcome.md", include_str!("../../../sample-vault/Welcome.md")),
    ("Rope Notes.md", include_str!("../../../sample-vault/Rope Notes.md")),
    ("Projects/Quire Roadmap.md", include_str!("../../../sample-vault/Projects/Quire Roadmap.md")),
    ("Projects/Board.md", include_str!("../../../sample-vault/Projects/Board.md")),
    ("Projects/Importers.md", include_str!("../../../sample-vault/Projects/Importers.md")),
    ("Projects/Mobile app.md", include_str!("../../../sample-vault/Projects/Mobile app.md")),
    ("Projects/Version history.md", include_str!("../../../sample-vault/Projects/Version history.md")),
    ("Reading/How Editors Store Text.md", include_str!("../../../sample-vault/Reading/How Editors Store Text.md")),
    ("Daily/2026-11-01.md", include_str!("../../../sample-vault/Daily/2026-11-01.md")),
    ("Templates/Daily.md", include_str!("../../../sample-vault/Templates/Daily.md")),
    ("Templates/Meeting.md", include_str!("../../../sample-vault/Templates/Meeting.md")),
];

pub(crate) struct QuireApp {
    ws: Workspace,
    editor: EditorState,
    fast: FastEditor,
    focus_find: bool,
    sidebar: SidebarState,
    preview_cache: CommonMarkCache,
    overlay: Option<Overlay>,
    fonts: FontSupport,
    applied: Option<(bool, u32)>,
    last_title: String,
    last_version: u64,
}

fn sample_dir() -> PathBuf {
    directories::UserDirs::new()
        .map_or_else(std::env::temp_dir, |u| u.document_dir().unwrap_or_else(|| u.home_dir()).to_path_buf())
        .join("Quire Sample Vault")
}

fn write_sample(dir: &Path) -> std::io::Result<()> {
    for (rel, body) in SAMPLE {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !p.exists() {
            std::fs::write(p, body)?;
        }
    }
    Ok(())
}

impl QuireApp {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>, vault_arg: Option<PathBuf>) -> Self {
        let fonts = theme::install_fonts(&cc.egui_ctx);
        let settings = Settings::load();
        let session = Session::path().map(|p| Session::load_from(&p)).unwrap_or_default();
        let mut ws = Workspace::new(settings);
        let target = vault_arg.or_else(|| ws.settings.last_vault.clone()).filter(|p| p.is_dir());
        if let Some(dir) = target {
            match ws.open_vault(&dir) {
                Ok(()) => {
                    if session.vault.as_deref().map(Path::canonicalize).and_then(Result::ok) == dir.canonicalize().ok() {
                        ws.restore(&session);
                    }
                }
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
        }
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let mut editor = EditorState::default();
        editor.request_focus = true;
        let mut fast = FastEditor::default();
        fast.request_focus = true;
        Self {
            ws,
            editor,
            fast,
            focus_find: false,
            sidebar: SidebarState::default(),
            preview_cache: CommonMarkCache::default(),
            overlay: None,
            fonts,
            applied: None,
            last_title: String::new(),
            last_version: 0,
        }
    }

    fn palette(&self, ctx: &egui::Context) -> &'static Palette {
        let dark = match self.ws.settings.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => ctx.theme() == egui::Theme::Dark,
        };
        if dark { &DARK } else { &LIGHT }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let pal = self.palette(ctx);
        let key = (pal.dark, self.ws.settings.font_size.to_bits());
        if self.applied == Some(key) {
            return;
        }
        self.applied = Some(key);
        ctx.set_visuals_of(if pal.dark { egui::Theme::Dark } else { egui::Theme::Light }, theme::visuals(pal));
        if self.ws.settings.theme != Theme::System {
            ctx.set_theme(if pal.dark { egui::Theme::Dark } else { egui::Theme::Light });
        }
        ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 5.0);
            s.spacing.button_padding = egui::vec2(8.0, 3.0);
        });
    }

    fn classic(&self) -> bool {
        self.ws.settings.editor == "classic"
    }

    fn focus_editor(&mut self) {
        self.editor.request_focus = true;
        self.fast.request_focus = true;
    }

    fn reveal_caret(&mut self) {
        self.editor.scroll_to_caret = true;
        self.fast.scroll_to_caret = true;
    }

    fn run(&mut self, cmd: Command) {
        if let Some(req) = self.ws.run(cmd) {
            self.overlay = Some(match req {
                UiRequest::QuickOpen => Overlay::palette(Mode::Notes),
                UiRequest::CommandPalette => Overlay::palette(Mode::Commands),
                UiRequest::PickTemplate => Overlay::palette(Mode::Templates),
                UiRequest::ShowTasks => Overlay::palette(Mode::Tasks),
                UiRequest::Search => {
                    self.ws.settings.show_sidebar = true;
                    self.ws.focus_mode = false;
                    self.sidebar.tab = SidebarTab::Search;
                    self.sidebar.focus_query = true;
                    return;
                }
                UiRequest::Rename => match self.ws.active_doc() {
                    Some(d) if d.path.is_some() => Overlay::Rename { input: d.title(), focused: false },
                    _ => return,
                },
                UiRequest::ConfirmDelete => Overlay::ConfirmDelete,
                UiRequest::OpenVault => {
                    Overlay::OpenVault { input: self.ws.vault_root().map(|p| p.display().to_string()).unwrap_or_default() }
                }
                UiRequest::Find => {
                    self.focus_find = true;
                    return;
                }
                UiRequest::VaultReplace => {
                    if self.ws.find.vault_query.is_empty() {
                        self.ws.find.vault_query = self.ws.find.query.clone();
                    }
                    Overlay::VaultReplace { focused: false }
                }
                UiRequest::Ask => Overlay::Ask { focused: false },
                UiRequest::Settings => Overlay::Settings(crate::dialogs::SettingsState::default()),
                UiRequest::NewProject => Overlay::NewProject(crate::dialogs::ProjectState::default()),
                UiRequest::Shortcuts => Overlay::Shortcuts { query: String::new(), focused: false },
                UiRequest::Tour => Overlay::Tour { step: 0 },
                UiRequest::SyncConflicts => Overlay::SyncConflicts,
                UiRequest::NewInFolder => Overlay::Folder(crate::dialogs::FolderState::new(&self.ws, crate::dialogs::FolderMode::NewNote)),
                UiRequest::MoveNote => Overlay::Folder(crate::dialogs::FolderState::new(&self.ws, crate::dialogs::FolderMode::Move)),
                UiRequest::History => Overlay::History(crate::dialogs::HistoryState::new(&self.ws)),
                UiRequest::Import => {
                    if self.ws.vault().is_none() {
                        self.ws.notify(NoticeLevel::Info, "Open a vault first, then import into it");
                        return;
                    }
                    Overlay::Import(crate::dialogs::ImportState::default())
                }
                UiRequest::Editor(c) => {
                    if self.classic() {
                        self.ws.notify(NoticeLevel::Info, "Multiple cursors need the fast editor (View: Switch editor)");
                    } else {
                        self.fast.command(c, &mut self.ws);
                    }
                    return;
                }
            });
        } else {
            // Most commands edit or move the caret; keep the editor focused.
            self.focus_editor();
            if matches!(
                cmd,
                Command::NewNote
                    | Command::DailyNote
                    | Command::Scratchpad
                    | Command::FollowLink
                    | Command::NextTab
                    | Command::PrevTab
                    | Command::FindNext
                    | Command::FindPrev
            ) {
                self.reveal_caret();
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.overlay.is_some() {
            return;
        }
        // When a text field outside the note has focus (find bar, properties,
        // search), editing shortcuts like Ctrl+A or Ctrl+Z belong to that field.
        let editor = self.ws.active_doc().map(|d| crate::editor::editor_id(d.id));
        let other_field_focused = ctx.memory(egui::Memory::focused).is_some_and(|f| Some(f) != editor);
        // egui ignores extra Shift/Alt when matching, so try the most specific combos first.
        let mut combos: Vec<(u8, Command, KeyboardShortcut)> = Vec::new();
        for info in COMMANDS {
            if other_field_focused && matches!(info.category, "Edit" | "Format") {
                continue;
            }
            for k in info.keys {
                let Some(key) = Key::from_name(k.key) else {
                    continue;
                };
                let mut m = Modifiers::NONE;
                if k.cmd {
                    m |= Modifiers::COMMAND;
                }
                if k.shift {
                    m |= Modifiers::SHIFT;
                }
                if k.alt {
                    m |= Modifiers::ALT;
                }
                combos.push((u8::from(k.shift) + u8::from(k.alt) + u8::from(k.cmd), info.command, KeyboardShortcut::new(m, key)));
            }
        }
        combos.sort_by_key(|c| std::cmp::Reverse(c.0));
        let mut fired = Vec::new();
        ctx.input_mut(|i| {
            for (_, cmd, sc) in &combos {
                if i.consume_shortcut(sc) {
                    fired.push(*cmd);
                }
            }
        });
        for cmd in fired {
            self.run(cmd);
        }
    }

    fn after_panel(&mut self, out: &PanelOut) {
        if out.skip_update {
            self.ws.skip_update();
        }
        if let Some(c) = out.run {
            self.run(c);
        }
        if out.jumped {
            self.focus_editor();
            self.reveal_caret();
        }
        if out.reveal_only {
            self.reveal_caret();
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.ws.active_doc() {
            Some(d) => format!("{}{} — {} — Quire", d.title(), if d.is_dirty() { " •" } else { "" }, self.ws.vault_name()),
            None => "Quire".into(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

impl eframe::App for QuireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let changed = self.ws.tick(now);
        trace("tick (autosave, watcher, results)", now);
        if changed {
            ctx.request_repaint();
        }
        let v = self.ws.index().version();
        if v != self.last_version {
            self.last_version = v;
            ctx.request_repaint();
        }
        if self.ws.index().status().busy
            || self.ws.assist_pending()
            || self.ws.ask.pending()
            || self.ws.index().semantic_busy()
            || self.ws.updates.checking()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(t) = self.ws.next_deadline() {
            ctx.request_repaint_after(t.saturating_duration_since(now));
        }
        // Poll for outside edits even when idle.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let started = Instant::now();
        self.ui_inner(ui, frame);
        // QUIRE_TRACE=1 prints how long each frame's UI work took.
        if std::env::var_os("QUIRE_TRACE").is_some() {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let lines = self.ws.active_doc().map_or(0, |d| d.buffer().len_lines());
            eprintln!("frame {ms:.2} ms · {lines} lines");
        }
    }
}

impl QuireApp {
    fn ui_inner(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        let pal = self.palette(&ctx);
        self.handle_shortcuts(&ctx);
        if self.overlay.is_none() && self.classic() {
            EditorState::handle_smart_keys(&ctx, &mut self.ws);
        }
        // Escape closes the find bar (unless it's clearing extra cursors).
        let extra_cursors = self.ws.active_doc().is_some_and(|d| self.fast.has_extra_cursors(d.id));
        if self.overlay.is_none() && self.ws.find.open && !extra_cursors && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.ws.find.open = false;
            self.focus_editor();
        }
        // Files dropped on the window become attachments / notes.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            if self.ws.active_doc().is_none() {
                self.ws.new_note(None);
            }
            self.ws.import_files(&dropped);
            self.focus_editor();
        }

        if self.ws.vault().is_none() {
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(pal.bg)).show(ui, |ui| match panels::welcome(ui, pal) {
                WelcomeAction::Browse => self.overlay = Some(Overlay::OpenVault { input: String::new() }),
                WelcomeAction::Sample => {
                    let dir = sample_dir();
                    match write_sample(&dir).map_err(|e| e.to_string()).and_then(|()| self.ws.open_vault(&dir)) {
                        Ok(()) => {
                            let welcome = quire_app::vault::NotePath::new("Welcome.md");
                            self.ws.open_note(&welcome);
                            self.focus_editor();
                        }
                        Err(e) => self.ws.notify(NoticeLevel::Error, e),
                    }
                }
                WelcomeAction::None => {}
            });
        } else {
            let focus = self.ws.focus_mode;
            if !focus {
                let out = egui::Panel::top("quire-tabs")
                    .frame(egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 6)))
                    .show(ui, |ui| panels::tab_bar(ui, &mut self.ws, pal))
                    .inner;
                self.after_panel(&out);
            }
            let status = egui::Panel::bottom("quire-status")
                .frame(egui::Frame::NONE.fill(if focus { pal.bg } else { pal.panel }).inner_margin(egui::Margin::symmetric(12, 4)))
                .show(ui, |ui| panels::status_bar(ui, &self.ws, pal))
                .inner;
            self.after_panel(&status);
            if self.ws.settings.show_sidebar && !focus {
                let out = egui::Panel::left("quire-sidebar")
                    .resizable(true)
                    .default_size(250.0)
                    .size_range(170.0..=480.0)
                    .frame(egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 4)))
                    .show(ui, |ui| panels::sidebar(ui, &mut self.ws, &mut self.sidebar, pal))
                    .inner;
                self.after_panel(&out);
            }
            if self.ws.settings.show_insights && !focus {
                let out = egui::Panel::right("quire-insights")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(220.0..=520.0)
                    .frame(egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(12, 4)))
                    .show(ui, |ui| {
                        let t = Instant::now();
                        let out = panels::insights(ui, &mut self.ws, pal);
                        trace("insights", t);
                        out
                    })
                    .inner;
                self.after_panel(&out);
            }
            let show_preview = self.ws.settings.show_preview && !focus;
            if show_preview && self.ws.active_doc().is_some() {
                let out = egui::Panel::right("quire-preview")
                    .resizable(true)
                    .default_size(ui.available_width() / 2.0)
                    .frame(egui::Frame::NONE.fill(pal.bg).stroke(egui::Stroke::new(1.0, pal.border)))
                    .show(ui, |ui| panels::preview(ui, &mut self.ws, &mut self.preview_cache, pal))
                    .inner;
                self.after_panel(&out);
            }
            if self.ws.active_doc().is_some_and(|d| d.pending_merge.is_some()) {
                let out = egui::Panel::top("quire-merge")
                    .frame(egui::Frame::NONE.fill(pal.mark_bg).inner_margin(egui::Margin::symmetric(12, 8)))
                    .show(ui, |ui| panels::merge_banner(ui, &mut self.ws, pal))
                    .inner;
                self.after_panel(&out);
            }
            if self.ws.find.open && self.ws.active_doc().is_some() && !focus {
                let out = egui::Panel::top("quire-find")
                    .frame(
                        egui::Frame::NONE
                            .fill(pal.panel)
                            .inner_margin(egui::Margin::symmetric(12, 6))
                            .stroke(egui::Stroke::new(1.0, pal.border)),
                    )
                    .show(ui, |ui| panels::find_bar(ui, &mut self.ws, &mut self.focus_find, pal))
                    .inner;
                self.after_panel(&out);
            }
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(pal.bg)).show(ui, |ui| {
                if self.ws.active_doc().is_some() {
                    if self.classic() {
                        self.editor.show(ui, &mut self.ws, pal, self.fonts);
                    } else {
                        let t = Instant::now();
                        self.fast.show(ui, &mut self.ws, pal, self.fonts);
                        trace("editor", t);
                    }
                    if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                        let r = ui.max_rect().shrink(24.0);
                        ui.painter().rect_filled(r, 12.0, pal.selection.gamma_multiply(0.5));
                        ui.painter().text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            "Drop to add to this note",
                            egui::FontId::proportional(22.0),
                            pal.strong,
                        );
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.3);
                        ui.label(egui::RichText::new("No note open").size(22.0).color(pal.muted));
                        ui.label(
                            egui::RichText::new("Ctrl+N new note  ·  Ctrl+P quick open  ·  Alt+D today  ·  Ctrl+K commands")
                                .color(pal.faint),
                        );
                    });
                }
            });
        }

        // First run: a short tour once there's a vault to look at.
        if self.overlay.is_none() && !self.ws.settings.tour_done && self.ws.vault().is_some() {
            self.overlay = Some(Overlay::Tour { step: 0 });
        }
        if let Some(ov) = self.overlay.as_mut() {
            let was_tour = matches!(ov, Overlay::Tour { .. });
            let res = overlays::show(&ctx, ov, &mut self.ws, pal);
            // However the tour closes (finish, skip, Escape), don't show it again.
            if res.close && was_tour && !self.ws.settings.tour_done {
                self.ws.settings.tour_done = true;
                let _ = self.ws.settings.save();
            }
            if res.close {
                self.overlay = None;
                self.focus_editor();
            }
            if res.jumped {
                self.reveal_caret();
            }
            if let Some(c) = res.run {
                self.run(c);
            }
        }
        overlays::toasts(&ctx, &self.ws, pal);
        let ids: Vec<_> = self.ws.docs().iter().map(|d| d.id).collect();
        self.editor.forget(&ids);
        self.fast.forget(&ids);
        self.update_title(&ctx);
    }
}

/// With `QUIRE_TRACE` set, report sections slower than 8 ms.
fn trace(what: &str, since: Instant) {
    let ms = since.elapsed().as_secs_f64() * 1000.0;
    if ms > 8.0 && std::env::var_os("QUIRE_TRACE").is_some() {
        eprintln!("  {what} {ms:.2} ms");
    }
}

impl Drop for QuireApp {
    fn drop(&mut self) {
        self.ws.shutdown();
    }
}
