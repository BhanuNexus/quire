// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Calendar sidebar tab: a month grid of periodic notes and due tasks (design §E.4).
//!
//! A dot means the day has a daily note, a ring that tasks are due, and a
//! darker day that more was written in its note. Clicking a day opens (or
//! makes) its daily note; clicking a week number, the weekly note. Also the
//! periodic-notes section of Settings → Planning.
//!
//! Owned by WS-E. Registered in `registry.rs`.

use std::fmt::Write as _;

use chrono::{Datelike, Local, NaiveDate};
use egui::{Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use quire_app::Workspace;
use quire_app::periodic::{add_months, first_weekday};
use quire_app::plan::periodic::{Period, PeriodicPrefs};

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct CalendarUi {
    /// The month shown; `None` follows today.
    month: Option<(i32, u32)>,
    /// Periodic settings being edited, and the last save error.
    draft: Option<PeriodicPrefs>,
    error: Option<String>,
}

/// Whether the tab is offered.
pub(crate) fn available(ws: &Workspace) -> bool {
    ws.vault().is_some()
}

/// How dark a day is, from the words written in its note.
fn intensity(words: usize) -> f32 {
    match words {
        0 => 0.0,
        1..=99 => 0.25,
        100..=299 => 0.45,
        300..=799 => 0.65,
        _ => 0.85,
    }
}

pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let st = &mut st.calendar;
    let today = Local::now().date_naive();
    let (y, m) = st.month.unwrap_or_else(|| (today.year(), today.month()));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.small_button("◀").on_hover_text("Previous month").clicked() {
            st.month = Some(add_months(y, m, -1));
        }
        let title = NaiveDate::from_ymd_opt(y, m, 1).map_or_else(String::new, |d| d.format("%B %Y").to_string());
        let r = ui.add(egui::Label::new(RichText::new(title).size(14.0).color(pal.strong).strong()).sense(Sense::click()));
        if r.on_hover_text("Open this month's note").clicked()
            && let Some(d) = NaiveDate::from_ymd_opt(y, m, 1)
            && ws.open_period(Period::Monthly, d).is_some()
        {
            out.jumped = true;
        }
        if ui.small_button("▶").on_hover_text("Next month").clicked() {
            st.month = Some(add_months(y, m, 1));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if st.month.is_some_and(|x| x != (today.year(), today.month())) && ui.small_button("Today").clicked() {
                st.month = None;
            }
        });
    });
    ui.add_space(4.0);
    let marks = ws.calendar_month(y, m);
    let w = ui.available_width().min(320.0);
    let cell = ((w - 4.0) / 8.0).floor().clamp(22.0, 38.0);
    let first = first_weekday(y, m) as usize;
    let rows = (first + marks.len()).div_ceil(7);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(cell * 8.0, cell * (rows as f32 + 1.0)), Sense::hover());
    let painter = ui.painter_at(rect);
    let font = egui::FontId::proportional((cell * 0.4).clamp(10.0, 13.0));
    let small = egui::FontId::proportional((cell * 0.33).clamp(9.0, 11.0));
    // Header: W, Mon … Sun.
    for (i, d) in ["W", "M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
        let c = Pos2::new(rect.left() + cell * (i as f32 + 0.5), rect.top() + cell * 0.5);
        painter.text(c, egui::Align2::CENTER_CENTER, *d, small.clone(), if i == 0 { pal.faint } else { pal.muted });
    }
    let mut open_day: Option<NaiveDate> = None;
    let mut open_week: Option<NaiveDate> = None;
    for row in 0..rows {
        let y0 = rect.top() + cell * (row as f32 + 1.0);
        // The week number (ISO) of the row's Monday.
        let monday_index = row * 7;
        let day_of_monday = monday_index as i64 - first as i64; // 0-based day of month, may be negative
        let Some(first_day) = NaiveDate::from_ymd_opt(y, m, 1) else { continue };
        let monday = first_day + chrono::Duration::days(day_of_monday);
        let wr = Rect::from_min_size(Pos2::new(rect.left(), y0), Vec2::splat(cell));
        let wresp = ui.interact(wr, ui.scope_id().with(("quire-cal-week", y, m, row)), Sense::click());
        if wresp.hovered() {
            painter.rect_filled(wr.shrink(2.0), 4.0, pal.hover);
        }
        painter.text(wr.center(), egui::Align2::CENTER_CENTER, monday.iso_week().week().to_string(), small.clone(), pal.faint);
        if wresp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Open week {}", monday.iso_week().week())).clicked()
        {
            open_week = Some(monday);
        }
        for col in 0..7 {
            let idx = row * 7 + col;
            if idx < first || idx >= first + marks.len() {
                continue;
            }
            let day = idx - first + 1;
            let date = first_day + chrono::Duration::days(day as i64 - 1);
            let mk = marks[day - 1];
            let r = Rect::from_min_size(Pos2::new(rect.left() + cell * (col as f32 + 1.0), y0), Vec2::splat(cell));
            let inner = r.shrink(2.0);
            let resp = ui.interact(r, ui.scope_id().with(("quire-cal-day", y, m, day)), Sense::click());
            let shade = intensity(mk.words);
            if shade > 0.0 {
                painter.rect_filled(inner, 5.0, pal.accent.gamma_multiply(shade * if pal.dark { 0.55 } else { 0.35 }));
            }
            if resp.hovered() {
                painter.rect_stroke(inner, 5.0, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
            }
            let is_today = date == today;
            if is_today {
                painter.rect_stroke(inner, 5.0, Stroke::new(1.5, pal.accent), egui::StrokeKind::Inside);
            }
            let color = if is_today {
                pal.accent
            } else if col >= 5 {
                pal.muted
            } else {
                pal.text
            };
            painter.text(
                Pos2::new(r.center().x, r.center().y - cell * 0.08),
                egui::Align2::CENTER_CENTER,
                day.to_string(),
                font.clone(),
                color,
            );
            let dot_y = r.bottom() - cell * 0.2;
            if mk.note {
                painter.circle_filled(Pos2::new(r.center().x - if mk.due > 0 { 4.0 } else { 0.0 }, dot_y), 2.2, pal.strong);
            }
            if mk.due > 0 {
                painter.circle_stroke(Pos2::new(r.center().x + if mk.note { 4.0 } else { 0.0 }, dot_y), 2.6, Stroke::new(1.3, pal.warn));
            }
            let mut tip = date.format("%A %-d %B").to_string();
            if mk.note {
                let _ = write!(tip, "\nDaily note · {} words", mk.words);
            }
            if mk.due > 0 {
                let _ = write!(tip, "\n{} task{} due", mk.due, if mk.due == 1 { "" } else { "s" });
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                open_day = Some(date);
            }
        }
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let legend = |ui: &mut Ui, glyph: &str, text: &str| {
            ui.label(RichText::new(format!("{glyph} {text}")).size(11.0).color(pal.muted));
        };
        legend(ui, "●", "note");
        legend(ui, "○", "tasks due");
        legend(ui, "▇", "more written");
    });
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        for (label, cmd) in [
            ("This week", quire_app::Command::OpenWeeklyNote),
            ("This month", quire_app::Command::OpenMonthlyNote),
            ("This year", quire_app::Command::OpenYearlyNote),
        ] {
            if ui.small_button(label).clicked() {
                out.run = Some(cmd);
            }
        }
    });
    if let Some(d) = open_day
        && ws.open_period(Period::Daily, d).is_some()
    {
        out.jumped = true;
    }
    if let Some(d) = open_week
        && ws.open_period(Period::Weekly, d).is_some()
    {
        out.jumped = true;
    }
    out
}

/// The periodic notes section of Settings → Planning.
pub(crate) fn periodic_settings(ui: &mut Ui, ws: &mut Workspace, st: &mut CalendarUi, pal: &Palette) {
    ui.label(RichText::new("PERIODIC NOTES").size(11.0).color(pal.muted).strong());
    ui.label(
        RichText::new("Kept in .quire/shared/vault.toml, so every device that syncs this vault uses them. Names use strftime: %Y-%m-%d, %G-W%V (ISO week), %B %Y.")
            .size(12.0)
            .color(pal.muted),
    );
    if let Some(why) = &ws.features.periodic.read_only {
        ui.label(RichText::new(format!("These settings were {why}; they're shown but not changed.")).color(pal.warn));
    }
    let draft = st.draft.get_or_insert_with(|| ws.periodic_prefs().clone());
    let today = Local::now().date_naive();
    ui.add_space(4.0);
    // Fixed sizes: a grid cell would otherwise shrink a text box to its last width.
    let h = ui.spacing().interact_size.y;
    // Narrow windows get narrower boxes: the label column is about 60 px.
    let room = ((ui.available_width() - 60.0 - 3.0 * 6.0) / 270.0).clamp(0.3, 1.0);
    let (fw, nw, tw) = (90.0 * room, 80.0 * room, 100.0 * room);
    egui::Grid::new("quire-periodic-grid").num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
        for head in ["", "Folder", "Name", "Template"] {
            ui.label(RichText::new(head).size(11.0).color(pal.faint));
        }
        ui.end_row();
        for p in Period::ALL {
            let resolved = draft.resolve(p);
            let example = draft.path_for(p, today);
            let c = draft.get_mut(p);
            let name = match p {
                Period::Daily => "Daily",
                Period::Weekly => "Weekly",
                Period::Monthly => "Monthly",
                Period::Yearly => "Yearly",
            };
            ui.label(RichText::new(name).strong()).on_hover_text(format!("Today's: {example}"));
            let mut folder = c.folder.clone().unwrap_or_else(|| resolved.folder.clone());
            if ui.add_sized([fw, h], egui::TextEdit::singleline(&mut folder)).changed() {
                c.folder = Some(folder);
            }
            let mut format = c.format.clone().unwrap_or_else(|| resolved.format.clone());
            if ui.add_sized([nw, h], egui::TextEdit::singleline(&mut format).font(egui::TextStyle::Monospace)).changed() {
                c.format = Some(format);
            }
            let mut tpl = c.template.clone().unwrap_or_else(|| resolved.template.clone());
            if ui
                .add_sized([tw, h], egui::TextEdit::singleline(&mut tpl).hint_text("none"))
                .on_hover_text("A note in the templates folder")
                .changed()
            {
                c.template = Some(tpl);
            }
            ui.end_row();
        }
    });
    let examples: Vec<String> = Period::ALL.iter().map(|p| draft.path_for(*p, today)).collect();
    ui.add(egui::Label::new(RichText::new(format!("Today: {}", examples.join(" · "))).size(11.5).color(pal.faint)).wrap());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let changed = st.draft.as_ref().is_some_and(|d| d != ws.periodic_prefs());
        if ui.add_enabled(changed && ws.features.periodic.read_only.is_none(), egui::Button::new("Save")).clicked()
            && let Some(d) = st.draft.clone()
        {
            st.error = ws.set_periodic_prefs(d).err();
        }
        if changed && ui.button("Revert").clicked() {
            st.draft = None;
        }
    });
    if let Some(e) = &st.error {
        ui.label(RichText::new(e).color(pal.danger));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darker_with_more_words() {
        assert!(intensity(0) < intensity(50) && intensity(50) < intensity(500) && intensity(500) < intensity(5000));
    }
}
