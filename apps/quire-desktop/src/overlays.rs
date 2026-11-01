// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Command palette, quick open, pickers, dialogs and toasts.

use std::time::Duration;

use egui::{Align2, Key, Modifiers, RichText};
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
    Palette { mode: Mode, query: String, selected: usize, focused: bool },
    Rename { input: String, focused: bool },
    ConfirmDelete,
    OpenVault { input: String },
    VaultReplace { focused: bool },
    Ask { focused: bool },
    Settings(crate::dialogs::SettingsState),
    History(crate::dialogs::HistoryState),
    Import(crate::dialogs::ImportState),
    NewProject(crate::dialogs::ProjectState),
    Folder(crate::dialogs::FolderState),
    Shortcuts { query: String, focused: bool },
    Tour { step: usize },
    SyncConflicts,
}

impl Overlay {
    pub(crate) fn palette(mode: Mode) -> Self {
        Self::Palette { mode, query: String::new(), selected: 0, focused: false }
    }
}

#[derive(Clone)]
enum Action {
    Run(Command),
    Open(NotePath),
    OpenAt(NotePath, usize),
    Create(String),
    Template(NotePath),
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
            let mut v: Vec<(i32, Item)> = COMMANDS
                .iter()
                .filter(|c| has_doc || !c.needs_doc)
                .filter(|c| c.command != Command::CommandPalette)
                .filter_map(|c| {
                    let s = fuzzy_score(&format!("{} {}", c.category, c.title), query)?;
                    let keys = c.keys.first().map(|k| k.label(mac)).unwrap_or_default();
                    Some((s, Item { label: format!("{}: {}", c.category, c.title), detail: keys, action: Action::Run(c.command) }))
                })
                .collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.0));
            v.into_iter().map(|(_, i)| i).collect()
        }
        Mode::Notes => {
            // Empty query: recently opened notes first.
            let recent = if query.trim().is_empty() { ws.recent_notes(8) } else { Vec::new() };
            let ix = ws.index().read();
            let mut v: Vec<Item> = recent
                .iter()
                .map(|p| Item {
                    label: p.stem().to_string(),
                    detail: if p.folder().is_empty() { "recent".into() } else { format!("recent · {}", p.folder()) },
                    action: Action::Open(p.clone()),
                })
                .collect();
            v.extend(ix.quick_open(query, 40).into_iter().filter(|h| !recent.contains(&h.path)).map(|h| Item {
                label: h.path.stem().to_string(),
                detail: if h.title == h.path.stem() { h.path.folder().to_string() } else { format!("{} · {}", h.title, h.path.folder()) },
                action: Action::Open(h.path),
            }));
            let q = query.trim();
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

pub(crate) struct OverlayResult {
    pub close: bool,
    pub run: Option<Command>,
    /// The editor should take focus / scroll to the caret.
    pub jumped: bool,
}

/// Draw the active overlay. Returns what the app should do next.
pub(crate) fn show(ctx: &egui::Context, overlay: &mut Overlay, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
    let mut res = OverlayResult { close: false, run: None, jumped: false };
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        res.close = true;
        return res;
    }
    let mac = cfg!(target_os = "macos");
    match overlay {
        Overlay::Settings(st) => res.close = crate::dialogs::settings(ctx, st, ws, pal),
        Overlay::History(st) => res.close = crate::dialogs::history(ctx, st, ws, pal),
        Overlay::Import(st) => res.close = crate::dialogs::import(ctx, st, ws, pal),
        Overlay::NewProject(st) => (res.close, res.jumped) = crate::dialogs::new_project(ctx, st, ws, pal),
        Overlay::Folder(st) => (res.close, res.jumped) = crate::dialogs::folder_picker(ctx, st, ws, pal),
        Overlay::Shortcuts { query, focused } => res.close = shortcuts(ctx, query, focused, pal, mac),
        Overlay::Tour { step } => res.close = crate::dialogs::tour(ctx, step, ws, pal),
        Overlay::SyncConflicts => (res.close, res.jumped) = crate::dialogs::sync_conflicts(ctx, ws, pal),
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
                Mode::Templates => "Title for the new note, then pick a template",
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
                            for (i, item) in list.iter().enumerate() {
                                let is_sel = i == *selected;
                                let resp = ui
                                    .horizontal(|ui| {
                                        let r = ui.selectable_label(is_sel, RichText::new(&item.label).size(15.0));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(RichText::new(&item.detail).color(pal.muted).size(12.5));
                                        });
                                        r
                                    })
                                    .inner;
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
                    Action::Template(t) => {
                        let title = if query.trim().is_empty() { t.stem().to_string() } else { query.trim().to_string() };
                        res.jumped = ws.new_from_template(&t, &title).is_some();
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
        Overlay::Ask { focused } => {
            let mut go = false;
            let mut open_at: Option<(NotePath, usize)> = None;
            let mut insert = false;
            egui::Modal::new(egui::Id::unique("quire-ask")).show(ctx, |ui| {
                let w = 640.0_f32.min(ctx.content_rect().width() - 48.0);
                ui.set_width(w);
                ui.set_max_width(w);
                ui.heading("Ask your notes");
                ui.label(RichText::new(format!("Answers come only from your vault · {}", ws.assistant_name())).color(pal.muted).size(12.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let r = ui
                        .add(egui::TextEdit::singleline(&mut ws.ask.question).hint_text("What did I decide about…?").desired_width(500.0));
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        go = true;
                    }
                    if ui.add_enabled(!ws.ask.pending(), egui::Button::new("Ask")).clicked() {
                        go = true;
                    }
                });
                if ws.ask.pending() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label("Thinking…");
                    });
                }
                if let Some(e) = &ws.ask.error {
                    ui.label(RichText::new(format!("Model unavailable ({e}); showing an offline answer.")).color(pal.warn).size(12.0));
                }
                if let Some(a) = ws.ask.answer.clone() {
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        ui.label(RichText::new(a.markdown.replace("[[", "").replace("]]", "")).size(14.0));
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("SOURCES · {}", a.by)).size(11.0).color(pal.muted).strong());
                        for s in &a.sources {
                            ui.horizontal_wrapped(|ui| {
                                if ui.link(format!("{}. {}", s.n, s.citation().trim_start_matches("[[").trim_end_matches("]]"))).clicked() {
                                    open_at = Some((s.path.clone(), s.offset));
                                }
                            });
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ws.active_doc().is_some() && ui.button("Insert into note").clicked() {
                            insert = true;
                        }
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(a.markdown.clone());
                        }
                        if ui.button("Close").clicked() {
                            res.close = true;
                        }
                    });
                }
            });
            if go {
                ws.ask();
            }
            if insert && ws.insert_answer() {
                res.close = true;
                res.jumped = true;
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
const CATEGORIES: [&str; 9] = ["Note", "Go", "Edit", "Format", "Find", "View", "Assist", "App", "Help"];

/// Ctrl+/ cheat sheet: every command, grouped, with its shortcuts. Generated
/// from `COMMANDS`, so it can't drift from what the keys actually do.
fn shortcuts(ctx: &egui::Context, query: &mut String, focused: &mut bool, pal: &Palette, mac: bool) -> bool {
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
            RichText::new("Everything is also in the command palette (Ctrl+K), including commands without a shortcut.")
                .size(12.0)
                .color(pal.muted),
        );
        ui.separator();
        let q = query.trim().to_lowercase();
        let col_w = (w - 24.0) / 2.0;
        egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.7).show(ui, |ui| {
            ui.columns(2, |cols| {
                let mut shown = 0;
                for cat in CATEGORIES {
                    let cmds: Vec<_> = COMMANDS
                        .iter()
                        .filter(|c| c.category == cat && !c.keys.is_empty())
                        .filter(|c| q.is_empty() || c.title.to_lowercase().contains(&q) || cat.to_lowercase().contains(&q))
                        .collect();
                    if cmds.is_empty() {
                        continue;
                    }
                    // Fill the shorter column so the two stay balanced.
                    let col = &mut cols[shown % 2];
                    shown += 1;
                    col.set_max_width(col_w);
                    col.label(RichText::new(cat.to_uppercase()).size(11.0).color(pal.muted).strong());
                    egui::Grid::new(("shortcuts", cat)).num_columns(2).spacing([12.0, 4.0]).show(col, |ui| {
                        for c in cmds {
                            ui.label(RichText::new(c.title.trim_end_matches('…')).color(pal.text));
                            let keys: Vec<String> = c.keys.iter().map(|k| k.label(mac)).collect();
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
