// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where 0.9 features plug into the desktop (design §6.1 F0.12, §6.3).
//!
//! `app.rs`, `panels.rs`, `overlays.rs` and `dialogs.rs` are frozen after
//! the foundation step. A feature's UI lives in its own files and is
//! registered here, in the block of lines that belongs to its workstream:
//!
//! * [`RIGHT_TABS`] — tabs of the right panel (Insights is built in)
//! * [`SIDEBAR_TABS`] — extra sidebar tabs (Files, Search, Tags, Tasks are built in)
//! * [`SETTINGS_TABS`] — tabs of the Settings dialog
//! * [`EDITOR_BANNERS`] — bars above the note (the review banner)
//! * [`INBOX_KINDS`] — how a feature's Inbox items read and what they offer
//! * [`ui_target`] — what a feature command's `UiRequest::Feature` opens
//!   (a tab, or a dialog with [`UiTarget::Overlay`])
//! * [`paint_widget`] — widgets a feature puts under a line of a note
//! * [`open_surface`] — editors for non-Markdown files (`surfaces.rs`)
//! * `layout_samples` — every feature overlay, for `layout_check` (tests)
//!
//! Each table is in workstream order; add to your block only.

use quire_app::agent::inbox::{InboxItem, StoredItem};
use quire_app::decor::{ActionId, WidgetId};
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;
use quire_app::{Command, Workspace};

use crate::panels::PanelOut;
use crate::theme::Palette;

/// Draws a tab's content.
pub(crate) type ShowFn = fn(&mut egui::Ui, &mut Workspace, &mut FeatureUi, &Palette) -> PanelOut;

/// A tab in the right panel, the sidebar or Settings.
#[derive(Clone, Copy)]
pub(crate) struct TabDef {
    /// Stable id (`"inbox"`), stored while the app runs.
    pub id: &'static str,
    /// The tab's label; may include a count ("Inbox · 3").
    pub label: fn(&Workspace) -> String,
    /// Offered only when this says so.
    pub available: fn(&Workspace) -> bool,
    pub show: ShowFn,
}

/// UI state of every feature's panels, one field per owner file.
#[derive(Default)]
pub(crate) struct FeatureUi {
    // ---- WS-A
    pub inbox: crate::inbox_ui::InboxUi,
    pub activity: crate::activity_ui::ActivityUi,
    pub agents_settings: crate::agents_settings::AgentsSettingsUi,
    // ---- WS-B
    pub mcp: crate::mcp_ui::McpUi,
    // ---- WS-C
    pub timeline: crate::timeline_ui::TimelineUi,
    pub trust: crate::trust_ui::TrustSettingsUi,
    // ---- WS-D
    pub agent_panel: crate::agent_panel_ui::AgentPanelUi,
    pub ai_settings: crate::ai_settings_ui::AiSettingsUi,
    // ---- WS-E
    pub agenda: crate::agenda_ui::AgendaUi,
    pub calendar: crate::calendar_ui::CalendarUi,
    // ---- WS-F
    pub themes: crate::theme_ui::ThemeUi,
    pub git: crate::git_ui::GitUi,
    // ---- WS-G
    pub canvas: crate::canvas_ui::CanvasUi,
    // ---- 0.11 editor: Settings → Commands and Toolbar, the toolbar, Insights rows
    pub commands: crate::commands_ui::CommandsUi,
    pub toolbar: crate::toolbar_ui::ToolbarUi,
    pub insights: crate::panels::InsightsRows,
    // ---- 0.12 typed links: the Hierarchy tab
    pub hierarchy: crate::links_ui::HierarchyUi,
    // ---- 0.13 typed links: Settings → Links
    pub links_settings: crate::links_ui::LinksSettingsUi,
    // ---- 0.12 switching tools: Settings → Linter and Home
    pub switch: crate::lint_ui::SwitchUi,
    // ---- app
    /// The file of the surface in the central panel (a `.base`, a canvas,
    /// a PDF), set each frame; `None` while a note is shown.
    pub active_surface: Option<NotePath>,
}

/// The built-in right-panel tab.
pub(crate) const INSIGHTS: &str = "insights";

fn always(_ws: &Workspace) -> bool {
    true
}

macro_rules! label {
    ($text:literal) => {{
        fn label(_ws: &Workspace) -> String {
            $text.to_string()
        }
        label
    }};
}

fn insights_tab(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    // A base, canvas or PDF in the middle: about that file, never the note
    // that was open before it.
    if let Some(path) = st.active_surface.clone() {
        crate::panels::surface_insights(ui, ws, &path, pal);
        return PanelOut::default();
    }
    crate::panels::insights(ui, ws, pal, &mut st.insights)
}

pub(crate) const RIGHT_TABS: &[TabDef] = &[
    // ---- F0 (built in)
    TabDef { id: INSIGHTS, label: label!("Insights"), available: always, show: insights_tab },
    // ---- WS-A
    TabDef { id: "inbox", label: crate::inbox_ui::label, available: crate::inbox_ui::available, show: crate::inbox_ui::show },
    TabDef { id: "activity", label: label!("Activity"), available: crate::activity_ui::available, show: crate::activity_ui::show },
    // ---- WS-C
    TabDef { id: "timeline", label: label!("Timeline"), available: crate::timeline_ui::available, show: crate::timeline_ui::show },
    // ---- WS-D
    TabDef { id: "agent", label: label!("Agent"), available: crate::agent_panel_ui::available, show: crate::agent_panel_ui::show },
    // ---- WS-E
    TabDef { id: "agenda", label: label!("Agenda"), available: crate::agenda_ui::available, show: crate::agenda_ui::show },
    // ---- WS-F
    TabDef { id: "git", label: crate::git_ui::label, available: crate::git_ui::available, show: crate::git_ui::show },
];

pub(crate) const SIDEBAR_TABS: &[TabDef] = &[
    // ---- WS-E
    TabDef { id: "agenda", label: label!("Agenda"), available: crate::agenda_ui::available, show: crate::agenda_ui::show },
    TabDef { id: "calendar", label: label!("Calendar"), available: crate::calendar_ui::available, show: crate::calendar_ui::show },
    // ---- 0.12 typed links
    TabDef {
        id: "hierarchy",
        label: label!("Hierarchy"),
        available: crate::links_ui::hierarchy_available,
        show: crate::links_ui::hierarchy,
    },
];

/// Feature tabs of Settings, after the built-in Editor, AI & search, Sync
/// and About tabs. WS-D's rebuilt AI tab replaces the built-in one by
/// registering `id: "ai"` here.
pub(crate) const SETTINGS_TABS: &[TabDef] = &[
    // ---- WS-A
    TabDef { id: "agents", label: label!("Agents"), available: crate::agents_settings::available, show: crate::agents_settings::show },
    // ---- WS-B (HTTP MCP and tokens: a section of the Agents tab, or a tab here)
    TabDef { id: "mcp", label: label!("Connections"), available: crate::mcp_ui::available, show: crate::mcp_ui::show },
    // ---- WS-C: Ask, citations, staleness and contradictions are in AI → Features
    // ---- WS-D: replaces the built-in "AI & search" tab
    TabDef { id: "ai", label: label!("AI"), available: crate::ai_settings_ui::available, show: crate::ai_settings_ui::show },
    // ---- WS-E
    TabDef { id: "planning", label: label!("Planning"), available: crate::agenda_ui::available, show: crate::agenda_ui::settings_tab },
    // ---- WS-F
    TabDef { id: "editing", label: label!("Editing"), available: crate::view::settings::available, show: crate::view::settings::show },
    TabDef { id: "appearance", label: label!("Appearance"), available: crate::theme_ui::available, show: crate::theme_ui::show },
    // ---- WS-G
    TabDef {
        id: "citations",
        label: label!("Citations"),
        available: crate::cite_ui::settings_available,
        show: crate::cite_ui::settings_tab,
    },
    // ---- 0.11 editor
    TabDef { id: "commands", label: label!("Commands"), available: crate::commands_ui::available, show: crate::commands_ui::show },
    TabDef { id: "toolbar", label: label!("Toolbar"), available: crate::commands_ui::available, show: crate::commands_ui::toolbar_tab },
    // ---- 0.12 switching tools
    TabDef { id: "linter", label: label!("Linter"), available: crate::lint_ui::available, show: crate::lint_ui::settings_tab },
    TabDef { id: "home", label: label!("Home"), available: crate::home_ui::available, show: crate::home_ui::settings_tab },
    // ---- 0.13 typed links
    TabDef { id: "links", label: label!("Links"), available: crate::links_ui::settings_available, show: crate::links_ui::settings_tab },
];

/// A bar drawn above the note, below the find bar: full width, as tall as
/// it needs. `show` returns what the app should do next (`PanelOut`).
#[derive(Clone, Copy)]
pub(crate) struct BannerDef {
    pub id: &'static str,
    /// Drawn only while this says so (cheap: called every frame).
    pub available: fn(&Workspace) -> bool,
    pub show: ShowFn,
}

/// Bars above the note, top to bottom, by workstream.
pub(crate) const EDITOR_BANNERS: &[BannerDef] = &[
    // ---- WS-A: "Claude Code suggests 4 changes · Why … · ◀ Prev  Next ▶ · …"
    BannerDef { id: "review", available: crate::inbox_ui::review_ui::banner_available, show: crate::inbox_ui::review_ui::banner },
    // ---- WS-C: "2 citations in this note no longer match the cited text · Check again"
    BannerDef { id: "citations", available: crate::trust_ui::banner_available, show: crate::trust_ui::banner },
    // ---- WS-D: "New in 0.9: AI is your choice…" for people upgrading from 0.8
    BannerDef { id: "ai-upgrade", available: crate::onboarding_ui::banner_available, show: crate::onboarding_ui::banner },
    // ---- WS-G: a canvas open as text (for review): its changes in words
    BannerDef { id: "canvas-review", available: crate::canvas_ui::banner_available, show: crate::canvas_ui::banner },
    // ---- 0.10 people: an Excalidraw note's picture, read-only
    BannerDef { id: "drawing", available: crate::drawing_ui::banner_available, show: crate::drawing_ui::banner },
    // ---- 0.12 typed links: "Maths › Calculus › Limits"
    BannerDef { id: "trail", available: crate::links_ui::trail_available, show: crate::links_ui::trail },
];

/// A tooltip for the note's text under the pointer: `tip(ws, line, byte)`
/// gets the hovered line (without its newline) and the byte in it, and
/// says what is there, or `None`. Called while the pointer rests on the
/// text (keep it cheap).
#[derive(Clone, Copy)]
pub(crate) struct HoverDef {
    pub tip: fn(&Workspace, &str, usize) -> Option<String>,
}

/// Tooltips over the note's text, first match wins, by workstream.
pub(crate) const EDITOR_HOVERS: &[HoverDef] = &[
    // ---- WS-G: a citation's full reference
    HoverDef { tip: quire_app::citations::hover_text },
];

/// The tooltip for `byte` of `line`, if a feature has one.
pub(crate) fn hover_tip(ws: &Workspace, line: &str, byte: usize) -> Option<String> {
    EDITOR_HOVERS.iter().find_map(|h| (h.tip)(ws, line, byte))
}

/// Something to do once the Inbox has drawn (drawing borrows its data).
pub(crate) type InboxAct = Box<dyn FnOnce(&mut Workspace, &mut PanelOut)>;

/// How a feature's Inbox items read and what they offer. The Inbox draws a
/// card per item: `body` inside it, then a row with `actions` followed by
/// the Inbox's own Dismiss and Snooze. Items no entry claims get a plain
/// sentence and Open.
#[derive(Clone, Copy)]
pub(crate) struct InboxKindDef {
    /// Whether this entry draws `item` (cheap: called per item per frame).
    pub handles: fn(&InboxItem) -> bool,
    pub body: fn(&mut egui::Ui, &Workspace, &StoredItem, &Palette),
    /// The item's buttons; return what a click should do.
    pub actions: fn(&mut egui::Ui, &Workspace, &StoredItem, &Palette) -> Option<InboxAct>,
}

/// Inbox item kinds with their own text and actions, by workstream.
pub(crate) const INBOX_KINDS: &[InboxKindDef] = &[
    // ---- WS-C: contradictions, staleness, citations
    InboxKindDef { handles: crate::trust_ui::is_trust_item, body: crate::trust_ui::inbox_body, actions: crate::trust_ui::inbox_actions },
];

/// The tabs of `table` that are offered now.
pub(crate) fn offered<'a>(table: &'a [TabDef], ws: &Workspace) -> Vec<&'a TabDef> {
    table.iter().filter(|t| (t.available)(ws)).collect()
}

/// Makes a feature's dialog (it may read or prepare state in the workspace).
pub(crate) type MakeOverlay = fn(&mut Workspace) -> Box<dyn crate::overlays::OverlayView>;

/// Where a feature command's UI is.
pub(crate) enum UiTarget {
    Right(&'static str),
    Sidebar(&'static str),
    Settings(&'static str),
    /// A dialog in its own file, shown as `Overlay::Feature` (background
    /// jobs pause while it is open; Escape closes it).
    Overlay(MakeOverlay),
    /// A page in the tab bar (Home), shown like a `.base` file.
    Surface(fn() -> Box<dyn crate::surfaces::Surface>),
}

/// The UI a `UiRequest::Feature(cmd)` opens, by workstream.
pub(crate) const fn ui_target(cmd: Command) -> Option<UiTarget> {
    Some(match cmd {
        // ---- WS-A
        Command::OpenInbox | Command::LintVault => UiTarget::Right("inbox"),
        Command::ShowAgentActivity => UiTarget::Right("activity"),
        Command::UndoAgentChanges => UiTarget::Overlay(crate::activity_ui::RevertOverlay::make),
        // 0.11: Connections is a view of the Agents tab (`secure_state` picks it).
        Command::AgentSettings | Command::AgentConnections => UiTarget::Settings("agents"),
        // ---- WS-B
        Command::ConnectAgent => UiTarget::Settings("mcp"),
        Command::CaptureWebPage => UiTarget::Overlay(crate::mcp_ui::CaptureDialog::make),
        // ---- WS-C
        Command::ThinkingTimeline => UiTarget::Right("timeline"),
        // ---- WS-D
        Command::ToggleAgentPanel => UiTarget::Right("agent"),
        Command::AiSetup => UiTarget::Overlay(crate::onboarding_ui::OnboardingOverlay::make),
        Command::IngestIntoWiki => UiTarget::Overlay(crate::agent_panel_ui::ingest_ui::IngestDialog::make),
        Command::ConnectExternalAgent => UiTarget::Overlay(crate::agent_panel_ui::acp_ui::ConnectOverlay::make),
        // ---- 0.10 AI
        Command::RecordVoiceNote => UiTarget::Overlay(crate::voice_ui::VoiceDialog::make),
        // ---- WS-E
        Command::ShowAgenda => UiTarget::Sidebar("agenda"),
        Command::ShowCalendar => UiTarget::Sidebar("calendar"),
        Command::RenameTag | Command::RenameProperty => UiTarget::Overlay(crate::meta_refactor_ui::RefactorOverlay::make),
        Command::StartReview | Command::ReviewThisNote => UiTarget::Overlay(crate::srs_ui::ReviewOverlay::make),
        // ---- 0.11 tasks
        Command::NewTask => UiTarget::Overlay(crate::task_ui::NewTaskDialog::make),
        Command::TimeReport => UiTarget::Overlay(crate::task_ui::TimeReportOverlay::make),
        // ---- WS-F
        Command::ChooseTheme => UiTarget::Settings("appearance"),
        // ---- WS-G
        Command::InsertCitation => UiTarget::Overlay(crate::cite_ui::CitePicker::make),
        Command::ImportBibliography => UiTarget::Overlay(crate::cite_ui::ImportDialog::make),
        // ---- 0.11 security
        Command::LockFolder => UiTarget::Overlay(crate::lock_ui::LockDialog::make_lock),
        Command::UnlockFolder => UiTarget::Overlay(crate::lock_ui::LockDialog::make_unlock),
        Command::ChangeFolderPassphrase => UiTarget::Overlay(crate::lock_ui::LockDialog::make_change),
        Command::StopLockingFolder => UiTarget::Overlay(crate::lock_ui::LockDialog::make_stop),
        Command::ShowRecoveryKey => UiTarget::Overlay(crate::lock_ui::LockDialog::make_show_key),
        // ---- 0.11 editor
        Command::CustomizeCommands => UiTarget::Settings("commands"),
        // ---- 0.12 typed links
        Command::AddTypedLink => UiTarget::Overlay(crate::links_ui::AddLinkDialog::make),
        Command::ShowHierarchy => UiTarget::Sidebar("hierarchy"),
        // ---- 0.12 switching tools
        Command::LintFolder => UiTarget::Overlay(crate::lint_ui::LintFolderDialog::make),
        Command::OpenHome => UiTarget::Surface(crate::home_ui::HomeSurface::make),
        _ => return None,
    })
}

/// Whether `id` is offered in `table` (built-in settings tabs always are).
pub(crate) fn tab_offered(table: &[TabDef], id: &str, ws: &Workspace) -> bool {
    table.iter().any(|t| t.id == id && (t.available)(ws))
}

/// Tell the person a feature's UI isn't there yet.
pub(crate) fn not_available(ws: &mut Workspace, cmd: Command) {
    ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…')));
}

/// Draw a feature's widget under a line, in `ui` (as wide as the text).
/// Returns the height used and an action if one was clicked.
pub(crate) fn paint_widget(ui: &mut egui::Ui, ws: &Workspace, id: WidgetId, pal: &Palette) -> (f32, Option<ActionId>) {
    // Route by `id.owner` to the owner's painter, one arm per workstream:
    // ---- WS-A review toolbars · WS-C citation drift · WS-E task widgets
    // ---- WS-F embedded editors · WS-G canvas and PDF previews
    match id.owner {
        // ---- WS-A
        quire_app::Owner::Agent => crate::inbox_ui::review_ui::paint_widget(ui, ws, id, pal),
        // ---- WS-C: notes under drifted citations
        quire_app::Owner::Trust => crate::trust_ui::paint_widget(ui, ws, id, pal),
        // ---- WS-E: Dataview/Tasks results, Kanban boards, template fields
        quire_app::Owner::Plan => crate::agenda_ui::paint_widget(ui, ws, id, pal),
        quire_app::Owner::Core => (0.0, None),
        _ => {
            let _ = (ui, ws, id, pal);
            (0.0, None)
        }
    }
}

/// Whether a file that isn't a note has an editor here (the Files tab
/// lists those files, and `UiRequest::OpenFile` opens them).
pub(crate) fn has_surface(path: &NotePath) -> bool {
    !path.is_markdown() && open_surface(path).is_some()
}

/// An editor for a non-Markdown file, by extension.
pub(crate) fn open_surface(path: &NotePath) -> Option<Box<dyn crate::surfaces::Surface>> {
    let ext = path.as_str().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        // ---- WS-E: `.base` (Obsidian Bases)
        "base" => Some(Box::new(crate::base_surface::BaseSurface::new(path))),
        // ---- WS-G: `.canvas`, `.pdf`, sketches (`.svg`)
        "canvas" => Some(Box::new(crate::canvas_ui::CanvasSurface::new(path))),
        "pdf" => Some(Box::new(crate::pdf_ui::PdfSurface::new(path))),
        "svg" => Some(Box::new(crate::canvas_ui::SketchSurface::new(path))),
        // ---- 0.10 people: Excalidraw drawings (`.excalidraw.md` opens as a note with its picture)
        "excalidraw" => Some(Box::new(crate::drawing_ui::DrawingSurface::new(path))),
        // ---- 0.11 security: a locked folder's file opens to its lock screen
        "qenc" => Some(Box::new(crate::lock_ui::LockedSurface::new(path))),
        _ => None,
    }
}

/// Every feature overlay in a state worth checking, for `layout_check`.
/// Each workstream adds its dialogs (in every interesting state) to its block.
#[cfg(test)]
pub(crate) fn layout_samples(ws: &mut Workspace) -> Vec<(&'static str, crate::overlays::Overlay)> {
    use crate::overlays::Overlay;
    vec![
        // ---- F0
        ("shortcuts (feature overlay)", Overlay::Feature(Box::new(crate::overlays::ShortcutsOverlay::default()))),
        // ---- WS-A
        ("undo agent changes", Overlay::Feature(crate::activity_ui::RevertOverlay::make(ws))),
        ("undo agent changes: plan", Overlay::Feature(Box::new(crate::activity_ui::RevertOverlay::sample()))),
        // ---- WS-B
        ("mcp: consent", crate::mcp_ui::layout_sample(0)),
        ("mcp: consent, error", crate::mcp_ui::layout_sample(1)),
        ("capture web page", crate::mcp_ui::layout_sample(2)),
        ("capture web page, error", crate::mcp_ui::layout_sample(3)),
        // ---- WS-C
        ("ask", Overlay::Feature(Box::new(crate::ask_ui::AskOverlay::sample(crate::ask_ui::samples::empty)))),
        ("ask: what do you expect", Overlay::Feature(Box::new(crate::ask_ui::AskOverlay::sample(crate::ask_ui::samples::expect)))),
        ("ask: evidence", Overlay::Feature(Box::new(crate::ask_ui::AskOverlay::sample(crate::ask_ui::samples::evidence)))),
        ("ask: answer", Overlay::Feature(Box::new(crate::ask_ui::AskOverlay::sample(crate::ask_ui::samples::answer)))),
        ("ask: not found", Overlay::Feature(Box::new(crate::ask_ui::AskOverlay::sample(crate::ask_ui::samples::not_found)))),
        // ---- WS-D
        ("set up AI: choose", Overlay::Feature(crate::onboarding_ui::OnboardingOverlay::make(ws))),
        ("set up AI: chosen", Overlay::Feature(Box::new(crate::onboarding_ui::OnboardingOverlay::sample(1)))),
        ("set up AI: on this computer", Overlay::Feature(Box::new(crate::onboarding_ui::OnboardingOverlay::sample(2)))),
        ("external agent: warning", Overlay::Feature(Box::new(crate::agent_panel_ui::acp_ui::ConnectOverlay::sample(false)))),
        ("external agent: acknowledged", Overlay::Feature(Box::new(crate::agent_panel_ui::acp_ui::ConnectOverlay::sample(true)))),
        ("external agent: run a command?", Overlay::Feature(Box::new(crate::agent_panel_ui::acp_ui::PermissionOverlay::sample("execute")))),
        ("external agent: read files?", Overlay::Feature(Box::new(crate::agent_panel_ui::acp_ui::PermissionOverlay::sample("read")))),
        ("ingest: choose", Overlay::Feature(Box::new(crate::agent_panel_ui::ingest_ui::IngestDialog::sample(0)))),
        ("ingest: running", Overlay::Feature(Box::new(crate::agent_panel_ui::ingest_ui::IngestDialog::sample(1)))),
        ("ingest: done", Overlay::Feature(Box::new(crate::agent_panel_ui::ingest_ui::IngestDialog::sample(2)))),
        // ---- 0.10 AI
        ("voice: ready", Overlay::Feature(Box::new(crate::voice_ui::VoiceDialog::sample(0)))),
        ("voice: recording", Overlay::Feature(Box::new(crate::voice_ui::VoiceDialog::sample(1)))),
        ("voice: transcribing", Overlay::Feature(Box::new(crate::voice_ui::VoiceDialog::sample(2)))),
        ("voice: failed", Overlay::Feature(Box::new(crate::voice_ui::VoiceDialog::sample(3)))),
        // ---- WS-E
        ("rename tag", Overlay::Feature(Box::new(crate::meta_refactor_ui::RefactorOverlay::sample(false, false)))),
        ("rename tag: preview", Overlay::Feature(Box::new(crate::meta_refactor_ui::RefactorOverlay::sample(false, true)))),
        ("change a property value: preview", Overlay::Feature(Box::new(crate::meta_refactor_ui::RefactorOverlay::sample(true, true)))),
        ("review: question", Overlay::Feature(Box::new(crate::srs_ui::ReviewOverlay::sample(false, false)))),
        ("review: answer", Overlay::Feature(Box::new(crate::srs_ui::ReviewOverlay::sample(true, false)))),
        ("review: done", Overlay::Feature(Box::new(crate::srs_ui::ReviewOverlay::sample(false, true)))),
        ("new note from template: questions", Overlay::Feature(Box::new(crate::template_prompt_ui::TemplateDialog::sample()))),
        // ---- 0.11 tasks
        ("new task", Overlay::Feature(Box::new(crate::task_ui::NewTaskDialog::sample(false)))),
        ("new task: error", Overlay::Feature(Box::new(crate::task_ui::NewTaskDialog::sample(true)))),
        ("time this week", Overlay::Feature(crate::task_ui::TimeReportOverlay::make(ws))),
        // ---- WS-F
        // ---- 0.11 editor
        ("diagram, full screen", Overlay::Feature(Box::new(crate::diagram_ui::DiagramOverlay::new("graph TD; A-->B".into())))),
        // ---- WS-G
        ("insert citation: no library", Overlay::Feature(Box::new(crate::cite_ui::CitePicker::sample(false)))),
        ("insert citation", Overlay::Feature(Box::new(crate::cite_ui::CitePicker::sample(true)))),
        ("import bibliography", Overlay::Feature(Box::new(crate::cite_ui::ImportDialog::sample(false)))),
        ("import bibliography: dry run", Overlay::Feature(Box::new(crate::cite_ui::ImportDialog::sample(true)))),
        // ---- 0.11 security
        ("lock a folder", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(0)))),
        ("unlock a folder: wrong passphrase", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(1)))),
        ("change a folder's passphrase", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(2)))),
        ("stop locking a folder", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(3)))),
        ("locked folder: recovery key", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(4)))),
        ("show a locked folder's recovery key", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(5)))),
        ("unlock with the recovery key", Overlay::Feature(Box::new(crate::lock_ui::LockDialog::sample(6)))),
        (
            "quit with unsaved locked notes",
            Overlay::Feature(Box::new(crate::lock_ui::QuitDialog::new(vec!["2026-11-08".into(), "Letter to Sam".into()]))),
        ),
        // ---- 0.12 typed links
        ("add typed link: type", Overlay::Feature(Box::new(crate::links_ui::AddLinkDialog::sample(0)))),
        ("add typed link: note", Overlay::Feature(Box::new(crate::links_ui::AddLinkDialog::sample(1)))),
        // ---- 0.12 switching tools
        ("tidy a folder: choose", Overlay::Feature(Box::new(crate::lint_ui::LintFolderDialog::sample(0)))),
        ("tidy a folder: review", Overlay::Feature(Box::new(crate::lint_ui::LintFolderDialog::sample(1)))),
        ("tidy a folder: done", Overlay::Feature(Box::new(crate::lint_ui::LintFolderDialog::sample(2)))),
    ]
}
