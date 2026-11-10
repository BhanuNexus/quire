// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live `quire-view` results drawn under the block in the fast editor, and
//! the properties section of the insights panel.

use egui::{Id, RichText, Sense, Ui};
use quire_app::DocId;
use quire_app::Workspace;
use quire_app::index::views::{Layout, ViewResult, ViewSpec};
use quire_app::markdown::Kind;
use quire_app::properties::parse_input;
use quire_app::vault::NotePath;

use crate::theme::Palette;

/// What the user did inside a rendered view.
pub(crate) enum ViewAction {
    Open(NotePath),
    /// Kanban drop: set `field` to `value` in the note.
    Move {
        path: NotePath,
        field: String,
        value: String,
    },
    /// A table cell was edited: set `field` from what was typed.
    Edit {
        path: NotePath,
        field: String,
        text: String,
    },
}

/// Fields computed from the note itself, shown read-only in tables.
const COMPUTED: [&str; 11] = ["title", "name", "path", "file", "folder", "tags", "tasks", "open_tasks", "done_tasks", "words", "links"];

/// The date format for views, set by the app each frame (`[editing] date_format`).
pub(crate) fn date_format(ctx: &egui::Context) -> String {
    ctx.data(|d| d.get_temp::<String>(Id::unique("quire-date-format"))).unwrap_or_default()
}

pub(crate) fn set_date_format(ctx: &egui::Context, format: &str) {
    if date_format(ctx) != format {
        ctx.data_mut(|d| d.insert_temp(Id::unique("quire-date-format"), format.to_string()));
    }
}

/// A table cell that edits its property in place: click to type, Enter or
/// clicking away saves, Escape cancels. Yes/no values are checkboxes.
pub(crate) fn editable_cell(ui: &mut Ui, pal: &Palette, key: Id, path: &NotePath, field: &str, cell: &str, act: &mut Option<ViewAction>) {
    let id = key.with(("cell", path.as_str(), field));
    if matches!(cell, "true" | "false") {
        let mut on = cell == "true";
        if ui.checkbox(&mut on, "").on_hover_text(format!("{field} of {}", path.stem())).changed() {
            *act = Some(ViewAction::Edit { path: path.clone(), field: field.to_string(), text: on.to_string() });
        }
        return;
    }
    let editing: Option<String> = ui.data(|d| d.get_temp::<String>(id));
    // 0.12: dates in the chosen format; the file keeps ISO.
    let fmt = date_format(ui.ctx());
    let is_date = quire_app::markdown::frontmatter::is_date(cell.trim());
    let cell_shown = if is_date { quire_app::datefmt::display(cell, &fmt) } else { cell.to_string() };
    match editing {
        None => {
            let shown =
                if cell.is_empty() { RichText::new("—").color(pal.faint) } else { RichText::new(cell_shown.as_str()).color(pal.text) };
            let r = ui
                .add(egui::Label::new(shown.size(13.0)).truncate().sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::Text)
                .on_hover_text(format!("Click to edit {field}"));
            if r.clicked() {
                ui.data_mut(|d| d.insert_temp(id, cell_shown.clone()));
                ui.memory_mut(|m| m.request_focus(id.with("edit")));
            }
        }
        Some(mut draft) => {
            let r = ui.add(
                egui::TextEdit::singleline(&mut draft).id(id.with("edit")).desired_width(ui.available_width()).font(egui::TextStyle::Body),
            );
            let (enter, escape) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
            if escape {
                ui.data_mut(|d| d.remove::<String>(id));
            } else if r.lost_focus() || enter {
                ui.data_mut(|d| d.remove::<String>(id));
                if draft.trim() != cell_shown {
                    // A date field stays a date: text that isn't one is dropped.
                    let text =
                        if is_date && !draft.trim().is_empty() { quire_app::datefmt::parse(&draft, &fmt, cell) } else { Some(draft) };
                    if let Some(text) = text {
                        *act = Some(ViewAction::Edit { path: path.clone(), field: field.to_string(), text });
                    }
                }
            } else {
                ui.data_mut(|d| d.insert_temp(id, draft));
            }
        }
    }
}

const MAX_ROWS: usize = 200;

fn header(ui: &mut Ui, pal: &Palette, label: &str, r: &ViewResult) {
    let more = if r.total > r.rows.len() { format!(" · showing {}", r.rows.len()) } else { String::new() };
    ui.label(
        RichText::new(format!("{label} · {} note{}{more}", r.total, if r.total == 1 { "" } else { "s" }))
            .size(11.0)
            .color(pal.muted)
            .strong(),
    );
}

fn title_link(ui: &mut Ui, pal: &Palette, title: &str, path: &NotePath, act: &mut Option<ViewAction>) {
    let r = ui.add(egui::Label::new(RichText::new(title).color(pal.accent)).truncate().sense(Sense::click()));
    if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(path.as_str()).clicked() {
        *act = Some(ViewAction::Open(path.clone()));
    }
}

/// Draw a view; returns the action, if any. `key` makes widget ids stable.
pub(crate) fn show(ui: &mut Ui, ws: &Workspace, pal: &Palette, spec_src: &str, from: Option<&NotePath>, key: Id) -> Option<ViewAction> {
    let mut act = None;
    let frame = egui::Frame::NONE.fill(pal.panel).stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(10.0);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let result = ws.run_view(spec_src, from);
        let spec = quire_app::index::views::parse(spec_src).unwrap_or_default();
        match result.as_ref() {
            Err(e) => {
                ui.label(RichText::new(format!("View error: {e}")).color(pal.danger).size(12.5));
            }
            Ok(r) => match spec.layout {
                // 0.11: boards (task boards too) with order, swimlanes and colours; maps.
                Layout::Kanban => {
                    let target = from.map(|n| quire_app::tasks_state::BoardTarget::Block { note: n.clone(), spec: spec_src.to_string() });
                    let b = crate::board_ui::Board { spec: &spec, result: r, target, from, key };
                    act = crate::board_ui::show(ui, ws, pal, &b);
                }
                Layout::Map => {
                    if let Some(p) = crate::map_ui::show(ui, ws, pal, r, key) {
                        act = Some(ViewAction::Open(p));
                    }
                }
                // WS-E: task rows, agenda and timeline layouts.
                _ if spec.tasks => crate::agenda_ui::view(ui, ws, pal, &spec, r, key),
                Layout::Agenda | Layout::Timeline => crate::agenda_ui::view(ui, ws, pal, &spec, r, key),
                Layout::Table => table(ui, pal, &spec, r, key, &mut act),
                Layout::List => list(ui, pal, &spec, r, &mut act),
                Layout::Calendar => calendar(ui, pal, r, key, &mut act),
            },
        }
        // WS-E: "+ New" for views with `new: Templates/…`.
        if let Ok(r) = result.as_ref() {
            crate::template_prompt_ui::new_button(ui, ws, pal, &spec, from, &r.groups, key);
        }
    });
    act
}

fn table(ui: &mut Ui, pal: &Palette, spec: &ViewSpec, r: &ViewResult, key: Id, act: &mut Option<ViewAction>) {
    header(ui, pal, "Table", r);
    if r.rows.is_empty() {
        ui.label(RichText::new("No notes match.").color(pal.muted).size(12.5));
        return;
    }
    // 0.11: `group:` on a table gives one table per group, under its value.
    if spec.group.is_some() && !r.groups.is_empty() {
        for g in &r.groups {
            let rows: Vec<_> = r.rows.iter().filter(|x| x.group.eq_ignore_ascii_case(g)).cloned().collect();
            let shown = if g.is_empty() { "(none)" } else { g.as_str() };
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} {shown} · {}", spec.group.as_deref().unwrap_or(""), rows.len()))
                    .size(12.0)
                    .color(pal.strong)
                    .strong(),
            );
            let sub = ViewResult { total: rows.len(), rows, ..ViewResult::default() };
            let flat = ViewSpec { group: None, ..spec.clone() };
            table_rows(ui, pal, &flat, &sub, key.with(("group", g.as_str())), act);
        }
        return;
    }
    table_rows(ui, pal, spec, r, key, act);
}

fn table_rows(ui: &mut Ui, pal: &Palette, spec: &ViewSpec, r: &ViewResult, key: Id, act: &mut Option<ViewAction>) {
    let cols = spec.fields.len().max(1);
    let col_w = ((ui.available_width() - 8.0 * cols as f32) / cols as f32).max(60.0);
    egui::ScrollArea::horizontal().id_salt(key.with("h")).show(ui, |ui| {
        egui::Grid::new(key.with("grid")).striped(true).num_columns(cols).spacing([8.0, 4.0]).min_col_width(col_w.min(220.0)).show(
            ui,
            |ui| {
                for f in &spec.fields {
                    ui.label(RichText::new(f.replace('_', " ")).size(12.0).color(pal.muted).strong());
                }
                ui.end_row();
                for row in r.rows.iter().take(MAX_ROWS) {
                    for (i, (f, cell)) in spec.fields.iter().zip(&row.cells).enumerate() {
                        ui.scope(|ui| {
                            ui.set_max_width(col_w.max(80.0));
                            if i == 0 || f == "title" {
                                title_link(ui, pal, if cell.is_empty() { &row.title } else { cell }, &row.path, act);
                            } else if COMPUTED.iter().any(|c| c.eq_ignore_ascii_case(f)) {
                                ui.add(egui::Label::new(RichText::new(cell).size(13.0).color(pal.muted)).truncate())
                                    .on_hover_text(format!("{f} is worked out from the note"));
                            } else {
                                editable_cell(ui, pal, key, &row.path, f, cell, act);
                            }
                        });
                    }
                    ui.end_row();
                }
            },
        );
    });
}

fn list(ui: &mut Ui, pal: &Palette, spec: &ViewSpec, r: &ViewResult, act: &mut Option<ViewAction>) {
    header(ui, pal, "List", r);
    for row in r.rows.iter().take(MAX_ROWS) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("•").color(pal.muted));
            title_link(ui, pal, &row.title, &row.path, act);
            let fmt = date_format(ui.ctx());
            let extra: Vec<String> = spec
                .fields
                .iter()
                .zip(&row.cells)
                .filter(|(f, c)| *f != "title" && !c.is_empty())
                .map(|(_, c)| quire_app::datefmt::display(c, &fmt))
                .collect();
            if !extra.is_empty() {
                ui.add(egui::Label::new(RichText::new(extra.join(" · ")).size(12.0).color(pal.muted)).truncate());
            }
        });
    }
    if r.rows.is_empty() {
        ui.label(RichText::new("No notes match.").color(pal.muted).size(12.5));
    }
}

/// Day of week, 0 = Monday (Sakamoto's method).
fn weekday(y: i32, m: u32, d: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    let sun0 = (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d as i32).rem_euclid(7) as u32;
    (sun0 + 6) % 7
}

fn days_in(y: i32, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn calendar(ui: &mut Ui, pal: &Palette, r: &ViewResult, key: Id, act: &mut Option<ViewAction>) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let parse = |s: &str| -> (i32, u32) { (s[..4].parse().unwrap_or(2026), s[5..7].parse().unwrap_or(1)) };
    let mem = key.with("month");
    // Default: the month of the first upcoming dated note, else today.
    let default = r.rows.iter().filter_map(|x| x.date.clone()).filter(|d| *d >= today).min().map_or_else(|| parse(&today), |d| parse(&d));
    let (mut y, mut m) = ui.data(|d| d.get_temp::<(i32, u32)>(mem)).unwrap_or(default);
    ui.horizontal(|ui| {
        if ui.small_button("‹").clicked() {
            (y, m) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        }
        ui.label(RichText::new(format!("{} {y}", MONTHS[(m - 1) as usize])).strong().color(pal.text));
        if ui.small_button("›").clicked() {
            (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        }
        let dated = r.rows.iter().filter(|x| x.date.is_some()).count();
        ui.label(RichText::new(format!("· {dated} dated note{}", if dated == 1 { "" } else { "s" })).size(11.0).color(pal.muted));
    });
    ui.data_mut(|d| d.insert_temp(mem, (y, m)));
    let prefix = format!("{y:04}-{m:02}-");
    let cell_w = ((ui.available_width() - 6.0 * 4.0) / 7.0).max(40.0);
    egui::Grid::new(key.with("cal")).num_columns(7).spacing([4.0, 4.0]).min_col_width(cell_w).max_col_width(cell_w).show(ui, |ui| {
        for d in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
            ui.label(RichText::new(d).size(11.0).color(pal.muted));
        }
        ui.end_row();
        let first = weekday(y, m, 1);
        for _ in 0..first {
            ui.label("");
        }
        let mut col = first;
        for day in 1..=days_in(y, m) {
            let date = format!("{prefix}{day:02}");
            let is_today = date == today;
            ui.vertical(|ui| {
                ui.set_min_height(44.0);
                ui.set_width(cell_w);
                let c = if is_today { pal.accent } else { pal.muted };
                ui.label(RichText::new(day.to_string()).size(11.0).color(c).strong());
                for row in r.rows.iter().filter(|x| x.date.as_deref() == Some(date.as_str())).take(3) {
                    title_link(ui, pal, &row.title, &row.path, act);
                }
                let more = r.rows.iter().filter(|x| x.date.as_deref() == Some(date.as_str())).count();
                if more > 3 {
                    ui.label(RichText::new(format!("+{}", more - 3)).size(11.0).color(pal.muted));
                }
            });
            col += 1;
            if col == 7 {
                col = 0;
                ui.end_row();
            }
        }
    });
}

// ------------------------------------------------------------ properties

/// The "Properties" section of the insights panel for the active note.
pub(crate) fn properties(ui: &mut Ui, ws: &mut Workspace, pal: &Palette, doc: DocId) {
    let props = ws.active_properties();
    let date_fmt = ws.settings.editing.date_format.clone();
    let mut bad_date: Option<String> = None;
    let base = Id::unique(("quire-props", doc.0));
    let mut set: Option<(String, quire_app::markdown::Value)> = None;
    let mut remove: Option<String> = None;
    let mut set_path: Option<(Vec<String>, quire_app::markdown::Value)> = None;
    let mut remove_path: Option<Vec<String>> = None;
    for p in &props {
        if p.kind == Kind::Map {
            // Nested fields: the map's name, then each field under it, edited in place.
            ui.horizontal(|ui| {
                ui.add_sized([72.0, 20.0], egui::Label::new(RichText::new(&p.key).size(12.5).color(pal.muted)).truncate())
                    .on_hover_text(format!("{} · nested fields", p.key));
                // Where the value boxes of the other rows are, so × lines up.
                ui.add_space((ui.available_width() - 48.0).max(60.0) + 8.0);
                if ui.small_button("×").on_hover_text("Remove these fields").clicked() {
                    remove = Some(p.key.clone());
                }
            });
            let mut leaves = Vec::new();
            map_leaves(std::slice::from_ref(&p.key), &p.value, &mut leaves);
            for (path, v) in leaves {
                let depth = path.len() - 1;
                ui.horizontal(|ui| {
                    ui.add_space(10.0 * depth as f32);
                    let name = path.last().cloned().unwrap_or_default();
                    ui.add_sized(
                        [(72.0 - 10.0 * depth as f32).max(40.0), 20.0],
                        egui::Label::new(RichText::new(&name).size(12.0).color(pal.muted)).truncate(),
                    )
                    .on_hover_text(path.join("."));
                    let stored = v.display();
                    let current = if v.kind() == Kind::Date { quire_app::datefmt::display(&stored, &date_fmt) } else { stored.clone() };
                    let w = (ui.available_width() - 48.0).max(60.0);
                    let id = base.with(("draft", path.join(".")));
                    let mut draft = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| current.clone());
                    let r = ui.add(egui::TextEdit::singleline(&mut draft).desired_width(w));
                    if r.has_focus() {
                        ui.data_mut(|d| d.insert_temp(id, draft.clone()));
                    } else {
                        ui.data_mut(|d| d.remove::<String>(id));
                    }
                    if r.lost_focus() && draft.trim() != current {
                        match typed_value(&draft, &v, &stored, &date_fmt) {
                            Ok(nv) => set_path = Some((path.clone(), nv)),
                            Err(msg) => bad_date = Some(msg),
                        }
                    }
                    if ui.small_button("×").on_hover_text("Remove this field").clicked() {
                        remove_path = Some(path.clone());
                    }
                });
            }
            continue;
        }
        ui.horizontal(|ui| {
            ui.add_sized([72.0, 20.0], egui::Label::new(RichText::new(&p.key).size(12.5).color(pal.muted)).truncate())
                .on_hover_text(format!("{} · {}", p.key, kind_name(p.kind)));
            let stored = p.value.display();
            let current = if p.kind == Kind::Date { quire_app::datefmt::display(&stored, &date_fmt) } else { stored.clone() };
            // Leave room for the × button and TextEdit margins, so the panel doesn't grow.
            let w = (ui.available_width() - 48.0).max(60.0);
            if p.kind == Kind::Bool {
                let mut on = current == "true";
                if ui.checkbox(&mut on, "").changed() {
                    set = Some((p.key.clone(), quire_app::markdown::Value::Str(on.to_string())));
                }
                ui.add_space(w - 24.0);
            } else {
                let id = base.with(("draft", &p.key));
                let mut draft = ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| current.clone());
                let r = ui.add(egui::TextEdit::singleline(&mut draft).desired_width(w).hint_text(kind_name(p.kind)));
                if r.has_focus() {
                    ui.data_mut(|d| d.insert_temp(id, draft.clone()));
                } else {
                    ui.data_mut(|d| d.remove::<String>(id));
                }
                if r.lost_focus() && draft.trim() != current {
                    match typed_value(&draft, &p.value, &stored, &date_fmt) {
                        Ok(nv) => set = Some((p.key.clone(), nv)),
                        Err(msg) => bad_date = Some(msg),
                    }
                }
            }
            if ui.small_button("×").on_hover_text("Remove property").clicked() {
                remove = Some(p.key.clone());
            }
        });
    }
    // Add a property.
    let nk = base.with("new-key");
    let nv = base.with("new-value");
    let mut key = ui.data(|d| d.get_temp::<String>(nk)).unwrap_or_default();
    let mut val = ui.data(|d| d.get_temp::<String>(nv)).unwrap_or_default();
    let mut add = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut key).hint_text("new…").desired_width(64.0));
        let w = (ui.available_width() - 72.0).max(60.0);
        let r = ui.add(egui::TextEdit::singleline(&mut val).hint_text("value").desired_width(w));
        add = (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            || ui.add_enabled(!key.trim().is_empty(), egui::Button::new("Add").small()).clicked();
    });
    if add && !key.trim().is_empty() {
        set = Some((key.trim().to_string(), parse_input(&val, false)));
        key.clear();
        val.clear();
    }
    ui.data_mut(|d| {
        d.insert_temp(nk, key);
        d.insert_temp(nv, val);
    });
    if props.is_empty() {
        ui.label(RichText::new("Properties live in the note's front matter, so other apps can read them.").size(11.5).color(pal.faint));
    }
    if let Some(msg) = bad_date {
        ws.notify(quire_app::workspace::NoticeLevel::Warn, msg);
    }
    if let Some((k, v)) = set {
        ws.set_property(&k, &v);
    }
    if let Some(k) = remove {
        ws.remove_property(&k);
    }
    if let Some((path, v)) = set_path {
        let parts: Vec<&str> = path.iter().map(String::as_str).collect();
        ws.set_property_path(&parts, &v);
    }
    if let Some(path) = remove_path {
        let parts: Vec<&str> = path.iter().map(String::as_str).collect();
        ws.remove_property_path(&parts);
    }
}

/// What was typed into a property field as the value to store. A date
/// typed in the chosen format (or as ISO) is stored as ISO; text that is
/// no date in that format is refused with a message, so the field stays a date.
fn typed_value(draft: &str, old: &quire_app::markdown::Value, stored: &str, date_fmt: &str) -> Result<quire_app::markdown::Value, String> {
    if old.kind() == Kind::Date && !draft.trim().is_empty() {
        return quire_app::datefmt::parse(draft, date_fmt, stored).map(quire_app::markdown::Value::Str).ok_or_else(|| {
            let eg = quire_app::datefmt::display("2026-11-05", date_fmt);
            format!("“{}” isn't a date. Type it like {eg} (or 2026-11-05).", draft.trim())
        });
    }
    Ok(parse_input(draft, old.kind() == Kind::List))
}

/// The fields under a nested map, with their key paths (an empty map has none).
fn map_leaves(prefix: &[String], v: &quire_app::markdown::Value, out: &mut Vec<(Vec<String>, quire_app::markdown::Value)>) {
    match v.as_map() {
        Some(m) => {
            for (k, child) in m {
                let mut path = prefix.to_vec();
                path.push(k.clone());
                map_leaves(&path, child, out);
            }
        }
        None => out.push((prefix.to_vec(), v.clone())),
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Date => "date",
        Kind::Number => "number",
        Kind::Bool => "yes/no",
        Kind::List => "list",
        Kind::Text => "text",
        Kind::Map => "nested fields",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_math() {
        assert_eq!(weekday(2026, 11, 1), 6, "Nov 1 2026 is a Sunday");
        assert_eq!(weekday(2024, 2, 29), 3, "Thursday");
        assert_eq!(days_in(2024, 2), 29);
        assert_eq!(days_in(2026, 2), 28);
    }
}
