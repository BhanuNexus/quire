// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ink in the desktop: drawing sketches (strokes as triangle ribbons with
//! round ends, shapes), the pen, marker, highlighter, eraser and shape
//! tools, and the surface for a sketch file on its own (`.svg` from
//! "Insert sketch").

use std::time::{Duration, Instant};

use egui::{Color32, Pos2, RichText, Sense, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::vault::{NotePath, Vault};
use quire_canvas::ink::{self, Item, Point, Shape, ShapeKind, Sketch, Stroke as InkStroke, Tool};

use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;

/// Ink colours offered (the first is the default ink, drawn light on a dark theme).
pub(crate) const INK_COLORS: [(&str, &str); 7] = [
    ("#1e1e1e", "Ink"),
    ("#e03131", "Red"),
    ("#f08c00", "Orange"),
    ("#ffd43b", "Yellow"),
    ("#2f9e44", "Green"),
    ("#1971c2", "Blue"),
    ("#9c36b5", "Purple"),
];

/// A drawing tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InkTool {
    Draw(Tool),
    Eraser,
    Shape(ShapeKind),
}

impl InkTool {
    pub(crate) const ALL: [Self; 8] = [
        Self::Draw(Tool::Pen),
        Self::Draw(Tool::Marker),
        Self::Draw(Tool::Highlighter),
        Self::Eraser,
        Self::Shape(ShapeKind::Rect),
        Self::Shape(ShapeKind::Ellipse),
        Self::Shape(ShapeKind::Line),
        Self::Shape(ShapeKind::Arrow),
    ];

    pub(crate) const fn icon(self) -> &'static str {
        match self {
            Self::Draw(Tool::Pen) => "✏",
            Self::Draw(Tool::Marker) => "✒",
            Self::Draw(Tool::Highlighter) => "🖊",
            Self::Eraser => "⌫",
            Self::Shape(ShapeKind::Rect) => "□",
            Self::Shape(ShapeKind::Ellipse) => "◯",
            Self::Shape(ShapeKind::Line) => "—",
            Self::Shape(ShapeKind::Arrow) => "↗",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Draw(Tool::Pen) => "Pen (P)",
            Self::Draw(Tool::Marker) => "Marker",
            Self::Draw(Tool::Highlighter) => "Highlighter",
            Self::Eraser => "Eraser (E)",
            Self::Shape(ShapeKind::Rect) => "Rectangle",
            Self::Shape(ShapeKind::Ellipse) => "Ellipse",
            Self::Shape(ShapeKind::Line) => "Line",
            Self::Shape(ShapeKind::Arrow) => "Arrow",
        }
    }
}

/// The ink tool's settings, kept while the surface is open.
#[derive(Debug, Clone)]
pub(crate) struct InkSettings {
    pub color: usize,
    /// The highlighter has its own colour (yellow by default).
    pub highlight_color: usize,
    /// Multiplies each tool's default width.
    pub width: f32,
}

impl Default for InkSettings {
    fn default() -> Self {
        Self { color: 0, highlight_color: 3, width: 1.0 }
    }
}

impl InkSettings {
    pub(crate) fn color_for(&self, tool: InkTool) -> &'static str {
        let i = if tool == InkTool::Draw(Tool::Highlighter) { self.highlight_color } else { self.color };
        INK_COLORS[i.min(INK_COLORS.len() - 1)].0
    }
}

/// A colour from a sketch, readable on this theme: the default ink is light on dark.
pub(crate) fn ink_color(hex: &str, pal: &Palette, opacity: f32) -> Color32 {
    let c = quire_canvas::Color::parse(hex).map_or((30, 30, 30), quire_canvas::Color::rgb);
    let (r, g, b) = if pal.dark && u16::from(c.0) + u16::from(c.1) + u16::from(c.2) < 150 {
        let t = pal.text;
        (t.r(), t.g(), t.b())
    } else {
        c
    };
    Color32::from_rgba_unmultiplied(r, g, b, (opacity.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Where a sketch is on screen: `screen = origin + p × scale`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Place {
    pub origin: Pos2,
    pub scale: f32,
}

impl Place {
    pub(crate) fn to_screen(self, x: f32, y: f32) -> Pos2 {
        self.origin + Vec2::new(x, y) * self.scale
    }

    pub(crate) fn to_sketch(self, p: Pos2) -> (f32, f32) {
        let v = (p - self.origin) / self.scale.max(1e-6);
        (v.x, v.y)
    }
}

/// Draw one stroke: a triangle ribbon, with round ends (not for the highlighter).
pub(crate) fn paint_stroke(painter: &egui::Painter, s: &InkStroke, at: Place, pal: &Palette) {
    let color = ink_color(&s.color, pal, s.tool.opacity());
    let rib = ink::ribbon(&s.points, s.size, s.tool.thinning());
    if rib.len() == 1 {
        painter.circle_filled(at.to_screen(rib[0].center.0, rib[0].center.1), (rib[0].radius * at.scale).max(0.5), color);
        return;
    }
    let mut mesh = egui::Mesh::default();
    for (i, p) in rib.iter().enumerate() {
        mesh.colored_vertex(at.to_screen(p.left.0, p.left.1), color);
        mesh.colored_vertex(at.to_screen(p.right.0, p.right.1), color);
        if i > 0 {
            let k = u32::try_from(2 * i).unwrap_or(0);
            mesh.add_triangle(k - 2, k - 1, k);
            mesh.add_triangle(k - 1, k + 1, k);
        }
    }
    painter.add(mesh);
    if s.tool != Tool::Highlighter {
        for p in [rib[0], rib[rib.len() - 1]] {
            painter.circle_filled(at.to_screen(p.center.0, p.center.1), (p.radius * at.scale).max(0.5), color);
        }
    }
}

/// Draw one shape.
pub(crate) fn paint_shape(painter: &egui::Painter, s: &Shape, at: Place, pal: &Palette) {
    let stroke = Stroke::new((s.width * at.scale).max(1.0), ink_color(&s.color, pal, 1.0));
    let a = at.to_screen(s.from.0, s.from.1);
    let b = at.to_screen(s.to.0, s.to.1);
    match s.kind {
        ShapeKind::Rect => {
            painter.rect_stroke(egui::Rect::from_two_pos(a, b), 0.0, stroke, egui::StrokeKind::Middle);
        }
        ShapeKind::Ellipse => {
            let r = egui::Rect::from_two_pos(a, b);
            let pts: Vec<Pos2> = (0..=48)
                .map(|k| {
                    let t = std::f32::consts::TAU * k as f32 / 48.0;
                    r.center() + Vec2::new(r.width() / 2.0 * t.cos(), r.height() / 2.0 * t.sin())
                })
                .collect();
            painter.add(egui::Shape::line(pts, stroke));
        }
        ShapeKind::Line => {
            painter.line_segment([a, b], stroke);
        }
        ShapeKind::Arrow => {
            painter.line_segment([a, b], stroke);
            let [h1, h2] = s.arrow_head();
            painter.add(egui::Shape::line(vec![at.to_screen(h1.0, h1.1), b, at.to_screen(h2.0, h2.1)], stroke));
        }
    }
}

/// Draw a whole sketch.
pub(crate) fn paint_sketch(painter: &egui::Painter, sk: &Sketch, at: Place, pal: &Palette) {
    for item in &sk.items {
        match item {
            Item::Stroke(s) => paint_stroke(painter, s, at, pal),
            Item::Shape(s) => paint_shape(painter, s, at, pal),
            Item::Raw(_) => {}
        }
    }
}

/// Whether a sketch has drawing Quire can't show (elements another app wrote).
pub(crate) fn has_foreign(sk: &Sketch) -> bool {
    sk.items.iter().any(|i| matches!(i, Item::Raw(_)))
}

/// A stroke or shape being drawn, in sketch units.
#[derive(Debug, Clone)]
pub(crate) struct Gesture {
    pub tool: InkTool,
    pub points: Vec<Point>,
    pub color: String,
    pub size: f32,
}

impl Gesture {
    pub(crate) fn new(tool: InkTool, settings: &InkSettings, scale: f32) -> Self {
        let base = match tool {
            InkTool::Draw(t) => t.default_size(),
            InkTool::Shape(_) => 2.5,
            InkTool::Eraser => 12.0,
        };
        // Widths are in screen points at the time of drawing.
        Self { tool, points: Vec::new(), color: settings.color_for(tool).to_string(), size: base * settings.width / scale.max(1e-3) }
    }

    pub(crate) fn push(&mut self, p: (f32, f32), pressure: f32) {
        if self.points.last().is_some_and(|l| (l.x - p.0).hypot(l.y - p.1) < 0.2) {
            return;
        }
        self.points.push(Point::new(p.0, p.1, pressure));
    }

    /// The finished item (`None` for the eraser or nothing drawn). `epsilon`
    /// is half a screen pixel in sketch units.
    pub(crate) fn finish(&self, epsilon: f32) -> Option<Item> {
        let first = *self.points.first()?;
        match self.tool {
            InkTool::Draw(tool) => {
                let points = ink::simplify(&self.points, epsilon);
                Some(Item::Stroke(InkStroke { tool, color: self.color.clone(), size: round2(self.size), points: round_points(&points) }))
            }
            InkTool::Shape(kind) => {
                let last = *self.points.last()?;
                ((first.x - last.x).hypot(first.y - last.y) > 2.0).then(|| {
                    Item::Shape(Shape {
                        kind,
                        from: (round2(first.x), round2(first.y)),
                        to: (round2(last.x), round2(last.y)),
                        color: self.color.clone(),
                        width: round2(self.size),
                    })
                })
            }
            InkTool::Eraser => None,
        }
    }

    /// Draw it while it is being made.
    pub(crate) fn paint(&self, painter: &egui::Painter, at: Place, pal: &Palette) {
        match self.finish(0.0) {
            Some(Item::Stroke(s)) => paint_stroke(painter, &s, at, pal),
            Some(Item::Shape(s)) => paint_shape(painter, &s, at, pal),
            _ => {
                if let Some(p) = self.points.last() {
                    painter.circle_stroke(at.to_screen(p.x, p.y), self.size * at.scale / 2.0, Stroke::new(1.0, pal.muted));
                }
            }
        }
    }
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

fn round_points(p: &[Point]) -> Vec<Point> {
    p.iter().map(|q| Point::new(round2(q.x), round2(q.y), round2(q.pressure))).collect()
}

/// Remove items under the eraser. Returns whether anything went.
pub(crate) fn erase(sk: &mut Sketch, p: (f32, f32), r: f32) -> bool {
    let before = sk.items.len();
    sk.items.retain(|i| !i.hit(p, r));
    sk.items.len() != before
}

/// The pen pressure this frame, from touch input (1.0 when the device gives
/// none, so mouse and touch draw alike; 0.5 for the mouse).
pub(crate) fn pressure(ui: &egui::Ui) -> f32 {
    ui.input(|i| {
        i.events.iter().rev().find_map(|e| match e {
            egui::Event::Touch { force: Some(f), .. } => Some(f.clamp(0.05, 1.0)),
            _ => None,
        })
    })
    .unwrap_or(0.5)
}

/// The tool palette: tools, colours and width. Returns true when something changed.
pub(crate) fn toolbar(ui: &mut egui::Ui, tool: &mut Option<InkTool>, settings: &mut InkSettings, pal: &Palette) -> bool {
    let mut changed = false;
    for t in InkTool::ALL {
        let on = *tool == Some(t);
        if ui.selectable_label(on, RichText::new(t.icon()).size(15.0)).on_hover_text(t.label()).clicked() {
            *tool = if on { None } else { Some(t) };
            changed = true;
        }
    }
    if tool.is_some() && *tool != Some(InkTool::Eraser) {
        ui.separator();
        let hl = *tool == Some(InkTool::Draw(Tool::Highlighter));
        for (i, (hex, name)) in INK_COLORS.iter().enumerate() {
            let chosen = if hl { settings.highlight_color } else { settings.color } == i;
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
            ui.painter().circle_filled(r.center(), 7.0, ink_color(hex, pal, 1.0));
            if chosen {
                ui.painter().circle_stroke(r.center(), 9.0, Stroke::new(1.5, pal.accent));
            }
            if resp.on_hover_text(*name).clicked() {
                if hl {
                    settings.highlight_color = i;
                } else {
                    settings.color = i;
                }
                changed = true;
            }
        }
        ui.separator();
        for (w, label, tip) in [(0.6, "S", "Thin"), (1.0, "M", "Medium"), (1.8, "L", "Thick")] {
            if ui.selectable_label((settings.width - w).abs() < 0.01, label).on_hover_text(tip).clicked() {
                settings.width = w;
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- the sketch surface

/// A sketch file on its own (from "Insert sketch", or any `.svg`).
pub(crate) struct SketchSurface {
    path: NotePath,
    vault: Option<Vault>,
    sketch: Option<Result<Sketch, String>>,
    disk_text: String,
    disk_gen: u64,
    dirty: bool,
    last_edit: Instant,
    undo: Vec<Sketch>,
    redo: Vec<Sketch>,
    tool: Option<InkTool>,
    settings: InkSettings,
    gesture: Option<Gesture>,
    ctx: Option<egui::Context>,
}

impl SketchSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self {
            path: path.clone(),
            vault: None,
            sketch: None,
            disk_text: String::new(),
            disk_gen: 0,
            dirty: false,
            last_edit: Instant::now(),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Some(InkTool::Draw(Tool::Pen)),
            settings: InkSettings::default(),
            gesture: None,
            ctx: None,
        }
    }

    fn load(&mut self, ws: &Workspace) {
        let Some(vault) = ws.vault().cloned() else { return };
        self.disk_gen = ws.features.canvas.disk_generation(&self.path);
        match vault.read(&self.path) {
            Ok(t) => {
                self.sketch = Some(ink::parse_svg(&t));
                self.disk_text = t;
            }
            Err(e) => self.sketch = Some(Err(e.to_string())),
        }
        self.vault = Some(vault);
    }

    fn save(&mut self) {
        let (Some(vault), Some(Ok(sk))) = (&self.vault, &self.sketch) else { return };
        let text = ink::to_svg(sk);
        if text != self.disk_text && vault.storage().write_atomic(&self.path, &text).is_ok() {
            self.disk_text = text;
            if let (Some(ctx), Some(abs)) = (&self.ctx, vault.storage().absolute(&self.path)) {
                super::loader::forget(ctx, &abs);
            }
        }
        self.dirty = false;
    }

    fn edit(&mut self) {
        if let Some(Ok(sk)) = &self.sketch {
            self.undo.push(sk.clone());
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.dirty = true;
        self.last_edit = Instant::now();
    }
}

impl Drop for SketchSurface {
    fn drop(&mut self) {
        if self.dirty {
            self.save();
        }
    }
}

impl Surface for SketchSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        if self.ctx.is_none() {
            self.ctx = Some(ui.ctx().clone());
        }
        if self.sketch.is_none() || (!self.dirty && ws.features.canvas.disk_generation(&self.path) != self.disk_gen) {
            self.load(ws);
        }
        if self.dirty && self.last_edit.elapsed() > Duration::from_millis(600) {
            self.save();
        } else if self.dirty {
            ui.ctx().request_repaint_after(Duration::from_millis(650));
        }
        let mut undo_now = false;
        let mut redo_now = false;
        egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.path.file_name()).color(pal.strong));
                ui.separator();
                toolbar(ui, &mut self.tool, &mut self.settings, pal);
                ui.separator();
                undo_now = ui.add_enabled(!self.undo.is_empty(), egui::Button::new("⟲")).on_hover_text("Undo (Ctrl+Z)").clicked();
                redo_now = ui.add_enabled(!self.redo.is_empty(), egui::Button::new("⟳")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked();
            });
        });
        let (undo_key, redo_key) = ui.input(|i| {
            let c = i.modifiers.command;
            (
                c && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                c && (i.key_pressed(egui::Key::Y) || (i.modifiers.shift && i.key_pressed(egui::Key::Z))),
            )
        });
        if (undo_now || undo_key)
            && let (Some(prev), Some(Ok(sk))) = (self.undo.pop(), &mut self.sketch)
        {
            self.redo.push(std::mem::replace(sk, prev));
            self.dirty = true;
            self.last_edit = Instant::now();
        }
        if (redo_now || redo_key)
            && let (Some(next), Some(Ok(sk))) = (self.redo.pop(), &mut self.sketch)
        {
            self.undo.push(std::mem::replace(sk, next));
            self.dirty = true;
            self.last_edit = Instant::now();
        }
        let sketch = match &self.sketch {
            Some(Ok(s)) => s.clone(),
            Some(Err(e)) => {
                ui.centered_and_justified(|ui| ui.label(RichText::new(format!("Can't show this drawing: {e}")).color(pal.muted)));
                return PanelOut::default();
            }
            None => return PanelOut::default(),
        };
        let avail = ui.available_rect_before_wrap();
        let (rect, resp) = ui.allocate_exact_size(avail.size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, pal.bg);
        let (w, h) = (sketch.width.max(1.0), sketch.height.max(1.0));
        let scale = ((rect.width() - 48.0) / w).min((rect.height() - 48.0) / h).clamp(0.1, 3.0);
        let paper = egui::Rect::from_center_size(rect.center(), Vec2::new(w * scale, h * scale));
        painter.rect_filled(paper, 4.0, if pal.dark { pal.panel } else { Color32::WHITE });
        painter.rect_stroke(paper, 4.0, Stroke::new(1.0, pal.border), egui::StrokeKind::Outside);
        let at = Place { origin: paper.min, scale };
        paint_sketch(&painter, &sketch, at, pal);
        if has_foreign(&sketch) {
            painter.text(
                rect.left_bottom() + Vec2::new(12.0, -10.0),
                egui::Align2::LEFT_BOTTOM,
                "Some of this drawing was made in another app; Quire keeps it but doesn't show it.",
                egui::FontId::proportional(12.0),
                pal.muted,
            );
        }
        if let Some(tool) = self.tool {
            if resp.drag_started()
                && let Some(p) = resp.interact_pointer_pos()
            {
                let mut g = Gesture::new(tool, &self.settings, scale);
                g.push(at.to_sketch(p), pressure(ui));
                if tool == InkTool::Eraser {
                    self.edit();
                }
                self.gesture = Some(g);
            }
            if let (Some(g), Some(p)) = (&mut self.gesture, resp.interact_pointer_pos())
                && resp.dragged()
            {
                g.push(at.to_sketch(p), pressure(ui));
                if g.tool == InkTool::Eraser
                    && let Some(Ok(sk)) = &mut self.sketch
                {
                    let r = g.size / 2.0;
                    if erase(sk, at.to_sketch(p), r) {
                        self.dirty = true;
                        self.last_edit = Instant::now();
                    }
                }
            }
            if resp.drag_stopped()
                && let Some(g) = self.gesture.take()
                && let Some(item) = g.finish(0.5 / scale)
            {
                self.edit();
                if let Some(Ok(sk)) = &mut self.sketch {
                    sk.items.push(item);
                    // Grow the drawing area to fit what was drawn outside it.
                    if let Some(b) = sk.content_bounds() {
                        let (dx, dy) = ((-b.x + 8.0).max(0.0), (-b.y + 8.0).max(0.0));
                        if dx > 0.0 || dy > 0.0 {
                            for i in &mut sk.items {
                                i.translate(dx, dy);
                            }
                        }
                        sk.width = sk.width.max(b.right() + dx + 8.0).ceil();
                        sk.height = sk.height.max(b.bottom() + dy + 8.0).ceil();
                    }
                }
            }
            if let Some(g) = &self.gesture {
                g.paint(&painter, at, pal);
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }
        PanelOut::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gestures_become_strokes_and_shapes() {
        let s = InkSettings::default();
        let mut g = Gesture::new(InkTool::Draw(Tool::Pen), &s, 2.0);
        for i in 0..20 {
            g.push((i as f32, 0.0), 0.5);
        }
        let Some(Item::Stroke(st)) = g.finish(0.25) else { panic!("a stroke") };
        assert_eq!(st.points.len(), 2, "a straight line keeps its ends");
        assert!((st.size - 1.5).abs() < 0.01, "3 screen points at 2× zoom");
        let mut sh = Gesture::new(InkTool::Shape(ShapeKind::Arrow), &s, 1.0);
        sh.push((0.0, 0.0), 0.5);
        sh.push((40.0, 30.0), 0.5);
        assert!(matches!(sh.finish(0.5), Some(Item::Shape(Shape { kind: ShapeKind::Arrow, .. }))));
        let mut tap = Gesture::new(InkTool::Shape(ShapeKind::Rect), &s, 1.0);
        tap.push((0.0, 0.0), 0.5);
        assert!(tap.finish(0.5).is_none(), "a click isn't a shape");
        assert_eq!(s.color_for(InkTool::Draw(Tool::Highlighter)), "#ffd43b");
        let dark = ink_color("#1e1e1e", &crate::theme::DARK, 1.0);
        assert!(dark.r() > 150, "default ink is light on dark");
    }
}
