// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task notes and time tracking in the desktop app (0.11): the timer
//! button on task rows and cards, "New task…", "Time this week", and the
//! agenda's task-note cards, board and calendar.
//!
//! Owned by the 0.11 tasks workstream.

use chrono::{Datelike, Duration, Local, NaiveDate};
use egui::{Id, RichText, Sense, Stroke, Ui};
use quire_app::Workspace;
use quire_app::index::views;
use quire_app::plan::task_note::{NewTask, TaskNote};
use quire_app::plan::time::{format_clock, format_duration, parse_duration};
use quire_app::plan::timer::TaskRef;
use quire_app::tasks_state::Action;
use quire_app::vault::NotePath;

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// ▶ (start the timer on this task) or ■ with the clock (stop it).
pub(crate) fn timer_button(ui: &mut Ui, ws: &Workspace, pal: &Palette, path: &NotePath, line: Option<&str>, title: &str) {
    let timing = ws.is_timing(path, line);
    if timing {
        let secs = ws.timer_clock().map_or(0, |c| c.0);
        let r =
            ui.add(egui::Button::new(RichText::new(format!("■ {}", format_clock(secs))).size(11.0).color(pal.danger)).small().frame(false));
        if r.on_hover_text("Stop the timer and log the time").clicked() {
            ws.plan_request(Action::Timer { task: None, pomodoro: false });
        }
    } else {
        let r = ui.add(egui::Button::new(RichText::new("▶").size(11.0).color(pal.muted)).small().frame(false));
        let task = TaskRef { path: path.as_str().to_string(), line: line.map(str::to_string), title: title.to_string() };
        let r = r.on_hover_text("Start the timer on this task (right-click: a pomodoro)");
        if r.clicked() {
            ws.plan_request(Action::Timer { task: Some(task.clone()), pomodoro: false });
        }
        r.context_menu(|ui| {
            if ui.button("Start timer").clicked() {
                ws.plan_request(Action::Timer { task: Some(task.clone()), pomodoro: false });
                ui.close();
            }
            let work = ws.task_prefs().pomodoro.work;
            if ui.button(format!("Start a {work}-minute pomodoro")).clicked() {
                ws.plan_request(Action::Timer { task: Some(task.clone()), pomodoro: true });
                ui.close();
            }
        });
    }
}

/// The running timer at the top of the agenda: the task, the clock, stop.
pub(crate) fn timer_strip(ui: &mut Ui, ws: &Workspace, pal: &Palette) {
    let (Some(r), Some((secs, phase))) = (ws.timer(), ws.timer_clock()) else { return };
    egui::Frame::NONE.fill(pal.active_bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            let what = phase.map_or_else(|| "Timing".to_string(), |p| format!("🍅 {}", p.label()));
            ui.label(RichText::new(format_clock(secs)).size(18.0).monospace().color(pal.strong));
            ui.vertical(|ui| {
                ui.label(RichText::new(what).size(11.0).color(pal.muted));
                ui.add(egui::Label::new(RichText::new(&r.task.title).color(pal.text)).truncate());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Stop").on_hover_text("Stop and log the time to the task").clicked() {
                    ws.plan_request(Action::Timer { task: None, pomodoro: false });
                }
            });
        });
    });
    ui.add_space(4.0);
}

// ---------------------------------------------------------------- "New task…"

/// "New task…" (Alt+N, quick capture, the agenda): a task note's title and properties.
#[derive(Default)]
pub(crate) struct NewTaskDialog {
    title: String,
    status: String,
    priority: String,
    due: String,
    scheduled: String,
    contexts: String,
    projects: String,
    tags: String,
    estimate: String,
    error: Option<String>,
    focused: bool,
}

impl NewTaskDialog {
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        ws.reload_task_prefs();
        let status = ws.task_prefs().open_status();
        let title = ws.features.tasks.new_task_title.take().unwrap_or_default();
        Box::new(Self { title, status, priority: "normal".into(), ..Self::default() })
    }

    #[cfg(test)]
    pub(crate) fn sample(error: bool) -> Self {
        Self {
            title: "Write the quarterly report".into(),
            status: "todo".into(),
            priority: "high".into(),
            due: "2026-11-20".into(),
            contexts: "@desk".into(),
            projects: "[[Website]]".into(),
            estimate: if error { "soon".into() } else { "1h30m".into() },
            error: error.then(|| "“soon” isn't a duration (25m, 1h30m)".into()),
            ..Self::default()
        }
    }

    fn task(&self) -> Result<NewTask, String> {
        let list = |s: &str| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect::<Vec<_>>();
        let date = |s: &str, what: &str| -> Result<Option<String>, String> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            let today = Local::now().date_naive();
            let d = match s.to_ascii_lowercase().as_str() {
                "today" => today,
                "tomorrow" => today + Duration::days(1),
                _ => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("The {what} date should look like 2026-11-20"))?,
            };
            Ok(Some(d.format("%Y-%m-%d").to_string()))
        };
        let estimate = match self.estimate.trim() {
            "" => None,
            e => Some(parse_duration(e).ok_or_else(|| format!("“{e}” isn't a duration (25m, 1h30m)"))?),
        };
        Ok(NewTask {
            title: self.title.trim().to_string(),
            status: Some(self.status.clone()).filter(|s| !s.is_empty()),
            priority: Some(self.priority.clone()).filter(|p| !p.is_empty() && p != "normal"),
            due: date(&self.due, "due")?,
            scheduled: date(&self.scheduled, "scheduled")?,
            contexts: list(&self.contexts),
            projects: list(&self.projects).into_iter().map(|p| if p.starts_with("[[") { p } else { format!("[[{p}]]") }).collect(),
            tags: list(&self.tags).into_iter().map(|t| t.trim_start_matches('#').to_string()).collect(),
            estimate,
            ..NewTask::default()
        })
    }
}

impl OverlayView for NewTaskDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let prefs = ws.task_prefs();
        egui::Modal::new(Id::unique("quire-new-task")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 440.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("New task");
            ui.label(RichText::new(format!("A note in {}/ with the task's properties.", prefs.folder)).size(12.0).color(pal.muted));
            ui.add_space(6.0);
            let mut enter = false;
            let r = ui.add(egui::TextEdit::singleline(&mut self.title).hint_text("What needs doing?").desired_width(f32::INFINITY));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(4.0);
            egui::Grid::new("quire-new-task-grid").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                ui.label("Status");
                egui::ComboBox::from_id_salt("quire-new-task-status").selected_text(self.status.clone()).width(160.0).show_ui(ui, |ui| {
                    for s in &prefs.statuses {
                        ui.selectable_value(&mut self.status, s.clone(), s);
                    }
                });
                ui.end_row();
                ui.label("Priority");
                egui::ComboBox::from_id_salt("quire-new-task-priority").selected_text(self.priority.clone()).width(160.0).show_ui(
                    ui,
                    |ui| {
                        for p in ["highest", "high", "medium", "normal", "low", "lowest"] {
                            ui.selectable_value(&mut self.priority, p.to_string(), p);
                        }
                    },
                );
                ui.end_row();
                for (label, field, hint) in [
                    ("Due", &mut self.due, "2026-11-20, today, tomorrow"),
                    ("Scheduled", &mut self.scheduled, "when to work on it"),
                    ("Contexts", &mut self.contexts, "@home, @desk"),
                    ("Projects", &mut self.projects, "Website, [[Launch]]"),
                    ("Tags", &mut self.tags, "work, writing"),
                    ("Estimate", &mut self.estimate, "25m, 1h30m"),
                ] {
                    ui.label(label);
                    let r = ui.add(egui::TextEdit::singleline(field).hint_text(hint).desired_width(240.0));
                    enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                }
            });
            if !prefs.tags.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Tags:").size(11.5).color(pal.muted));
                    for t in &prefs.tags {
                        if ui.small_button(format!("#{t}")).clicked() {
                            if !self.tags.is_empty() {
                                self.tags.push_str(", ");
                            }
                            self.tags.push_str(t);
                        }
                    }
                });
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).size(12.0).color(pal.danger));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let ok = !self.title.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Create")).clicked() || (enter && ok) {
                    match self.task() {
                        Ok(t) => {
                            res.jumped = ws.new_task(&t, true).is_some();
                            res.close = true;
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
        });
        res
    }
}

// ---------------------------------------------------------------- "Time this week"

/// The weekly time report: task notes by day.
pub(crate) struct TimeReportOverlay {
    day: NaiveDate,
}

impl TimeReportOverlay {
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self { day: Local::now().date_naive() })
    }
}

impl OverlayView for TimeReportOverlay {
    fn yields_to_capture(&self) -> bool {
        true
    }

    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(Id::unique("quire-time-report")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 640.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            if let Some(p) = report(ui, ws, pal, &mut self.day) {
                if ws.open_note(&p).is_some() {
                    res.jumped = true;
                }
                res.close = true;
            }
            ui.add_space(8.0);
            if ui.button("Close").clicked() {
                res.close = true;
            }
        });
        res
    }
}

/// The report's table; returns a task to open.
pub(crate) fn report(ui: &mut Ui, ws: &Workspace, pal: &Palette, day: &mut NaiveDate) -> Option<NotePath> {
    let r = ws.week_report(*day);
    let monday = r.monday.unwrap_or(*day);
    let mut open = None;
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("‹").on_hover_text("Previous week").clicked() {
            *day -= Duration::days(7);
        }
        ui.label(RichText::new(format!("Week of {}", monday.format("%-d %b %Y"))).size(14.0).color(pal.strong).strong());
        if ui.small_button("›").on_hover_text("Next week").clicked() {
            *day += Duration::days(7);
        }
        if ui.small_button("This week").clicked() {
            *day = Local::now().date_naive();
        }
    });
    ui.add_space(4.0);
    if r.rows.is_empty() {
        ui.label(
            RichText::new("No time logged on task notes this week. Start a timer on a task (▶ in the agenda, Ctrl+Alt+T in a note).")
                .size(12.0)
                .color(pal.muted),
        );
        return None;
    }
    let today = Local::now().date_naive();
    // A narrow panel: each task with its week's total; the days are the bars below.
    if ui.available_width() < 520.0 {
        for row in &r.rows {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(format_duration(row.total)).monospace().strong().color(pal.strong));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let l = ui.add(egui::Label::new(RichText::new(&row.title).color(pal.accent)).truncate().sense(Sense::click()));
                        if l.on_hover_text(&row.path).clicked() {
                            open = Some(NotePath::new(&row.path));
                        }
                    });
                });
            });
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("All tasks").strong().color(pal.strong));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(format_duration(r.total)).monospace().strong().color(pal.strong));
            });
        });
    } else {
        grid(ui, pal, &r, monday, today, &mut open);
    }
    bars(ui, pal, &r, monday);
    open
}

/// The report as a table: tasks by day.
fn grid(
    ui: &mut Ui,
    pal: &Palette,
    r: &quire_app::plan::time::WeekReport,
    monday: NaiveDate,
    today: NaiveDate,
    open: &mut Option<NotePath>,
) {
    egui::Grid::new("quire-time-grid").striped(true).num_columns(9).spacing([10.0, 4.0]).show(ui, |ui| {
        ui.label(RichText::new("Task").size(11.5).color(pal.muted).strong());
        for i in 0..7 {
            let d = monday + Duration::days(i);
            let c = if d == today { pal.accent } else { pal.muted };
            ui.label(RichText::new(d.format("%a %-d").to_string()).size(11.5).color(c).strong());
        }
        ui.label(RichText::new("Total").size(11.5).color(pal.muted).strong());
        ui.end_row();
        let cell = |ui: &mut Ui, m: u32| {
            if m == 0 {
                ui.label(RichText::new("·").color(pal.faint));
            } else {
                ui.label(RichText::new(format_duration(m)).monospace().size(12.0).color(pal.text));
            }
        };
        for row in &r.rows {
            let title: String = row.title.chars().take(40).collect();
            let l = ui.add(egui::Label::new(RichText::new(title).color(pal.accent)).extend().sense(Sense::click()));
            if l.on_hover_text(&row.path).clicked() {
                *open = Some(NotePath::new(&row.path));
            }
            for m in row.days {
                cell(ui, m);
            }
            ui.label(RichText::new(format_duration(row.total)).monospace().strong().color(pal.strong));
            ui.end_row();
        }
        ui.label(RichText::new("All tasks").strong().color(pal.strong));
        for m in r.days {
            cell(ui, m);
        }
        ui.label(RichText::new(format_duration(r.total)).monospace().strong().color(pal.strong));
        ui.end_row();
    });
}

/// Bars: minutes per day of the week, with the day under each.
fn bars(ui: &mut Ui, pal: &Palette, r: &quire_app::plan::time::WeekReport, monday: NaiveDate) {
    ui.add_space(6.0);
    let max = r.days.iter().copied().max().unwrap_or(1).max(1);
    let (rect, _) = ui.allocate_exact_size(egui::vec2((ui.available_width() - 8.0).max(140.0), 56.0), Sense::hover());
    let bw = rect.width() / 7.0;
    for (i, m) in r.days.iter().enumerate() {
        let h = 44.0 * *m as f32 / max as f32;
        let x = rect.left() + bw * i as f32;
        let bar =
            egui::Rect::from_min_max(egui::pos2(x + bw * 0.25, rect.bottom() - 12.0 - h), egui::pos2(x + bw * 0.75, rect.bottom() - 12.0));
        ui.painter().rect_filled(bar, 2.0, pal.accent.gamma_multiply(0.8));
        let d = monday + Duration::days(i as i64);
        ui.painter().text(
            egui::pos2(x + bw / 2.0, rect.bottom()),
            egui::Align2::CENTER_BOTTOM,
            d.format("%a").to_string(),
            egui::FontId::proportional(10.0),
            pal.muted,
        );
    }
}

// ---------------------------------------------------------------- agenda modes

/// The agenda's view: the list (as before), task-note cards on a board by
/// status, or a month calendar by scheduled and due dates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum AgendaMode {
    #[default]
    List,
    Board,
    Calendar,
    Time,
}

/// The mode switch.
pub(crate) fn mode_switch(ui: &mut Ui, mode: &mut AgendaMode) {
    for (m, label, tip) in [
        (AgendaMode::List, "List", "Overdue, today and the next 7 days"),
        (AgendaMode::Board, "Board", "Task notes by status: drag to change"),
        (AgendaMode::Calendar, "Calendar", "Tasks by scheduled and due date"),
        (AgendaMode::Time, "Time", "Time logged this week"),
    ] {
        ui.selectable_value(mode, m, RichText::new(label).size(12.0)).on_hover_text(tip);
    }
}

/// The board of task notes (and open task lines with a status): a quire-view
/// `from: tasks`, `layout: kanban`, grouped by status.
pub(crate) fn board(ui: &mut Ui, ws: &Workspace, pal: &Palette) -> Option<NotePath> {
    let spec_src = "from: tasks\nlayout: kanban\ngroup: status\nfields: [text, priority, due, time_spent, time_estimate, contexts]\n";
    let Ok(spec) = views::parse(spec_src) else { return None };
    let result = ws.run_view(spec_src, None);
    let Ok(r) = result.as_ref() else { return None };
    // Only task notes: lines in notes have no status beyond open and done.
    let mut r = r.clone();
    r.rows.retain(|x| x.task.as_ref().is_some_and(|t| t.is_note));
    r.total = r.rows.len();
    if r.rows.is_empty() {
        ui.label(
            RichText::new("No task notes yet. “New task…” (Alt+N) makes one; “Convert task to note” turns a task line into one.")
                .size(12.0)
                .color(pal.muted),
        );
        return None;
    }
    let b = crate::board_ui::Board { spec: &spec, result: &r, target: None, from: None, key: Id::unique("quire-agenda-board") };
    match crate::board_ui::show(ui, ws, pal, &b) {
        Some(crate::view_ui::ViewAction::Open(p)) => Some(p),
        _ => None,
    }
}

/// A month of tasks by scheduled (⏳) and due (📅) dates; returns (note, offset) to open.
pub(crate) fn calendar(ui: &mut Ui, ws: &Workspace, pal: &Palette, month: &mut (i32, u32)) -> Option<(NotePath, usize)> {
    let result = ws.run_view("from: tasks\nwhere: status != done\nfields: [text, due, scheduled]\n", None);
    let Ok(r) = result.as_ref() else { return None };
    let today = Local::now().date_naive();
    let (y, m) = *month;
    let mut open = None;
    ui.horizontal(|ui| {
        if ui.small_button("‹").clicked() {
            *month = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        }
        let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(today);
        ui.label(RichText::new(first.format("%B %Y").to_string()).strong().color(pal.text));
        if ui.small_button("›").clicked() {
            *month = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        }
        if ui.small_button("Today").clicked() {
            *month = (today.year(), today.month());
        }
    });
    let first = NaiveDate::from_ymd_opt(y, m, 1)?;
    let lead = i64::from(first.weekday().num_days_from_monday());
    let start = first - Duration::days(lead);
    let cell_w = ((ui.available_width() - 6.0 * 2.0) / 7.0).max(30.0);
    let on = |d: NaiveDate, which: fn(&quire_app::markdown::Task) -> Option<&String>| {
        let key = d.format("%Y-%m-%d").to_string();
        r.rows.iter().filter(move |row| row.task.as_ref().and_then(|t| which(&t.task)).and_then(|s| s.get(..10)) == Some(key.as_str()))
    };
    egui::Grid::new("quire-agenda-cal").num_columns(7).spacing([2.0, 2.0]).min_col_width(cell_w).max_col_width(cell_w).show(ui, |ui| {
        for d in ["M", "T", "W", "T", "F", "S", "S"] {
            ui.label(RichText::new(d).size(10.5).color(pal.muted));
        }
        ui.end_row();
        for week in 0..6 {
            for i in 0..7 {
                let d = start + Duration::days(week * 7 + i);
                ui.vertical(|ui| {
                    ui.set_width(cell_w);
                    ui.set_min_height(46.0);
                    let c = if d == today {
                        pal.accent
                    } else if d.month() == m {
                        pal.text
                    } else {
                        pal.faint
                    };
                    ui.label(RichText::new(d.day().to_string()).size(10.5).color(c).strong());
                    let due: Vec<_> = on(d, |t| t.due.as_ref()).collect();
                    let sched: Vec<_> = on(d, |t| t.scheduled.as_ref()).filter(|row| !due.iter().any(|x| x.id == row.id)).collect();
                    for (row, mark) in due.iter().map(|r| (r, "📅")).chain(sched.iter().map(|r| (r, "⏳"))).take(3) {
                        let overdue = d < today && mark == "📅";
                        let t =
                            RichText::new(format!("{mark}{}", row.title)).size(10.5).color(if overdue { pal.danger } else { pal.accent });
                        let l = ui.add(egui::Label::new(t).truncate().sense(Sense::click()));
                        if l.on_hover_text(format!("{} · {}", row.title, row.path.stem())).clicked() {
                            open = Some((row.path.clone(), row.task.as_ref().map_or(0, |t| if t.is_note { 0 } else { t.task.offset })));
                        }
                    }
                    let more = due.len() + sched.len();
                    if more > 3 {
                        ui.label(RichText::new(format!("+{}", more - 3)).size(10.0).color(pal.muted));
                    }
                });
            }
            ui.end_row();
        }
    });
    open
}

/// Task notes as cards in the agenda list: status, priority, due, contexts,
/// time spent against the estimate, and the timer.
pub(crate) fn note_cards(ui: &mut Ui, ws: &Workspace, pal: &Palette, notes: &[(NotePath, TaskNote)]) -> Option<NotePath> {
    let today = Local::now().date_naive();
    let now = quire_app::task_timer::now();
    let mut open = None;
    for (path, n) in notes {
        egui::Frame::NONE.fill(pal.panel).stroke(Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            // Clear of the scroll bar.
            ui.set_width(ui.available_width() - 10.0);
            ui.horizontal(|ui| {
                let mut done = n.task.done;
                if ui.checkbox(&mut done, "").on_hover_text("Done").changed() {
                    ws.plan_request(Action::ToggleTaskNote { path: path.clone(), done });
                }
                let l = ui.add(egui::Label::new(RichText::new(&n.task.text).color(pal.accent).strong()).truncate().sense(Sense::click()));
                if l.on_hover_text(path.as_str()).clicked() {
                    open = Some(path.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    timer_button(ui, ws, pal, path, None, &n.task.text);
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
                if !n.status.is_empty() {
                    ui.label(RichText::new(&n.status).size(11.0).color(pal.muted));
                }
                if n.task.priority.is_some() {
                    let p = quire_app::markdown::tasks::priority_word(n.task.priority);
                    let c = if n.task.priority.is_some_and(|p| p <= 1) { pal.danger } else { pal.muted };
                    ui.label(RichText::new(p).size(11.0).color(c));
                }
                if let Some(d) = n.task.due.as_deref().and_then(|d| NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok()) {
                    let c = if d < today { pal.danger } else { pal.text };
                    ui.label(RichText::new(format!("due {}", crate::agenda_ui::relative(d, today))).size(11.0).color(c));
                }
                for c in &n.contexts {
                    ui.label(RichText::new(c).size(11.0).color(pal.tag));
                }
                for p in &n.projects {
                    ui.label(RichText::new(p.trim_start_matches("[[").trim_end_matches("]]")).size(11.0).color(pal.link));
                }
            });
            let spent = n.spent(now);
            if spent > 0 || n.estimate.is_some() {
                let est = n.estimate.unwrap_or(0);
                let text = if est > 0 { format!("{} of {}", format_duration(spent), format_duration(est)) } else { format_duration(spent) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("⏱ {text}")).size(11.0).color(if est > 0 && spent > est {
                        pal.warn
                    } else {
                        pal.muted
                    }));
                    if est > 0 {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(160.0), 5.0), Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, pal.border);
                        let f = (spent as f32 / est as f32).min(1.0);
                        let mut fill = rect;
                        fill.set_width(rect.width() * f);
                        ui.painter().rect_filled(fill, 2.0, if spent > est { pal.warn } else { pal.accent });
                    }
                });
            }
        });
        ui.add_space(4.0);
    }
    open
}

/// Settings → Planning: task notes and the pomodoro (`[tasks]` in the vault's vault.toml).
pub(crate) fn settings(ui: &mut Ui, ws: &mut Workspace, pal: &Palette) {
    ui.label(RichText::new("TASK NOTES AND TIME").size(11.0).color(pal.muted).strong());
    ui.add(
        egui::Label::new(
            RichText::new("Kept in this vault (.quire/shared/vault.toml), so every device and Quire user of the vault shares them.")
                .size(12.0)
                .color(pal.muted),
        )
        .wrap(),
    );
    let mut p = ws.task_prefs();
    let mut changed = false;
    let id = Id::unique("quire-task-prefs");
    let text_field = |ui: &mut Ui, label: &str, key: &str, value: &str, hint: &str| -> Option<String> {
        let mem = id.with(key);
        let mut draft: String = ui.data(|d| d.get_temp(mem)).unwrap_or_else(|| value.to_string());
        let mut out = None;
        ui.horizontal(|ui| {
            ui.label(label);
            let r = ui.add(egui::TextEdit::singleline(&mut draft).hint_text(hint).desired_width(220.0));
            if r.lost_focus() && draft != value {
                out = Some(draft.clone());
            }
            if r.has_focus() {
                ui.data_mut(|d| d.insert_temp(mem, draft.clone()));
            } else {
                ui.data_mut(|d| d.remove::<String>(mem));
            }
        });
        out
    };
    let join = |v: &[String]| v.join(", ");
    let split = |s: &str| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect::<Vec<_>>();
    if let Some(v) = text_field(ui, "Folder", "folder", &p.folder, "Tasks") {
        p.folder = v.trim().trim_matches('/').to_string();
        changed = true;
    }
    if let Some(v) = text_field(ui, "Statuses", "statuses", &join(&p.statuses), "todo, doing, done") {
        p.statuses = split(&v);
        changed = true;
    }
    if let Some(v) = text_field(ui, "Done when", "done", &join(&p.done_statuses), "done, cancelled") {
        p.done_statuses = split(&v);
        changed = true;
    }
    if let Some(v) = text_field(ui, "Tags offered", "tags", &join(&p.tags), "work, home") {
        p.tags = split(&v);
        changed = true;
    }
    if let Some(v) = text_field(ui, "Task tag", "tag", &p.tag, "task") {
        p.tag = v.trim().trim_start_matches('#').to_string();
        changed = true;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Write properties as");
        changed |= ui.radio_value(&mut p.field_names, "quire".to_string(), "Quire (time_entries, start/end)").changed();
        changed |= ui.radio_value(&mut p.field_names, "tasknotes".to_string(), "TaskNotes (timeEntries, startTime/endTime)").changed();
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Pomodoro");
        changed |= ui.add(egui::DragValue::new(&mut p.pomodoro.work).range(1..=180).suffix(" min work")).changed();
        changed |= ui.add(egui::DragValue::new(&mut p.pomodoro.short_break).range(1..=60).suffix(" min break")).changed();
        changed |= ui.add(egui::DragValue::new(&mut p.pomodoro.long_break).range(1..=120).suffix(" min long break")).changed();
        changed |= ui.add(egui::DragValue::new(&mut p.pomodoro.long_every).range(0..=12).prefix("every ")).changed();
    });
    changed |= ui.checkbox(&mut p.pomodoro.auto_continue, "Start the next pomodoro when a break ends").changed();
    if changed && let Err(e) = ws.set_task_prefs(&p) {
        ws.notify(quire_app::workspace::NoticeLevel::Error, e);
    }
}

/// Spec text used by tests and the board (kept here so both agree).
#[cfg(test)]
pub(crate) fn board_spec() -> views::ViewSpec {
    views::parse("from: tasks\nlayout: kanban\ngroup: status\n").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_form_reads_its_fields() {
        let d = NewTaskDialog::sample(false);
        let t = d.task().unwrap();
        assert_eq!(
            (t.title.as_str(), t.priority.as_deref(), t.due.as_deref(), t.estimate),
            ("Write the quarterly report", Some("high"), Some("2026-11-20"), Some(90))
        );
        assert_eq!(t.projects, ["[[Website]]"]);
        assert!(NewTaskDialog::sample(true).task().unwrap_err().contains("soon"));
        let mut d = NewTaskDialog::sample(false);
        d.due = "tomorrow".into();
        assert_eq!(d.task().unwrap().due, Some((Local::now().date_naive() + Duration::days(1)).format("%Y-%m-%d").to_string()));
        d.due = "next blue moon".into();
        assert!(d.task().is_err());
        assert_eq!(board_spec().group.as_deref(), Some("status"));
    }
}
