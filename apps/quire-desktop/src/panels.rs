// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tab bar, sidebar (files / search / tags / tasks), insights panel, status
//! bar, preview and the welcome screen.

use std::collections::BTreeMap;

use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use quire_app::index::HybridHit;
use quire_app::intel::llm::Assist;
use quire_app::markdown::render::{for_preview, parse_note_url};
use quire_app::vault::NotePath;
use quire_app::{Command, Workspace};

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SidebarTab {
    #[default]
    Files,
    Search,
    Tags,
    Tasks,
    /// A tab registered in `registry::SIDEBAR_TABS`, by id.
    Feature(&'static str),
}

#[derive(Default)]
pub(crate) struct SidebarState {
    pub tab: SidebarTab,
    pub query: String,
    pub focus_query: bool,
    cache: Option<(String, u64, u64, Vec<HybridHit>)>,
    /// The file of the surface in front of the notes (set every frame).
    pub active_surface: Option<NotePath>,
    /// Pages of PDFs (and images) matching the same query.
    page_hits: Vec<quire_app::index::SectionHit>,
    open_tag: Option<String>,
    /// 0.12: nested tags (`a/b`) folded under their parent in the Tags tab.
    collapsed_tags: std::collections::HashSet<String>,
    /// Two-pane browser: the folder whose notes are listed.
    browse_folder: Option<String>,
    /// The two-pane setting last frame (turning it on shows the Files tab).
    two_pane_seen: bool,
    /// The Files tab's tree, sorted, kept while nothing it shows changed:
    /// building and sorting it every frame took most of a frame in a
    /// 10,000-note vault (0.10 perf).
    files: Option<FilesCache>,
}

/// What the Files tab lists, and what it was built from.
struct FilesCache {
    key: (u64, u64, Vec<NotePath>, Option<NotePath>),
    /// Every listed path (notes and files with an editor of their own).
    count: usize,
    tree: BTreeMap<String, Vec<NotePath>>,
    /// The tree's folders with their notes in the folder's sort order.
    ordered: Vec<(String, Vec<NotePath>)>,
}

/// What a panel asks the app to do after drawing.
#[derive(Default)]
pub(crate) struct PanelOut {
    pub run: Option<Command>,
    /// Focus the editor and scroll to the caret.
    pub jumped: bool,
    /// Scroll to the caret without taking focus.
    pub reveal_only: bool,
    /// Hide the available update until a newer one appears.
    pub skip_update: bool,
    /// Open this note in the other half of a split editor.
    pub open_side: Option<NotePath>,
    /// Copy this note as HTML.
    pub copy_html: Option<NotePath>,
}

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).size(11.0).color(pal.muted).strong());
    ui.add_space(2.0);
}

// ---------------------------------------------------------------- merge banner

/// Shown above a note whose outside change conflicts with unsaved edits.
pub(crate) fn merge_banner(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(doc) = ws.active_doc() else { return out };
    let Some(m) = &doc.pending_merge else { return out };
    let (id, n) = (doc.id, m.conflicts);
    let mut choice = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!(
                "⚠ This note was also changed elsewhere. {n} change{} conflict{} with your unsaved edits. Autosave is paused.",
                if n == 1 { "" } else { "s" },
                if n == 1 { "s" } else { "" }
            ))
            .color(pal.text),
        );
        if ui.button("Keep mine").on_hover_text("Save your version; the other one stays in version history").clicked() {
            choice = Some(quire_app::sync::Resolution::KeepMine);
        }
        if ui.button("Take theirs").on_hover_text("Use the other version (Undo brings yours back)").clicked() {
            choice = Some(quire_app::sync::Resolution::TakeTheirs);
        }
        if ui.button("Keep both").on_hover_text("Put both versions in the note between <<<<<<< and >>>>>>> lines").clicked() {
            choice = Some(quire_app::sync::Resolution::MergeWithMarkers);
        }
    });
    if let Some(how) = choice {
        ws.resolve_merge(id, how);
        out.jumped = true;
    }
    out
}

// ---------------------------------------------------------------- note menu

#[derive(Clone, Copy)]
enum NoteAction {
    OpenSide,
    Rename,
    Move,
    History,
    CopyLink,
    CopyHtml,
    Reveal,
    Trash,
}

/// Right-click menu shared by sidebar entries and tabs.
fn note_menu(resp: &egui::Response) -> Option<NoteAction> {
    let mut act = None;
    resp.context_menu(|ui| {
        for (label, a) in [
            ("Open to the side", NoteAction::OpenSide),
            ("Rename…", NoteAction::Rename),
            ("Move to folder…", NoteAction::Move),
            ("Version history…", NoteAction::History),
            ("Copy [[link]]", NoteAction::CopyLink),
            ("Copy as HTML", NoteAction::CopyHtml),
            ("Show in folder", NoteAction::Reveal),
        ] {
            if ui.button(label).clicked() {
                act = Some(a);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Move to trash…").clicked() {
            act = Some(NoteAction::Trash);
            ui.close();
        }
    });
    act
}

fn apply_note_action(ctx: &egui::Context, ws: &mut Workspace, path: &NotePath, act: NoteAction, out: &mut PanelOut) {
    match act {
        NoteAction::OpenSide => out.open_side = Some(path.clone()),
        NoteAction::CopyHtml => out.copy_html = Some(path.clone()),
        NoteAction::CopyLink => {
            ctx.copy_text(format!("[[{}]]", path.stem()));
            ws.notify(quire_app::workspace::NoticeLevel::Info, format!("Copied [[{}]]", path.stem()));
        }
        NoteAction::Reveal => {
            if let Some(dir) = ws.vault_root().map(|r| r.join(path.folder())) {
                ctx.open_url(egui::OpenUrl::new_tab(format!("file://{}", dir.display())));
            }
        }
        NoteAction::Rename | NoteAction::Move | NoteAction::History | NoteAction::Trash => {
            // These act on the open note, so open it first.
            if ws.active_doc().and_then(|d| d.path.as_ref()) != Some(path) && ws.open_note(path).is_none() {
                return;
            }
            out.run = Some(match act {
                NoteAction::Rename => Command::RenameNote,
                NoteAction::Move => Command::MoveNote,
                NoteAction::History => Command::ShowHistory,
                _ => Command::DeleteNote,
            });
        }
    }
}

// ---------------------------------------------------------------- tab bar

pub(crate) fn tab_bar(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
    crate::theme::sync(ui.ctx(), ws);
    let mut out = PanelOut::default();
    // Names for the tabs: display titles and icons.
    let names: Vec<String> = ws
        .docs()
        .iter()
        .map(|d| match &d.path {
            Some(p) => {
                let name = quire_app::nav::display_name(ws, p);
                match quire_app::themes::note_icon(ws, p) {
                    Some(icon) => format!("{icon} {name}"),
                    None => name,
                }
            }
            None => d.title(),
        })
        .collect();
    let preview = ws.features.nav.preview;
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new(ws.vault_name()).strong().color(pal.strong));
        ui.add_space(4.0);
        ui.separator();
        let mut close = None;
        let mut activate = None;
        let mut pin = None;
        let mut tab_action: Option<(usize, NotePath, NoteAction)> = None;
        egui::ScrollArea::horizontal().id_salt("tabs").max_width(ui.available_width() - 440.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, d) in ws.docs().iter().enumerate() {
                    let active = ws.active_index() == Some(i);
                    let dot = if d.conflict {
                        " ⚠"
                    } else if d.is_dirty() {
                        " •"
                    } else {
                        ""
                    };
                    let label = format!("{}{dot}", names.get(i).cloned().unwrap_or_else(|| d.title()));
                    // The preview tab (reuse mode) is in italics until it's kept.
                    let is_preview = preview == Some(d.id);
                    let text = if is_preview { RichText::new(label).italics() } else { RichText::new(label) };
                    let r = ui.selectable_label(active, text);
                    let r = match &d.path {
                        Some(p) if is_preview => r.on_hover_text(format!(
                            "{}\nPreview tab: the next note you open replaces it. Double-click or edit to keep it.",
                            p.as_str()
                        )),
                        Some(p) => r.on_hover_text(format!("{}\nDouble-click to rename · right-click for more", p.as_str())),
                        None => r,
                    };
                    if r.clicked() {
                        activate = Some(i);
                    }
                    if let Some(p) = &d.path {
                        if r.double_clicked() && is_preview {
                            pin = Some(d.id);
                        } else if r.double_clicked() {
                            tab_action = Some((i, p.clone(), NoteAction::Rename));
                        } else if let Some(a) = note_menu(&r) {
                            tab_action = Some((i, p.clone(), a));
                        }
                    }
                    if r.middle_clicked() {
                        close = Some(d.id);
                    }
                    if active && ui.small_button("×").on_hover_text("Close tab (Ctrl+W)").clicked() {
                        close = Some(d.id);
                    }
                }
            });
        });
        if let Some(i) = activate {
            ws.set_active(i);
            out.jumped = true;
        }
        if let Some(id) = pin {
            quire_app::nav::pin(ws, id);
        }
        if let Some(id) = close {
            ws.close(id);
        }
        if let Some((i, p, a)) = tab_action {
            ws.set_active(i);
            apply_note_action(ui.ctx(), ws, &p, a, &mut out);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.selectable_label(ws.settings.show_insights, "Insights").on_hover_text("Ctrl+Shift+I").clicked() {
                out.run = Some(Command::ToggleInsights);
            }
            if ui.selectable_label(ws.settings.show_preview, "Preview").on_hover_text("Ctrl+E").clicked() {
                out.run = Some(Command::TogglePreview);
            }
            let (palette_keys, help_keys) = if cfg!(target_os = "macos") { ("⌘K", "⌘/") } else { ("Ctrl+K", "Ctrl+/") };
            let search = egui::Button::new(RichText::new(format!("Search or run a command   {palette_keys}")).color(pal.muted).size(12.5))
                .min_size(egui::vec2(220.0, 24.0));
            if ui.add(search).clicked() {
                out.run = Some(Command::CommandPalette);
            }
            if ui.button("?").on_hover_text(format!("Keyboard shortcuts ({help_keys})")).clicked() {
                out.run = Some(Command::ShowShortcuts);
            }
        });
    });
    out
}

// ---------------------------------------------------------------- sidebar

fn folder_tree(paths: &[NotePath]) -> BTreeMap<String, Vec<NotePath>> {
    let mut m: BTreeMap<String, Vec<NotePath>> = BTreeMap::new();
    for p in paths {
        m.entry(p.folder().to_string()).or_default().push(p.clone());
    }
    for v in m.values_mut() {
        v.sort_by_key(|p| p.stem().to_lowercase());
    }
    m
}

/// What the Files tab asks for after drawing.
#[derive(Default)]
struct FileEvents {
    /// A note to open, and whether in a tab of its own (Ctrl+click, middle click).
    open: Option<(NotePath, bool)>,
    file_action: Option<(NotePath, NoteAction)>,
    folder_action: Option<(String, Command)>,
    /// Keep this tab (double-click on the preview tab's note).
    pin: Option<quire_app::DocId>,
    /// (folder, mode, the folder's notes as shown).
    sort: Option<(String, quire_app::nav::SortMode, Vec<NotePath>)>,
    /// Manual order: (folder, notes as shown, the note moved, the note it goes before).
    reorder: Option<(String, Vec<NotePath>, NotePath, Option<NotePath>)>,
    /// (folder, icon or "" to remove it).
    icon: Option<(String, String)>,
}

/// Icons offered for folders.
const FOLDER_ICONS: &[&str] = &["📁", "🗂", "📂", "📚", "📘", "📒", "📝", "🗒", "💡", "⭐", "🏠", "💼", "🧪", "🎯", "🗓", "📦"];

/// A note's row label: its icon and its name (or title).
fn note_label(ws: &Workspace, p: &NotePath) -> String {
    let name = quire_app::nav::display_name(ws, p);
    match quire_app::themes::note_icon(ws, p) {
        Some(icon) => format!("{icon} {name}"),
        None => name,
    }
}

/// One note in the Files tab. In a manually ordered folder rows can be
/// dragged; a drop puts the dragged note before this one.
/// The height of a row of the Files tab.
const NOTE_ROW_HEIGHT: f32 = 26.0;

fn note_row(ui: &mut egui::Ui, ws: &Workspace, p: &NotePath, active: Option<&NotePath>, draggable: bool, pal: &Palette) -> egui::Response {
    let is_active = active == Some(p);
    let label = note_label(ws, p);
    let r = crate::theme::row(ui, is_active, NOTE_ROW_HEIGHT, pal, |painter, rect| {
        let color = if is_active { pal.strong } else { pal.text };
        let mut job = LayoutJob::simple_singleline(label.clone(), FontId::proportional(13.5), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 20.0);
        let galley = painter.layout_job(job);
        let y = rect.center().y - galley.size().y / 2.0;
        painter.galley(egui::pos2(rect.left() + 10.0, y), galley, color);
    });
    let r = if draggable { ui.interact(r.rect, r.id, egui::Sense::click_and_drag()) } else { r };
    if draggable && r.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), p.clone());
    }
    if draggable && r.dnd_hover_payload::<NotePath>().is_some_and(|d| *d != *p) {
        let y = r.rect.top();
        ui.painter().line_segment([egui::pos2(r.rect.left(), y), egui::pos2(r.rect.right(), y)], egui::Stroke::new(2.0, pal.accent));
    }
    r.on_hover_text(p.as_str())
}

/// The pinned agent instructions: the file, and a badge that says whether
/// this device approved them.
fn instructions_row(ui: &mut egui::Ui, path: &NotePath, approved: bool, active: bool, pal: &Palette) -> egui::Response {
    let (badge, color) = if approved { ("Agent instructions", pal.muted) } else { ("Review changes", pal.warn) };
    let name = path.file_name().to_string();
    let r = crate::theme::row(ui, active, 26.0, pal, |painter, rect| {
        painter.text(
            egui::pos2(rect.left() + 10.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            &name,
            FontId::proportional(13.5),
            if active { pal.strong } else { pal.text },
        );
        painter.text(egui::pos2(rect.right() - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, badge, FontId::proportional(11.0), color);
    });
    let hint = if approved {
        "What agents read first. Edit it here; saving approves it on this device."
    } else {
        "Changed outside Quire: agents get it only after you approve it (Inbox)."
    };
    r.on_hover_text(hint)
}

/// Clicks on a note row: open (in a tab of its own with Ctrl or the middle
/// button), keep on double-click, the context menu.
fn note_row_events(ui: &egui::Ui, ws: &Workspace, r: &egui::Response, p: &NotePath, ev: &mut FileEvents) {
    let mods = ui.input(|i| i.modifiers);
    if r.clicked() {
        ev.open = Some((p.clone(), mods.command));
    }
    if r.middle_clicked() {
        ev.open = Some((p.clone(), true));
    }
    if r.double_clicked()
        && let Some(d) = ws.docs().iter().find(|d| d.path.as_ref() == Some(p))
    {
        ev.pin = Some(d.id);
    }
    if let Some(a) = note_menu(r) {
        ev.file_action = Some((p.clone(), a));
    }
}

/// The context menu of a folder: new note, sort order, icon.
fn folder_menu(ui: &mut egui::Ui, ws: &Workspace, folder: &str, shown: &[NotePath], ev: &mut FileEvents) {
    if ui.button("New note here").clicked() {
        ev.folder_action = Some((folder.to_string(), Command::NewInFolder));
        ui.close();
    }
    if ui.button("New project…").clicked() {
        ev.folder_action = Some((folder.to_string(), Command::NewProject));
        ui.close();
    }
    ui.separator();
    let current = quire_app::nav::sort_mode(ws, folder);
    ui.menu_button("Sort by", |ui| {
        for m in quire_app::nav::SortMode::ALL {
            if ui.selectable_label(m == current, m.label()).clicked() {
                ev.sort = Some((folder.to_string(), m, shown.to_vec()));
                ui.close();
            }
        }
    });
    if !folder.is_empty() {
        ui.menu_button("Icon", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.set_max_width(200.0);
                for icon in FOLDER_ICONS {
                    if ui.button(*icon).clicked() {
                        ev.icon = Some((folder.to_string(), (*icon).to_string()));
                        ui.close();
                    }
                }
            });
            if ws.features.themes.folder_icon(folder).is_some() && ui.button("No icon").clicked() {
                ev.icon = Some((folder.to_string(), String::new()));
                ui.close();
            }
        });
    }
}

/// A folder's name with its icon.
fn folder_label(ws: &Workspace, folder: &str) -> String {
    // 0.11: a locked folder shows its lock (crate::lock_ui).
    if let Some(badge) = crate::lock_ui::folder_badge(ws, folder) {
        return format!("{badge} {folder}");
    }
    match ws.features.themes.folder_icon(folder) {
        Some(icon) => format!("{icon} {folder}"),
        None => folder.to_string(),
    }
}

/// The notes of a folder in the folder's order.
fn sorted_notes(ws: &mut Workspace, folder: &str, notes: &[NotePath]) -> Vec<NotePath> {
    let mut v = notes.to_vec();
    quire_app::nav::sorted(ws, folder, &mut v);
    v
}

/// Folders and their notes, one tree.
fn file_tree(
    ui: &mut egui::Ui,
    ws: &mut Workspace,
    ordered: &[(String, Vec<NotePath>)],
    active: Option<&NotePath>,
    pal: &Palette,
    ev: &mut FileEvents,
) {
    let reveal = ws.features.nav.reveal_folder.take();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (folder, notes) in ordered {
            let manual = quire_app::nav::sort_mode(ws, folder) == quire_app::nav::SortMode::Manual;
            let mut draw = |ui: &mut egui::Ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let dragging = egui::DragAndDrop::has_any_payload(ui.ctx());
                for (i, p) in notes.iter().enumerate() {
                    // Rows out of view only take their space: a 10,000-note
                    // vault laid every row out every frame (0.10 perf).
                    let next = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), NOTE_ROW_HEIGHT));
                    if !ui.is_rect_visible(next) && !dragging && active != Some(p) && i + 1 < notes.len() {
                        ui.allocate_exact_size(next.size(), egui::Sense::hover());
                        continue;
                    }
                    let r = note_row(ui, ws, p, active, manual, pal);
                    note_row_events(ui, ws, &r, p, ev);
                    if manual && let Some(dragged) = r.dnd_release_payload::<NotePath>() {
                        if dragged.folder() == p.folder() && *dragged != *p {
                            ev.reorder = Some((folder.clone(), notes.clone(), (*dragged).clone(), Some(p.clone())));
                        }
                    } else if manual && i + 1 == notes.len() {
                        // Below the last row: put the dragged note last.
                        let (rest, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
                        if let Some(dragged) = resp.dnd_release_payload::<NotePath>()
                            && dragged.folder() == p.folder()
                        {
                            ev.reorder = Some((folder.clone(), notes.clone(), (*dragged).clone(), None));
                        }
                        if resp.dnd_hover_payload::<NotePath>().is_some() {
                            ui.painter().line_segment([rest.left_top(), rest.right_top()], egui::Stroke::new(2.0, pal.accent));
                        }
                    }
                }
            };
            if folder.is_empty() {
                draw(ui);
                continue;
            }
            let revealing = reveal.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(folder));
            let mut header = egui::CollapsingHeader::new(RichText::new(folder_label(ws, folder)).color(pal.muted).size(12.5))
                .id_salt(folder)
                .default_open(true);
            if revealing {
                header = header.open(Some(true));
            }
            let resp = header.show(ui, draw);
            if revealing {
                let r = resp.header_response.rect;
                ui.painter().rect_stroke(r.expand(2.0), 6.0, egui::Stroke::new(1.5, pal.accent), egui::StrokeKind::Outside);
                ui.scroll_to_rect(r, Some(egui::Align::TOP));
            }
            resp.header_response.context_menu(|ui| folder_menu(ui, ws, folder, notes, ev));
        }
    });
}

/// The two-pane browser: folders on the left, the folder's notes on the
/// right with their title, date and the start of their text.
fn two_pane(
    ui: &mut egui::Ui,
    ws: &mut Workspace,
    st: &mut SidebarState,
    tree: &BTreeMap<String, Vec<NotePath>>,
    active: Option<&NotePath>,
    pal: &Palette,
    ev: &mut FileEvents,
) {
    if let Some(f) = ws.features.nav.reveal_folder.take() {
        st.browse_folder = Some(f);
    }
    let folder = st
        .browse_folder
        .clone()
        .filter(|f| tree.contains_key(f))
        .unwrap_or_else(|| active.map(|p| p.folder().to_string()).unwrap_or_default());
    let notes = tree.get(&folder).map(|n| sorted_notes(ws, &folder, n)).unwrap_or_default();
    let times = quire_app::nav::note_times(ws, &notes);
    let full = ui.available_rect_before_wrap();
    let left_w = (full.width() * 0.38).clamp(90.0, 180.0);
    let left = egui::Rect::from_min_size(full.min, egui::vec2(left_w, full.height()));
    let right = egui::Rect::from_min_max(egui::pos2(full.min.x + left_w + 8.0, full.min.y), full.max);
    ui.painter().line_segment(
        [egui::pos2(left.right() + 4.0, full.top()), egui::pos2(left.right() + 4.0, full.bottom())],
        egui::Stroke::new(1.0, pal.border),
    );
    let mut l = ui.new_child(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt("quire-browse-folders").auto_shrink([false, false]).show(&mut l, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for f in tree.keys() {
            let depth = if f.is_empty() { 0 } else { f.matches('/').count() };
            let name = if f.is_empty() { "Notes".to_string() } else { f.rsplit('/').next().unwrap_or(f).to_string() };
            let label = match ws.features.themes.folder_icon(f) {
                Some(icon) => format!("{icon} {name}"),
                None => name,
            };
            let selected = *f == folder;
            let r = crate::theme::row(ui, selected, 24.0, pal, |painter, rect| {
                let mut job =
                    LayoutJob::simple_singleline(label.clone(), FontId::proportional(12.5), if selected { pal.strong } else { pal.text });
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 12.0 - depth as f32 * 10.0);
                let g = painter.layout_job(job);
                painter.galley(egui::pos2(rect.left() + 6.0 + depth as f32 * 10.0, rect.center().y - g.size().y / 2.0), g, pal.text);
            });
            if r.clicked() {
                st.browse_folder = Some(f.clone());
            }
            let shown = tree.get(f).cloned().unwrap_or_default();
            r.context_menu(|ui| folder_menu(ui, ws, f, &shown, ev));
        }
    });
    let mut r = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().id_salt(("quire-browse-notes", folder.as_str())).auto_shrink([false, false]).show(&mut r, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let ix = ws.index().read();
        let previews: Vec<String> = notes
            .iter()
            .map(|p| {
                let plain = ix.get(p).map(|r| r.plain.trim().replace('\n', " ")).unwrap_or_default();
                plain.chars().take(160).collect()
            })
            .collect();
        drop(ix);
        for (p, preview) in notes.iter().zip(&previews) {
            let is_active = active == Some(p);
            let title = note_label(ws, p);
            let date =
                times.get(p).map(|(m, _)| quire_app::chrono_fmt(*m).split(' ').next().unwrap_or_default().to_string()).unwrap_or_default();
            let resp = crate::theme::row(ui, is_active, 64.0, pal, |painter, rect| {
                let w = rect.width() - 16.0;
                let mut job = LayoutJob::simple_singleline(title.clone(), FontId::proportional(13.0), pal.strong);
                job.wrap = egui::text::TextWrapping::truncate_at_width(w);
                let g = painter.layout_job(job);
                painter.galley(egui::pos2(rect.left() + 8.0, rect.top() + 5.0), g, pal.strong);
                painter.text(
                    egui::pos2(rect.left() + 8.0, rect.top() + 23.0),
                    egui::Align2::LEFT_TOP,
                    &date,
                    FontId::proportional(11.0),
                    pal.muted,
                );
                let mut job = LayoutJob::simple(preview.clone(), FontId::proportional(11.5), pal.muted, w);
                job.wrap.max_rows = 2;
                job.wrap.break_anywhere = false;
                job.wrap.overflow_character = Some('…');
                let g = painter.layout_job(job);
                painter.galley(egui::pos2(rect.left() + 8.0, rect.top() + 37.0), g, pal.muted);
            });
            let resp = resp.on_hover_text(p.as_str());
            note_row_events(ui, ws, &resp, p, ev);
        }
        if notes.is_empty() {
            ui.label(RichText::new("No notes in this folder.").color(pal.muted).size(12.0));
        }
    });
    ui.allocate_rect(full, egui::Sense::hover());
}

fn highlighted(text: &str, ranges: &[std::ops::Range<usize>], pal: &Palette) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = TextFormat { font_id: FontId::proportional(12.5), color: pal.muted, ..Default::default() };
    let hl = TextFormat { color: pal.text, background: pal.mark_bg, ..normal.clone() };
    let mut pos = 0;
    for r in ranges {
        if r.start < pos || r.end > text.len() || !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end) {
            continue;
        }
        job.append(&text[pos..r.start], 0.0, normal.clone());
        job.append(&text[r.clone()], 0.0, hl.clone());
        pos = r.end;
    }
    job.append(&text[pos..], 0.0, normal);
    job.wrap.max_width = f32::INFINITY;
    job
}

pub(crate) fn sidebar(
    ui: &mut egui::Ui,
    ws: &mut Workspace,
    st: &mut SidebarState,
    features: &mut crate::registry::FeatureUi,
    pal: &Palette,
) -> PanelOut {
    let mut out = PanelOut::default();
    ui.add_space(6.0);
    let feature_tabs = crate::registry::offered(crate::registry::SIDEBAR_TABS, ws);
    if let SidebarTab::Feature(id) = st.tab
        && !feature_tabs.iter().any(|t| t.id == id)
    {
        st.tab = SidebarTab::Files;
    }
    ui.horizontal_wrapped(|ui| {
        for (tab, label) in
            [(SidebarTab::Files, "Files"), (SidebarTab::Search, "Search"), (SidebarTab::Tags, "Tags"), (SidebarTab::Tasks, "Tasks")]
        {
            if ui.selectable_value(&mut st.tab, tab, label).clicked() && tab == SidebarTab::Search {
                st.focus_query = true;
            }
        }
        for t in &feature_tabs {
            ui.selectable_value(&mut st.tab, SidebarTab::Feature(t.id), (t.label)(ws));
        }
    });
    ui.separator();
    // A followed `[[Folder/]]` link, or the two-pane browser just turned on: the Files tab.
    if ws.features.nav.reveal_folder.is_some() || (ws.settings.editing.two_pane && !st.two_pane_seen) {
        st.tab = SidebarTab::Files;
    }
    st.two_pane_seen = ws.settings.editing.two_pane;
    let active_path = st.active_surface.clone().or_else(|| ws.active_doc().and_then(|d| d.path.clone()));
    let mut open: Option<(NotePath, Option<usize>)> = None;
    let mut file_action: Option<(NotePath, NoteAction)> = None;
    let mut folder_action: Option<(String, Command)> = None;

    match st.tab {
        SidebarTab::Feature(id) => {
            if let Some(t) = feature_tabs.iter().find(|t| t.id == id) {
                out = (t.show)(ui, ws, features, pal);
            }
        }
        SidebarTab::Files => {
            ui.horizontal(|ui| {
                ui.menu_button("+ New", |ui| {
                    for (label, keys, cmd) in [
                        ("Note", "Ctrl+N", Command::NewNote),
                        ("Project…", "Ctrl+Alt+N", Command::NewProject),
                        ("Note in folder…", "", Command::NewInFolder),
                        ("From template…", "Ctrl+Shift+N", Command::NewFromTemplate),
                    ] {
                        if ui.add(egui::Button::new(label).shortcut_text(keys)).clicked() {
                            out.run = Some(cmd);
                            ui.close();
                        }
                    }
                });
                if ui.button("Today").on_hover_text("Alt+D").clicked() {
                    out.run = Some(Command::DailyNote);
                }
                if ui.button("Scratch").on_hover_text("Alt+S — never saved to the vault").clicked() {
                    out.run = Some(Command::Scratchpad);
                }
            });
            ui.add_space(4.0);
            // Files with an editor of their own (`.base`, `.canvas`, `.pdf`) next to the notes.
            let others: Vec<NotePath> = ws.other_files().into_iter().filter(crate::registry::has_surface).collect();
            let mut ev = FileEvents::default();
            // The agent instructions (`AGENTS.md`/`CLAUDE.md`) pinned on top (design §A.6).
            let instructions = ws.features.agent.inbox.instructions.as_ref().map(|i| (i.path.clone(), i.approved));
            if let Some((pinned, approved)) = &instructions {
                let r = instructions_row(ui, pinned, *approved, active_path.as_ref() == Some(pinned), pal);
                note_row_events(ui, ws, &r, pinned, &mut ev);
                ui.add_space(4.0);
            }
            let key = (ws.index().version(), ws.features.nav.sort_generation(), others, instructions.map(|(p, _)| p));
            let files = match st.files.take() {
                Some(c) if c.key == key => c,
                _ => {
                    let mut paths = ws.index().read().paths();
                    paths.extend(key.2.iter().cloned());
                    if let Some(pinned) = &key.3 {
                        paths.retain(|p| p != pinned);
                    }
                    let tree = folder_tree(&paths);
                    let ordered = tree.iter().map(|(f, n)| (f.clone(), sorted_notes(ws, f, n))).collect();
                    FilesCache { key, count: paths.len(), tree, ordered }
                }
            };
            if ws.settings.editing.two_pane {
                two_pane(ui, ws, st, &files.tree, active_path.as_ref(), pal, &mut ev);
            } else {
                file_tree(ui, ws, &files.ordered, active_path.as_ref(), pal, &mut ev);
            }
            let empty = files.count == 0;
            st.files = Some(files);
            if empty {
                ui.label(RichText::new("No notes yet. Press Ctrl+N.").color(pal.muted));
            }
            open = ev.open.map(|(p, pinned)| {
                ws.features.nav.open_pinned |= pinned;
                (p, None)
            });
            file_action = ev.file_action;
            folder_action = ev.folder_action;
            if let Some(id) = ev.pin {
                quire_app::nav::pin(ws, id);
            }
            if let Some((folder, mode, shown)) = ev.sort
                && let Err(e) = quire_app::nav::set_sort_mode(ws, &folder, mode, &shown)
            {
                ws.notify(quire_app::workspace::NoticeLevel::Error, format!("Couldn't save the order: {e}"));
            }
            if let Some((folder, shown, note, before)) = ev.reorder
                && let Err(e) = quire_app::nav::move_note_in_order(ws, &folder, &shown, &note, before.as_ref())
            {
                ws.notify(quire_app::workspace::NoticeLevel::Error, format!("Couldn't save the order: {e}"));
            }
            if let Some((folder, icon)) = ev.icon
                && let Err(e) = quire_app::themes::set_folder_icon(ws, &folder, &icon)
            {
                ws.notify(quire_app::workspace::NoticeLevel::Error, format!("Couldn't save the icon: {e}"));
            }
        }
        SidebarTab::Search => {
            let r =
                ui.add(egui::TextEdit::singleline(&mut st.query).hint_text("rope \"undo tree\" #tag -draft").desired_width(f32::INFINITY));
            if std::mem::take(&mut st.focus_query) {
                r.request_focus();
            }
            let (version, sv) = (ws.index().version(), ws.index().semantic_version());
            let stale = st.cache.as_ref().is_none_or(|(q, v, s, _)| *q != st.query || *v != version || *s != sv);
            if stale {
                let hits = ws.index().hybrid_search(&st.query, 50);
                st.page_hits = if st.query.trim().is_empty() || !ws.index().read().has_attachments() {
                    Vec::new()
                } else {
                    let filters = quire_app::index::SearchFilters { kind: quire_app::index::SearchKind::Attachments, ..Default::default() };
                    ws.index().search_sections(&st.query, &filters, 10)
                };
                st.cache = Some((st.query.clone(), version, sv, hits));
            }
            let hits = st.cache.as_ref().map(|c| c.3.clone()).unwrap_or_default();
            let meaning = hits.iter().filter(|h| h.by_meaning).count();
            let total = hits.len() + st.page_hits.len();
            let summary = match (st.query.trim().is_empty(), meaning) {
                (true, _) => String::new(),
                (false, 0) => format!("{total} results"),
                (false, m) => format!("{total} results · {m} by meaning (≈)"),
            };
            ui.label(RichText::new(summary).color(pal.muted).size(11.5));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for h in &hits {
                    let resp = ui
                        .vertical(|ui| {
                            let t = ui
                                .horizontal_wrapped(|ui| {
                                    let t = ui.add(
                                        egui::Label::new(RichText::new(&h.title).strong().color(pal.strong)).sense(egui::Sense::click()),
                                    );
                                    if h.by_meaning {
                                        ui.label(RichText::new("≈").color(pal.tag)).on_hover_text("Found by meaning, not exact words");
                                    }
                                    if let Some(hd) = h.heading.as_ref().filter(|hd| **hd != h.title) {
                                        ui.label(RichText::new(format!("› {hd}")).color(pal.muted).size(11.5));
                                    }
                                    t
                                })
                                .inner;
                            ui.add(egui::Label::new(highlighted(&h.snippet, &h.highlights, pal)).wrap());
                            ui.add_space(6.0);
                            t
                        })
                        .inner;
                    if resp.clicked() {
                        // Jump to the first occurrence of the first term.
                        let ix = ws.index().read();
                        let term = quire_app::index::Query::parse(&st.query).highlight_terms().into_iter().next();
                        let at = term.and_then(|t| ix.source(&h.path).and_then(|s| s.to_lowercase().find(&t))).or(h.offset);
                        open = Some((h.path.clone(), at));
                    }
                }
                // 0.11: notes of unlocked locked folders (crate::lock_ui).
                if let Some(p) = crate::lock_ui::search_section(ui, ws, &st.query, pal) {
                    open = Some((p, None));
                }
                let mut open_page = None;
                for h in &st.page_hits {
                    let resp = ui
                        .vertical(|ui| {
                            let t = ui
                                .horizontal_wrapped(|ui| {
                                    let t = ui.add(
                                        egui::Label::new(RichText::new(h.path.file_name()).strong().color(pal.strong))
                                            .sense(egui::Sense::click()),
                                    );
                                    if let Some(p) = h.page {
                                        ui.label(RichText::new(format!("p. {p}")).color(pal.muted).size(11.5));
                                    }
                                    if h.by_meaning {
                                        ui.label(RichText::new("≈").color(pal.tag)).on_hover_text("Found by meaning, not exact words");
                                    }
                                    t
                                })
                                .inner;
                            ui.add(egui::Label::new(RichText::new(&h.snippet).color(pal.muted)).wrap());
                            ui.add_space(6.0);
                            t
                        })
                        .inner;
                    if resp.clicked() {
                        open_page = Some((h.path.clone(), h.page));
                    }
                }
                if let Some((path, page)) = open_page {
                    let fragment = page.map(|p| format!("page={p}"));
                    if path.as_str().to_ascii_lowercase().ends_with(".pdf") {
                        quire_app::pdf_state::open_link(ws, &path, fragment.as_deref());
                    } else {
                        ws.request_ui(quire_app::workspace::UiRequest::OpenFile(path));
                    }
                }
            });
        }
        SidebarTab::Tags => {
            let tags = ws.index().read().tags();
            let mut tag_color: Option<(String, Option<quire_app::themes::Rgba>)> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (i, (tag, count)) in tags.iter().enumerate() {
                    // Under a folded parent: not shown (the list is in tree order).
                    if tag.match_indices('/').any(|(at, _)| st.collapsed_tags.contains(&tag[..at])) {
                        continue;
                    }
                    let depth = tag.matches('/').count();
                    let leaf = tag.rsplit('/').next().unwrap_or(tag);
                    let has_children =
                        tags.get(i + 1).is_some_and(|(n, _)| n.strip_prefix(tag.as_str()).is_some_and(|r| r.starts_with('/')));
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 14.0);
                        if has_children {
                            let folded = st.collapsed_tags.contains(tag);
                            let arrow = ui.add(
                                egui::Label::new(RichText::new(if folded { "▸" } else { "▾" }).color(pal.muted))
                                    .sense(egui::Sense::click()),
                            );
                            if arrow.on_hover_text(if folded { "Show nested tags" } else { "Hide nested tags" }).clicked()
                                && !st.collapsed_tags.remove(tag)
                            {
                                st.collapsed_tags.insert(tag.clone());
                            }
                        } else {
                            ui.add_space(12.0);
                        }
                        let selected = st.open_tag.as_deref() == Some(tag.as_str());
                        let color = ws.features.themes.tag_color(tag).map_or(pal.tag, crate::theme::color);
                        let r = ui.selectable_label(selected, RichText::new(format!("#{leaf}")).color(color));
                        if r.clicked() {
                            st.open_tag = if selected { None } else { Some(tag.clone()) };
                        }
                        r.context_menu(|ui| {
                            ui.label(RichText::new("Colour").size(12.0).color(pal.muted));
                            ui.horizontal_wrapped(|ui| {
                                ui.set_max_width(190.0);
                                for c in crate::theme_ui::TAG_COLORS {
                                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                                    ui.painter().circle_filled(rect.center(), 7.0, egui::Color32::from_rgb(c[0], c[1], c[2]));
                                    if resp.clicked() {
                                        tag_color = Some((tag.clone(), Some([c[0], c[1], c[2], 255])));
                                        ui.close();
                                    }
                                }
                            });
                            if ws.features.themes.tag_colors.contains_key(&tag.to_lowercase()) && ui.button("No colour").clicked() {
                                tag_color = Some((tag.clone(), None));
                                ui.close();
                            }
                        });
                        ui.label(RichText::new(count.to_string()).color(pal.muted).size(11.0));
                    });
                    if st.open_tag.as_deref() == Some(tag.as_str()) {
                        let notes = ws.index().read().notes_with_tag(tag);
                        for p in notes {
                            ui.horizontal(|ui| {
                                ui.add_space(depth as f32 * 14.0 + 18.0);
                                if ui.link(note_label(ws, &p)).clicked() {
                                    open = Some((p.clone(), None));
                                }
                            });
                        }
                    }
                }
                if tags.is_empty() {
                    ui.label(RichText::new("Tags like #idea or #area/topic show up here.").color(pal.muted));
                }
            });
            if let Some((tag, c)) = tag_color
                && let Err(e) = quire_app::themes::set_tag_color(ws, &tag, c)
            {
                ws.notify(quire_app::workspace::NoticeLevel::Error, format!("Couldn't save the tag colour: {e}"));
            }
        }
        SidebarTab::Tasks => {
            // 0.12: with the tasks of unlocked folders (memory only).
            let tasks = ws.with_notes(|n| n.tasks(false));
            ui.label(RichText::new(format!("{} open", tasks.len())).color(pal.muted).size(11.5));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut last: Option<NotePath> = None;
                for (p, t) in &tasks {
                    if last.as_ref() != Some(p) {
                        section(ui, pal, p.stem());
                        last = Some(p.clone());
                    }
                    let mut done = false;
                    ui.horizontal_wrapped(|ui| {
                        if ui.checkbox(&mut done, "").on_hover_text("Mark done").changed() && ws.open_note(p).is_some() {
                            ws.jump_to_byte(t.offset);
                            ws.run(Command::ToggleTask);
                            ws.save_all();
                        }
                        if ui.link(&t.text).clicked() {
                            open = Some((p.clone(), Some(t.offset)));
                        }
                    });
                }
            });
        }
    }

    if let Some((p, a)) = file_action {
        apply_note_action(ui.ctx(), ws, &p, a, &mut out);
    }
    if let Some((folder, cmd)) = folder_action {
        if cmd == Command::NewInFolder {
            // Straight into a new note there; the dialog is for picking.
            if let Err(e) = ws.new_note_in(&folder) {
                ws.notify(quire_app::workspace::NoticeLevel::Error, e);
            } else {
                out.jumped = true;
            }
        } else {
            out.run = Some(cmd);
        }
    }
    if let Some((p, _)) = open.as_ref().filter(|(p, _)| !p.is_markdown()) {
        // A `.base`, canvas or PDF: its surface (`registry::open_surface`).
        ws.request_ui(quire_app::workspace::UiRequest::OpenFile(p.clone()));
    } else if let Some((p, at)) = open
        && ws.open_note(&p).is_some()
    {
        if let Some(b) = at {
            ws.jump_to_byte(b);
        }
        out.jumped = true;
        // The preview tab is settled on the next frame.
        ui.ctx().request_repaint();
    }
    out
}

// ---------------------------------------------------------------- insights

/// Insights while a file that isn't a note is open (a `.base`, a canvas, a
/// PDF): what it is, and for a base its views. Notes' outline, backlinks
/// and related notes don't apply.
pub(crate) fn surface_insights(ui: &mut egui::Ui, ws: &Workspace, path: &NotePath, pal: &Palette) {
    egui::ScrollArea::vertical().id_salt("quire-surface-insights").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        ui.label(RichText::new(path.file_name()).strong().color(pal.strong));
        let ext = path.as_str().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let what = match ext.as_str() {
            "base" => "Obsidian Base: a saved view of notes and their properties",
            "canvas" => "Canvas",
            "pdf" => "PDF",
            "excalidraw" => "Excalidraw drawing",
            _ => "File",
        };
        ui.label(RichText::new(what).size(12.5).color(pal.muted));
        if !path.folder().is_empty() {
            ui.label(RichText::new(format!("In {}", path.folder())).size(12.0).color(pal.faint));
        }
        if ext == "base"
            && let Some(text) = ws.vault().and_then(|v| v.read(path).ok())
        {
            match quire_import::obsidian::base::parse(&text) {
                Ok(b) => {
                    section(ui, pal, &format!("Views · {}", b.views.len()));
                    for v in &b.views {
                        let kind = v.kind.name();
                        let cols = v.columns.len();
                        ui.label(
                            RichText::new(format!("{} · {kind} · {cols} column{}", v.name, if cols == 1 { "" } else { "s" })).size(12.5),
                        );
                        for u in &v.unsupported {
                            ui.label(RichText::new(format!("Not shown: {u}")).size(11.5).color(pal.faint));
                        }
                    }
                    if !b.formulas.is_empty() {
                        section(ui, pal, &format!("Formulas · {}", b.formulas.len()));
                        for (name, f) in &b.formulas {
                            ui.label(RichText::new(format!("{name} = {f}")).size(12.0).monospace().color(pal.text));
                        }
                    }
                }
                Err(e) => {
                    ui.label(RichText::new(format!("This base can't be read: {e}")).size(12.5).color(pal.danger));
                }
            }
        }
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(RichText::new("Outline, backlinks and related notes are shown for notes.").size(12.0).color(pal.faint)).wrap(),
        );
    });
}

/// 0.11: the Insights panel's outline and links, kept between frames.
///
/// They follow the note once typing pauses ([`SETTLE`]), not on every
/// keystroke, and only the rows in view are laid out and drawn: a 100 KB
/// note has about a thousand of them.
#[derive(Default)]
pub(crate) struct InsightsRows {
    /// (note, revision, index version) shown, and the latest seen with when it was first seen.
    shown: Option<(quire_app::DocId, u64, u64)>,
    seen: Option<((quire_app::DocId, u64, u64), std::time::Instant)>,
    outline: std::rc::Rc<Vec<quire_app::markdown::Heading>>,
    outgoing: std::rc::Rc<Vec<(String, Option<NotePath>)>>,
    /// Where each link goes in the wrapped list, for a panel width.
    links_at: Option<(f32, Vec<egui::Rect>, f32)>,
}

/// How long typing must pause before the outline and links follow it.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

impl InsightsRows {
    /// Bring the rows up to date when the note changed and typing paused
    /// (another note: at once).
    fn refresh(&mut self, ws: &Workspace, ctx: &egui::Context) {
        let Some(doc) = ws.active_doc() else { return };
        let key = (doc.id, doc.revision(), ws.index().version());
        let now = std::time::Instant::now();
        if self.seen.is_none_or(|(k, _)| k != key) {
            self.seen = Some((key, now));
        }
        if self.shown == Some(key) {
            return;
        }
        let since = self.seen.map_or(SETTLE, |(_, t)| now.duration_since(t));
        let other_note = self.shown.is_none_or(|(id, _, _)| id != doc.id);
        if other_note || since >= SETTLE {
            self.outline = std::rc::Rc::new(doc.outline());
            self.outgoing = std::rc::Rc::new(ws.outgoing());
            self.links_at = None;
            self.shown = Some(key);
        } else {
            ctx.request_repaint_after(SETTLE.saturating_sub(since));
        }
    }
}

/// One clickable row of text in `r`, cut to fit.
fn text_row(ui: &egui::Ui, r: egui::Rect, id: egui::Id, text: RichText, hover: egui::Color32) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let g = egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Truncate), r.width(), egui::TextStyle::Body);
    let pos = egui::pos2(r.left(), r.center().y - g.size().y / 2.0);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let line = egui::Rect::from_min_size(pos, g.size());
        ui.painter().line_segment([line.left_bottom(), line.right_bottom()], egui::Stroke::new(1.0, hover));
    }
    ui.painter().galley(pos, g, hover);
    resp
}

/// The range of `n` rows of height `h` from `top` that the clip rect shows.
fn rows_in_view(ui: &egui::Ui, top: f32, h: f32, n: usize) -> std::ops::Range<usize> {
    let clip = ui.clip_rect();
    let first = ((clip.top() - top) / h).floor().max(0.0) as usize;
    let last = (((clip.bottom() - top) / h).ceil().max(0.0) as usize).min(n);
    first.min(last)..last
}

pub(crate) fn insights(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, rows: &mut InsightsRows) -> PanelOut {
    let mut out = PanelOut::default();
    if ws.active_doc().is_none() {
        ui.label(RichText::new("Open a note to see its outline, backlinks and related notes.").color(pal.muted));
        return out;
    }
    rows.refresh(ws, ui.ctx());
    let doc = ws.active_doc().expect("checked");
    let outline = std::rc::Rc::clone(&rows.outline);
    let ins = ws.insights();
    let outgoing = std::rc::Rc::clone(&rows.outgoing);
    let mut jump_byte = None;
    let mut open_link: Option<(String, Option<String>)> = None;
    let mut open_path: Option<(NotePath, Option<usize>)> = None;
    let mut link_one = None;
    let mut add_tag = None;

    let doc_id = doc.id;
    let has_path = doc.path.is_some();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if has_path {
            let n = ws.active_properties().len();
            egui::CollapsingHeader::new(RichText::new(format!("PROPERTIES · {n}")).size(11.0).color(pal.muted).strong())
                .id_salt(("quire-props-header", doc_id.0))
                .default_open(n > 0)
                .show(ui, |ui| crate::view_ui::properties(ui, ws, pal, doc_id));
        }
        if !outline.is_empty() {
            section(ui, pal, "Outline");
            // Only the rows in view are laid out (a long note has hundreds).
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h * outline.len() as f32), egui::Sense::hover());
            for i in rows_in_view(ui, rect.top(), row_h, outline.len()) {
                let h = &outline[i];
                let indent = f32::from(h.level.saturating_sub(1)) * 12.0;
                let r = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + indent, rect.top() + row_h * i as f32),
                    egui::pos2(rect.right(), rect.top() + row_h * (i + 1) as f32),
                );
                if text_row(ui, r, egui::Id::unique(("quire-outline-row", i)), RichText::new(&h.text).color(pal.text), pal.text).clicked() {
                    jump_byte = Some(h.offset);
                }
            }
        }

        // 0.12: typed links first, grouped by type; the backlinks below are the untyped ones.
        let rel = ws.note_relations();
        if !rel.is_empty() {
            section(ui, pal, "Links by type");
            if let Some(p) = crate::links_ui::insights_groups(ui, &rel, pal) {
                open_path = Some((p, None));
            }
        }
        let untyped: Vec<&quire_app::index::Backlink> = ins.backlinks.iter().filter(|b| b.link.field.is_none()).collect();
        let typed = ins.backlinks.len() - untyped.len();
        let title = if typed > 0 { format!("Backlinks · {} untyped", untyped.len()) } else { format!("Backlinks · {}", untyped.len()) };
        section(ui, pal, &title);
        if untyped.is_empty() {
            ui.label(
                RichText::new(if typed > 0 { "Every link here is typed (above)." } else { "No notes link here yet." })
                    .color(pal.muted)
                    .size(12.5),
            );
        }
        for b in untyped {
            if ui.link(RichText::new(b.from.stem()).strong()).clicked() {
                open_path = Some((b.from.clone(), Some(b.link.range.start)));
            }
            ui.label(RichText::new(&b.context).color(pal.muted).size(12.5));
            ui.add_space(4.0);
        }

        if !outgoing.is_empty() {
            section(ui, pal, "Links");
            // Wrapped like words; where each goes is worked out once per width.
            let width = ui.available_width();
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            if rows.links_at.as_ref().is_none_or(|(w, _, _)| (w - width).abs() > 0.5) {
                let (mut x, mut y) = (0.0_f32, 0.0_f32);
                let gap = ui.spacing().item_spacing.x * 2.0;
                let mut at = Vec::with_capacity(outgoing.len());
                for (target, _) in outgoing.iter() {
                    let w = egui::WidgetText::from(RichText::new(target))
                        .into_galley(ui, Some(egui::TextWrapMode::Truncate), width, egui::TextStyle::Body)
                        .size()
                        .x
                        .min(width);
                    if x > 0.0 && x + w > width {
                        x = 0.0;
                        y += row_h;
                    }
                    at.push(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, row_h)));
                    x += w + gap;
                }
                rows.links_at = Some((width, at, y + row_h));
            }
            let (_, at, height) = rows.links_at.as_ref().expect("just set");
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, *height), egui::Sense::hover());
            let clip = ui.clip_rect();
            for (i, ((target, resolved), r)) in outgoing.iter().zip(at).enumerate() {
                let r = r.translate(rect.min.to_vec2());
                if !clip.intersects(r) {
                    continue;
                }
                let color = if resolved.is_some() { pal.accent } else { pal.muted };
                let id = egui::Id::unique(("quire-outgoing-link", i));
                let tip = if resolved.is_some() { "Open" } else { "Not created yet — click to create" };
                if text_row(ui, r, id, RichText::new(target).color(color), color).on_hover_text(tip).clicked() {
                    open_link = Some((target.clone(), None));
                }
            }
        }

        if !ins.link_suggestions.is_empty() {
            section(ui, pal, &format!("Suggested links · {}", ins.link_suggestions.len()));
            for (i, s) in ins.link_suggestions.iter().enumerate().take(8) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("“{}”", s.text)));
                    ui.label(RichText::new(format!("→ {}", s.target.stem())).color(pal.muted).size(12.0));
                    if ui.small_button("Link").clicked() {
                        link_one = Some(i);
                    }
                });
            }
            if ui.small_button("Link all").clicked() {
                out.run = Some(Command::LinkAllSuggestions);
            }
        }

        if !ins.unlinked.is_empty() {
            section(ui, pal, &format!("Unlinked mentions · {}", ins.unlinked.len()));
            for m in ins.unlinked.iter().take(8) {
                if ui.link(RichText::new(m.from.stem()).strong()).clicked() {
                    open_path = Some((m.from.clone(), Some(m.range.start)));
                }
                ui.label(RichText::new(&m.context).color(pal.muted).size(12.5));
            }
        }

        if !ins.related.is_empty() {
            section(ui, pal, "Related");
            for r in &ins.related {
                ui.horizontal_wrapped(|ui| {
                    if ui.link(&r.title).clicked() {
                        open_path = Some((r.path.clone(), None));
                    }
                    let why = if r.linked { format!("linked · {}", r.because.join(", ")) } else { r.because.join(", ") };
                    ui.label(RichText::new(why).color(pal.muted).size(11.5));
                });
            }
        }

        if !ins.tag_suggestions.is_empty() {
            section(ui, pal, "Suggested tags");
            ui.horizontal_wrapped(|ui| {
                for (t, _) in &ins.tag_suggestions {
                    if ui.button(RichText::new(format!("+ #{t}")).color(pal.tag).size(12.5)).clicked() {
                        add_tag = Some(t.clone());
                    }
                }
            });
        }

        if !ins.summary.is_empty() {
            section(ui, pal, "Key sentences");
            for s in &ins.summary {
                ui.label(RichText::new(format!("• {s}")).size(12.5));
            }
        }

        section(ui, pal, &format!("Assistant · {}", ws.assistant_name()));
        ui.horizontal_wrapped(|ui| {
            for task in
                [Assist::Summarize, Assist::FixGrammar, Assist::Rewrite { tone: "clearer and more concise".into() }, Assist::Continue]
            {
                if ui.add_enabled(!ws.assist_pending(), egui::Button::new(task.label()).small()).clicked() {
                    ws.start_assist(task);
                }
            }
        });
        if ws.assist_pending() {
            ui.add(egui::Spinner::new());
        }
        let mut accept = None;
        let mut dismiss = false;
        if let Some(r) = &ws.assist_result {
            egui::Frame::group(ui.style()).show(ui, |ui| match &r.output {
                Ok(t) => {
                    ui.label(RichText::new(t).size(12.5));
                    ui.horizontal(|ui| {
                        if ui.small_button("Insert below").clicked() {
                            accept = Some(false);
                        }
                        if !r.selection.is_empty() && ui.small_button("Replace selection").clicked() {
                            accept = Some(true);
                        }
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(t.clone());
                        }
                        if ui.small_button("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                }
                Err(e) => {
                    ui.label(RichText::new(e).color(pal.warn).size(12.5));
                    ui.label(
                        RichText::new("Build with --features remote-llm and set ANTHROPIC_API_KEY for generative help.")
                            .color(pal.muted)
                            .size(11.5),
                    );
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = true;
                    }
                }
            });
        }
        if let Some(replace) = accept {
            ws.accept_assist(replace);
        }
        if dismiss {
            ws.assist_result = None;
        }
        ui.add_space(16.0);
    });

    if let Some(b) = jump_byte {
        ws.jump_to_byte(b);
        out.jumped = true;
    }
    if let Some(i) = link_one {
        ws.apply_link_suggestion(&ins.link_suggestions[i]);
    }
    if let Some(t) = add_tag {
        let at_end = ws.active_doc().map_or(0, |d| d.buffer().len_chars());
        if let Some(d) = ws.active_doc_mut() {
            d.set_selection(quire_app::text::Selection::cursor(at_end));
        }
        let sep = if ws.active_doc().is_some_and(|d| d.text().ends_with('\n')) { "" } else { "\n" };
        ws.insert_text(&format!("{sep}#{t} "));
    }
    if let Some((t, h)) = open_link {
        out.jumped = ws.open_link(&t, h.as_deref()).is_some();
    }
    if let Some((p, at)) = open_path
        && ws.open_note(&p).is_some()
    {
        if let Some(b) = at {
            ws.jump_to_byte(b);
        }
        out.jumped = true;
    }
    out
}

// ---------------------------------------------------------------- status bar

/// `640 / 1,500 words · 43% · 3,812 chars · 3 min read`, with "✓ goal" in
/// the success colour once a `goal` property is reached (issue #16).
fn word_count(ui: &mut egui::Ui, pal: &Palette, st: &quire_app::markdown::Stats, goal: Option<usize>) {
    use quire_app::word_goal;
    let wc = word_goal::word_count(st.words, goal);
    let font = FontId::proportional(12.0);
    let fmt = |color: egui::Color32| TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = LayoutJob::default();
    job.append(&wc.text, 0.0, fmt(pal.muted));
    if wc.reached {
        // Inter has U+2713; a theme's own UI font may not, and then egui
        // would draw a box, so say it in words instead.
        let check = ui.fonts_mut(|f| f.has_glyph(&font, '✓'));
        job.append(" · ", 0.0, fmt(pal.muted));
        job.append(if check { word_goal::REACHED } else { word_goal::REACHED_TEXT }, 0.0, fmt(pal.added));
    }
    let rest = format!(" · {} chars · {} min read", word_goal::count(st.chars), st.reading_minutes.max(1));
    job.append(&rest, 0.0, fmt(pal.muted));
    let r = ui.label(job);
    if let Some(g) = goal {
        r.on_hover_text(format!("Goal: {} words (the note's goal property)", word_goal::count(g)));
    }
}

/// `surface`: a base, canvas or PDF has the central panel, so the note
/// open before it isn't described.
pub(crate) fn status_bar(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette, surface: bool) -> PanelOut {
    let mut out = PanelOut::default();
    ui.horizontal(|ui| {
        let small = |s: String| RichText::new(s).size(12.0).color(pal.muted);
        if let Some(doc) = ws.active_doc().filter(|_| !surface) {
            let (l, c) = doc.caret_line_col();
            if doc.large {
                let b = doc.buffer();
                ui.label(small(format!("{} lines · {} chars", b.len_lines(), b.len_chars())));
            } else {
                let ex = doc.extracted();
                let st = ex.stats;
                word_count(ui, pal, &st, ex.front_matter.as_ref().and_then(quire_app::markdown::FrontMatter::word_goal));
            }
            ui.separator();
            ui.label(small(format!("Ln {l}, Col {c}")));
            if doc.large {
                ui.separator();
                let classic = ws.settings.editor == "classic";
                let (label, tip) = if classic {
                    ("plain mode", "The classic editor turns off styling for files this large")
                } else {
                    ("large file", "Outline, word count and preview are off for files this large; editing and styling stay fast")
                };
                ui.label(RichText::new(label).size(12.0).color(pal.warn)).on_hover_text(tip);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Feature slots: the inbox badge, Vim mode, due cards, the engine.
            for item in ws.status_items() {
                let color = if item.warn { pal.warn } else { pal.muted };
                let r = ui
                    .add(egui::Label::new(RichText::new(&item.text).size(12.0).color(color)).sense(egui::Sense::click()))
                    .on_hover_text(&item.tooltip);
                if let Some(c) = item.command
                    && r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                {
                    out.run = Some(c);
                }
                ui.separator();
            }
            {
                use quire_app::vault_sync::SyncStatus;
                let st = &ws.sync.status;
                if *st != SyncStatus::Off {
                    let (color, hover) = match st {
                        SyncStatus::Synced(at) => (pal.muted, format!("Up to date (checked {}s ago)", at.elapsed().as_secs())),
                        SyncStatus::Syncing => (pal.accent, "Syncing".to_string()),
                        SyncStatus::Offline(e) => (pal.warn, format!("Can't sync right now; will keep trying.\n{e}")),
                        SyncStatus::Waiting(e) => (pal.warn, e.clone()),
                        SyncStatus::Error(e) => (pal.danger, e.clone()),
                        SyncStatus::Elsewhere(e) => (pal.muted, e.clone()),
                        SyncStatus::Off => (pal.muted, String::new()),
                    };
                    if ui
                        .add(egui::Label::new(RichText::new(st.label()).size(12.0).color(color)).sense(egui::Sense::click()))
                        .on_hover_text(hover)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        out.run = Some(Command::SyncSettings);
                    }
                    ui.separator();
                }
            }
            if let Some(r) = &ws.updates.available {
                let link = ui
                    .add(
                        egui::Label::new(RichText::new(format!("↑ Quire {} available", r.version)).size(12.0).color(pal.accent))
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text("Open the release page. Right-click to skip this version.")
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if link.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(r.url.clone()));
                }
                link.context_menu(|ui| {
                    if ui.button("Skip this version").clicked() {
                        out.skip_update = true;
                        ui.close();
                    }
                });
                ui.separator();
            }
            let conflicts = ws.conflict_copy_count();
            if conflicts > 0 {
                let label = format!("⚠ {conflicts} sync conflict{}", if conflicts == 1 { "" } else { "s" });
                if ui
                    .add(egui::Label::new(RichText::new(label).size(12.0).color(pal.warn)).sense(egui::Sense::click()))
                    .on_hover_text("A sync tool saved a conflicting copy of a note. Click to merge it back.")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    out.run = Some(Command::ResolveSyncConflicts);
                }
                ui.separator();
            }
            let s = ws.index().status();
            let n = ws.index().read().len();
            ui.label(small(if s.busy && s.total > 0 { format!("Indexing {}/{}", s.indexed, s.total) } else { format!("{n} notes") }));
            ui.separator();
            let sem = if ws.index().semantic_busy() {
                "Learning meanings…".to_string()
            } else {
                match ws.index().semantic() {
                    Some(si) => format!("≈ {} passages", si.len()),
                    None => "≈ —".into(),
                }
            };
            ui.label(small(sem)).on_hover_text("On-device semantic index used by search, related notes and Ask");
            ui.separator();
            if let Some(doc) = ws.active_doc() {
                let state = if doc.conflict {
                    RichText::new("Changed on disk").color(pal.warn)
                } else if doc.path.is_none() {
                    small("Scratchpad".into())
                } else if doc.is_dirty() {
                    small("Editing…".into())
                } else {
                    small("Saved".into())
                };
                ui.label(state.size(12.0));
            }
        });
    });
    out
}

// ---------------------------------------------------------------- preview

/// Obsidian's callout types as the preview's alert boxes.
pub(crate) fn callout_alerts() -> egui_commonmark::AlertBundle {
    egui_commonmark::AlertBundle::from_alerts(
        quire_app::markdown::callout::KINDS
            .iter()
            .map(|k| egui_commonmark::Alert {
                accent_color: egui::Color32::from_rgb(k.color[0], k.color[1], k.color[2]),
                icon: k.glyph,
                identifier: k.id.to_uppercase(),
                identifier_rendered: k.label.to_string(),
            })
            .collect(),
    )
}

pub(crate) fn preview(ui: &mut egui::Ui, ws: &mut Workspace, cache: &mut CommonMarkCache, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(doc) = ws.active_doc() else {
        return out;
    };
    if doc.large {
        ui.label(RichText::new("Preview is off for very large files.").color(pal.muted));
        return out;
    }
    // Mermaid blocks become display math holding a marker; the math hook draws the diagram.
    let text = doc.text().to_string();
    // WS-C: ⚠ after footnotes whose cited text changed (as the editor shows).
    let text = match doc.path.clone() {
        Some(p) => crate::trust_ui::mark_flagged_citations(ws, &p, text),
        None => text,
    };
    let figures = quire_app::markdown::blocks::figures(&text);
    let mut diagrams: Vec<String> = Vec::new();
    let text = if figures.iter().any(|f| f.kind == quire_app::markdown::blocks::FigureKind::Diagram) {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            if let Some(f) = figures.iter().find(|f| f.first_line == i && f.kind == quire_app::markdown::blocks::FigureKind::Diagram) {
                // Keep the block's indent so a diagram inside a list stays in it.
                let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
                out.push(format!("{indent}$$quire-diagram:{}$$", diagrams.len()));
                diagrams.push(f.source.clone());
                i = f.last_line + 1;
            } else {
                out.push(lines[i].to_string());
                i += 1;
            }
        }
        out.join("\n")
    } else {
        text
    };
    // WS-G: `[@key]` citations as author-date text and a reference list.
    let text = quire_app::citations::render_source(ws, &text);
    // 0.12: pictures in locked folders show from memory while unlocked.
    let ctx = ui.ctx().clone();
    let md = for_preview(&text, &|t| crate::locked_media::uri(&ctx, ws, t));
    let urls: Vec<String> =
        md.match_indices("(quire://note/").filter_map(|(i, _)| md[i + 1..].find(')').map(|e| md[i + 1..i + 1 + e].to_string())).collect();
    for u in &urls {
        cache.add_link_hook(u.clone());
    }
    egui::ScrollArea::vertical().id_salt("preview").auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(18, 22)).show(ui, |ui| {
            let (text_color, dark, size) = (pal.text, pal.dark, ui.style().text_styles[&egui::TextStyle::Body].size);
            let pal_c = *pal;
            let math = move |ui: &mut egui::Ui, src: &str, inline: bool| {
                let diagram = src.trim().strip_prefix("quire-diagram:").and_then(|n| n.parse::<usize>().ok()).and_then(|n| diagrams.get(n));
                let job = match diagram {
                    Some(source) => crate::typeset_ui::Job::Diagram { source: source.clone(), dark },
                    None => crate::typeset_ui::Job::Math {
                        latex: src.trim().to_string(),
                        display: !inline,
                        size: size.round() as u32,
                        color: crate::typeset_ui::rgba(text_color),
                    },
                };
                match crate::typeset_ui::request(ui.ctx(), &job) {
                    crate::typeset_ui::Out::Ready(p) => {
                        let scale = (ui.available_width() / p.size.x).min(1.0);
                        if let Some(source) = diagram {
                            // 0.11: drag, zoom and full screen.
                            let (r, _) = ui.allocate_exact_size(p.size * scale, egui::Sense::hover());
                            let _ = crate::diagram_ui::show(ui, r, &p, source, &pal_c, false);
                        } else {
                            ui.image((p.texture.id(), p.size * scale));
                        }
                    }
                    crate::typeset_ui::Out::Pending => {
                        ui.label(RichText::new("…").weak());
                    }
                    crate::typeset_ui::Out::Failed(e) => {
                        ui.label(RichText::new(if inline { src.to_string() } else { format!("{src}\n({e})") }).monospace().weak());
                    }
                }
            };
            // Quire Lite has no typesetter: math stays as its source.
            CommonMarkViewer::new()
                .alerts(callout_alerts())
                .render_math_fn(crate::typeset_ui::AVAILABLE.then_some(&math as &egui_commonmark::RenderMathFn))
                .show(ui, cache, &md);
        });
    });
    let mut target = None;
    for u in &urls {
        if cache.get_link_hook(u) == Some(true) {
            target = parse_note_url(u);
        }
        cache.remove_link_hook(u);
    }
    if let Some((t, h)) = target {
        out.jumped = ws.open_link(&t, h.as_deref()).is_some();
    }
    out
}

// ---------------------------------------------------------------- welcome

pub(crate) enum WelcomeAction {
    None,
    Browse,
    Sample,
}

pub(crate) fn welcome(ui: &mut egui::Ui, pal: &Palette) -> WelcomeAction {
    let mut act = WelcomeAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.22).max(24.0));
        ui.label(RichText::new("Quire").size(44.0).strong().color(pal.strong));
        ui.label(RichText::new("A fast, local-first Markdown notepad.").size(17.0).color(pal.muted));
        ui.add_space(28.0);
        if ui.add(egui::Button::new(RichText::new("Open a notes folder…").size(16.0)).min_size([260.0, 40.0].into())).clicked() {
            act = WelcomeAction::Browse;
        }
        ui.add_space(8.0);
        if ui.add(egui::Button::new(RichText::new("Explore the sample vault").size(16.0)).min_size([260.0, 40.0].into())).clicked() {
            act = WelcomeAction::Sample;
        }
        ui.add_space(24.0);
        ui.label(
            RichText::new("Notes stay plain .md files on your disk. Works with folders from Obsidian, iA Writer, Logseq…")
                .color(pal.muted)
                .size(13.0),
        );
    });
    act
}

// ---------------------------------------------------------------- find bar

fn toggle(ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str, pal: &Palette) -> bool {
    let text = RichText::new(label).monospace().color(if *on { pal.accent } else { pal.muted });
    let r = ui.selectable_label(*on, text).on_hover_text(tip);
    if r.clicked() {
        *on = !*on;
    }
    r.clicked()
}

/// In-note find & replace. Enter = next, Shift+Enter = previous, Esc closes.
pub(crate) fn find_bar(ui: &mut egui::Ui, ws: &mut Workspace, focus: &mut bool, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let mut step: Option<bool> = None;
    ui.horizontal_wrapped(|ui| {
        let q = ui.add(
            egui::TextEdit::singleline(&mut ws.find.query).hint_text("Find").desired_width(220.0).id(egui::Id::unique("quire-find-query")),
        );
        if std::mem::take(focus) {
            q.request_focus();
        }
        if q.changed() {
            // Incremental search: re-select from where the current match starts.
            if let Some(d) = ws.active_doc_mut() {
                let s = d.selection().start();
                d.set_selection(quire_app::text::Selection::cursor(s));
            }
            step = Some(false);
        }
        if q.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            step = Some(ui.input(|i| i.modifiers.shift));
            q.request_focus();
        }
        let mut o = ws.find.opts;
        toggle(ui, &mut o.case_sensitive, "Aa", "Match case", pal);
        toggle(ui, &mut o.whole_word, "W", "Whole word", pal);
        toggle(ui, &mut o.regex, ".*", "Regular expression ($1 in replacement)", pal);
        toggle(ui, &mut o.match_accents, "é", "Match accents (off: “cafe” also finds “café”)", pal);
        ws.find.opts = o;
        let (m, err) = ws.find_matches();
        let label = match (&err, m.len(), ws.find_current()) {
            (Some(e), _, _) => RichText::new(e.lines().last().unwrap_or("invalid pattern")).color(pal.danger),
            (None, 0, _) if !ws.find.query.is_empty() => RichText::new("No results").color(pal.warn),
            (None, 0, _) => RichText::new(""),
            (None, n, Some(i)) => RichText::new(format!("{} of {n}", i + 1)).color(pal.muted),
            (None, n, None) => RichText::new(format!("{n} found")).color(pal.muted),
        };
        ui.label(label.size(12.5));
        if ui.small_button("↑").on_hover_text("Previous (Shift+Enter, Shift+F3)").clicked() {
            step = Some(true);
        }
        if ui.small_button("↓").on_hover_text("Next (Enter, F3)").clicked() {
            step = Some(false);
        }
        ui.separator();
        if ws.find.replace_mode {
            ui.add(egui::TextEdit::singleline(&mut ws.find.replacement).hint_text("Replace with").desired_width(180.0));
            if ui.small_button("Replace").clicked() {
                ws.replace_current();
                out.jumped = true;
            }
            if ui.small_button("All").on_hover_text("Replace all in this note (one undo step)").clicked() {
                ws.replace_all_in_note();
            }
        } else if ui.small_button("Replace…").on_hover_text("Ctrl+H").clicked() {
            ws.find.replace_mode = true;
        }
        if ui.small_button("In all notes…").on_hover_text("Ctrl+Shift+R").clicked() {
            out.run = Some(Command::VaultReplace);
        }
        if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
            ws.find.open = false;
            out.jumped = true;
        }
    });
    if let Some(back) = step
        && ws.find_step(back)
    {
        // Scroll to the match but keep typing in the find field.
        out.reveal_only = true;
    }
    out
}
