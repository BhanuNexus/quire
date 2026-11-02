// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Painting the decorations features put on a note (`Workspace::decorations`):
//! tinted and struck-through lines, ghost lines with action buttons, and
//! feature widgets under a line. The editor view calls these per visible
//! line; clicks come back as `DecorationEvent`s.
//!
//! WS-A owns the look of ghost lines and their toolbar (review); WS-F owns
//! the hook lines in `view/mod.rs`.

use egui::{Color32, Pos2, Rect, RichText, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::decor::{ActionId, Decoration, DecorationKind, Tone};

use crate::theme::Palette;

pub(crate) fn tone_color(tone: Tone, pal: &Palette) -> Color32 {
    match tone {
        Tone::Added => Color32::from_rgb(0x2e, 0xa0, 0x43).gamma_multiply(if pal.dark { 0.28 } else { 0.18 }),
        Tone::Removed => pal.danger.gamma_multiply(if pal.dark { 0.28 } else { 0.16 }),
        Tone::Conflict | Tone::Warning => pal.warn.gamma_multiply(if pal.dark { 0.3 } else { 0.2 }),
        Tone::Info => pal.selection.gamma_multiply(0.6),
    }
}

/// The tint of `line`, if a decoration tints it (the last one wins).
pub(crate) fn tint(decos: &[Decoration], line: usize) -> Option<Tone> {
    decos.iter().rev().find_map(|d| match d.kind {
        DecorationKind::Tint(t) if d.lines.contains(&line) => Some(t),
        _ => None,
    })
}

pub(crate) fn struck(decos: &[Decoration], line: usize) -> bool {
    decos.iter().any(|d| matches!(d.kind, DecorationKind::Strike) && d.lines.contains(&line))
}

/// Strike every row of a laid-out line.
pub(crate) fn paint_strike(painter: &egui::Painter, galley: &egui::Galley, pos: Pos2, pal: &Palette) {
    for row in &galley.rows {
        let r = row.rect().translate(pos.to_vec2());
        painter.line_segment([Pos2::new(r.left(), r.center().y), Pos2::new(r.right(), r.center().y)], Stroke::new(1.2, pal.danger));
    }
}

/// Where decorations after a line go: left edge, top, text width, font size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Place {
    pub x0: f32,
    pub top: f32,
    pub width: f32,
    pub size: f32,
}

/// Draw the ghost lines and widgets anchored after `line`, starting at
/// `at.top`. Returns the height used and a clicked action, if any.
pub(crate) fn paint_after(
    ui: &mut egui::Ui,
    ws: &Workspace,
    decos: &[Decoration],
    line: usize,
    at: Place,
    pal: &Palette,
) -> (f32, Option<ActionId>) {
    let Place { x0, top, width, size } = at;
    let mut h = 0.0;
    let mut clicked = None;
    for d in decos {
        match &d.kind {
            DecorationKind::Ghost { after_line, lines, tone, actions } if *after_line == line => {
                let (used, c) = ghost(ui, lines, *tone, actions, Pos2::new(x0, top + h), width, size, pal);
                h += used;
                clicked = clicked.or(c);
            }
            DecorationKind::Widget { after_line, id } if *after_line == line => {
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt(("quire-decor-widget", *id))
                        .max_rect(Rect::from_min_size(Pos2::new(x0, top + h), Vec2::new(width, 4000.0)))
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                let (used, c) = crate::registry::paint_widget(&mut child, ws, *id, pal);
                h += used;
                clicked = clicked.or(c);
            }
            _ => {}
        }
    }
    (h, clicked)
}

/// One block of ghost lines: read-only text on a tint, buttons below.
fn ghost(
    ui: &mut egui::Ui,
    lines: &[String],
    tone: Tone,
    actions: &[(ActionId, String)],
    at: Pos2,
    width: f32,
    size: f32,
    pal: &Palette,
) -> (f32, Option<ActionId>) {
    let font = egui::FontId::proportional(size);
    let mut y = at.y + 2.0;
    let painter = ui.painter().clone();
    for l in lines {
        let g = painter.layout(l.clone(), font.clone(), pal.text.gamma_multiply(0.85), width);
        let r = Rect::from_min_size(Pos2::new(at.x, y), Vec2::new(width, g.size().y));
        painter.rect_filled(r, 2.0, tone_color(tone, pal));
        painter.galley(r.min, g, pal.text);
        y += r.height();
    }
    let mut clicked = None;
    if !actions.is_empty() {
        let row = Rect::from_min_size(Pos2::new(at.x, y + 2.0), Vec2::new(width, size * 1.6));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)));
        for (id, label) in actions {
            if child.small_button(RichText::new(label).size(size * 0.75)).clicked() {
                clicked = Some(*id);
            }
        }
        y = row.bottom();
    }
    (y - at.y + 4.0, clicked)
}

#[cfg(test)]
mod tests {
    use quire_app::Owner;
    use quire_app::decor::WidgetId;

    use super::*;

    #[test]
    fn decorations_lay_out_and_report_heights() {
        let a = ActionId { owner: Owner::Agent, id: 7 };
        let decos = vec![
            Decoration::tint(2..4, Tone::Removed),
            Decoration::strike(2..4),
            Decoration::ghost(3, vec!["new line one".into(), "new line two".into()], Tone::Added, vec![(a, "Accept".into())]),
            Decoration::widget(3, WidgetId { owner: Owner::Spatial, id: 1 }),
        ];
        assert_eq!(tint(&decos, 3), Some(Tone::Removed));
        assert_eq!(tint(&decos, 4), None);
        assert!(struck(&decos, 2) && !struck(&decos, 5));
        let ws = Workspace::new(quire_app::Settings::default());
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut heights = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let at = Place { x0: 0.0, top: 0.0, width: 400.0, size: 16.0 };
                let (h, clicked) = paint_after(ui, &ws, &decos, 3, at, &crate::theme::DARK);
                assert!(clicked.is_none());
                heights.push(h);
                assert!(paint_after(ui, &ws, &decos, 2, at, &crate::theme::DARK).0 < 0.01, "nothing after line 2");
            });
            out.textures_delta.clear();
        }
        assert!(heights.iter().all(|h| *h > 2.0 * 16.0 && (*h - heights[0]).abs() < 0.5), "{heights:?}");
    }
}
