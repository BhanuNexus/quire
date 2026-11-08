// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Boards (0.11): the kanban layout of `quire-view` blocks, `.base` kanban
//! views and the agenda's task board.
//!
//! * Drag a card by its handle to another column (its group property
//!   changes) or to another place in its column (the hand-placed order is
//!   kept in the view or base, not in the notes: `order.<column>` or
//!   `cardOrder`).
//! * Swimlanes: rows of columns by a second property.
//! * Coloured columns: right-click a column's heading.
//! * Cards edit their status, priority and due date in place (front-matter
//!   edits, or the task line's fields).
//! * "+" in a column makes a note from the view's template, or a task note,
//!   with the column's value set.
//!
//! Owned by the 0.11 tasks workstream.

use chrono::{Duration, Local, NaiveDate};
use egui::{Color32, Id, Rect, RichText, Sense, Stroke, Ui};
use quire_app::Workspace;
use quire_app::index::views::{ViewResult, ViewRow, ViewSpec};
use quire_app::markdown::Value;
use quire_app::markdown::tasks::Field;
use quire_app::plan::task_note::{NewTask, Prop};
use quire_app::tasks_state::{Action, BoardSetting, BoardTarget};
use quire_app::vault::NotePath;

use crate::theme::Palette;
use crate::view_ui::ViewAction;

/// Named column colours (and any `#rrggbb`).
pub(crate) const COLORS: [(&str, u32); 9] = [
    ("red", 0x00e5_484d),
    ("orange", 0x00f7_6b15),
    ("yellow", 0x00e2_a336),
    ("green", 0x0030_a46c),
    ("teal", 0x0012_a594),
    ("blue", 0x003e_63dd),
    ("purple", 0x008e_4ec6),
    ("pink", 0x00d6_409f),
    ("gray", 0x008b_8d98),
];

/// A colour by name or `#rrggbb`.
pub(crate) fn color(name: &str) -> Option<Color32> {
    let n = name.trim().to_ascii_lowercase();
    if let Some((_, rgb)) = COLORS.iter().find(|(c, _)| *c == n || (n == "grey" && *c == "gray")) {
        return Some(Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, *rgb as u8));
    }
    let hex = n.strip_prefix('#').filter(|h| h.len() == 6)?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// What's dragged: one card.
#[derive(Clone, Debug)]
struct CardDrag {
    id: String,
    path: NotePath,
    group: String,
    lane: String,
    /// An inline task: (offset, text, done).
    inline: Option<(usize, String, bool)>,
    is_task_note: bool,
}

/// The status values a column can take for a task board.
fn task_columns(ws: &Workspace, r: &ViewResult) -> Vec<String> {
    let prefs = ws.task_prefs();
    prefs.columns(r.groups.iter().map(String::as_str).filter(|g| !g.is_empty()))
}

/// Where a board's settings go, and what it may change.
pub(crate) struct Board<'a> {
    pub spec: &'a ViewSpec,
    pub result: &'a ViewResult,
    pub target: Option<BoardTarget>,
    /// The note holding the view (for "+ New" in its folder).
    pub from: Option<&'a NotePath>,
    pub key: Id,
}

/// What a card's drop means, worked out after the frame's drawing.
struct Drop {
    card: CardDrag,
    column: String,
    lane: Option<String>,
    index: usize,
}

/// Draw a board. Returns a note to open, or a note's group value to change.
pub(crate) fn show(ui: &mut Ui, ws: &Workspace, pal: &Palette, b: &Board<'_>) -> Option<ViewAction> {
    let spec = b.spec;
    let r = b.result;
    let field = spec.group.clone().unwrap_or_else(|| "status".into());
    let mut open = None;
    let tasks = spec.tasks;
    // Columns: the view's (and for a task board grouped by status, the vault's statuses).
    let mut columns = if r.groups.is_empty() { vec![String::new()] } else { r.groups.clone() };
    if tasks && field == "status" {
        columns = task_columns(ws, r);
        if r.rows.iter().any(|x| x.group.is_empty()) {
            columns.push(String::new());
        }
    } else if field == "status" && spec.columns.is_empty() {
        // Statuses in the vault's order (`[tasks] statuses`), others after.
        let order = ws.task_prefs().statuses;
        let pos = |g: &String| order.iter().position(|s| s.eq_ignore_ascii_case(g)).unwrap_or(order.len());
        columns.sort_by_key(|g| (g.is_empty(), pos(g)));
    }
    let lanes: Vec<Option<String>> =
        if spec.lanes.is_some() && !r.lanes.is_empty() { r.lanes.iter().cloned().map(Some).collect() } else { vec![None] };
    ui.horizontal(|ui| {
        let n = r.total;
        let what = if tasks { "task" } else { "note" };
        ui.label(
            RichText::new(format!("Board by {field} · {n} {what}{}", if n == 1 { "" } else { "s" })).size(11.0).color(pal.muted).strong(),
        );
        if let Some(t) = &b.target {
            lanes_menu(ui, ws, pal, spec, t, b.key);
        }
    });
    let n = columns.len();
    let col_w = ((ui.available_width() - 8.0 * (n as f32 - 1.0) - 12.0 * n as f32) / n as f32).clamp(170.0, 260.0);
    let mut drop: Option<Drop> = None;
    egui::ScrollArea::horizontal().id_salt(b.key.with("board-h")).show(ui, |ui| {
        ui.vertical(|ui| {
            for lane in &lanes {
                if let Some(l) = lane {
                    let shown = if l.is_empty() { "(none)".to_string() } else { l.clone() };
                    let count = r.rows.iter().filter(|x| x.lane.eq_ignore_ascii_case(l)).count();
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("{} {shown} · {count}", spec.lanes.as_deref().unwrap_or("")))
                            .size(12.0)
                            .color(pal.strong)
                            .strong(),
                    );
                }
                let in_lane = |x: &&ViewRow| lane.as_ref().is_none_or(|l| x.lane.eq_ignore_ascii_case(l));
                let tallest = columns
                    .iter()
                    .map(|g| r.rows.iter().filter(in_lane).filter(|x| x.group.eq_ignore_ascii_case(g)).count())
                    .max()
                    .unwrap_or(0);
                let col_h = 30.0 + tallest.max(1) as f32 * 64.0;
                ui.horizontal_top(|ui| {
                    for g in &columns {
                        let tint = spec.colors.iter().find(|(c, _)| c.eq_ignore_ascii_case(g)).and_then(|(_, c)| color(c));
                        let plain = blend(pal.bg, pal.text, 0.04);
                        let fill = tint.map_or(plain, |c| blend(pal.bg, c, if pal.dark { 0.16 } else { 0.10 }));
                        let frame = egui::Frame::NONE.fill(fill).corner_radius(6.0).inner_margin(6.0);
                        let mut rects: Vec<Rect> = Vec::new();
                        let (zone, dropped) = ui.dnd_drop_zone::<CardDrag, ()>(frame, |ui| {
                            ui.set_width(col_w);
                            ui.set_min_height(col_h);
                            ui.vertical(|ui| {
                                let rows: Vec<&ViewRow> =
                                    r.rows.iter().filter(in_lane).filter(|x| x.group.eq_ignore_ascii_case(g)).collect();
                                column_header(ui, ws, pal, b, g, rows.len(), tint, lane.as_deref());
                                for row in rows.iter().take(200) {
                                    let rect = card(ui, ws, pal, b, &field, row, col_w, &mut open);
                                    rects.push(rect);
                                }
                            });
                        });
                        if let Some(tint) = tint {
                            let top = zone.response.rect;
                            ui.painter().line_segment(
                                [top.left_top() + egui::vec2(6.0, 1.5), top.right_top() + egui::vec2(-6.0, 1.5)],
                                Stroke::new(3.0, tint),
                            );
                        }
                        // A line where the card would land.
                        if let Some(p) = ui.ctx().pointer_hover_pos().filter(|p| zone.response.rect.contains(*p))
                            && egui::DragAndDrop::has_payload_of_type::<CardDrag>(ui.ctx())
                        {
                            let i = rects.iter().filter(|rc| rc.center().y < p.y).count();
                            let y = rects.get(i).map_or_else(
                                || rects.last().map_or_else(|| zone.response.rect.top() + 28.0, |l| l.bottom() + 3.0),
                                |rc| rc.top() - 3.0,
                            );
                            let x = zone.response.rect.x_range();
                            ui.painter().hline(x.min + 6.0..=x.max - 6.0, y, Stroke::new(2.0, pal.accent));
                        }
                        if let Some(card) = dropped {
                            let p = ui.ctx().pointer_interact_pos().unwrap_or_default();
                            let index = rects.iter().filter(|rc| rc.center().y < p.y).count();
                            drop = Some(Drop { card: (*card).clone(), column: g.clone(), lane: lane.clone(), index });
                        }
                    }
                });
            }
        });
    });
    let moved = drop.and_then(|d| dropped(ws, b, &field, r, &d));
    open.map(ViewAction::Open).or(moved)
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// "Lanes: …": swimlanes by a property, written to the view.
fn lanes_menu(ui: &mut Ui, ws: &Workspace, pal: &Palette, spec: &ViewSpec, target: &BoardTarget, key: Id) {
    let current = spec.lanes.clone();
    let mut options: Vec<String> = ["priority", "status", "tags"].map(String::from).to_vec();
    if spec.tasks {
        options.extend(["contexts", "projects", "note"].map(String::from));
    }
    for f in &spec.fields {
        if !options.contains(f) && !matches!(f.as_str(), "title" | "text") {
            options.push(f.clone());
        }
    }
    options.retain(|o| Some(o) != spec.group.as_ref());
    let label = current.as_deref().map_or_else(|| "Swimlanes: none".to_string(), |l| format!("Swimlanes: {l}"));
    egui::ComboBox::from_id_salt(key.with("lanes")).selected_text(RichText::new(label).size(11.5).color(pal.muted)).width(150.0).show_ui(
        ui,
        |ui| {
            if ui.selectable_label(current.is_none(), "None").clicked() {
                ws.plan_request(Action::ViewSetting { target: target.clone(), setting: BoardSetting::Lanes(None) });
            }
            for o in &options {
                if ui.selectable_label(current.as_deref() == Some(o.as_str()), o).clicked() {
                    ws.plan_request(Action::ViewSetting { target: target.clone(), setting: BoardSetting::Lanes(Some(o.clone())) });
                }
            }
        },
    );
}

/// A column's heading: name and count, a colour menu (right-click), and "+".
#[allow(clippy::too_many_arguments, reason = "a board column's whole context; split it and every caller passes a struct")]
fn column_header(
    ui: &mut Ui,
    ws: &Workspace,
    pal: &Palette,
    b: &Board<'_>,
    g: &str,
    count: usize,
    tint: Option<Color32>,
    lane: Option<&str>,
) {
    let name = if g.is_empty() { "(none)".to_string() } else { g.to_string() };
    let new_id = b.key.with(("new-in", g, lane.unwrap_or("")));
    ui.horizontal(|ui| {
        let head = ui.add(
            egui::Label::new(RichText::new(format!("{name} · {count}")).size(12.0).color(tint.unwrap_or(pal.muted)).strong())
                .sense(Sense::click()),
        );
        if let Some(t) = &b.target {
            head.on_hover_text("Right-click for the column's colour").context_menu(|ui| {
                ui.label(RichText::new("Column colour").size(11.0).color(pal.muted));
                for (c, _) in COLORS {
                    let swatch = color(c).unwrap_or(pal.muted);
                    if ui.button(RichText::new(format!("● {c}")).color(swatch)).clicked() {
                        ws.plan_request(Action::ViewSetting {
                            target: t.clone(),
                            setting: BoardSetting::Color { column: g.to_string(), color: Some(c.to_string()) },
                        });
                        ui.close();
                    }
                }
                if ui.button("No colour").clicked() {
                    ws.plan_request(Action::ViewSetting {
                        target: t.clone(),
                        setting: BoardSetting::Color { column: g.to_string(), color: None },
                    });
                    ui.close();
                }
            });
        }
        // A template, a task board, or a view of a folder (a plain note there).
        let can_new = b.spec.new.is_some()
            || (b.spec.tasks && b.spec.group.as_deref().unwrap_or("status") == "status")
            || (!b.spec.tasks && crate::template_prompt_ui::folder_of(b.spec, b.from).is_some());
        if can_new {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = if b.spec.new.is_some() || !b.spec.tasks {
                    format!("New from template, {} = {name}", b.spec.group.as_deref().unwrap_or("status"))
                } else {
                    format!("New task, status {name}")
                };
                if ui.small_button("+").on_hover_text(hint).clicked() {
                    ui.data_mut(|d| {
                        d.insert_temp(new_id, String::new());
                        d.insert_temp(new_id.with("fresh"), true);
                    });
                }
            });
        }
    });
    // The "+" form: a title, Enter makes it.
    let Some(mut draft) = ui.data(|d| d.get_temp::<String>(new_id)) else { return };
    let r = ui.add(
        egui::TextEdit::singleline(&mut draft)
            .id(new_id.with("t"))
            .hint_text(if b.spec.tasks && b.spec.new.is_none() { "New task…" } else { "Title" })
            .desired_width(f32::INFINITY),
    );
    // Focus the box once, when it opens (the editor would take the keys otherwise).
    if ui.data_mut(|d| d.remove_temp::<bool>(new_id.with("fresh"))).is_some() {
        r.request_focus();
    }
    let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
    if esc || (r.lost_focus() && !enter && draft.trim().is_empty() && !r.gained_focus()) {
        ui.data_mut(|d| d.remove::<String>(new_id));
        return;
    }
    if enter && !draft.trim().is_empty() {
        ui.data_mut(|d| d.remove::<String>(new_id));
        let field = b.spec.group.clone().unwrap_or_else(|| "status".into());
        let mut preset = vec![(field, Value::Str(g.to_string()))];
        if let (Some(lf), Some(l)) = (b.spec.lanes.as_ref(), lane.filter(|l| !l.is_empty())) {
            preset.push((lf.clone(), Value::Str(l.to_string())));
        }
        if let Some(tpl) = b.spec.new.as_deref() {
            let template =
                NotePath::new(if std::path::Path::new(tpl).extension().is_some() { tpl.to_string() } else { format!("{tpl}.md") });
            let folder = crate::template_prompt_ui::folder_of(b.spec, b.from);
            let title = Some(draft.trim().to_string());
            ws.plan_request(Action::New { template, title, folder, answers: Vec::new(), preset });
        } else if !b.spec.tasks {
            let folder = crate::template_prompt_ui::folder_of(b.spec, b.from).unwrap_or_default();
            ws.plan_request(Action::NewNoteIn { folder, title: draft.trim().to_string(), preset });
        } else {
            let mut t =
                NewTask { title: draft.trim().to_string(), status: Some(g.to_string()).filter(|s| !s.is_empty()), ..NewTask::default() };
            if let (Some("priority"), Some(l)) = (b.spec.lanes.as_deref(), lane) {
                t.priority = Some(l.to_string()).filter(|s| !s.is_empty());
            }
            ws.plan_request(Action::NewTask(Box::new(t), false));
        }
    } else {
        ui.data_mut(|d| d.insert_temp(new_id, draft));
    }
}

/// One card. Returns its rect (for working out where a drop lands).
#[allow(clippy::too_many_arguments, reason = "a card's whole context")]
fn card(
    ui: &mut Ui,
    ws: &Workspace,
    pal: &Palette,
    b: &Board<'_>,
    field: &str,
    row: &ViewRow,
    col_w: f32,
    open: &mut Option<NotePath>,
) -> Rect {
    let id = b.key.with(("card", row.id.as_str()));
    let payload = CardDrag {
        id: row.id.clone(),
        path: row.path.clone(),
        group: row.group.clone(),
        lane: row.lane.clone(),
        inline: row.task.as_ref().filter(|t| !t.is_note).map(|t| (t.task.offset, t.task.text.clone(), t.task.done)),
        is_task_note: row.task.as_ref().is_some_and(|t| t.is_note),
    };
    let resp = egui::Frame::NONE.fill(pal.panel).stroke(Stroke::new(1.0, pal.border)).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.set_width(col_w - 12.0);
        ui.horizontal(|ui| {
            ui.dnd_drag_source(id, payload, |ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), Sense::hover());
                for k in 0..3 {
                    let y = r.top() + 3.0 + k as f32 * 4.0;
                    ui.painter().line_segment([egui::pos2(r.left() + 1.0, y), egui::pos2(r.right() - 1.0, y)], Stroke::new(1.5, pal.faint));
                }
            })
            .response
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text(format!("Drag to another column to change {field}, or up and down to reorder"));
            let done = row.task.as_ref().is_some_and(|t| t.task.done);
            let mut title = RichText::new(&row.title).color(pal.accent);
            if done {
                title = title.strikethrough().color(pal.muted);
            }
            let r = ui.add(egui::Label::new(title).truncate().sense(Sense::click()));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(row.path.as_str()).clicked() {
                *open = Some(row.path.clone());
            }
            if let Some(t) = &row.task {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let line = (!t.is_note).then_some((t.task.offset, t.task.text.as_str()));
                    crate::task_ui::timer_button(ui, ws, pal, &row.path, line, &t.task.text);
                });
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 2.0);
            chips(ui, ws, pal, b, field, row);
        });
    });
    resp.response.rect
}

/// A small clickable chip.
fn chip(ui: &mut Ui, pal: &Palette, text: &str, color: Color32, hover: &str) -> egui::Response {
    let t = RichText::new(text).size(11.0).color(color);
    ui.add(egui::Button::new(t).fill(pal.bg).stroke(Stroke::new(1.0, pal.border)).corner_radius(8.0).min_size(egui::vec2(0.0, 16.0)))
        .on_hover_text(hover)
}

/// The card's properties: status, priority and due edit in place; the
/// view's other fields are shown.
fn chips(ui: &mut Ui, ws: &Workspace, pal: &Palette, b: &Board<'_>, field: &str, row: &ViewRow) {
    let today = Local::now().date_naive();
    let get = |name: &str| b.spec.fields.iter().position(|f| f == name).and_then(|i| row.cells.get(i)).filter(|c| !c.is_empty()).cloned();
    let (status, priority, due) = if let Some(t) = &row.task {
        (Some(t.status.clone()), Some(quire_app::markdown::tasks::priority_word(t.task.priority).to_string()), t.task.due.clone())
    } else {
        let ix = ws.index().read();
        let rec = ix.get(&row.path);
        let prop = |n: &str| rec.and_then(|r| quire_app::index::views::property(r, n)).map(Value::display).filter(|s| !s.is_empty());
        (prop("status"), prop("priority"), prop("due"))
    };
    // Status (unless the columns are the status).
    if field != "status"
        && let Some(s) = status
    {
        let r = chip(ui, pal, &s, pal.muted, "Click to change the status");
        let options = if row.task.is_some() { ws.task_prefs().statuses } else { b.result.groups.clone() };
        popup(&r, |ui| {
            for o in options.iter().filter(|o| !o.is_empty()) {
                if ui.selectable_label(o.eq_ignore_ascii_case(&s), o).clicked() {
                    set(ws, row, Prop::Status, "status", Some(o.clone()));
                    ui.close();
                }
            }
        });
    }
    if field != "priority"
        && let Some(p) = priority
    {
        let normal = p == "normal";
        let color = match quire_app::markdown::tasks::parse_priority(&p) {
            Some(0 | 1) => pal.danger,
            Some(2) => pal.warn,
            _ => pal.muted,
        };
        let r = chip(ui, pal, if normal { "priority" } else { &p }, if normal { pal.faint } else { color }, "Click to change the priority");
        popup(&r, |ui| {
            for o in ["highest", "high", "medium", "normal", "low", "lowest"] {
                if ui.selectable_label(o == p, o).clicked() {
                    let v = (o != "normal").then(|| o.to_string());
                    set(ws, row, Prop::Priority, "priority", v);
                    ui.close();
                }
            }
        });
    }
    if field != "due" {
        let d = due.as_deref().and_then(|d| NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok());
        let (text, color) = match d {
            Some(d) if d < today => (format!("due {}", crate::agenda_ui::relative(d, today)), pal.danger),
            Some(d) => (format!("due {}", crate::agenda_ui::relative(d, today)), pal.text),
            None => ("+ due".to_string(), pal.faint),
        };
        let r = chip(ui, pal, &text, color, "Click to set the due date");
        let draft_id = b.key.with(("due-draft", row.id.as_str()));
        popup(&r, |ui| {
            let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
            for (label, v) in [("Today", today), ("Tomorrow", today + Duration::days(1)), ("Next week", today + Duration::days(7))] {
                if ui.button(format!("{label} · {}", v.format("%a %-d %b"))).clicked() {
                    set(ws, row, Prop::Due, "due", Some(fmt(v)));
                    ui.close();
                }
            }
            let mut draft: String = ui.data(|m| m.get_temp(draft_id)).unwrap_or_else(|| due.clone().unwrap_or_default());
            let te = ui.add(egui::TextEdit::singleline(&mut draft).hint_text("YYYY-MM-DD").desired_width(120.0));
            if te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && quire_app::markdown::frontmatter::is_date(draft.trim()) {
                set(ws, row, Prop::Due, "due", Some(draft.trim().to_string()));
                ui.data_mut(|m| m.remove::<String>(draft_id));
                ui.close();
            } else {
                ui.data_mut(|m| m.insert_temp(draft_id, draft));
            }
            if due.is_some() && ui.button("No due date").clicked() {
                set(ws, row, Prop::Due, "due", None);
                ui.close();
            }
        });
    }
    if let Some(t) = &row.task
        && (t.spent.is_some() || t.estimate.is_some())
    {
        let fmt = quire_app::plan::time::format_duration;
        let text = match (t.spent, t.estimate) {
            (Some(s), Some(e)) => format!("⏱ {} / {}", fmt(s), fmt(e)),
            (Some(s), None) => format!("⏱ {}", fmt(s)),
            (None, Some(e)) => format!("est. {}", fmt(e)),
            (None, None) => String::new(),
        };
        let over = matches!((t.spent, t.estimate), (Some(s), Some(e)) if s > e);
        ui.add(egui::Label::new(RichText::new(text).size(11.0).color(if over { pal.warn } else { pal.muted })).extend());
    }
    for f in &b.spec.fields {
        if matches!(f.as_str(), "title" | "text" | "status" | "priority" | "due" | "time_spent" | "time_estimate") || f == field {
            continue;
        }
        if let Some(v) = get(f) {
            ui.label(RichText::new(v).size(11.0).color(pal.muted));
        }
    }
}

/// A popup under a chip while it's open.
fn popup(r: &egui::Response, add: impl FnOnce(&mut Ui)) {
    egui::Popup::menu(r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.set_min_width(140.0);
        add(ui);
    });
}

/// Set a card's property: a task note's (under the name it uses), a task
/// line's field, or a note's front matter.
fn set(ws: &Workspace, row: &ViewRow, prop: Prop, key: &str, value: Option<String>) {
    match &row.task {
        Some(t) if t.is_note => ws.plan_request(Action::SetTaskProperty { path: row.path.clone(), prop, value: value.map(Value::Str) }),
        Some(t) => {
            let field = match prop {
                Prop::Priority => Field::Priority,
                Prop::Due => Field::Due,
                Prop::Status => {
                    // A task line is open or done.
                    let done = value.as_deref().is_some_and(|v| ws.task_prefs().is_done(v));
                    if done != t.task.done {
                        ws.plan_request(Action::ToggleTask { path: row.path.clone(), offset: t.task.offset, text: t.task.text.clone() });
                    }
                    return;
                }
                _ => return,
            };
            ws.plan_request(Action::SetTaskField {
                path: row.path.clone(),
                offset: t.task.offset,
                text: t.task.text.clone(),
                field,
                value,
            });
        }
        None => match value {
            Some(v) => ws.plan_request(Action::SetProperty { path: row.path.clone(), key: key.to_string(), value: Value::Str(v) }),
            None => ws.plan_request(Action::SetProperty { path: row.path.clone(), key: key.to_string(), value: Value::Str(String::new()) }),
        },
    }
}

/// A card dropped: its group (and lane) change if it moved, and its place
/// in the column is written to the view. A plain note's new group comes
/// back as [`ViewAction::Move`].
fn dropped(ws: &Workspace, b: &Board<'_>, field: &str, r: &ViewResult, d: &Drop) -> Option<ViewAction> {
    let c = &d.card;
    let moved = !c.group.eq_ignore_ascii_case(&d.column);
    let mut out = None;
    if moved {
        let value = d.column.clone();
        if c.is_task_note && field == "status" {
            ws.plan_request(Action::SetTaskProperty { path: c.path.clone(), prop: Prop::Status, value: Some(Value::Str(value)) });
        } else if let Some((offset, text, done)) = &c.inline {
            if field == "status" {
                let to_done = ws.task_prefs().is_done(&value);
                if to_done != *done {
                    ws.plan_request(Action::ToggleTask { path: c.path.clone(), offset: *offset, text: text.clone() });
                }
            }
        } else {
            out = Some(ViewAction::Move { path: c.path.clone(), field: field.to_string(), value });
        }
    }
    if let (Some(lf), Some(l)) = (b.spec.lanes.as_ref(), d.lane.as_ref())
        && !c.lane.eq_ignore_ascii_case(l)
        && c.inline.is_none()
    {
        let v = Value::Str(l.clone());
        if c.is_task_note && lf == "priority" {
            ws.plan_request(Action::SetTaskProperty { path: c.path.clone(), prop: Prop::Priority, value: (!l.is_empty()).then_some(v) });
        } else {
            ws.plan_request(Action::SetProperty { path: c.path.clone(), key: lf.clone(), value: v });
        }
    }
    // The order: the column's cards as shown (all lanes), with this one at its place.
    let Some(target) = &b.target else { return out };
    let in_col: Vec<&ViewRow> = r.rows.iter().filter(|x| x.group.eq_ignore_ascii_case(&d.column) && x.id != c.id).collect();
    // `index` counts cards in the drop's lane; find the card it lands before.
    let lane_cards: Vec<&&ViewRow> = in_col.iter().filter(|x| d.lane.as_ref().is_none_or(|l| x.lane.eq_ignore_ascii_case(l))).collect();
    let before = lane_cards.get(d.index).map(|x| x.id.clone());
    let mut ids: Vec<String> = in_col.iter().map(|x| x.id.clone()).collect();
    let at = before.and_then(|b| ids.iter().position(|i| *i == b)).unwrap_or(ids.len());
    ids.insert(at, c.id.clone());
    let unchanged = !moved && r.rows.iter().filter(|x| x.group.eq_ignore_ascii_case(&d.column)).map(|x| &x.id).eq(ids.iter());
    if !unchanged {
        ws.plan_request(Action::ViewSetting { target: target.clone(), setting: BoardSetting::Order { column: d.column.clone(), ids } });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(color("Blue"), Some(Color32::from_rgb(0x3e, 0x63, 0xdd)));
        assert_eq!(color("#102030"), Some(Color32::from_rgb(0x10, 0x20, 0x30)));
        assert_eq!(color("grey"), color("gray"));
        assert!(color("sparkly").is_none() && color("#12").is_none());
    }
}
