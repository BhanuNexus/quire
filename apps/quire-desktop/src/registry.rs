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
//! * [`ui_target`] — what a feature command's `UiRequest::Feature` opens
//! * [`paint_widget`] — widgets a feature puts under a line of a note
//! * [`open_surface`] — editors for non-Markdown files (`surfaces.rs`)
//! * `layout_samples` — every feature overlay, for `layout_check` (tests)
//!
//! Each table is in workstream order; add to your block only.

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
    // ---- WS-C
    pub timeline: crate::timeline_ui::TimelineUi,
    // ---- WS-D
    pub agent_panel: crate::agent_panel_ui::AgentPanelUi,
    // ---- WS-E
    pub agenda: crate::agenda_ui::AgendaUi,
    pub calendar: crate::calendar_ui::CalendarUi,
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

fn insights_tab(ui: &mut egui::Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    crate::panels::insights(ui, ws, pal)
}

pub(crate) const RIGHT_TABS: &[TabDef] = &[
    // ---- F0 (built in)
    TabDef { id: INSIGHTS, label: label!("Insights"), available: always, show: insights_tab },
    // ---- WS-A
    TabDef { id: "inbox", label: label!("Inbox"), available: crate::inbox_ui::available, show: crate::inbox_ui::show },
    TabDef { id: "activity", label: label!("Activity"), available: crate::activity_ui::available, show: crate::activity_ui::show },
    // ---- WS-C
    TabDef { id: "timeline", label: label!("Timeline"), available: crate::timeline_ui::available, show: crate::timeline_ui::show },
    // ---- WS-D
    TabDef { id: "agent", label: label!("Agent"), available: crate::agent_panel_ui::available, show: crate::agent_panel_ui::show },
    // ---- WS-E
    TabDef { id: "agenda", label: label!("Agenda"), available: crate::agenda_ui::available, show: crate::agenda_ui::show },
];

pub(crate) const SIDEBAR_TABS: &[TabDef] = &[
    // ---- WS-E
    TabDef { id: "agenda", label: label!("Agenda"), available: crate::agenda_ui::available, show: crate::agenda_ui::show },
    TabDef { id: "calendar", label: label!("Calendar"), available: crate::calendar_ui::available, show: crate::calendar_ui::show },
];

/// Feature tabs of Settings, after the built-in Editor, AI & search, Sync
/// and About tabs. WS-D's rebuilt AI tab replaces the built-in one by
/// registering `id: "ai"` here.
pub(crate) const SETTINGS_TABS: &[TabDef] = &[
    // ---- WS-A
    TabDef { id: "agents", label: label!("Agents"), available: crate::agents_settings::available, show: crate::agents_settings::show },
    // ---- WS-B (HTTP MCP and tokens: a section of the Agents tab, or a tab here)
    // ---- WS-D
    // ---- WS-F
];

/// The tabs of `table` that are offered now.
pub(crate) fn offered<'a>(table: &'a [TabDef], ws: &Workspace) -> Vec<&'a TabDef> {
    table.iter().filter(|t| (t.available)(ws)).collect()
}

/// Where a feature command's UI is.
pub(crate) enum UiTarget {
    Right(&'static str),
    Sidebar(&'static str),
    Settings(&'static str),
}

/// The UI a `UiRequest::Feature(cmd)` opens, by workstream.
pub(crate) const fn ui_target(cmd: Command) -> Option<UiTarget> {
    Some(match cmd {
        // ---- WS-A
        Command::OpenInbox => UiTarget::Right("inbox"),
        Command::ShowAgentActivity => UiTarget::Right("activity"),
        Command::AgentSettings => UiTarget::Settings("agents"),
        // ---- WS-C
        Command::ThinkingTimeline => UiTarget::Right("timeline"),
        // ---- WS-D
        Command::ToggleAgentPanel => UiTarget::Right("agent"),
        Command::AiSetup => UiTarget::Settings("ai"),
        // ---- WS-E
        Command::ShowAgenda => UiTarget::Sidebar("agenda"),
        Command::ShowCalendar => UiTarget::Sidebar("calendar"),
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
    let _ = (ui, ws, id, pal);
    (0.0, None)
}

/// An editor for a non-Markdown file, by extension.
#[cfg_attr(not(test), expect(dead_code, reason = "WS-E and WS-G open surfaces from the file tree and links"))]
pub(crate) fn open_surface(path: &NotePath) -> Option<Box<dyn crate::surfaces::Surface>> {
    let ext = path.as_str().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        // ---- WS-E: `.base` (Obsidian Bases)
        "base" => Some(Box::new(crate::surfaces::SystemViewer::new(path, "Bases views"))),
        // ---- WS-G: `.canvas`, `.pdf`
        "canvas" => Some(Box::new(crate::surfaces::SystemViewer::new(path, "Canvas editing"))),
        "pdf" => Some(Box::new(crate::surfaces::SystemViewer::new(path, "The PDF viewer"))),
        _ => None,
    }
}

/// Every feature overlay in a state worth checking, for `layout_check`.
/// Each workstream adds its dialogs (in every interesting state) to its block.
#[cfg(test)]
pub(crate) fn layout_samples(ws: &Workspace) -> Vec<(&'static str, crate::overlays::Overlay)> {
    use crate::overlays::Overlay;
    let _ = ws;
    vec![
        // ---- F0
        ("shortcuts (feature overlay)", Overlay::Feature(Box::new(crate::overlays::ShortcutsOverlay::default()))),
        // ---- WS-A
        // ---- WS-B
        // ---- WS-C
        // ---- WS-D
        // ---- WS-E
        // ---- WS-F
        // ---- WS-G
    ]
}
