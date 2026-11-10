// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Home (0.12): an optional start page, built from `vault.toml` `[home]`.
//!
//! Home is a surface in the tab bar (not a note). It draws the sections
//! the vault asks for, in the order it asks, from `Workspace::home_data`
//! (the same queries as the sidebar, Agenda and Inbox: nothing is indexed
//! twice). It costs nothing while it isn't the tab in front, and its first
//! read happens after the first frame. Every long list draws only the rows
//! in view. Everything is a focusable button, and `/` jumps to quick capture.

use egui::{Button, Id, Key, Modifiers, RichText, TextEdit, Ui};
use quire_app::switch_state::{HOME_SECTIONS, HomeConfig, HomeNote, HomeTask};
use quire_app::vault::NotePath;
use quire_app::workspace::UiRequest;
use quire_app::{Command, Workspace};

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::surfaces::Surface;
use crate::theme::Palette;
use crate::view_ui::ViewAction;

/// The page has no file; this name is its identity among the open surfaces.
const HOME_PATH: &str = "Home.quire-home";
const ROW_H: f32 = 22.0;
/// Lists longer than this scroll in a box of this many rows.
const BOX_ROWS: usize = 10;
const CAPTURE_ID: &str = "quire-home-capture";

#[derive(Default)]
pub(crate) struct HomeUi {
    capture: String,
    /// A `.base` file's text and the spec of the chosen view, read when the
    /// setting or the file changes (not every frame).
    base: Option<(String, usize, String, Result<String, String>)>,
}

pub(crate) struct HomeSurface {
    path: NotePath,
    ui: HomeUi,
}

impl HomeSurface {
    pub(crate) fn make() -> Box<dyn Surface> {
        Box::new(Self { path: HOME_PATH.into(), ui: HomeUi::default() })
    }
}

impl Surface for HomeSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn title(&self) -> String {
        "Home".into()
    }

    fn show(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        show_page(ui, ws, pal, &mut self.ui)
    }
}

fn link(ui: &mut Ui, pal: &Palette, text: &str, hover: &str) -> bool {
    let b = Button::new(RichText::new(text).color(pal.accent)).frame(false);
    ui.add(b).on_hover_text(hover).clicked()
}

fn heading(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).strong().color(pal.muted));
    ui.add_space(3.0);
}

/// `n` rows of `ROW_H`: all of them when few, else a box that draws the rows in view.
fn rows(ui: &mut Ui, id: &str, n: usize, mut row: impl FnMut(&mut Ui, usize)) {
    if n <= BOX_ROWS {
        for i in 0..n {
            row(ui, i);
        }
        return;
    }
    egui::ScrollArea::vertical().id_salt(("quire-home-rows", id)).max_height(ROW_H * BOX_ROWS as f32).auto_shrink([false, true]).show_rows(
        ui,
        ROW_H,
        n,
        |ui, range| {
            for i in range {
                row(ui, i);
            }
        },
    );
}

fn note_rows(ui: &mut Ui, pal: &Palette, id: &str, notes: &[HomeNote], open: &mut Option<NotePath>) {
    rows(ui, id, notes.len(), |ui, i| {
        let n = &notes[i];
        let title = if n.title.is_empty() { n.path.stem() } else { n.title.as_str() };
        if ui.add(Button::new(RichText::new(title).color(pal.accent)).frame(false).truncate()).on_hover_text(n.path.as_str()).clicked() {
            *open = Some(n.path.clone());
        }
    });
}

fn task_rows(ui: &mut Ui, pal: &Palette, id: &str, tasks: &[HomeTask], overdue: bool, open: &mut Option<NotePath>) {
    rows(ui, id, tasks.len(), |ui, i| {
        let t = &tasks[i];
        ui.horizontal(|ui| {
            ui.label(RichText::new(&t.when).size(11.5).color(if overdue { pal.danger } else { pal.muted }));
            if ui.add(Button::new(RichText::new(&t.text).color(pal.text)).frame(false).truncate()).on_hover_text(t.path.as_str()).clicked()
            {
                *open = Some(t.path.clone());
            }
        });
    });
}

fn show_page(ui: &mut Ui, ws: &mut Workspace, pal: &Palette, st: &mut HomeUi) -> PanelOut {
    let mut out = PanelOut::default();
    // The first frame is for the editor and the tabs; Home reads once it has been drawn.
    if ui.ctx().cumulative_pass_nr() < 2 {
        ui.ctx().request_repaint();
        return out;
    }
    let prefs = ws.switch_prefs();
    let cfg = &prefs.home;
    let data = ws.home_data();
    let mut open: Option<NotePath> = None;
    let mut capture = false;
    let mut random = false;
    let capture_id = Id::unique(CAPTURE_ID);
    // `/` goes to quick capture, unless something is being typed.
    if cfg.shows("capture") && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Slash)) {
        ui.memory_mut(|m| m.request_focus(capture_id));
        // The key's own text event would be typed into the box.
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(t) if t == "/")));
    }
    egui::ScrollArea::vertical().id_salt("quire-home").auto_shrink([false, false]).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.set_max_width(720.0_f32.min(ui.available_width()));
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("Home").size(24.0).color(pal.strong));
                for id in cfg.active_sections() {
                    match id {
                        "recent" => {
                            heading(ui, pal, "Recent notes");
                            if data.recent.is_empty() {
                                ui.label(RichText::new("Notes you open will show up here.").color(pal.muted));
                            }
                            note_rows(ui, pal, "recent", &data.recent, &mut open);
                        }
                        "pinned" => {
                            heading(ui, pal, "Pinned");
                            if data.pinned.is_empty() && data.missing_pins.is_empty() {
                                ui.label(
                                    RichText::new("Pin a note with “Pin to Home” in the command palette, or list paths in [home] pinned.")
                                        .color(pal.muted),
                                );
                            }
                            note_rows(ui, pal, "pinned", &data.pinned, &mut open);
                            if !data.missing_pins.is_empty() {
                                ui.label(
                                    RichText::new(format!(
                                        "{} pinned notes no longer exist: {}",
                                        data.missing_pins.len(),
                                        data.missing_pins.join(", ")
                                    ))
                                    .size(12.0)
                                    .color(pal.warn),
                                );
                            }
                        }
                        "tasks" => {
                            heading(ui, pal, "Today");
                            if data.overdue.is_empty() && data.today.is_empty() {
                                ui.label(RichText::new("Nothing is due today.").color(pal.muted));
                            }
                            task_rows(ui, pal, "overdue", &data.overdue, true, &mut open);
                            task_rows(ui, pal, "today", &data.today, false, &mut open);
                        }
                        "view" => {
                            heading(ui, pal, "View");
                            view_section(ui, ws, pal, cfg, st, &mut open);
                        }
                        "inbox" => {
                            heading(ui, pal, "Inbox");
                            if data.suggestions.is_empty() && data.other_waiting == 0 {
                                ui.label(RichText::new("Nothing is waiting for you.").color(pal.muted));
                            }
                            for s in data.suggestions.iter().take(5) {
                                ui.add(egui::Label::new(RichText::new(s).color(pal.text)).truncate());
                            }
                            if data.suggestions.len() > 5 {
                                ui.label(
                                    RichText::new(format!("and {} more suggestions", data.suggestions.len() - 5))
                                        .size(12.0)
                                        .color(pal.muted),
                                );
                            }
                            if data.other_waiting > 0 {
                                ui.label(
                                    RichText::new(format!("{} checks and other items waiting", data.other_waiting))
                                        .size(12.0)
                                        .color(pal.muted),
                                );
                            }
                            if (!data.suggestions.is_empty() || data.other_waiting > 0)
                                && link(ui, pal, "Open the Inbox", "Show the Inbox panel")
                            {
                                out.run = Some(Command::OpenInbox);
                            }
                        }
                        "capture" => {
                            heading(ui, pal, "Quick capture");
                            ui.horizontal(|ui| {
                                let r = ui.add(
                                    TextEdit::singleline(&mut st.capture)
                                        .id(capture_id)
                                        .hint_text("Add a line to today's daily note   ( / )")
                                        .desired_width((ui.available_width() - 70.0).max(120.0)),
                                );
                                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                                if (ui.add_enabled(!st.capture.trim().is_empty(), Button::new("Add")).clicked() || enter)
                                    && !st.capture.trim().is_empty()
                                {
                                    capture = true;
                                }
                                if enter {
                                    r.request_focus();
                                }
                            });
                        }
                        "daily" => {
                            heading(ui, pal, "Daily note");
                            let label = if data.daily_exists { "Open today's note" } else { "Start today's note" };
                            if link(ui, pal, label, "Open (or create) the daily note") {
                                out.run = Some(Command::DailyNote);
                            }
                            if let Some(p) = &data.daily {
                                ui.label(RichText::new(p.as_str()).size(12.0).color(pal.faint));
                            }
                        }
                        "random" => {
                            heading(ui, pal, "Random note");
                            if link(ui, pal, "Open a random note", "A note picked at random") {
                                random = true;
                            }
                        }
                        "stats" => {
                            heading(ui, pal, "Writing");
                            let s = &data.stats;
                            ui.label(
                                RichText::new(format!("{} notes · {} words · {} open tasks", s.notes, s.words, s.open_tasks))
                                    .color(pal.text),
                            );
                            let sign = if s.words_today > 0 { "+" } else { "" };
                            let goal = if cfg.word_goal > 0 { format!(" of {}", cfg.word_goal) } else { String::new() };
                            ui.label(RichText::new(format!("Today on this device: {sign}{} words{goal}", s.words_today)).color(pal.muted));
                            if cfg.word_goal > 0 {
                                let frac = (s.words_today.max(0) as f32 / cfg.word_goal as f32).clamp(0.0, 1.0);
                                ui.add(egui::ProgressBar::new(frac).desired_height(8.0));
                            }
                        }
                        _ => {}
                    }
                }
                ui.add_space(24.0);
            });
        });
    });
    if capture {
        let text = std::mem::take(&mut st.capture);
        if ws.capture(&text).is_some() {
            ws.notify(quire_app::workspace::NoticeLevel::Info, "Added to today's daily note");
        } else {
            st.capture = text;
        }
    }
    if random {
        match ws.random_note() {
            Some(p) => open = Some(p),
            None => ws.notify(quire_app::workspace::NoticeLevel::Info, "No other notes to pick from"),
        }
    }
    if let Some(p) = open {
        ws.request_ui(UiRequest::OpenFile(p));
    }
    out
}

/// The chosen view: a `quire-view` spec, or a view of a `.base` file. Read-only here.
fn view_section(ui: &mut Ui, ws: &Workspace, pal: &Palette, cfg: &HomeConfig, st: &mut HomeUi, open: &mut Option<NotePath>) {
    let src = cfg.view.trim();
    if src.is_empty() {
        ui.label(RichText::new("Choose a view in Settings → Home: a query, or a .base file.").color(pal.muted));
        return;
    }
    let spec = if src.to_ascii_lowercase().ends_with(".base") {
        let text = ws.vault().and_then(|v| v.read(&NotePath::from(src)).ok()).unwrap_or_default();
        let stale = st.base.as_ref().is_none_or(|(p, i, t, _)| p != src || *i != cfg.base_view || *t != text);
        if stale {
            let spec = quire_import::obsidian::base::parse(&text).and_then(|b| {
                b.views
                    .get(cfg.base_view)
                    .or_else(|| b.views.first())
                    .map(|v| v.spec.clone())
                    .ok_or_else(|| "This base has no views.".to_string())
            });
            st.base = Some((src.to_string(), cfg.base_view, text, spec));
        }
        match st.base.as_ref().map(|b| &b.3) {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(pal.danger));
                return;
            }
            None => return,
        }
    } else {
        src.to_string()
    };
    if let Some(ViewAction::Open(p)) = crate::view_ui::show(ui, ws, pal, &spec, None, Id::unique("quire-home-view")) {
        *open = Some(p);
    }
}

// ------------------------------------------------------------------ Settings → Home

pub(crate) fn available(_ws: &Workspace) -> bool {
    true
}

pub(crate) fn settings_tab(ui: &mut Ui, ws: &mut Workspace, fui: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let out = PanelOut::default();
    if ws.vault().is_none() {
        ui.label(RichText::new("Open a vault to set up Home.").color(pal.muted));
        return out;
    }
    let before = ws.switch_prefs().home.clone();
    let mut cfg = before.clone();
    ui.label(
        RichText::new("Home is a page for the notes you come back to. It is saved in this vault (vault.toml, [home]).").color(pal.muted),
    );
    ui.add_space(6.0);
    ui.checkbox(&mut cfg.open_on_start, "Open Home when Quire starts");
    ui.add_space(8.0);
    ui.label(RichText::new("Sections").strong());
    // The shown sections in their order, then the ones that are off.
    let mut order: Vec<String> = cfg.active_sections().iter().map(|s| (*s).to_string()).collect();
    let mut swap: Option<(usize, usize)> = None;
    let mut remove: Option<usize> = None;
    for (i, id) in order.iter().enumerate() {
        let title = HOME_SECTIONS.iter().find(|(s, _)| s == id).map_or(id.as_str(), |(_, t)| *t);
        ui.horizontal_wrapped(|ui| {
            let mut on = true;
            if ui.checkbox(&mut on, title).changed() {
                remove = Some(i);
            }
            if ui.add_enabled(i > 0, Button::new("↑").small()).on_hover_text("Move up").clicked() {
                swap = Some((i, i - 1));
            }
            if ui.add_enabled(i + 1 < order.len(), Button::new("↓").small()).on_hover_text("Move down").clicked() {
                swap = Some((i, i + 1));
            }
        });
    }
    for (id, title) in HOME_SECTIONS {
        if !order.iter().any(|s| s == id) {
            let mut on = false;
            if ui.checkbox(&mut on, *title).changed() {
                order.push((*id).to_string());
                cfg.sections.clone_from(&order);
            }
        }
    }
    if let Some((a, b)) = swap {
        order.swap(a, b);
        cfg.sections.clone_from(&order);
    }
    if let Some(i) = remove {
        order.remove(i);
        cfg.sections.clone_from(&order);
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.label("Recent notes");
        ui.add(egui::DragValue::new(&mut cfg.recent).range(1..=50));
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Words a day to aim for");
        ui.add(egui::DragValue::new(&mut cfg.word_goal).range(0..=20_000));
        ui.label(RichText::new("0 for no goal").size(12.0).color(pal.muted));
    });
    ui.add_space(8.0);
    ui.label(RichText::new("View").strong());
    ui.label(RichText::new("A query (from: #project) or the path of a .base file. It is read-only on Home.").size(12.0).color(pal.muted));
    // What is typed is kept here and saved when you leave the box, not on every key.
    let mut typed = fui.switch.view.take().unwrap_or_else(|| cfg.view.clone());
    let r = ui.add(TextEdit::singleline(&mut typed).hint_text("from: #project  or  Tasks.base").desired_width(f32::INFINITY));
    if r.lost_focus() {
        cfg.view.clone_from(&typed);
    } else if typed != cfg.view {
        fui.switch.view = Some(typed);
    }
    if cfg.view.trim().to_ascii_lowercase().ends_with(".base") {
        ui.horizontal_wrapped(|ui| {
            ui.label("View number in the base");
            ui.add(egui::DragValue::new(&mut cfg.base_view).range(0..=50));
        });
        ui.label(RichText::new("Formulas and summaries in a base aren't computed on Home.").size(12.0).color(pal.muted));
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Pinned").strong());
    if cfg.pinned.is_empty() {
        ui.label(RichText::new("None. Use “Pin to Home” on a note.").size(12.0).color(pal.muted));
    }
    let mut unpin: Option<usize> = None;
    for (i, p) in cfg.pinned.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(p).truncate());
            if ui.small_button("Remove").clicked() {
                unpin = Some(i);
            }
        });
    }
    if let Some(i) = unpin {
        cfg.pinned.remove(i);
    }
    if cfg != before {
        ws.set_home_config(cfg);
    }
    out
}

/// A Home page with every section filled, for layout checks.
#[cfg(test)]
pub(crate) fn sample() -> HomeSurface {
    HomeSurface { path: HOME_PATH.into(), ui: HomeUi::default() }
}
