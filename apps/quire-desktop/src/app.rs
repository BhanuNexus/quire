// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The egui application: wires panels, editor and overlays to the workspace.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Key, KeyboardShortcut, Modifiers};
use egui_commonmark::CommonMarkCache;
use quire_app::keymap::{Action, Keymap};
use quire_app::vault::NotePath;
use quire_app::workspace::{NoticeLevel, UiRequest};
use quire_app::{Command, DocId, Session, Settings, Theme, Workspace};

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
    #[cfg(feature = "bench")]
    bench: Option<crate::bench::Run>,
    /// Set by the first frame; the vault is read from the second on.
    first_frame_shown: bool,
    ws: Workspace,
    editor: EditorState,
    fast: FastEditor,
    focus_find: bool,
    /// Text a command wants on the clipboard (copied at the end of the frame).
    clipboard: Option<String>,
    /// HTML a command wants on the clipboard ("Copy as HTML").
    clipboard_html: Option<quire_app::copy_html::CopiedHtml>,
    /// The system clipboard for HTML, made on first use and kept: on Linux
    /// the copy lasts only while it exists.
    html_board: Option<arboard::Clipboard>,
    /// The global quick-capture shortcut while it's registered; `capture_key_failed`
    /// stops retrying after it couldn't be.
    capture_key: Option<crate::capture_key::CaptureKey>,
    capture_key_failed: bool,
    sidebar: SidebarState,
    preview_cache: CommonMarkCache,
    overlay: Option<Overlay>,
    fonts: FontSupport,
    applied: Option<(bool, u32)>,
    last_title: String,
    last_version: u64,
    /// Split editor: the note in the other half, and whether that half is on
    /// the left (the active note is always in the other half).
    split: Option<DocId>,
    split_left: bool,
    last_active: Option<DocId>,
    /// Quick open was shown to pick the other half's note; this note stays.
    pick_side: Option<DocId>,
    /// UI state of 0.9 feature panels (`registry.rs`).
    features: crate::registry::FeatureUi,
    /// The right panel's tab (`registry::RIGHT_TABS` id).
    right_tab: &'static str,
    /// Open non-Markdown files (canvas, PDF, `.base`).
    surfaces: crate::surfaces::Surfaces,
    /// Frames in which text typed with Alt is dropped (an Alt shortcut fired).
    drop_alt_text: u8,
    /// The active note and how many notes were open, last frame: a note
    /// opened from anywhere (a dialog, a job) takes the central panel back
    /// from a PDF or canvas.
    front_note: (Option<DocId>, usize),
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
        // 0.10: "Record voice note…" is offered when this build can record.
        ws.features.ai.voice.can_record = crate::voice_ui::CAN_RECORD;
        let target = vault_arg.or_else(|| ws.settings.last_vault.clone()).filter(|p| p.is_dir());
        if let Some(dir) = target {
            // The index and the watcher start after the first frame (`logic`).
            match ws.open_vault_deferred(&dir) {
                Ok(()) => {
                    if session.vault.as_deref().map(Path::canonicalize).and_then(Result::ok) == dir.canonicalize().ok() {
                        ws.restore(&session);
                    }
                }
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
        }
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // WS-G: Quire sketches (`*-ink-*.svg`) as pictures in notes, both editions.
        crate::canvas_ui::install_sketch_loader(&cc.egui_ctx);
        crate::drawing_ui::install(&cc.egui_ctx);
        let mut editor = EditorState::default();
        editor.request_focus = true;
        let mut fast = FastEditor::default();
        fast.request_focus = true;
        Self {
            #[cfg(feature = "bench")]
            bench: crate::bench::Run::from_env(),
            first_frame_shown: false,
            ws,
            editor,
            fast,
            focus_find: false,
            clipboard: None,
            clipboard_html: None,
            html_board: None,
            capture_key: None,
            capture_key_failed: false,
            sidebar: SidebarState::default(),
            preview_cache: CommonMarkCache::default(),
            overlay: None,
            fonts,
            applied: None,
            last_title: String::new(),
            last_version: 0,
            split: None,
            split_left: false,
            last_active: None,
            pick_side: None,
            features: crate::registry::FeatureUi::default(),
            right_tab: crate::registry::INSIGHTS,
            surfaces: crate::surfaces::Surfaces::default(),
            drop_alt_text: 0,
            front_note: (None, 0),
        }
    }

    /// Show a feature's UI (`UiRequest::Feature`), or say it isn't there yet.
    fn show_feature(&mut self, cmd: Command) {
        use crate::registry::{RIGHT_TABS, SETTINGS_TABS, SIDEBAR_TABS, UiTarget, tab_offered, ui_target};
        match ui_target(cmd) {
            Some(UiTarget::Right(id)) if tab_offered(RIGHT_TABS, id, &self.ws) => {
                self.ws.settings.show_insights = true;
                self.ws.focus_mode = false;
                self.right_tab = id;
            }
            Some(UiTarget::Sidebar(id)) if tab_offered(SIDEBAR_TABS, id, &self.ws) => {
                self.ws.settings.show_sidebar = true;
                self.ws.focus_mode = false;
                self.sidebar.tab = SidebarTab::Feature(id);
            }
            Some(UiTarget::Settings(id)) if tab_offered(SETTINGS_TABS, id, &self.ws) => {
                self.set_overlay(Some(Overlay::Settings(Box::new(crate::dialogs::SettingsState::feature_tab(id)))));
            }
            Some(UiTarget::Overlay(make)) => {
                let view = make(&mut self.ws);
                self.set_overlay(Some(Overlay::Feature(view)));
            }
            _ => crate::registry::not_available(&mut self.ws, cmd),
        }
    }

    /// Replace the overlay, telling a feature dialog that it went away.
    fn set_overlay(&mut self, overlay: Option<Overlay>) {
        if let Some(Overlay::Feature(mut old)) = std::mem::replace(&mut self.overlay, overlay) {
            old.closed(&mut self.ws);
        }
    }

    /// Open a file that isn't a note in its surface (`registry::open_surface`).
    fn open_file(&mut self, path: &NotePath) {
        if path.is_markdown() {
            if self.ws.open_note(path).is_some() {
                self.surfaces.active = None;
                self.focus_editor();
                self.reveal_caret();
            }
        } else if let Some(s) = crate::registry::open_surface(path) {
            self.surfaces.open(s);
        } else {
            self.ws.notify(NoticeLevel::Info, format!("Quire can't open {} files", path.as_str().rsplit('.').next().unwrap_or("these")));
        }
    }

    /// UI the workspace asked for outside a command (`Workspace::request_ui`).
    fn take_ui_requests(&mut self) {
        for req in self.ws.take_ui_requests() {
            self.request(req);
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
        ctx.global_style_mut(theme::style);
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
        // 0.11: formatting goes to every cursor of the fast editor, as one undo step.
        if quire_app::format_cmds::handles(cmd) && !self.classic() && self.surfaces.active.is_none() && self.ws.active_doc().is_some() {
            self.fast.format(cmd, &mut self.ws);
            self.focus_editor();
            return;
        }
        match cmd {
            Command::InsertAttachment => return self.insert_attachment(),
            Command::FocusToolbar => {
                self.features.toolbar.focus_first();
                return;
            }
            _ => {}
        }
        if let Some(req) = self.ws.run(cmd) {
            self.request(req);
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

    /// A shortcut, toolbar button or palette entry: a command or a macro.
    fn run_action(&mut self, action: Action) {
        match action {
            Action::Command(c) => self.run(c),
            Action::Macro(name) => self.run_macro(&name),
        }
    }

    /// Run a macro's commands in order. It stops at a command that opens
    /// a dialog (the rest would run behind it) and skips commands this
    /// version doesn't have.
    fn run_macro(&mut self, name: &str) {
        let Some(m) = self.ws.settings.commands.macros.iter().find(|m| m.name == name).cloned() else {
            self.ws.notify(NoticeLevel::Warn, format!("There's no macro called “{name}” any more"));
            return;
        };
        for (i, id) in m.steps.iter().enumerate() {
            let Some(cmd) = Command::from_id(id).filter(|c| self.ws.command_available(*c)) else {
                self.ws.notify(NoticeLevel::Info, format!("Macro “{name}”: skipped “{id}”, which isn't available"));
                continue;
            };
            if cmd.info().needs_doc && self.ws.active_doc().is_none() {
                self.ws.notify(NoticeLevel::Info, format!("Macro “{name}” stopped at “{}”: no note is open", cmd.info().title));
                return;
            }
            let had_dialog = self.overlay.is_some();
            self.run(cmd);
            if !had_dialog && self.overlay.is_some() && i + 1 < m.steps.len() {
                self.ws.notify(NoticeLevel::Info, format!("Macro “{name}” stopped at “{}”: it opened a dialog", cmd.info().title));
                return;
            }
        }
    }

    /// Insert image or attachment: pick files, copy them into the vault,
    /// link them at the caret.
    fn insert_attachment(&mut self) {
        if self.ws.vault().is_none() || self.ws.active_doc().is_none() {
            self.ws.notify(NoticeLevel::Info, "Open a note first, then insert into it");
            return;
        }
        if let Some(files) = rfd::FileDialog::new().set_title("Insert image or attachment").pick_files() {
            self.ws.import_files(&files);
        }
        self.focus_editor();
    }

    /// Vim's Normal and Visual modes take Ctrl+V (Visual block) for
    /// themselves while the note has the keyboard.
    fn vim_takes_ctrl_v(&self, ctx: &egui::Context) -> bool {
        self.ws.settings.editing.vim
            && !self.ws.features.nav.vim.typing()
            && self.overlay.is_none()
            && !self.classic()
            && self.ws.active_doc().is_some_and(|d| ctx.memory(|m| m.has_focus(crate::view::editor_id(d.id))))
    }

    /// Show what a command or a feature asked for.
    fn request(&mut self, req: UiRequest) {
        let next = match req {
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
            UiRequest::OpenVault => Overlay::OpenVault { input: self.ws.vault_root().map(|p| p.display().to_string()).unwrap_or_default() },
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
            UiRequest::Ask => Overlay::Feature(crate::ask_ui::AskOverlay::make(&mut self.ws)),
            UiRequest::Settings => Overlay::Settings(Box::default()),
            UiRequest::SyncSettings => Overlay::Settings(Box::new(crate::dialogs::SettingsState::sync_tab(&self.ws))),
            UiRequest::NewProject => Overlay::NewProject(crate::dialogs::ProjectState::default()),
            UiRequest::Publish => Overlay::Publish(crate::dialogs::PublishState::new(&self.ws)),
            UiRequest::Shortcuts => Overlay::Feature(Box::new(overlays::ShortcutsOverlay::default())),
            UiRequest::Tour => Overlay::Tour { step: 0 },
            UiRequest::SyncConflicts => Overlay::SyncConflicts,
            UiRequest::NewInFolder => Overlay::Folder(crate::dialogs::FolderState::new(&self.ws, crate::dialogs::FolderMode::NewNote)),
            UiRequest::MoveNote => Overlay::Folder(crate::dialogs::FolderState::new(&self.ws, crate::dialogs::FolderMode::Move)),
            UiRequest::History => Overlay::History(crate::dialogs::HistoryState::new(&self.ws)),
            UiRequest::Graph => Overlay::Graph(Box::new(crate::graph_view::GraphState::new(&self.ws))),
            UiRequest::Split => {
                self.toggle_split();
                return;
            }
            UiRequest::SideBySide(a, b) => {
                self.side_by_side(&a, &b);
                return;
            }
            UiRequest::Copy(text) => {
                self.clipboard = Some(text);
                return;
            }
            UiRequest::CopyHtml(c) => {
                self.clipboard_html = Some(c);
                return;
            }
            UiRequest::Capture => Overlay::Capture { text: String::new(), focused: false, minimize: false },
            UiRequest::Import => {
                if self.ws.vault().is_none() {
                    self.ws.notify(NoticeLevel::Info, "Open a vault first, then import into it");
                    return;
                }
                Overlay::Import(crate::dialogs::ImportState::default())
            }
            UiRequest::Feature(c) => {
                self.show_feature(c);
                return;
            }
            UiRequest::Editor(c) => {
                if self.classic() {
                    self.ws.notify(NoticeLevel::Info, "Multiple cursors need the fast editor (View: Switch editor)");
                } else {
                    self.fast.command(c, &mut self.ws);
                }
                return;
            }
            UiRequest::RevealCaret => {
                self.reveal_caret();
                return;
            }
            UiRequest::OpenFile(p) => {
                self.open_file(&p);
                return;
            }
        };
        self.set_overlay(Some(next));
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
        // 0.11: the shortcuts in effect (Settings → Commands), macros included.
        let keymap = Keymap::new(&self.ws.settings.commands);
        let vim_keys = self.vim_takes_ctrl_v(ctx);
        let mut combos: Vec<(u8, Action, KeyboardShortcut)> = Vec::new();
        for (k, action) in keymap.bindings() {
            match action {
                Action::Command(c) => {
                    let info = c.info();
                    if other_field_focused && matches!(info.category, "Edit" | "Format" | "Review") {
                        continue;
                    }
                    // Features that aren't ready have no keys yet.
                    if !self.ws.command_available(*c) {
                        continue;
                    }
                }
                Action::Macro(_) if other_field_focused => continue,
                Action::Macro(_) => {}
            }
            // Vim's Normal mode keeps Ctrl+V and Ctrl+Q (Visual block).
            if vim_keys && k.cmd && !k.alt && !k.shift && matches!(k.key, "V" | "Q") {
                continue;
            }
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
            combos.push((u8::from(k.shift) + u8::from(k.alt) + u8::from(k.cmd), action.clone(), KeyboardShortcut::new(m, key)));
        }
        combos.sort_by_key(|c| std::cmp::Reverse(c.0));
        let mut fired = Vec::new();
        let typed = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if !t.is_empty())));
        let mut alt_only = false;
        ctx.input_mut(|i| {
            for (_, action, sc) in &combos {
                if altgr_typed(sc.modifiers, typed) {
                    continue;
                }
                if i.consume_shortcut(sc) {
                    fired.push(action.clone());
                    alt_only |= sc.modifiers.alt && !sc.modifiers.command;
                }
            }
        });
        // The key of an Alt shortcut also types its character (Alt+] gives
        // "]" on Linux and Windows, "‘" with Option on macOS): keep it out
        // of the note, this frame and the next (it can arrive a frame late,
        // and Alt may be up again by the end of the frame).
        if alt_only {
            self.drop_alt_text = 2;
        }
        if self.drop_alt_text > 0 {
            self.drop_alt_text -= 1;
            ctx.input_mut(|i| drop_typed_text(&mut i.events));
        }
        for action in fired {
            self.run_action(action);
        }
    }

    /// Keep the split editor consistent with what's open and active.
    fn settle_split(&mut self) {
        let Some(other) = self.split else { return };
        let active = self.ws.active_doc().map(|d| d.id);
        let open = |id: DocId| self.ws.docs().iter().any(|d| d.id == id);
        if !open(other) || active.is_none() || self.ws.settings.show_preview || self.classic() || self.ws.focus_mode {
            self.split = None;
            return;
        }
        if active == Some(other) {
            // The other half's note was chosen (a tab, a link): the halves
            // trade roles, each note staying where it is.
            match self.last_active.filter(|a| open(*a) && Some(*a) != active) {
                Some(prev) => {
                    self.split = Some(prev);
                    self.split_left = !self.split_left;
                }
                None => self.split = None,
            }
        }
    }

    /// Split the editor in two, or go back to one.
    fn toggle_split(&mut self) {
        if self.split.take().is_some() {
            return;
        }
        if self.classic() {
            self.ws.notify(NoticeLevel::Info, "Split view needs the fast editor (View: Switch editor)");
            return;
        }
        let Some(active) = self.ws.active_doc().map(|d| d.id) else { return };
        // The other half shows the most recently opened other note.
        let other = self.ws.docs().iter().rev().map(|d| d.id).find(|id| *id != active);
        if let Some(id) = other {
            self.ws.settings.show_preview = false;
            self.split = Some(id);
            self.split_left = false;
        } else {
            // Nothing else open: pick the note for the other half.
            self.pick_side = Some(active);
            self.overlay = Some(Overlay::palette(Mode::Notes));
        }
    }

    /// `a` on the left, `b` on the right and active, whether or not a split
    /// was open.
    fn side_by_side(&mut self, a: &NotePath, b: &NotePath) {
        if self.classic() {
            self.ws.notify(NoticeLevel::Info, "Split view needs the fast editor (View: Switch editor)");
            self.ws.open_note(b);
            return;
        }
        let (Some(id_a), Some(id_b)) = (self.ws.open_note(a), self.ws.open_note(b)) else { return };
        if id_a == id_b {
            return;
        }
        self.ws.settings.show_preview = false;
        self.split = Some(id_a);
        self.split_left = true;
        self.last_active = Some(id_b);
    }

    /// Open `p` in the other half of the editor (splitting it if needed).
    fn open_side(&mut self, p: &NotePath) {
        let Some(active) = self.ws.active_doc().map(|d| d.id) else {
            self.ws.open_note(p);
            return;
        };
        if self.ws.open_note(p).is_some_and(|id| id != active) {
            self.ws.settings.show_preview = false;
            self.split = Some(active);
            self.split_left = true;
            self.last_active = self.ws.active_doc().map(|d| d.id);
            self.focus_editor();
        }
    }

    /// "Copy as HTML": HTML with the Markdown as plain text, so a paste
    /// into mail or a document keeps the formatting. Where the system
    /// clipboard won't take HTML, the HTML source goes as text, and the
    /// notice says so.
    fn copy_html(&mut self, ctx: &egui::Context, c: quire_app::copy_html::CopiedHtml) {
        let what = if c.selection { "the selection" } else { "the note" };
        if self.html_board.is_none() {
            self.html_board = arboard::Clipboard::new().ok();
        }
        let copied = self.html_board.as_mut().is_some_and(|b| b.set_html(c.html.as_str(), Some(c.text.as_str())).is_ok());
        if copied {
            self.ws.notify(NoticeLevel::Info, format!("Copied {what} as HTML: paste it into mail or a document"));
        } else {
            self.html_board = None;
            ctx.copy_text(c.html);
            self.ws.notify(
                NoticeLevel::Info,
                format!("Copied {what} as HTML source (plain text): this system's clipboard didn't take formatted text"),
            );
        }
    }

    fn after_panel(&mut self, out: &PanelOut) {
        if let Some(p) = &out.open_side {
            self.open_side(p);
        }
        if let Some(p) = &out.copy_html {
            self.clipboard_html = self.ws.copy_as_html(Some(p));
        }
        if out.skip_update {
            self.ws.skip_update();
        }
        if let Some(c) = out.run {
            self.run(c);
        }
        if out.jumped {
            // A note was opened or chosen: notes get the central panel again.
            self.surfaces.active = None;
            self.focus_editor();
            self.reveal_caret();
        }
        if out.reveal_only {
            self.reveal_caret();
        }
    }

    /// Two notes side by side: the active one and `other`, each with a slim
    /// title bar. Clicking into either makes it the active note.
    fn split_panes(&mut self, ui: &mut egui::Ui, other: DocId, pal: &Palette) {
        let full = ui.available_rect_before_wrap();
        let mid = full.center().x;
        let left = egui::Rect::from_min_max(full.min, egui::pos2(mid - 0.5, full.max.y));
        let right = egui::Rect::from_min_max(egui::pos2(mid + 0.5, full.min.y), full.max);
        ui.painter().line_segment([egui::pos2(mid, full.top()), egui::pos2(mid, full.bottom())], egui::Stroke::new(1.0, pal.border));
        let (active_rect, other_rect) = if self.split_left { (right, left) } else { (left, right) };
        let header = |ui: &mut egui::Ui, ws: &Workspace, id: DocId, active: bool| -> bool {
            let title = ws.docs().iter().find(|d| d.id == id).map(quire_app::Document::title).unwrap_or_default();
            let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
            let color = if active { pal.strong } else { pal.muted };
            ui.painter().text(
                egui::pos2(r.left() + 16.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                title,
                egui::FontId::proportional(12.5),
                color,
            );
            if active {
                ui.painter().line_segment(
                    [egui::pos2(r.left() + 16.0, r.bottom() - 1.0), egui::pos2(r.right() - 16.0, r.bottom() - 1.0)],
                    egui::Stroke::new(1.5, pal.accent.gamma_multiply(0.6)),
                );
            }
            resp.clicked()
        };
        // The active note first: it gets this frame's keys, focus and scroll requests.
        let mut a = ui.new_child(egui::UiBuilder::new().max_rect(active_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        let Some(active) = self.ws.active_doc().map(|d| d.id) else { return };
        header(&mut a, &self.ws, active, true);
        self.fast.show_pane(&mut a, &mut self.ws, pal, self.fonts, true);
        // The active note may have changed (a followed link); the other half keeps its note.
        let active = self.ws.active_doc().map_or(active, |d| d.id);
        if !self.ws.activate(other) {
            self.split = None;
            return;
        }
        let mut b = ui.new_child(egui::UiBuilder::new().max_rect(other_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        let header_clicked = header(&mut b, &self.ws, other, false);
        self.fast.show_pane(&mut b, &mut self.ws, pal, self.fonts, false);
        let ctx = ui.ctx().clone();
        if header_clicked {
            ctx.memory_mut(|m| m.request_focus(crate::view::editor_id(other)));
        }
        if header_clicked || ctx.memory(|m| m.has_focus(crate::view::editor_id(other))) {
            // The other half was clicked into: it becomes the active note.
            self.split = Some(active);
            self.split_left = !self.split_left;
            self.last_active = Some(other);
        } else {
            self.ws.activate(active);
        }
    }

    /// The right panel: Insights, plus any feature tabs that are offered.
    fn right_panel(&mut self, ui: &mut egui::Ui, pal: &Palette) -> PanelOut {
        self.features.active_surface = self.surfaces.active.and_then(|i| self.surfaces.open.get(i)).map(|s| s.path().clone());
        let tabs = crate::registry::offered(crate::registry::RIGHT_TABS, &self.ws);
        let tab = tabs.iter().find(|t| t.id == self.right_tab).or_else(|| tabs.first()).copied();
        if tabs.len() > 1 {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for t in &tabs {
                    if ui.selectable_label(tab.is_some_and(|c| c.id == t.id), (t.label)(&self.ws)).clicked() {
                        self.right_tab = t.id;
                    }
                }
            });
            ui.separator();
        }
        match tab {
            Some(t) => (t.show)(ui, &mut self.ws, &mut self.features, pal),
            None => PanelOut::default(),
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
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(feature = "bench")]
        if let Some(b) = &mut self.bench {
            let index = self.ws.index();
            let indexed = !index.status().busy && !index.semantic_busy() && index.semantic_version() > 0;
            b.input(raw_input, indexed);
        }
        // egui closes the window on Ctrl+Q (⌘Q on macOS) at the end of every
        // frame (`Options::quit_shortcuts`), after Vim has seen the key. In
        // Vim's Normal and Visual mode Ctrl+Q is Visual block on Windows and
        // Linux, so it doesn't quit then; ⌘Q on macOS always does.
        let vim_keys = self.vim_takes_ctrl_v(ctx);
        let quit =
            if vim_keys && !cfg!(target_os = "macos") { Vec::new() } else { vec![KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)] };
        ctx.options_mut(|o| o.quit_shortcuts = quit);
        // Copy, cut and paste keys (main.rs turns off egui-winit's own handling).
        let keep = if crate::commands_ui::recording(ctx) {
            ClipboardKeys::KeepAll
        } else if vim_keys {
            ClipboardKeys::KeepPaste
        } else {
            ClipboardKeys::Translate
        };
        if clipboard_keys(&mut raw_input.events, keep) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ctx.request_repaint();
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The first frame is on screen: now read the vault (design §9).
        if self.first_frame_shown {
            self.ws.start_deferred();
        } else {
            self.first_frame_shown = true;
            ctx.request_repaint();
        }
        crate::drawing_ui::set_vault_root(self.ws.vault().and_then(|v| v.storage().absolute(&NotePath::new(""))));
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
            || self.ws.insights_pending()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.ws.ui_requested() {
            ctx.request_repaint();
        }
        if let Some(t) = self.ws.next_deadline() {
            ctx.request_repaint_after(t.saturating_duration_since(now));
        }
        // Poll for outside edits even when idle.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let started = Instant::now();
        // 0.11: idle locking of locked folders counts from the last input.
        if ui.ctx().input(|i| i.events.iter().any(|e| !matches!(e, egui::Event::WindowFocused(_)))) {
            self.ws.note_user_activity();
        }
        let now = (self.ws.active_doc().map(|d| d.id), self.ws.docs().len());
        // A different note became active and none was closed: it was opened.
        if now.0.is_some() && now.0 != self.front_note.0 && now.1 >= self.front_note.1 {
            self.surfaces.active = None;
        }
        self.front_note = now;
        #[cfg(feature = "bench")]
        if let Some(note) = self.bench.as_mut().and_then(crate::bench::Run::note_to_open) {
            self.ws.settings.tour_done = true;
            self.set_overlay(None);
            self.open_file(&NotePath::new(note));
        }
        self.ui_inner(ui, frame);
        #[cfg(feature = "bench")]
        if let Some(b) = &mut self.bench {
            b.frame_done(ui.ctx());
        }
        if let Some(text) = self.clipboard.take() {
            ui.ctx().copy_text(text);
        }
        if let Some(c) = self.clipboard_html.take() {
            self.copy_html(ui.ctx(), c);
        }
        // QUIRE_TRACE=1 prints how long each frame's UI work took.
        if std::env::var_os("QUIRE_TRACE").is_some() {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let lines = self.ws.active_doc().map_or(0, |d| d.buffer().len_lines());
            eprintln!("frame {ms:.2} ms · {lines} lines");
        }
    }
}

impl QuireApp {
    /// A toolbar button was pressed, or the keyboard left the toolbar.
    fn toolbar_pressed(&mut self, pressed: Option<crate::toolbar_ui::Pressed>) {
        match pressed {
            Some(crate::toolbar_ui::Pressed::Run(a)) => {
                self.run_action(a);
                self.focus_editor();
            }
            Some(crate::toolbar_ui::Pressed::Leave) => self.focus_editor(),
            None => {}
        }
    }

    /// Keep the global shortcut registered as the setting says, and answer it:
    /// bring the window up with the capture box.
    fn quick_capture_key(&mut self, ctx: &egui::Context) {
        let want = self.ws.settings.capture_shortcut && self.ws.vault().is_some();
        if want && self.capture_key.is_none() && !self.capture_key_failed {
            match crate::capture_key::CaptureKey::register(ctx) {
                Ok(k) => self.capture_key = Some(k),
                // Settings shows why (e.g. a Wayland session); no toast on every launch.
                Err(_) => self.capture_key_failed = true,
            }
        } else if !want {
            self.capture_key = None;
            self.capture_key_failed = false;
        }
        if self.capture_key.as_ref().is_some_and(crate::capture_key::CaptureKey::take) {
            let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            match &mut self.overlay {
                // Already capturing: keep the draft, just give it the keyboard again.
                Some(Overlay::Capture { focused, minimize, .. }) => {
                    *focused = false;
                    *minimize |= minimized;
                }
                // Lists and views can give way; a dialog with typing in it can't.
                None | Some(Overlay::Palette { .. } | Overlay::Graph(_) | Overlay::Tour { .. }) => {
                    self.overlay = Some(Overlay::Capture { text: String::new(), focused: false, minimize: minimized });
                }
                Some(Overlay::Feature(v)) if v.yields_to_capture() => {
                    self.set_overlay(Some(Overlay::Capture { text: String::new(), focused: false, minimize: minimized }));
                }
                Some(_) => self.ws.notify(NoticeLevel::Info, "Close the open dialog to use quick capture"),
            }
        }
    }

    fn ui_inner(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.quick_capture_key(&ctx);
        self.apply_theme(&ctx);
        let pal = self.palette(&ctx);
        // Background jobs wait while a dialog is open.
        self.ws.jobs.set_modal_open(self.overlay.is_some());
        self.handle_shortcuts(&ctx);
        self.take_ui_requests();
        if self.overlay.is_none() && self.classic() {
            EditorState::handle_smart_keys(&ctx, &mut self.ws);
        }
        // Escape closes the find bar (unless it's clearing extra cursors).
        let extra_cursors = self.ws.active_doc().is_some_and(|d| self.fast.has_extra_cursors(d.id)) || self.fast.suggesting();
        // With Vim keys, Escape in Insert mode is Vim's (back to Normal mode).
        let vim_typing = self.ws.settings.editing.vim && self.ws.features.nav.vim.typing();
        if self.overlay.is_none()
            && self.ws.find.open
            && !extra_cursors
            && !vim_typing
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
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
                            let welcome = NotePath::new("Welcome.md");
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
                    .show(ui, |ui| {
                        let out = panels::tab_bar(ui, &mut self.ws, pal);
                        self.surfaces.tab_strip(ui, pal);
                        out
                    })
                    .inner;
                self.after_panel(&out);
            }
            let status = egui::Panel::bottom("quire-status")
                .frame(egui::Frame::NONE.fill(if focus { pal.bg } else { pal.panel }).inner_margin(egui::Margin::symmetric(12, 4)))
                .show(ui, |ui| panels::status_bar(ui, &self.ws, pal, self.surfaces.active.is_some()))
                .inner;
            self.after_panel(&status);
            if self.ws.settings.show_sidebar && !focus {
                // A PDF or canvas in front is what the Files tab marks.
                self.sidebar.active_surface = self.surfaces.active.and_then(|i| self.surfaces.open.get(i)).map(|s| s.path().clone());
                let out = egui::Panel::left("quire-sidebar")
                    .resizable(true)
                    .default_size(250.0)
                    .size_range(170.0..=480.0)
                    .frame(egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 4)))
                    .show(ui, |ui| panels::sidebar(ui, &mut self.ws, &mut self.sidebar, &mut self.features, pal))
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
                        let out = self.right_panel(ui, pal);
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
            // Feature bars above the note (`registry::EDITOR_BANNERS`): the review banner.
            if self.surfaces.active.is_none() {
                for b in crate::registry::EDITOR_BANNERS {
                    if !(b.available)(&self.ws) {
                        continue;
                    }
                    let out = egui::Panel::top(egui::Id::unique(("quire-banner", b.id)))
                        .frame(
                            egui::Frame::NONE
                                .fill(pal.panel)
                                .inner_margin(egui::Margin::symmetric(12, 8))
                                .stroke(egui::Stroke::new(1.0, pal.border)),
                        )
                        .show(ui, |ui| (b.show)(ui, &mut self.ws, &mut self.features, pal))
                        .inner;
                    self.after_panel(&out);
                }
            }
            // 0.11: the formatting toolbar.
            if self.surfaces.active.is_none() && self.ws.active_doc().is_some() && !focus && self.features.toolbar.visible(&self.ws) {
                let keymap = Keymap::new(&self.ws.settings.commands);
                let pressed = egui::Panel::top("quire-toolbar")
                    .frame(
                        egui::Frame::NONE
                            .fill(pal.panel)
                            .inner_margin(egui::Margin::symmetric(10, 3))
                            .stroke(egui::Stroke::new(1.0, pal.border)),
                    )
                    .show(ui, |ui| crate::toolbar_ui::bar(ui, &self.ws, &keymap, pal, &mut self.features.toolbar))
                    .inner;
                self.toolbar_pressed(pressed);
            } else {
                self.features.toolbar.keyboard = false;
            }
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(pal.bg)).show(ui, |ui| {
                if let Some(out) = self.surfaces.show_active(ui, &mut self.ws, pal) {
                    // `jumped` (a note opened from a `.base`) gives notes the panel again.
                    self.after_panel(&out);
                } else if self.ws.active_doc().is_some() {
                    self.settle_split();
                    if let Some(other) = self.split {
                        self.split_panes(ui, other, pal);
                    } else if self.classic() {
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

        // 0.11: a diagram's full-screen button.
        if let Some(source) = crate::diagram_ui::take_fullscreen(&ctx) {
            self.set_overlay(Some(Overlay::Feature(Box::new(crate::diagram_ui::DiagramOverlay::new(source)))));
        }
        // 0.11: the small toolbar above selected text.
        if self.ws.settings.toolbar.floating
            && self.overlay.is_none()
            && !self.classic()
            && self.surfaces.active.is_none()
            && let Some(at) = self.fast.selection_rect.or_else(|| self.features.toolbar.floating_held(&ctx))
            && let Some(doc) = self.ws.active_doc().map(|d| d.id)
            && !ctx.is_being_dragged(crate::view::editor_id(doc))
        {
            let keymap = Keymap::new(&self.ws.settings.commands);
            let pressed = crate::toolbar_ui::floating(&ctx, at, &keymap, pal, &mut self.features.toolbar);
            self.toolbar_pressed(pressed);
        }
        // What panels and widgets asked for while drawing.
        self.take_ui_requests();
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
            if let Some(next) = res.then {
                self.set_overlay(Some(next));
            } else if res.close {
                self.set_overlay(None);
                self.focus_editor();
                if let Some(keep) = self.pick_side.take()
                    && self.ws.active_doc().is_some_and(|d| d.id != keep)
                {
                    self.ws.settings.show_preview = false;
                    self.split = Some(keep);
                    self.split_left = true;
                    self.last_active = self.ws.active_doc().map(|d| d.id);
                }
            }
            if res.jumped {
                self.reveal_caret();
            }
            if let Some(c) = res.run {
                self.run(c);
            }
        }
        // 0.11: a macro chosen in the command palette.
        if let Some(name) = self.ws.features.nav.run_macro.take() {
            self.run_macro(&name);
        }
        overlays::toasts(&ctx, &self.ws, pal);
        self.last_active = self.ws.active_doc().map(|d| d.id);
        let ids: Vec<_> = self.ws.docs().iter().map(|d| d.id).collect();
        self.editor.forget(&ids);
        self.fast.forget(&ids);
        self.update_title(&ctx);
    }
}

/// On Windows `AltGr` arrives as Ctrl+Alt (and `AltGr`+Shift as Ctrl+Alt+Shift):
/// when a character was typed this frame (é, ł, €, Ü…), a Ctrl+Alt
/// "shortcut" is really that character.
fn altgr_typed(m: Modifiers, typed: bool) -> bool {
    typed && m.alt && m.command
}

/// Take out typed text: right after an Alt shortcut (not `AltGr`, which is
/// Ctrl+Alt on Windows and types real characters), it is the shortcut key's.
fn drop_typed_text(events: &mut Vec<egui::Event>) {
    events.retain(|e| !matches!(e, egui::Event::Text(_)));
}

/// What [`clipboard_keys`] leaves as keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardKeys {
    /// Copy, cut and paste become clipboard events (what egui-winit does).
    Translate,
    /// Vim's Normal mode: Ctrl+V stays a key (Visual block), even with
    /// nothing on the clipboard; copy and cut are translated.
    KeepPaste,
    /// Settings → Commands is recording a shortcut: every key stays a key.
    KeepAll,
}

/// The copy, cut and paste keys, as egui-winit would handle them, except
/// what `keep` says. egui-winit turns Ctrl+V into a paste of the clipboard
/// and drops the key, and with an empty clipboard sends nothing at all, so
/// Vim never saw Ctrl+V; `main.rs` turns that off and this takes its
/// place. Returns true when the clipboard should be pasted
/// (`ViewportCommand::RequestPaste`).
fn clipboard_keys(events: &mut Vec<egui::Event>, keep: ClipboardKeys) -> bool {
    if keep == ClipboardKeys::KeepAll {
        return false;
    }
    let windows = cfg!(target_os = "windows");
    let mut paste = false;
    let old = std::mem::take(events);
    for e in old {
        if let egui::Event::Key { key, pressed, modifiers, .. } = &e {
            let only = modifiers.command_only();
            let cut = *key == Key::Cut || (only && *key == Key::X) || (windows && modifiers.shift && *key == Key::Delete);
            let copy = *key == Key::Copy || (only && *key == Key::C) || (windows && modifiers.ctrl && *key == Key::Insert);
            let paste_key = *key == Key::Paste || (only && *key == Key::V) || (windows && modifiers.shift && *key == Key::Insert);
            if *pressed && (cut || copy || paste_key) {
                if paste_key && keep == ClipboardKeys::KeepPaste {
                    events.push(e);
                } else if cut {
                    events.push(egui::Event::Cut);
                } else if copy {
                    events.push(egui::Event::Copy);
                } else {
                    paste = true;
                }
                continue;
            }
        }
        events.push(e);
    }
    paste
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn altgr_text_wins_over_ctrl_alt_shortcuts() {
        let ctrl_alt = Modifiers::COMMAND | Modifiers::ALT;
        assert!(altgr_typed(ctrl_alt, true), "AltGr+C types ć on a Polish layout");
        assert!(altgr_typed(ctrl_alt | Modifiers::SHIFT, true), "AltGr+Shift+Y types Ü on US-International");
        assert!(!altgr_typed(ctrl_alt, false), "no text: it's the shortcut");
        assert!(!altgr_typed(Modifiers::ALT, true));
        assert!(!altgr_typed(Modifiers::COMMAND | Modifiers::SHIFT, true));
    }

    #[test]
    fn ctrl_v_reaches_vim_even_with_an_empty_clipboard() {
        let key = |k: Key, m: Modifiers| egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: m };
        let typed = || {
            vec![
                key(Key::V, Modifiers::COMMAND),
                key(Key::C, Modifiers::COMMAND),
                key(Key::X, Modifiers::COMMAND),
                key(Key::V, Modifiers::NONE),
            ]
        };
        let mut ev = typed();
        assert!(clipboard_keys(&mut ev, ClipboardKeys::Translate), "Ctrl+V pastes");
        assert_eq!(ev, vec![egui::Event::Copy, egui::Event::Cut, key(Key::V, Modifiers::NONE)]);
        let mut ev = typed();
        assert!(!clipboard_keys(&mut ev, ClipboardKeys::KeepPaste), "Vim's Normal mode: no paste");
        assert_eq!(ev, vec![key(Key::V, Modifiers::COMMAND), egui::Event::Copy, egui::Event::Cut, key(Key::V, Modifiers::NONE)]);
        let mut ev = typed();
        assert!(!clipboard_keys(&mut ev, ClipboardKeys::KeepAll));
        assert_eq!(ev, typed(), "recording a shortcut: keys stay keys");
        // Ctrl+Shift+V isn't paste (egui-winit's rule too).
        let mut ev = vec![key(Key::V, Modifiers::COMMAND | Modifiers::SHIFT)];
        assert!(!clipboard_keys(&mut ev, ClipboardKeys::Translate));
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn alt_shortcuts_leave_no_character_in_the_note() {
        let text = |t: &str| egui::Event::Text(t.into());
        let mut ev = vec![text("]"), egui::Event::Copy];
        drop_typed_text(&mut ev);
        assert_eq!(ev, vec![egui::Event::Copy], "Alt+] typed \"]\"");
    }
}
