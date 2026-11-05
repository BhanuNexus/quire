// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian compatibility in the desktop (design §E.5): `.base` files open
//! as a view surface; Dataview and Tasks blocks show their results under
//! the block; Kanban plugin boards show as a board whose cards you drag.
//!
//! Owned by WS-E. Registered in `registry.rs` (`open_surface`, `paint_widget`).

use std::rc::Rc;
use std::time::{Duration, Instant};

use egui::{Id, RichText, Sense, Ui};
use quire_app::Workspace;
use quire_app::bases::{Cell, Table};
use quire_app::decor::ActionId;
use quire_app::document::DocId;
use quire_app::markdown::Value;
use quire_app::tasks_state::{Action, convert_action, kanban_move_action};
use quire_app::vault::NotePath;
use quire_import::obsidian::base::{self, Base, ViewKind};
use quire_import::obsidian::kanban;

use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;
use crate::view_ui::ViewAction;

/// Turn what a rendered view asks for into WS-E actions (views drawn here
/// only see `&Workspace`).
fn route(ws: &Workspace, a: ViewAction) {
    match a {
        ViewAction::Open(path) => ws.plan_request(Action::OpenAt { path, offset: 0 }),
        ViewAction::Move { path, field, value } => ws.plan_request(Action::SetProperty { path, key: field, value: Value::Str(value) }),
        ViewAction::Edit { path, field, text } => {
            let was_list = ws
                .index()
                .read()
                .get(&path)
                .and_then(|r| quire_app::index::views::property(r, &field).map(|v| matches!(v, Value::List(_))))
                .unwrap_or(false);
            ws.plan_request(Action::SetProperty { path, key: field, value: quire_app::properties::parse_input(&text, was_list) });
        }
    }
}

// ---------------------------------------------------------------- Dataview / Tasks blocks

/// A Dataview or Tasks block's results, shown under it.
pub(crate) fn compat_widget(ui: &mut Ui, ws: &Workspace, doc: DocId, block: usize, pal: &Palette) -> Option<ActionId> {
    let d = ws.doc(doc)?;
    let blocks = quire_app::tasks_state::compat_blocks(d);
    let b = blocks.get(block)?;
    let from = d.path.clone();
    let mut action = None;
    let source = match b.lang.as_str() {
        "tasks" => "Tasks query",
        "dataviewjs" => "DataviewJS",
        _ => "Dataview query",
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{source} · shown by Quire")).size(11.0).color(pal.muted).strong());
        if b.lang != "dataviewjs"
            && ui.small_button("Convert to quire-view").on_hover_text("Replace the block with a quire-view (you can undo it)").clicked()
        {
            action = Some(convert_action(block));
        }
    });
    if b.lang == "dataviewjs" {
        ui.label(RichText::new("DataviewJS runs code, which Quire never does. The block is kept as written.").size(12.0).color(pal.muted));
        return action;
    }
    let t = b.translate();
    if !t.unsupported.is_empty() {
        ui.add(egui::Label::new(RichText::new(format!("Not shown: {}.", t.unsupported.join("; "))).size(11.5).color(pal.warn)).wrap());
    }
    let key = ui.scope_id().with(("quire-compat", doc.0, block));
    if let Some(a) = crate::view_ui::show(ui, ws, pal, &t.spec, from.as_ref(), key) {
        route(ws, a);
    }
    action
}

// ---------------------------------------------------------------- Kanban boards

/// A Kanban plugin board: columns from `##` headings, cards from list items.
pub(crate) fn kanban_widget(ui: &mut Ui, ws: &Workspace, doc: DocId, pal: &Palette) -> Option<ActionId> {
    let d = ws.doc(doc)?;
    let board = kanban::parse(&d.text());
    let cards: usize = board.columns.iter().map(|c| c.cards.len()).sum();
    let mut action = None;
    // Scoped to this widget, so a board shown twice (split panes) doesn't clash.
    let base = ui.scope_id();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Kanban board · {cards} card{}", if cards == 1 { "" } else { "s" })).size(11.0).color(pal.muted).strong(),
        );
        ui.label(RichText::new("Drag cards between columns; the list items move in this note.").size(11.0).color(pal.faint));
    });
    if board.columns.is_empty() {
        ui.label(RichText::new("Add a “## Column” heading and “- [ ] card” items under it.").size(12.0).color(pal.muted));
        return None;
    }
    let n = board.columns.len();
    // Each column's frame adds 6 px a side; columns are 8 px apart.
    let col_w = ((ui.available_width() - 8.0 * (n as f32 - 1.0)) / n as f32 - 12.0).clamp(140.0, 240.0);
    let tallest = board.columns.iter().map(|c| c.cards.len()).max().unwrap_or(0).max(1);
    let col_h = 26.0 + tallest as f32 * 44.0;
    egui::ScrollArea::horizontal().id_salt(("quire-kanban", doc.0)).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for (ci, col) in board.columns.iter().enumerate() {
                let frame = egui::Frame::NONE.fill(pal.panel).corner_radius(6.0).inner_margin(6.0);
                let (_, dropped) = ui.dnd_drop_zone::<(usize, usize), ()>(frame, |ui| {
                    // The row's layout is horizontal; a column stacks.
                    ui.vertical(|ui| {
                        ui.set_width(col_w);
                        ui.set_min_height(col_h);
                        ui.label(RichText::new(format!("{} · {}", col.title, col.cards.len())).size(12.0).color(pal.muted).strong());
                        for (ki, card) in col.cards.iter().enumerate() {
                            let id = base.with(("quire-kanban-card", doc.0, ci, ki));
                            let r = ui
                                .dnd_drag_source(id, (ci, ki), |ui| {
                                    egui::Frame::NONE
                                        .fill(pal.bg)
                                        .stroke(egui::Stroke::new(1.0, pal.border))
                                        .corner_radius(4.0)
                                        .inner_margin(6.0)
                                        .show(ui, |ui| {
                                            ui.set_width(col_w - 14.0);
                                            let mut t =
                                                RichText::new(&card.text).size(13.0).color(if card.done { pal.muted } else { pal.text });
                                            if card.done {
                                                t = t.strikethrough();
                                            }
                                            ui.add(egui::Label::new(t).wrap().selectable(false));
                                        });
                                })
                                .response;
                            // Dropped on a card: goes before it.
                            if let Some(p) = r.dnd_release_payload::<(usize, usize)>() {
                                action = Some(kanban_move_action(p.0, p.1, ci, ki));
                            }
                        }
                    });
                });
                if let Some(p) = dropped
                    && action.is_none()
                    && p.0 != ci
                {
                    action = Some(kanban_move_action(p.0, p.1, ci, col.cards.len()));
                }
            }
        });
    });
    if board.has_archive {
        ui.label(RichText::new("The board's archive isn't shown.").size(11.0).color(pal.faint));
    }
    action
}

// ---------------------------------------------------------------- `.base` surface

/// What a worked-out table depends on: the index, the file, the view and the minute.
type TableKey = (u64, u64, usize, i64);

/// An Obsidian base, shown as its views.
pub(crate) struct BaseSurface {
    path: NotePath,
    text: Option<String>,
    base: Option<Result<Base, String>>,
    view: usize,
    read_at: Option<Instant>,
    table: Option<(TableKey, Rc<Table>)>,
}

impl BaseSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self { path: path.clone(), text: None, base: None, view: 0, read_at: None, table: None }
    }

    /// The view worked out (formulas, filters, summaries), cached until the
    /// index, the file or the minute changes.
    fn table(&mut self, ws: &Workspace, base: &Base, view: usize) -> Rc<Table> {
        use std::hash::{Hash, Hasher};
        let now = chrono::Local::now().naive_local();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.text.hash(&mut h);
        let key = (ws.index().version(), h.finish(), view, now.and_utc().timestamp() / 60);
        match &self.table {
            Some((k, t)) if *k == key => Rc::clone(t),
            _ => {
                let t = Rc::new(quire_app::bases::table(ws, base, view, now));
                self.table = Some((key, Rc::clone(&t)));
                t
            }
        }
    }

    /// Read the file again now and then (it may change on disk).
    fn refresh(&mut self, ws: &Workspace) {
        if self.read_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.read_at = Some(Instant::now());
        let text = ws.vault().and_then(|v| v.read(&self.path).ok());
        if text != self.text || self.base.is_none() {
            self.base = Some(text.as_deref().map_or_else(|| Err("This file can't be read.".to_string()), base::parse));
            self.text = text;
        }
    }
}

impl Surface for BaseSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let mut out = PanelOut::default();
        self.refresh(ws);
        let margin = egui::Margin::symmetric(24, 16);
        egui::Frame::NONE.inner_margin(margin).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.path.stem()).size(22.0).color(pal.strong));
                ui.label(RichText::new("Obsidian base").size(12.0).color(pal.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Convert to note")
                        .on_hover_text("Make a note with one quire-view per view; this file stays as it is")
                        .clicked()
                    {
                        ws.plan_request(Action::ConvertBase(self.path.clone()));
                    }
                });
            });
            let base = match &self.base {
                Some(Ok(b)) => b.clone(),
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(pal.danger));
                    return;
                }
                None => return,
            };
            if base.views.len() > 1 {
                ui.horizontal(|ui| {
                    for (i, v) in base.views.iter().enumerate() {
                        ui.selectable_value(&mut self.view, i, &v.name);
                    }
                });
            }
            let view = self.view.min(base.views.len().saturating_sub(1));
            let Some(v) = base.views.get(view) else { return };
            if !v.unsupported.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(format!("Not shown: {}.", v.unsupported.join("; "))).size(12.0).color(pal.warn)).wrap(),
                );
            }
            ui.add_space(6.0);
            let table = self.table(ws, &base, view);
            if let Some(e) = &table.error {
                ui.label(RichText::new(e).color(pal.danger));
                return;
            }
            egui::ScrollArea::vertical().id_salt(("quire-base", self.path.as_str())).auto_shrink([false, false]).show(ui, |ui| {
                let key = Id::unique(("quire-base-view", self.path.as_str(), self.view));
                let action = match v.kind {
                    ViewKind::Table => base_table(ui, pal, &table, key),
                    ViewKind::Cards => cards(ui, pal, &table),
                };
                if let Some(ViewAction::Open(p)) = &action {
                    if ws.open_note(p).is_some() {
                        out.jumped = true;
                    }
                } else if let Some(a) = action {
                    route(ws, a);
                }
            });
        });
        out
    }
}

fn count_line(ui: &mut Ui, pal: &Palette, t: &Table) {
    let more = if t.total > t.rows.len() { format!(" · showing {}", t.rows.len()) } else { String::new() };
    ui.label(RichText::new(format!("{} note{}{more}", t.total, if t.total == 1 { "" } else { "s" })).size(11.0).color(pal.muted).strong());
}

/// A worked-out cell: formulas read-only, properties edited in place, errors said in the cell.
fn cell(ui: &mut Ui, pal: &Palette, key: Id, path: &NotePath, c: &Cell, act: &mut Option<ViewAction>) {
    match c {
        Cell::Name(title) => {
            let r = ui.add(egui::Label::new(RichText::new(title).color(pal.accent)).truncate().sense(Sense::click()));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(path.as_str()).clicked() {
                *act = Some(ViewAction::Open(path.clone()));
            }
        }
        Cell::Property { field, value } => crate::view_ui::editable_cell(ui, pal, key, path, field, value, act),
        Cell::Computed(v) => {
            ui.add(egui::Label::new(RichText::new(v).size(13.0).color(pal.text)).truncate()).on_hover_text("Worked out by a formula");
        }
        Cell::Error(e) => {
            ui.add(egui::Label::new(RichText::new(format!("⚠ {e}")).size(12.0).color(pal.warn)).truncate()).on_hover_text(e.as_str());
        }
    }
}

/// A table view with formula columns and a row of summaries.
fn base_table(ui: &mut Ui, pal: &Palette, t: &Table, key: Id) -> Option<ViewAction> {
    let mut act = None;
    count_line(ui, pal, t);
    if t.rows.is_empty() {
        ui.label(RichText::new("No notes match.").color(pal.muted));
        return None;
    }
    let cols = t.headings.len().max(1);
    let col_w = ((ui.available_width() - 8.0 * cols as f32) / cols as f32).max(60.0);
    egui::ScrollArea::horizontal().id_salt(key.with("h")).show(ui, |ui| {
        egui::Grid::new(key.with("grid")).striped(true).num_columns(cols).spacing([10.0, 5.0]).min_col_width(col_w.min(200.0)).show(
            ui,
            |ui| {
                for h in &t.headings {
                    ui.label(RichText::new(h).size(12.0).color(pal.muted).strong());
                }
                ui.end_row();
                for row in t.rows.iter().take(MAX_ROWS) {
                    for c in &row.cells {
                        ui.scope(|ui| {
                            ui.set_max_width(col_w.max(80.0));
                            cell(ui, pal, key, &row.path, c, &mut act);
                        });
                    }
                    ui.end_row();
                }
                if t.summaries.iter().any(Option::is_some) {
                    for s in &t.summaries {
                        match s {
                            Some((name, Ok(v))) => {
                                ui.label(RichText::new(format!("{name}  {v}")).size(12.0).color(pal.strong).strong());
                            }
                            Some((name, Err(e))) => {
                                ui.label(RichText::new(format!("{name}: ⚠")).size(12.0).color(pal.warn)).on_hover_text(e.as_str());
                            }
                            None => {
                                ui.label("");
                            }
                        }
                    }
                    ui.end_row();
                }
            },
        );
    });
    act
}

/// A cards view: one card per note, its columns under the title, and the summaries under the cards.
fn cards(ui: &mut Ui, pal: &Palette, t: &Table) -> Option<ViewAction> {
    count_line(ui, pal, t);
    let mut act = None;
    let card_w = 220.0;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
        for row in t.rows.iter().take(MAX_ROWS) {
            egui::Frame::NONE.fill(pal.panel).stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(10.0).show(
                ui,
                |ui| {
                    ui.set_width(card_w);
                    ui.set_min_height(64.0);
                    ui.vertical(|ui| {
                        for (h, c) in t.headings.iter().zip(&row.cells) {
                            match c {
                                Cell::Name(title) => {
                                    let r = ui.add(
                                        egui::Label::new(RichText::new(title).size(14.0).color(pal.accent))
                                            .truncate()
                                            .sense(Sense::click()),
                                    );
                                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(row.path.as_str()).clicked() {
                                        act = Some(ViewAction::Open(row.path.clone()));
                                    }
                                }
                                Cell::Property { value: v, .. } | Cell::Computed(v) if !v.is_empty() => {
                                    ui.add(egui::Label::new(RichText::new(format!("{h}: {v}")).size(12.0).color(pal.muted)).truncate());
                                }
                                Cell::Error(e) => {
                                    ui.add(egui::Label::new(RichText::new(format!("{h}: ⚠ {e}")).size(12.0).color(pal.warn)).truncate());
                                }
                                _ => {}
                            }
                        }
                    });
                },
            );
        }
    });
    if t.rows.is_empty() {
        ui.label(RichText::new("No notes match.").color(pal.muted));
    }
    let sums: Vec<String> = t
        .headings
        .iter()
        .zip(&t.summaries)
        .filter_map(|(h, s)| s.as_ref().map(|(n, v)| format!("{h} · {n} {}", v.as_deref().unwrap_or("⚠"))))
        .collect();
    if !sums.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new(sums.join("    ")).size(12.0).color(pal.strong).strong());
    }
    act
}

/// Rows drawn at most (a base over a big vault).
const MAX_ROWS: usize = 300;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_opens_as_a_surface() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(quire_app::vault::Vault::in_memory(&[
            ("P/A.md", "---\nstatus: doing\n---\n# A\n"),
            ("Work.base", "filters:\n  and:\n    - file.inFolder(\"P\")\nviews:\n  - type: table\n    name: All\n    order: [status]\n  - type: cards\n    name: Cards\n"),
        ]));
        ws.index().flush();
        let mut s = crate::registry::open_surface(&NotePath::new("Work.base")).expect("registered");
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                s.show(ui, &mut ws, &crate::theme::LIGHT);
            });
            out.textures_delta.clear();
        }
        assert_eq!(s.title(), "Work.base");
    }
}
