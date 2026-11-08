// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Agenda (right panel and sidebar): reminders, overdue, today with its
//! time blocks, and the next seven days (design §E.2). Also draws task
//! rows and the agenda and timeline layouts of `quire-view` blocks, WS-E's
//! widgets under note lines, and hosts WS-E's dialogs.
//!
//! Owned by WS-E. Registered in `registry.rs`.

use chrono::{Local, NaiveDate, NaiveTime, Timelike};
use egui::{Color32, Id, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use quire_app::Workspace;
use quire_app::decor::{ActionId, WidgetId};
use quire_app::index::views::{Layout, ViewResult, ViewRow, ViewSpec};
use quire_app::markdown::Task;
use quire_app::tasks_state::{Action, PlanWidget};
use quire_app::vault::NotePath;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct AgendaUi {
    /// Sections the person folded.
    folded: std::collections::HashSet<&'static str>,
    /// 0.11: list, board, calendar or time.
    mode: crate::task_ui::AgendaMode,
    /// The calendar's month (year, month); `None` is this month.
    month: Option<(i32, u32)>,
    /// The time report's week (a day in it); `None` is this week.
    week: Option<NaiveDate>,
}

/// Whether the tab is offered.
pub(crate) fn available(ws: &Workspace) -> bool {
    ws.vault().is_some()
}

// ---------------------------------------------------------------- helpers

/// A small priority mark: ▲ red for highest/high, ▲ for medium, ▼ for low.
fn priority_mark(ui: &mut Ui, pal: &Palette, p: Option<u8>) {
    let (glyph, color, tip) = match p {
        Some(0) => ("▲", pal.danger, "highest priority"),
        Some(1) => ("▲", pal.warn, "high priority"),
        Some(2) => ("▲", pal.muted, "medium priority"),
        Some(3) => ("▼", pal.faint, "low priority"),
        Some(_) => ("▼", pal.faint, "lowest priority"),
        None => return,
    };
    ui.label(RichText::new(glyph).size(10.0).color(color)).on_hover_text(tip);
}

/// "today", "tomorrow", "in 3 days", "2 days ago", "Mon 9".
pub(crate) fn relative(date: NaiveDate, today: NaiveDate) -> String {
    match (date - today).num_days() {
        0 => "today".into(),
        1 => "tomorrow".into(),
        -1 => "yesterday".into(),
        n if n < 0 => format!("{} days ago", -n),
        n if n < 7 => date.format("%A").to_string(),
        _ => date.format("%a %-d %b").to_string(),
    }
}

fn note_name(path: &str) -> &str {
    let p = path.trim_end_matches(".md");
    p.rsplit('/').next().unwrap_or(p)
}

/// One task row: checkbox, priority, text (opens the note), details below.
/// Returns (toggled, opened).
/// `timer`: the workspace and note, for the ▶ timer button (0.11).
fn task_row(ui: &mut Ui, pal: &Palette, task: &Task, detail: &str, strike: bool, timer: Option<(&Workspace, &str)>) -> (bool, bool) {
    let mut toggled = false;
    let mut opened = false;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if let Some((ws, path)) = timer.filter(|_| !task.done) {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                // Clear of the scroll bar.
                ui.add_space(10.0);
                let line = (task.offset != 0).then_some((task.offset, task.text.as_str()));
                crate::task_ui::timer_button(ui, ws, pal, &NotePath::new(path), line, &task.text);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    row_body(ui, pal, task, detail, strike, &mut toggled, &mut opened);
                });
            });
            return;
        }
        row_body(ui, pal, task, detail, strike, &mut toggled, &mut opened);
    });
    (toggled, opened)
}

fn row_body(ui: &mut Ui, pal: &Palette, task: &Task, detail: &str, strike: bool, toggled: &mut bool, opened: &mut bool) {
    {
        let mut done = task.done;
        if ui.checkbox(&mut done, "").on_hover_text(if task.done { "Mark not done" } else { "Mark done" }).changed() {
            *toggled = true;
        }
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                priority_mark(ui, pal, task.priority);
                if let Some((a, b)) = &task.time_block {
                    let t = if b.is_empty() { a.clone() } else { format!("{a}–{b}") };
                    ui.label(RichText::new(t).size(12.0).color(pal.accent).monospace());
                }
                let mut text = RichText::new(&task.text).color(if task.done { pal.muted } else { pal.text });
                if strike || task.done {
                    text = text.strikethrough();
                }
                if ui
                    .add(egui::Label::new(text).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Open the note at this task")
                    .clicked()
                {
                    *opened = true;
                }
                if task.recurrence.is_some() {
                    ui.label(RichText::new("↻").size(12.0).color(pal.muted)).on_hover_text(task.recurrence.clone().unwrap_or_default());
                }
            });
            if !detail.is_empty() {
                ui.label(RichText::new(detail).size(11.0).color(pal.muted));
            }
        });
    }
}

/// A small triangle: down when open, right when folded (the UI font has no triangle glyphs).
pub(crate) fn disclosure(ui: &mut Ui, open: bool, color: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 12.0), Sense::hover());
    let c = r.center();
    let pts = if open {
        vec![Pos2::new(c.x - 3.5, c.y - 2.0), Pos2::new(c.x + 3.5, c.y - 2.0), Pos2::new(c.x, c.y + 2.5)]
    } else {
        vec![Pos2::new(c.x - 2.0, c.y - 3.5), Pos2::new(c.x - 2.0, c.y + 3.5), Pos2::new(c.x + 2.5, c.y)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

fn fold_header(
    ui: &mut Ui,
    st: &mut AgendaUi,
    pal: &Palette,
    key: &'static str,
    title: &str,
    count: usize,
    color: Option<Color32>,
) -> bool {
    ui.add_space(8.0);
    let open = !st.folded.contains(key);
    let r = ui
        .horizontal(|ui| {
            disclosure(ui, open, pal.faint);
            ui.label(RichText::new(title.to_uppercase()).size(11.0).color(color.unwrap_or(pal.muted)).strong());
            ui.label(RichText::new(count.to_string()).size(11.0).color(pal.faint));
        })
        .response
        .interact(Sense::click());
    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        if open {
            st.folded.insert(key);
        } else {
            st.folded.remove(key);
        }
    }
    ui.add_space(2.0);
    open
}

// ---------------------------------------------------------------- the tab

pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let st = &mut st.agenda;
    let today = Local::now().date_naive();
    let agenda = ws.agenda();
    let mut toggle: Option<(NotePath, Task)> = None;
    let mut open: Option<(NotePath, usize)> = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(today.format("%A").to_string()).size(15.0).color(pal.strong).strong());
            ui.label(RichText::new(today.format("%-d %B %Y").to_string()).size(12.0).color(pal.muted));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Week").on_hover_text("Open this week's note").clicked() {
                out.run = Some(quire_app::Command::OpenWeeklyNote);
            }
            if ui.small_button("Today").on_hover_text("Open today's daily note (Alt+D)").clicked() {
                out.run = Some(quire_app::Command::DailyNote);
            }
        });
    });
    // 0.11: the running timer, and list / board / calendar / time.
    crate::task_ui::timer_strip(ui, ws, pal);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        crate::task_ui::mode_switch(ui, &mut st.mode);
        if ui.small_button("+ Task").on_hover_text("New task… (Alt+N)").clicked() {
            out.run = Some(quire_app::Command::NewTask);
        }
    });
    match st.mode {
        crate::task_ui::AgendaMode::List => {}
        crate::task_ui::AgendaMode::Board => {
            egui::ScrollArea::vertical().id_salt("quire-agenda-board").auto_shrink([false, false]).show(ui, |ui| {
                if let Some(p) = crate::task_ui::board(ui, ws, pal) {
                    open = Some((p, 0));
                }
            });
            return finish(ws, out, None, open);
        }
        crate::task_ui::AgendaMode::Calendar => {
            use chrono::Datelike;
            let mut month = st.month.unwrap_or_else(|| (today.year(), today.month()));
            egui::ScrollArea::vertical().id_salt("quire-agenda-cal").auto_shrink([false, false]).show(ui, |ui| {
                open = crate::task_ui::calendar(ui, ws, pal, &mut month);
            });
            st.month = Some(month);
            return finish(ws, out, None, open);
        }
        crate::task_ui::AgendaMode::Time => {
            let mut day = st.week.unwrap_or(today);
            egui::ScrollArea::vertical().id_salt("quire-agenda-time").auto_shrink([false, false]).show(ui, |ui| {
                if let Some(p) = crate::task_ui::report(ui, ws, pal, &mut day) {
                    open = Some((p, 0));
                }
            });
            st.week = Some(day);
            return finish(ws, out, None, open);
        }
    }
    let task_notes: Vec<_> = ws.task_notes().into_iter().filter(|(_, n)| !n.task.done).collect();
    egui::ScrollArea::vertical().id_salt("quire-agenda").auto_shrink([false, false]).show(ui, |ui| {
        // Reminders that came due (and missed ones).
        let reminders: Vec<_> = ws.features.tasks.reminders.active().into_iter().cloned().collect();
        let mut snooze = None;
        let mut dismiss = None;
        if !reminders.is_empty() && fold_header(ui, st, pal, "reminders", "Reminders", reminders.len(), Some(pal.warn)) {
            for r in &reminders {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(r.at.format("%H:%M").to_string()).size(12.0).color(pal.warn).monospace());
                    let label = ui.add(egui::Label::new(RichText::new(&r.text).color(pal.text)).sense(Sense::click()));
                    if label.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open the note at this task").clicked() {
                        open = Some((NotePath::new(&r.path), r.offset));
                    }
                });
                ui.horizontal(|ui| {
                    let when = if r.at.date() == today { String::new() } else { format!("{} · ", r.at.format("%a %-d %b")) };
                    ui.label(RichText::new(format!("{when}{}", note_name(&r.path))).size(11.0).color(pal.muted));
                    if ui.small_button("Snooze").on_hover_text("Remind me again in 10 minutes").clicked() {
                        snooze = Some(r.clone());
                    }
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = Some(r.clone());
                    }
                });
                ui.add_space(2.0);
            }
        }
        if let Some(r) = snooze {
            ws.snooze_reminder(&r);
        }
        if let Some(r) = dismiss {
            ws.dismiss_reminder(&r);
        }
        if !agenda.overdue.is_empty() && fold_header(ui, st, pal, "overdue", "Overdue", agenda.overdue.len(), Some(pal.danger)) {
            for i in &agenda.overdue {
                let detail = format!("due {} · {}", relative(i.date, today), note_name(&i.path));
                let (t, o) = task_row(ui, pal, &i.task, &detail, false, Some((&*ws, i.path.as_str())));
                if t {
                    toggle = Some((NotePath::new(&i.path), i.task.clone()));
                }
                if o {
                    open = Some((NotePath::new(&i.path), i.task.offset));
                }
            }
        }
        if fold_header(ui, st, pal, "today", "Today", agenda.today.len(), Some(pal.accent)) {
            if agenda.today.is_empty() {
                ui.label(RichText::new("Nothing planned for today.").size(12.0).color(pal.muted));
            }
            for i in &agenda.today {
                let when = if i.date < today { format!("scheduled {}", relative(i.date, today)) } else { i.when.label().to_string() };
                let next = i.next.map(|n| format!(" · next {}", relative(n, today))).unwrap_or_default();
                let detail = format!("{when} · {}{next}", note_name(&i.path));
                let (t, o) = task_row(ui, pal, &i.task, &detail, false, Some((&*ws, i.path.as_str())));
                if t {
                    toggle = Some((NotePath::new(&i.path), i.task.clone()));
                }
                if o {
                    open = Some((NotePath::new(&i.path), i.task.offset));
                }
            }
        }
        if !agenda.blocks.is_empty()
            && fold_header(ui, st, pal, "timeline", "Timeline", agenda.blocks.len(), None)
            && let Some((path, offset)) = timeline(ui, pal, &agenda.blocks, Local::now().time())
        {
            open = Some((path, offset));
        }
        if fold_header(ui, st, pal, "upcoming", "Next 7 days", agenda.upcoming.len(), None) {
            if agenda.upcoming.is_empty() {
                ui.label(RichText::new("Nothing due this week.").size(12.0).color(pal.muted));
            }
            let mut last: Option<NaiveDate> = None;
            for i in &agenda.upcoming {
                if last != Some(i.date) {
                    ui.add_space(4.0);
                    ui.label(RichText::new(i.date.format("%A %-d").to_string()).size(12.0).color(pal.strong));
                    last = Some(i.date);
                }
                let next = i.next.map(|n| format!(" · then {}", relative(n, today))).unwrap_or_default();
                let detail = format!("{} · {}{next}", i.when.label(), note_name(&i.path));
                let (t, o) = task_row(ui, pal, &i.task, &detail, false, Some((&*ws, i.path.as_str())));
                if t {
                    toggle = Some((NotePath::new(&i.path), i.task.clone()));
                }
                if o {
                    open = Some((NotePath::new(&i.path), i.task.offset));
                }
            }
        }
        // 0.11: open task notes as cards.
        if !task_notes.is_empty() && fold_header(ui, st, pal, "task-notes", "Task notes", task_notes.len(), None)
            && let Some(p) = crate::task_ui::note_cards(ui, ws, pal, &task_notes)
        {
            open = Some((p, 0));
        }
        if agenda.is_empty() && reminders.is_empty() && task_notes.is_empty() {
            ui.add_space(12.0);
            ui.label(
                RichText::new("Tasks with dates show up here. Add one after a task, like [due:: 2026-11-20], [scheduled:: …] or [remind:: 2026-11-20 09:00] (the Tasks plugin's emoji work too), or start a task in a daily note with a time like 14:00 - 15:00.")
                    .size(12.0)
                    .color(pal.muted),
            );
        }
    });
    finish(ws, out, toggle, open)
}

/// Do what the tab asked for: tick a task, open a note.
fn finish(ws: &mut Workspace, mut out: PanelOut, toggle: Option<(NotePath, Task)>, open: Option<(NotePath, usize)>) -> PanelOut {
    if let Some((path, task)) = toggle {
        // A checkbox is never at byte 0 of a note; offset 0 is a task note (`type: task`).
        if task.offset == 0 {
            ws.plan_request(Action::ToggleTaskNote { path, done: !task.done });
        } else {
            ws.toggle_task(&path, task.offset, &task.text);
        }
    }
    if let Some((path, offset)) = open
        && ws.open_note(&path).is_some()
    {
        ws.jump_to_byte(offset);
        out.jumped = true;
    }
    out
}

/// A day's time blocks on a vertical hour scale. Returns a clicked block's task.
fn timeline(ui: &mut Ui, pal: &Palette, blocks: &[quire_app::plan::agenda::Block], now: NaiveTime) -> Option<(NotePath, usize)> {
    // From the first block's hour to the last one's end.
    let first = blocks.iter().map(|b| b.start.hour()).min().unwrap_or(8);
    let last = blocks.iter().map(|b| b.end.hour() + u32::from(b.end.minute() > 0)).max().unwrap_or(18).clamp(first + 1, 24);
    let hour_h = 34.0;
    let pad = 8.0;
    let w = ui.available_width();
    let h = (last - first).max(1) as f32 * hour_h + 2.0 * pad;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let painter = ui.painter_at(rect);
    let label_w = 34.0;
    let y_of = |t: NaiveTime| rect.top() + pad + ((t.hour() as f32 + t.minute() as f32 / 60.0) - first as f32) * hour_h;
    for hr in first..=last {
        let y = rect.top() + pad + (hr - first) as f32 * hour_h;
        painter.text(Pos2::new(rect.left(), y), egui::Align2::LEFT_CENTER, format!("{hr:02}"), egui::FontId::monospace(10.5), pal.faint);
        painter.line_segment([Pos2::new(rect.left() + label_w, y), Pos2::new(rect.right(), y)], Stroke::new(0.5, pal.border));
    }
    let mut clicked = None;
    // Overlapping blocks share the width.
    let mut lanes: Vec<NaiveTime> = Vec::new();
    let lane_of: Vec<usize> = blocks
        .iter()
        .map(|b| {
            if let Some(i) = lanes.iter().position(|end| *end <= b.start) {
                lanes[i] = b.end;
                i
            } else {
                lanes.push(b.end);
                lanes.len() - 1
            }
        })
        .collect();
    let lane_w = (w - label_w - 4.0) / lanes.len().max(1) as f32;
    for (b, lane) in blocks.iter().zip(lane_of) {
        let x = rect.left() + label_w + 2.0 + lane as f32 * lane_w;
        let r = Rect::from_min_max(
            Pos2::new(x, y_of(b.start) + 1.0),
            Pos2::new(x + lane_w - 2.0, (y_of(b.end) - 1.0).max(y_of(b.start) + 18.0)),
        );
        let fill = if b.task.done { pal.code_bg } else { pal.accent.gamma_multiply(if pal.dark { 0.35 } else { 0.18 }) };
        painter.rect_filled(r, 4.0, fill);
        painter.rect_stroke(r, 4.0, Stroke::new(1.0, pal.accent.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        let label = format!("{} {}", b.start.format("%H:%M"), b.task.text);
        let galley =
            painter.layout(label, egui::FontId::proportional(11.5), if b.task.done { pal.muted } else { pal.text }, r.width() - 8.0);
        painter.with_clip_rect(r).galley(Pos2::new(r.left() + 4.0, r.top() + 2.0), galley, pal.text);
        let resp = ui.interact(r, ui.scope_id().with(("quire-block", &b.path, b.task.offset)), Sense::click());
        if resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(format!("{}–{} · {}", b.start.format("%H:%M"), b.end.format("%H:%M"), note_name(&b.path)))
            .clicked()
        {
            clicked = Some((NotePath::new(&b.path), b.task.offset));
        }
    }
    // Now.
    if (first..last).contains(&now.hour()) {
        let y = y_of(now);
        painter.line_segment([Pos2::new(rect.left() + label_w - 4.0, y), Pos2::new(rect.right(), y)], Stroke::new(1.5, pal.danger));
        painter.circle_filled(Pos2::new(rect.left() + label_w - 4.0, y), 3.0, pal.danger);
    }
    clicked
}

// ---------------------------------------------------------------- views

const MAX_ROWS: usize = 200;

/// `from: tasks` rows and the agenda and timeline layouts of a quire-view.
pub(crate) fn view(ui: &mut Ui, ws: &Workspace, pal: &Palette, spec: &ViewSpec, r: &ViewResult, key: Id) {
    let what = if spec.tasks { "task" } else { "note" };
    let label = match spec.layout {
        Layout::Agenda => "Agenda",
        Layout::Timeline => "Timeline",
        _ if spec.tasks => "Tasks",
        _ => "List",
    };
    let more = if r.total > r.rows.len() { format!(" · showing {}", r.rows.len()) } else { String::new() };
    ui.label(
        RichText::new(format!("{label} · {} {what}{}{more}", r.total, if r.total == 1 { "" } else { "s" }))
            .size(11.0)
            .color(pal.muted)
            .strong(),
    );
    if r.rows.is_empty() {
        ui.label(RichText::new(if spec.tasks { "No tasks match." } else { "No notes match." }).color(pal.muted).size(12.5));
        return;
    }
    let today = Local::now().date_naive();
    let heading = |ui: &mut Ui, h: &str, first: bool| {
        ui.add_space(if first { 2.0 } else { 6.0 });
        let color = match h {
            "Overdue" => pal.danger,
            "Today" => pal.accent,
            _ => pal.strong,
        };
        ui.label(RichText::new(h).size(12.0).color(color).strong());
    };
    let rows = &r.rows[..r.rows.len().min(MAX_ROWS)];
    if !r.groups.is_empty() {
        // One heading per group, its rows in their sorted order.
        for (n, g) in r.groups.iter().enumerate() {
            let shown = if g.is_empty() { "(none)" } else { g.trim_end_matches(".md") };
            heading(ui, shown, n == 0);
            for row in rows.iter().filter(|x| &x.group == g) {
                view_row(ui, ws, pal, spec, row, key, today);
            }
        }
        return;
    }
    let mut last: Option<String> = None;
    for row in rows {
        if spec.layout == Layout::Timeline {
            let h = row
                .date
                .as_deref()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .map_or_else(|| "No date".into(), |d| d.format("%A %-d %B %Y").to_string());
            if last.as_ref() != Some(&h) {
                heading(ui, &h, last.is_none());
                last = Some(h);
            }
        }
        view_row(ui, ws, pal, spec, row, key, today);
    }
}

fn view_row(ui: &mut Ui, ws: &Workspace, pal: &Palette, spec: &ViewSpec, row: &ViewRow, key: Id, today: NaiveDate) {
    let extra: Vec<&str> = spec
        .fields
        .iter()
        .zip(&row.cells)
        .filter(|(f, c)| !matches!(f.as_str(), "text" | "title") && !c.is_empty())
        .map(|(_, c)| c.as_str())
        .collect();
    match &row.task {
        Some(t) => {
            let mut detail: Vec<String> = Vec::new();
            if spec.layout != Layout::Timeline
                && let Some((d, when)) = quire_app::plan::agenda::task_date(&t.task)
            {
                detail.push(format!("{} {}", when.label(), relative(d, today)));
            }
            detail.extend(extra.iter().map(|s| (*s).to_string()));
            detail.push(note_name(row.path.as_str()).to_string());
            let _ = key;
            let (toggled, opened) = task_row(ui, pal, &t.task, &detail.join(" · "), false, Some((ws, row.path.as_str())));
            if toggled {
                if t.is_note {
                    ws.plan_request(Action::ToggleTaskNote { path: row.path.clone(), done: !t.task.done });
                } else {
                    ws.plan_request(Action::ToggleTask { path: row.path.clone(), offset: t.task.offset, text: t.task.text.clone() });
                }
            }
            if opened {
                ws.plan_request(Action::OpenAt { path: row.path.clone(), offset: t.task.offset });
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("•").color(pal.muted));
                let r = ui.add(egui::Label::new(RichText::new(&row.title).color(pal.accent)).truncate().sense(Sense::click()));
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(row.path.as_str()).clicked() {
                    ws.plan_request(Action::OpenAt { path: row.path.clone(), offset: 0 });
                }
                let mut bits: Vec<String> = Vec::new();
                if let Some(d) = row
                    .date
                    .as_deref()
                    .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                    .filter(|_| spec.layout != Layout::Timeline)
                {
                    bits.push(relative(d, today));
                }
                bits.extend(extra.iter().map(|s| (*s).to_string()));
                if !bits.is_empty() {
                    ui.add(egui::Label::new(RichText::new(bits.join(" · ")).size(12.0).color(pal.muted)).truncate());
                }
            });
        }
    }
}

// ---------------------------------------------------------------- widgets

/// WS-E's widgets under note lines: Dataview/Tasks results, Kanban boards,
/// template fields to fill in.
pub(crate) fn paint_widget(ui: &mut Ui, ws: &Workspace, id: WidgetId, pal: &Palette) -> (f32, Option<ActionId>) {
    let top = ui.min_rect().top();
    let action = match PlanWidget::decode(id) {
        Some(PlanWidget::Compat { doc, block }) => crate::base_surface::compat_widget(ui, ws, doc, block, pal),
        Some(PlanWidget::Kanban { doc }) => crate::base_surface::kanban_widget(ui, ws, doc, pal),
        Some(PlanWidget::Fill { doc }) => {
            crate::template_prompt_ui::fill_widget(ui, ws, doc, pal);
            None
        }
        None => None,
    };
    (ui.min_rect().bottom() - top + 6.0, action)
}

// ---------------------------------------------------------------- settings

/// Settings → Planning: tasks, reminders, periodic notes, compatibility.
pub(crate) fn settings_tab(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    // The Settings dialog scrolls the tab.
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width());
        ui.label(RichText::new("TASKS").size(11.0).color(pal.muted).strong());
        let t = &mut ws.settings.tasks;
        let mut changed = false;
        ui.label("Write task dates as");
        ui.indent("quire-plan-format", |ui| {
            changed |= ui.radio_value(&mut t.format, "emoji".to_string(), "emoji, as Obsidian Tasks writes them").changed();
            changed |= ui.radio_value(&mut t.format, "dataview".to_string(), "Dataview fields, like [due:: 2026-12-01]").changed();
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("A repeating task's next copy goes");
            changed |= ui.radio_value(&mut t.new_occurrence, "above".to_string(), "above it").changed();
            changed |= ui.radio_value(&mut t.new_occurrence, "below".to_string(), "below it").changed();
        });
        ui.add_space(4.0);
        changed |= ui
            .checkbox(&mut t.reminders, "Show reminders as desktop notifications")
            .on_hover_text("Reminders always show in Quire; as notifications while Quire runs, or always with the setting below.")
            .changed();
        if changed {
            let _ = ws.settings.save();
        }
        // 0.10: reminders while Quire is closed.
        ui.add_space(4.0);
        crate::remind_ui::settings(ui, ws, pal);
        // 0.11: task notes, statuses and the pomodoro.
        ui.add_space(12.0);
        crate::task_ui::settings(ui, ws, pal);
        ui.add_space(12.0);
        crate::calendar_ui::periodic_settings(ui, ws, &mut st.calendar, pal);
        ui.add_space(12.0);
        ui.label(RichText::new("OBSIDIAN COMPATIBILITY").size(11.0).color(pal.muted).strong());
        ui.add(
            egui::Label::new(RichText::new("Quire reads these as they are and never runs code from a vault.").size(12.0).color(pal.muted))
                .wrap(),
        );
        for (what, ok, not) in quire_import::obsidian::COMPATIBILITY {
            ui.add_space(4.0);
            ui.label(RichText::new(*what).size(12.5).color(pal.strong));
            ui.add(egui::Label::new(RichText::new(*ok).size(11.5).color(pal.text)).wrap());
            if *not != "—" {
                ui.add(egui::Label::new(RichText::new(format!("Not supported: {not}")).size(11.5).color(pal.muted)).wrap());
            }
        }
    });
    PanelOut::default()
}

#[cfg(test)]
mod tests {
    use chrono::Datelike;

    use super::*;

    #[test]
    fn relative_dates() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        assert_eq!(relative(d, d), "today");
        assert_eq!(relative(d.succ_opt().unwrap(), d), "tomorrow");
        assert_eq!(relative(d - chrono::Duration::days(3), d), "3 days ago");
        assert_eq!(relative(d + chrono::Duration::days(3), d), "Friday");
        assert_eq!(relative(d + chrono::Duration::days(20), d), "Mon 23 Nov");
        assert_eq!(d.weekday(), chrono::Weekday::Tue);
    }
}
