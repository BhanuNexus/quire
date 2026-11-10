// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Command palette, quick open, pickers, dialogs and toasts.

use std::collections::HashMap;
use std::time::Duration;

use egui::{Align2, Key, Modifiers, Pos2, RichText};
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;
use quire_app::{COMMANDS, Command, Workspace};

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Commands,
    Notes,
    Templates,
    Tasks,
}

pub(crate) enum Overlay {
    Palette {
        mode: Mode,
        query: String,
        selected: usize,
        focused: bool,
    },
    Rename {
        input: String,
        focused: bool,
    },
    ConfirmDelete,
    OpenVault {
        input: String,
    },
    VaultReplace {
        focused: bool,
    },
    Settings(Box<crate::dialogs::SettingsState>),
    History(Box<crate::dialogs::HistoryState>),
    NewProject(crate::dialogs::ProjectState),
    Publish(crate::dialogs::PublishState),
    Folder(crate::dialogs::FolderState),
    Tour {
        step: usize,
    },
    SyncConflicts,
    Graph(Box<crate::graph_view::GraphState>),
    /// Quick capture: `minimize` puts the window back down after adding
    /// (it was minimized when the global shortcut brought it up).
    Capture {
        text: String,
        focused: bool,
        minimize: bool,
    },
    /// A dialog that lives in its own file (every 0.9 feature dialog).
    Feature(Box<dyn OverlayView>),
}

/// A dialog in its own file. Features implement this instead of adding a
/// variant to [`Overlay`]; `registry::layout_samples` lists them for
/// `layout_check`, which every dialog must pass.
pub(crate) trait OverlayView {
    /// Draw the dialog (usually an `egui::Modal`). Escape is handled by the caller.
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult;
    /// Lists and views give way to quick capture; dialogs with typing in them don't.
    fn yields_to_capture(&self) -> bool {
        false
    }
    /// The dialog went away, however (Escape, its own button, another
    /// dialog in its place): end what it started.
    fn closed(&mut self, ws: &mut Workspace) {
        let _ = ws;
    }
}

impl Overlay {
    pub(crate) fn palette(mode: Mode) -> Self {
        Self::Palette { mode, query: String::new(), selected: 0, focused: false }
    }
}

#[derive(Clone)]
enum Action {
    Run(Command),
    /// 0.11: a macro, by name.
    Macro(String),
    Open(NotePath),
    OpenAt(NotePath, usize),
    Create(String),
    Template(NotePath),
    /// A file that isn't a note, in its surface.
    OpenFile(NotePath),
}

struct Item {
    label: String,
    detail: String,
    action: Action,
}

/// Subsequence match with bonuses for word starts and contiguity. `None` = no match.
pub(crate) fn fuzzy_score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let mut score = 0;
    let mut hi = 0;
    let mut prev_match: Option<usize> = None;
    for nc in needle.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (hi..h.len()).find(|&i| h[i] == nc)?;
        score += 1;
        if found == 0 || !h[found - 1].is_alphanumeric() {
            score += 6;
        }
        if prev_match == Some(found.wrapping_sub(1)) {
            score += 4;
        }
        score -= (found - hi).min(5) as i32;
        prev_match = Some(found);
        hi = found + 1;
    }
    if hay.to_lowercase().contains(&needle.to_lowercase()) {
        score += 15;
    }
    Some(score)
}

fn items(ws: &Workspace, mode: Mode, query: &str, mac: bool) -> Vec<Item> {
    match mode {
        Mode::Commands => {
            let has_doc = ws.active_doc().is_some();
            // 0.11: shortcuts as Settings → Commands has them, pinned
            // commands and macros first, and macros as entries.
            let keymap = quire_app::keymap::Keymap::new(&ws.settings.commands);
            let pinned = &ws.settings.commands.pinned;
            let pin_rank = |id: &str| pinned.iter().position(|p| p == id);
            let boost = |id: &str| match pin_rank(id) {
                Some(i) if query.trim().is_empty() => 10_000 - i as i32,
                Some(_) => 10,
                None => 0,
            };
            let pin_mark = |id: &str| if pin_rank(id).is_some() { "📌 " } else { "" };
            let mut v: Vec<(i32, Item)> = COMMANDS
                .iter()
                .filter(|c| has_doc || !c.needs_doc)
                .filter(|c| ws.command_available(c.command))
                .filter(|c| c.command != Command::CommandPalette)
                .filter_map(|c| {
                    let s = fuzzy_score(&format!("{} {}", c.category, c.title), query)?;
                    let keys = keymap.label(c.command, mac);
                    let label = format!("{}: {}{}", c.category, pin_mark(c.id), c.title);
                    Some((s + boost(c.id), Item { label, detail: keys, action: Action::Run(c.command) }))
                })
                .collect();
            for m in &ws.settings.commands.macros {
                let id = quire_app::settings::commands::macro_id(&m.name);
                let Some(s) = fuzzy_score(&format!("Macro {}", m.name), query) else { continue };
                let keys = keymap.keys(&quire_app::keymap::Action::Macro(m.name.clone())).first().map(|k| k.label(mac)).unwrap_or_default();
                let label = format!("Macro: {}{}", pin_mark(&id), m.name);
                v.push((s + boost(&id), Item { label, detail: keys, action: Action::Macro(m.name.clone()) }));
            }
            v.sort_by_key(|x| std::cmp::Reverse(x.0));
            v.into_iter().map(|(_, i)| i).collect()
        }
        Mode::Notes => {
            // Labels follow Settings → Editing → "Show note titles"; quick open
            // matches titles and file names either way.
            let label = |p: &NotePath| quire_app::nav::display_name(ws, p);
            let titled = ws.settings.editing.display_titles;
            // Empty query: recently opened notes first.
            let recent = if query.trim().is_empty() { ws.recent_notes(8) } else { Vec::new() };
            let hits = ws.index().read().quick_open(query, 40);
            let mut v: Vec<Item> = recent
                .iter()
                .map(|p| Item {
                    label: label(p),
                    detail: if p.folder().is_empty() { "recent".into() } else { format!("recent · {}", p.folder()) },
                    action: Action::Open(p.clone()),
                })
                .collect();
            v.extend(hits.into_iter().filter(|h| !recent.contains(&h.path)).map(|h| {
                let shown = label(&h.path);
                // The other name (title or file name) goes in the detail when it differs.
                let other = if titled { h.path.stem().to_string() } else { h.title.clone() };
                Item {
                    detail: match (other == shown || other.trim().is_empty(), h.path.folder()) {
                        (true, f) => f.to_string(),
                        (false, "") => other,
                        (false, f) => format!("{other} · {f}"),
                    },
                    label: shown,
                    action: Action::Open(h.path),
                }
            }));
            // 0.11: notes of unlocked locked folders (crate::lock_ui).
            v.extend(crate::lock_ui::quick_open(ws, query).into_iter().map(|(label, detail, p)| Item {
                label,
                detail,
                action: Action::Open(p),
            }));
            let q = query.trim();
            if !q.is_empty() {
                // Files with an editor of their own: `.base`, canvases, PDFs.
                let mut files: Vec<(i32, NotePath)> = ws
                    .other_files()
                    .into_iter()
                    .filter(crate::registry::has_surface)
                    .filter(|p| crate::lock_ui::list_file(ws, p))
                    .filter_map(|p| Some((fuzzy_score(p.as_str(), q)?, p)))
                    .collect();
                files.sort_by_key(|(sc, _)| std::cmp::Reverse(*sc));
                v.extend(files.into_iter().take(10).map(|(_, p)| Item {
                    label: quire_app::nav::display_name(ws, &p),
                    detail: p.folder().to_string(),
                    action: Action::OpenFile(p),
                }));
            }
            if !q.is_empty() && !v.iter().any(|i| i.label.eq_ignore_ascii_case(q)) {
                v.push(Item { label: format!("Create “{q}”"), detail: "new note".into(), action: Action::Create(q.to_string()) });
            }
            v
        }
        Mode::Templates => ws
            .templates()
            .into_iter()
            .map(|t| Item { label: t.stem().to_string(), detail: "template".into(), action: Action::Template(t) })
            .collect(),
        Mode::Tasks => {
            let ix = ws.index().read();
            ix.tasks(false)
                .into_iter()
                .filter(|(p, t)| fuzzy_score(&format!("{} {}", t.text, p.stem()), query).is_some())
                .take(200)
                .map(|(p, t)| Item { label: format!("[ ] {}", t.text), detail: p.stem().to_string(), action: Action::OpenAt(p, t.offset) })
                .collect()
        }
    }
}

#[derive(Default)]
pub(crate) struct OverlayResult {
    pub close: bool,
    pub run: Option<Command>,
    /// The editor should take focus / scroll to the caret.
    pub jumped: bool,
    /// Show this overlay in place of the current one (a picker leading to
    /// a dialog that asks more).
    pub then: Option<Overlay>,
}

/// Draw the active overlay. Returns what the app should do next.
pub(crate) fn show(ctx: &egui::Context, overlay: &mut Overlay, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
    let mut res = OverlayResult::default();
    // Settings → Commands waiting for a shortcut takes Escape to cancel it.
    if !crate::commands_ui::recording(ctx) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        res.close = true;
        return res;
    }
    let mac = cfg!(target_os = "macos");
    match overlay {
        Overlay::Settings(st) => (res.close, res.run) = crate::dialogs::settings(ctx, st, ws, pal),
        Overlay::History(st) => (res.close, res.run) = crate::dialogs::history(ctx, st, ws, pal),
        Overlay::NewProject(st) => (res.close, res.jumped) = crate::dialogs::new_project(ctx, st, ws, pal),
        Overlay::Publish(st) => res.close = crate::dialogs::publish(ctx, st, ws, pal),
        Overlay::Folder(st) => (res.close, res.jumped) = crate::dialogs::folder_picker(ctx, st, ws, pal),
        Overlay::Feature(view) => res = view.show(ctx, ws, pal),
        Overlay::Tour { step } => res.close = crate::dialogs::tour(ctx, step, ws, pal),
        Overlay::SyncConflicts => (res.close, res.jumped) = crate::dialogs::sync_conflicts(ctx, ws, pal),
        Overlay::Graph(st) => (res.close, res.jumped) = crate::graph_view::show(ctx, st, ws, pal),
        Overlay::Capture { text, focused, minimize } => res.close = capture(ctx, text, focused, *minimize, ws, pal),
        Overlay::Palette { mode, query, selected, focused } => {
            let list = items(ws, *mode, query, mac);
            let (down, up, enter) = ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                )
            });
            if down {
                *selected = (*selected + 1).min(list.len().saturating_sub(1));
            }
            if up {
                *selected = selected.saturating_sub(1);
            }
            *selected = (*selected).min(list.len().saturating_sub(1));
            let mut chosen: Option<Action> = if enter { list.get(*selected).map(|i| i.action.clone()) } else { None };

            let hint = match mode {
                Mode::Commands => "Type a command…",
                Mode::Notes => "Jump to a note, or type a new title…",
                Mode::Templates => "Title for the new note (optional), then pick a template",
                Mode::Tasks => "Filter open tasks…",
            };
            egui::Area::new(egui::Id::unique("quire-palette")).order(egui::Order::Foreground).anchor(Align2::CENTER_TOP, [0.0, 72.0]).show(
                ctx,
                |ui| {
                    egui::Frame::popup(ui.style()).inner_margin(10).show(ui, |ui| {
                        ui.set_width(560.0_f32.min(ctx.content_rect().width() - 32.0));
                        let r = ui.add(
                            egui::TextEdit::singleline(query).hint_text(hint).desired_width(f32::INFINITY).font(egui::TextStyle::Heading),
                        );
                        if !*focused {
                            r.request_focus();
                            *focused = true;
                        }
                        if r.changed() {
                            *selected = 0;
                        }
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical().max_height(380.0).min_scrolled_height(380.0).show(ui, |ui| {
                            if list.is_empty() {
                                ui.label(RichText::new("No matches").color(pal.muted));
                            }
                            ui.spacing_mut().item_spacing.y = 2.0;
                            let commands = *mode == Mode::Commands;
                            for (i, item) in list.iter().enumerate() {
                                let is_sel = i == *selected;
                                // Commands read "Category: Title"; the category is shown quietly.
                                let (group, title) = match item.label.split_once(": ") {
                                    Some((g, t)) if commands => (g, t),
                                    _ => ("", item.label.as_str()),
                                };
                                let resp = crate::theme::row(ui, is_sel, 32.0, pal, |painter, rect| {
                                    let y = rect.center().y;
                                    let title_color = if is_sel { pal.strong } else { pal.text };
                                    let t = painter.text(
                                        Pos2::new(rect.left() + 10.0, y),
                                        Align2::LEFT_CENTER,
                                        title,
                                        egui::FontId::proportional(14.5),
                                        title_color,
                                    );
                                    if !group.is_empty() {
                                        painter.text(
                                            Pos2::new(t.right() + 8.0, y),
                                            Align2::LEFT_CENTER,
                                            group,
                                            egui::FontId::proportional(12.0),
                                            pal.faint,
                                        );
                                    }
                                    if !item.detail.is_empty() {
                                        let font = if commands { egui::FontId::monospace(11.5) } else { egui::FontId::proportional(12.5) };
                                        painter.text(
                                            Pos2::new(rect.right() - 10.0, y),
                                            Align2::RIGHT_CENTER,
                                            &item.detail,
                                            font,
                                            pal.muted,
                                        );
                                    }
                                });
                                if is_sel && (down || up) {
                                    resp.scroll_to_me(None);
                                }
                                if resp.clicked() {
                                    chosen = Some(item.action.clone());
                                }
                            }
                        });
                    });
                },
            );

            if let Some(action) = chosen {
                res.close = true;
                match action {
                    Action::Run(c) => res.run = Some(c),
                    Action::Macro(name) => ws.features.nav.run_macro = Some(name),
                    Action::Open(p) => {
                        res.jumped = ws.open_note(&p).is_some();
                    }
                    Action::OpenAt(p, byte) => {
                        if ws.open_note(&p).is_some() {
                            ws.jump_to_byte(byte);
                            res.jumped = true;
                        }
                    }
                    Action::Create(title) => {
                        res.jumped = ws.open_link(&title, None).is_some();
                    }
                    Action::OpenFile(p) => ws.request_ui(quire_app::workspace::UiRequest::OpenFile(p)),
                    Action::Template(t) => {
                        // The template's questions first, if it has any; then its
                        // `folder:` and `name:` (the typed title wins over `name:`).
                        let title = Some(query.trim().to_string()).filter(|q| !q.is_empty());
                        if ws.template_prompts(&t).is_empty() {
                            res.jumped = ws.new_note_from_template_titled(&t, title.as_deref(), None, &HashMap::new(), &[]).is_some();
                        } else {
                            res.then = Some(Overlay::Feature(Box::new(crate::template_prompt_ui::TemplateDialog::new(ws, t, title))));
                        }
                    }
                }
            }
        }
        Overlay::Rename { input, focused } => {
            egui::Modal::new(egui::Id::unique("quire-rename")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Rename note");
                ui.label(RichText::new("Links to this note are updated across the vault.").color(pal.muted));
                ui.add_space(8.0);
                let r = ui.add(egui::TextEdit::singleline(input).desired_width(f32::INFINITY));
                if !*focused {
                    r.request_focus();
                    *focused = true;
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || enter {
                        match ws.rename_active(input) {
                            Ok(()) => res.close = true,
                            Err(e) => ws.notify(NoticeLevel::Error, e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                });
            });
        }
        Overlay::ConfirmDelete => {
            let name = ws.active_doc().map(quire_app::Document::title).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("quire-delete")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading(format!("Move “{name}” to trash?"));
                ui.label(RichText::new("It goes to the vault's .trash folder, so you can restore it.").color(pal.muted));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Move to trash").color(pal.danger)).clicked() {
                        ws.trash_active();
                        res.close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                });
            });
        }
        Overlay::VaultReplace { focused } => {
            let mut refresh = false;
            let mut apply = false;
            let mut open_at: Option<(NotePath, usize)> = None;
            egui::Modal::new(egui::Id::unique("quire-vault-replace")).show(ctx, |ui| {
                let w = 640.0_f32.min(ctx.content_rect().width() - 48.0);
                ui.set_width(w);
                ui.set_max_width(w);
                ui.heading("Find and replace in all notes");
                ui.add_space(6.0);
                egui::Grid::new("vr-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Find");
                    let r = ui.add(egui::TextEdit::singleline(&mut ws.find.vault_query).desired_width(420.0));
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                        refresh = true;
                    }
                    refresh |= r.changed();
                    ui.end_row();
                    ui.label("Replace");
                    ui.add(egui::TextEdit::singleline(&mut ws.find.vault_replacement).desired_width(420.0));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    let o = &mut ws.find.vault_opts;
                    refresh |= ui.checkbox(&mut o.case_sensitive, "Match case").changed();
                    refresh |= ui.checkbox(&mut o.whole_word, "Whole word").changed();
                    refresh |= ui.checkbox(&mut o.regex, "Regex").changed();
                    refresh |= ui.checkbox(&mut o.match_accents, "Match accents").changed();
                });
                if let Some(e) = &ws.find.vault_error {
                    ui.label(RichText::new(e.lines().last().unwrap_or("invalid pattern")).color(pal.danger));
                }
                ui.separator();
                let total: usize =
                    ws.find.vault_results.iter().filter(|f| !ws.find.vault_excluded.contains(&f.path)).map(|f| f.count).sum();
                let files = ws.find.vault_results.iter().filter(|f| !ws.find.vault_excluded.contains(&f.path)).count();
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    if ws.find.vault_results.is_empty() && !ws.find.vault_query.is_empty() {
                        ui.label(RichText::new("No matches").color(pal.muted));
                    }
                    let results = ws.find.vault_results.clone();
                    for f in &results {
                        let mut on = !ws.find.vault_excluded.contains(&f.path);
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut on, RichText::new(f.path.stem()).strong()).changed() {
                                if on {
                                    ws.find.vault_excluded.remove(&f.path);
                                } else {
                                    ws.find.vault_excluded.insert(f.path.clone());
                                }
                            }
                            ui.label(RichText::new(format!("{} · {}", f.count, f.path.folder())).color(pal.muted).size(12.0));
                        });
                        for l in &f.lines {
                            ui.horizontal(|ui| {
                                ui.add_space(26.0);
                                let ctx_line: String = l.context.chars().take(110).collect();
                                let more = if l.context.chars().count() > 110 { "…" } else { "" };
                                let label =
                                    egui::Label::new(RichText::new(format!("{}: {ctx_line}{more}", l.line)).size(12.5).color(pal.text))
                                        .truncate()
                                        .sense(egui::Sense::click());
                                if ui.add(label).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    open_at = Some((f.path.clone(), l.offset));
                                }
                            });
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let label = format!(
                        "Replace {total} match{} in {files} note{}",
                        if total == 1 { "" } else { "es" },
                        if files == 1 { "" } else { "s" }
                    );
                    if ui.add_enabled(total > 0, egui::Button::new(label)).clicked() {
                        apply = true;
                    }
                    if ui.button("Close").clicked() {
                        res.close = true;
                    }
                    ui.label(RichText::new("Open notes get an undoable edit.").color(pal.muted).size(12.0));
                });
            });
            if refresh {
                ws.find.vault_excluded.clear();
                ws.vault_find();
            }
            if apply {
                ws.vault_replace();
            }
            if let Some((p, at)) = open_at
                && ws.open_note(&p).is_some()
            {
                ws.jump_to_byte(at);
                res.close = true;
                res.jumped = true;
            }
        }
        Overlay::OpenVault { input } => {
            egui::Modal::new(egui::Id::unique("quire-open-vault")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading("Open a vault");
                ui.label(RichText::new("Any folder of Markdown files works. It's created if it doesn't exist.").color(pal.muted));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(input).hint_text("/path/to/notes").desired_width(320.0));
                    if ui.button("Browse…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        *input = dir.display().to_string();
                    }
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() && !input.trim().is_empty() {
                        match ws.open_vault(std::path::Path::new(input.trim())) {
                            Ok(()) => res.close = true,
                            Err(e) => ws.notify(NoticeLevel::Error, e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                });
            });
        }
    }
    res
}

/// Categories in the order the cheat sheet shows them.
const CATEGORIES: [&str; 12] = ["Note", "Go", "Edit", "Format", "Find", "View", "Review", "Agent", "Assist", "Study", "App", "Help"];

/// The Ctrl+/ cheat sheet.
#[derive(Default)]
pub(crate) struct ShortcutsOverlay {
    query: String,
    focused: bool,
}

impl OverlayView for ShortcutsOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        OverlayResult {
            close: shortcuts(ctx, &mut self.query, &mut self.focused, ws, pal, cfg!(target_os = "macos")),
            ..Default::default()
        }
    }

    fn yields_to_capture(&self) -> bool {
        true
    }
}

/// Ctrl+/ cheat sheet: every command, grouped, with its shortcuts. Generated
/// from `COMMANDS`, so it can't drift from what the keys actually do.
fn shortcuts(ctx: &egui::Context, query: &mut String, focused: &mut bool, ws: &Workspace, pal: &Palette, mac: bool) -> bool {
    let mut close = false;
    egui::Modal::new(egui::Id::unique("quire-shortcuts")).show(ctx, |ui| {
        let w = 760.0_f32.min(ctx.content_rect().width() - 48.0);
        ui.set_width(w);
        ui.set_max_width(w);
        ui.horizontal(|ui| {
            ui.heading("Keyboard shortcuts");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui.button("Close").clicked();
                let r = ui.add(egui::TextEdit::singleline(query).hint_text("Filter…").desired_width(200.0));
                if !*focused {
                    r.request_focus();
                    *focused = true;
                }
            });
        });
        ui.label(
            RichText::new("Everything is also in the command palette (Ctrl+K), including commands without a shortcut. Change shortcuts in Settings → Commands.")
                .size(12.0)
                .color(pal.muted),
        );
        ui.separator();
        let q = query.trim().to_lowercase();
        let keymap = quire_app::keymap::Keymap::new(&ws.settings.commands);
        // Two columns when there's room for a title and its keys side by side.
        let ncols = if w >= 720.0 { 2 } else { 1 };
        let col_w = (w - 24.0) / ncols as f32;
        egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.7).show(ui, |ui| {
            ui.columns(ncols, |cols| {
                let mut shown = 0;
                for cat in CATEGORIES {
                    let cmds: Vec<_> = COMMANDS
                        .iter()
                        .filter(|c| c.category == cat && !keymap.keys_of(c.command).is_empty() && ws.command_available(c.command))
                        .filter(|c| q.is_empty() || c.title.to_lowercase().contains(&q) || cat.to_lowercase().contains(&q))
                        .collect();
                    if cmds.is_empty() {
                        continue;
                    }
                    // Fill the shorter column so the two stay balanced.
                    let col = &mut cols[shown % ncols];
                    shown += 1;
                    col.set_max_width(col_w);
                    col.label(RichText::new(cat.to_uppercase()).size(11.0).color(pal.muted).strong());
                    // Same label width in every group, so the keys line up down the column.
                    let label_w = (col_w * 0.52).max(150.0);
                    egui::Grid::new(("shortcuts", cat)).num_columns(2).spacing([12.0, 4.0]).show(col, |ui| {
                        for c in cmds {
                            ui.allocate_ui_with_layout(egui::vec2(label_w, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.set_min_width(label_w);
                                ui.add(egui::Label::new(RichText::new(c.title.trim_end_matches('…')).color(pal.text)).truncate());
                            });
                            let keys: Vec<String> = keymap.keys_of(c.command).iter().map(|k| k.label(mac)).collect();
                            ui.label(RichText::new(keys.join("  ·  ")).monospace().size(12.5).color(pal.accent));
                            ui.end_row();
                        }
                    });
                    col.add_space(10.0);
                }
                if shown == 0 {
                    cols[0].label(RichText::new("No matching shortcuts").color(pal.muted));
                }
            });
        });
    });
    close
}

/// The quick-capture box: Enter adds to today's note, Shift+Enter is a new line.
fn capture(ctx: &egui::Context, text: &mut String, focused: &mut bool, minimize: bool, ws: &mut Workspace, pal: &Palette) -> bool {
    let mut add = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    let mut close = false;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    egui::Modal::new(egui::Id::unique("quire-capture")).show(ctx, |ui| {
        ui.set_width(520.0_f32.min(ctx.content_rect().width() - 48.0));
        ui.heading("Quick capture");
        ui.label(
            RichText::new(format!("Adds a line with the time to today's note ({today}). Enter to add, Shift+Enter for a new line."))
                .size(12.0)
                .color(pal.muted),
        );
        ui.add_space(6.0);
        let r = ui.add(egui::TextEdit::multiline(text).desired_rows(3).desired_width(f32::INFINITY).hint_text("What's on your mind?"));
        if !*focused {
            r.request_focus();
            *focused = true;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            add |= ui.add_enabled(!text.trim().is_empty(), egui::Button::new("Add")).clicked();
            close |= ui.button("Cancel").clicked();
            // 0.10: record a voice note into today's note instead.
            if ws.features.ai.voice.can_record
                && ws.vault().is_some()
                && ui.button("Voice note…").on_hover_text("Record a voice note for today's note").clicked()
            {
                ws.features.ai.voice.to_capture = true;
                ws.request_ui(quire_app::workspace::UiRequest::Feature(Command::RecordVoiceNote));
                close = true;
            }
            // 0.11: the text as a task note instead.
            if ws.vault().is_some() && ui.button("New task…").on_hover_text("Make this a task note (Alt+N)").clicked() {
                ws.features.tasks.new_task_title = Some(text.trim().to_string());
                ws.request_ui(quire_app::workspace::UiRequest::Feature(Command::NewTask));
                close = true;
            }
            ui.label(RichText::new(format!("From any app: {}", crate::capture_key::label())).size(11.5).color(pal.faint));
        });
    });
    if add && !text.trim().is_empty() && ws.capture(text).is_some() {
        close = true;
        if minimize {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }
    close
}

/// Bottom-right toasts for recent notices.
pub(crate) fn toasts(ctx: &egui::Context, ws: &Workspace, pal: &Palette) {
    let notices: Vec<_> = ws.notices(Duration::from_secs(5)).cloned().collect();
    if notices.is_empty() {
        return;
    }
    egui::Area::new(egui::Id::unique("quire-toasts"))
        .order(egui::Order::Tooltip)
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -40.0])
        .interactable(false)
        .show(ctx, |ui| {
            for n in notices.iter().rev().take(3) {
                let color = match n.level {
                    NoticeLevel::Info => pal.accent,
                    NoticeLevel::Warn => pal.warn,
                    NoticeLevel::Error => pal.danger,
                };
                egui::Frame::popup(ui.style()).stroke(egui::Stroke::new(1.0, color)).inner_margin(10).show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.label(RichText::new(&n.text).color(pal.text));
                });
                ui.add_space(6.0);
            }
        });
    ctx.request_repaint_after(Duration::from_millis(500));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_quick_open_lists_recent_then_everything_else() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(quire_app::vault::Vault::in_memory(&[("A.md", "a"), ("B.md", "b"), ("C.md", "c")]));
        ws.index().flush();
        ws.open_note(&NotePath::new("B.md")).unwrap();
        ws.tick(std::time::Instant::now());
        ws.open_note(&NotePath::new("C.md")).unwrap();
        ws.tick(std::time::Instant::now());
        let labels: Vec<String> = items(&ws, Mode::Notes, "", false).into_iter().map(|i| i.label).collect();
        assert_eq!(labels[0], "B", "recent first");
        assert_eq!(labels.len(), 3, "then every other note");
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_substrings() {
        let a = fuzzy_score("Format: Toggle checklist item", "check").unwrap();
        let b = fuzzy_score("Note: Close tab", "check");
        assert!(b.is_none());
        let c = fuzzy_score("Format: Heading 1", "h1").unwrap();
        assert!(a > c);
        assert!(fuzzy_score("Quick open", "qo").unwrap() > fuzzy_score("Quick open", "ko").unwrap_or(-99));
    }
}
