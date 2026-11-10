// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Mermaid diagrams you can move around (0.11): in notes and in the
//! preview, drag to pan, Ctrl+wheel (or a pinch) to zoom, double-click to
//! reset; the buttons on the corner zoom, reset, show the diagram full
//! screen (where the wheel zooms) and, in the editor, open its source.
//! 0.12: a zoomed diagram is drawn again at the zoom's scale (once the zoom
//! rests), so it stays sharp past 2×.

use egui::{Color32, CursorIcon, Id, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;
use crate::typeset_ui::{Job, Out, Picture};

/// How a diagram is shown: zoom and where its top-left is, from the frame's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Pan {
    pub zoom: f32,
    pub offset: Vec2,
}

impl Default for Pan {
    fn default() -> Self {
        Self { zoom: 1.0, offset: Vec2::ZERO }
    }
}

const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 8.0;

impl Pan {
    /// Zoom by `factor` keeping the point `at` (in the frame) still.
    pub(crate) fn zoom_at(&mut self, factor: f32, at: Vec2) {
        let z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let k = z / self.zoom;
        self.offset = at - (at - self.offset) * k;
        self.zoom = z;
    }

    /// Keep at least a part of the picture (`size` at zoom 1) in a frame of `frame`.
    pub(crate) fn keep_in(&mut self, size: Vec2, frame: Vec2) {
        let shown = size * self.zoom;
        let margin = Vec2::splat(40.0).min(shown).min(frame);
        self.offset.x = self.offset.x.clamp(margin.x - shown.x, frame.x - margin.x);
        self.offset.y = self.offset.y.clamp(margin.y - shown.y, frame.y - margin.y);
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// How long the zoom has to rest before a diagram is drawn again at it.
const REDRAW_AFTER: f64 = 0.25;

/// What a zoomed diagram is drawn with (egui's temporary data, per diagram).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Sharp {
    /// The scale step the zoom asks for, and since when (egui time).
    want: Option<f32>,
    since: f64,
    /// The last step drawn and shown (the engine keeps its picture).
    shown: Option<f32>,
}

/// 0.12: the texture to draw `pic` with when it shows at `factor` times
/// its size in points. A zoomed diagram is drawn again at the zoom's scale
/// step ([`crate::typeset_ui::zoom_bucket`]) once the zoom has rested for
/// [`REDRAW_AFTER`]; until then the sharpest picture drawn so far is
/// stretched. The engine keeps a few such pictures (bounded).
fn sharp_texture(ctx: &egui::Context, id: Id, pic: &Picture, job: &Job, factor: f32) -> egui::TextureId {
    let key = id.with("sharp");
    let mut st: Sharp = ctx.data(|d| d.get_temp(key)).unwrap_or_default();
    let now = ctx.input(|i| i.time);
    let want = crate::typeset_ui::zoom_bucket(factor);
    if want != st.want {
        st.want = want;
        st.since = now;
    }
    let mut texture = pic.texture.id();
    if let Some(b) = want {
        let waited = now - st.since;
        if waited >= REDRAW_AFTER {
            match crate::typeset_ui::request_zoomed(ctx, job, b) {
                Out::Ready(p) => {
                    st.shown = Some(b);
                    texture = p.texture.id();
                }
                Out::Pending => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
                Out::Failed(_) => {}
            }
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(REDRAW_AFTER - waited));
        }
        // Meanwhile: the step shown before, if the engine still has it.
        if texture == pic.texture.id()
            && let Some(prev) = st.shown
            && let Out::Ready(p) = crate::typeset_ui::request_zoomed(ctx, job, prev)
        {
            texture = p.texture.id();
        }
    }
    ctx.data_mut(|d| d.insert_temp(key, st));
    texture
}

/// What a click on a diagram's buttons asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asked {
    /// Show the source (the editor puts the caret in the block).
    Edit,
}

fn key(source: &str) -> Id {
    Id::unique(("quire-diagram-pan", source))
}

const FULLSCREEN: &str = "quire-diagram-fullscreen";

/// The diagram the full-screen button asked for, once (the app opens it).
pub(crate) fn take_fullscreen(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<String>(Id::unique(FULLSCREEN)))
}

/// A small square button drawn on the picture; true when clicked.
fn corner_button(ui: &Ui, r: Rect, id: Id, text: &str, tip: &str, pal: &Palette) -> bool {
    let resp = ui.interact(r, id, Sense::click()).on_hover_text(tip);
    let fill = if resp.hovered() { pal.hover } else { pal.panel };
    let p = ui.painter();
    p.rect_filled(r, 4.0, fill.gamma_multiply(0.95));
    p.rect_stroke(r, 4.0, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    p.text(r.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.0), pal.text);
    resp.clicked()
}

/// Draw `pic` (the picture of `source`) in `frame` with pan and zoom, and
/// handle the pointer over it. `editable`: offer "Edit source".
pub(crate) fn show(ui: &Ui, frame: Rect, pic: &Picture, source: &str, pal: &Palette, editable: bool) -> Option<Asked> {
    let id = key(source);
    let ctx = ui.ctx();
    let mut pan: Pan = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
    let resp = ui.interact(frame, id.with("area"), Sense::click_and_drag());
    let hovered = resp.hovered() || ui.rect_contains_pointer(frame);
    if resp.dragged() {
        pan.offset += resp.drag_delta();
        ctx.set_cursor_icon(CursorIcon::Grabbing);
    } else if resp.hovered() {
        ctx.set_cursor_icon(CursorIcon::Grab);
    }
    if hovered {
        let z = ui.input(egui::InputState::zoom_delta);
        if (z - 1.0).abs() > f32::EPSILON
            && let Some(p) = ui.input(|i| i.pointer.hover_pos())
        {
            pan.zoom_at(z, p - frame.min);
        }
    }
    if resp.double_clicked() {
        pan = Pan::default();
    }
    pan.keep_in(frame.size(), frame.size());
    // The picture, cut to its frame.
    let painter = ui.painter().with_clip_rect(frame.intersect(ui.clip_rect()));
    let shown = Rect::from_min_size(frame.min + pan.offset, frame.size() * pan.zoom);
    let job = Job::Diagram { source: source.to_string(), dark: pal.dark };
    let texture = sharp_texture(ctx, id, pic, &job, shown.width() / pic.size.x.max(1.0));
    painter.image(texture, shown, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    if !pan.is_default() {
        painter.rect_stroke(frame, 4.0, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    }
    let mut asked = None;
    // Buttons on the top-right corner while the pointer is over the diagram.
    if hovered || !pan.is_default() {
        let s = 22.0;
        let mut x = frame.right() - s - 4.0;
        let y = frame.top() + 4.0;
        let mut next = |text: &str, tip: &str, salt: &str| {
            let r = Rect::from_min_size(Pos2::new(x, y), Vec2::splat(s));
            x -= s + 4.0;
            corner_button(ui, r, id.with(salt), text, tip, pal)
        };
        if next("⛶", "Full screen", "full") {
            ctx.data_mut(|d| d.insert_temp(Id::unique(FULLSCREEN), source.to_string()));
        }
        if !pan.is_default() && next("1:1", "Reset (or double-click the diagram)", "reset") {
            pan = Pan::default();
        }
        if next("+", "Zoom in (or Ctrl+wheel)", "in") {
            pan.zoom_at(1.25, frame.size() / 2.0);
        }
        if next("−", "Zoom out (or Ctrl+wheel)", "out") {
            pan.zoom_at(0.8, frame.size() / 2.0);
        }
        if editable && next("</>", "Edit the diagram's source", "edit") {
            asked = Some(Asked::Edit);
        }
    }
    if resp.hovered() && ui.input(|i| i.pointer.is_still() && !i.pointer.any_down()) {
        resp.on_hover_text("Drag to move · Ctrl+wheel to zoom · double-click to reset");
    }
    ctx.data_mut(|d| d.insert_temp(id, pan));
    asked
}

/// A diagram full screen.
pub(crate) struct DiagramOverlay {
    source: String,
    pan: Pan,
}

impl DiagramOverlay {
    pub(crate) const fn new(source: String) -> Self {
        Self { source, pan: Pan { zoom: 1.0, offset: Vec2::ZERO } }
    }
}

impl OverlayView for DiagramOverlay {
    fn show(&mut self, ctx: &egui::Context, _ws: &mut quire_app::Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let screen = ctx.content_rect();
        let job = Job::Diagram { source: self.source.clone(), dark: pal.dark };
        let out = crate::typeset_ui::request(ctx, &job);
        egui::Area::new(Id::unique("quire-diagram-full")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let (area, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
            ui.painter().rect_filled(area, 0.0, pal.bg);
            let bar = Rect::from_min_size(area.min, Vec2::new(area.width(), 40.0));
            ui.painter().rect_filled(bar, 0.0, pal.panel);
            ui.painter().text(
                Pos2::new(bar.left() + 16.0, bar.center().y),
                egui::Align2::LEFT_CENTER,
                "Diagram · drag to move · wheel to zoom · double-click to reset · Escape to close",
                egui::FontId::proportional(13.0),
                pal.muted,
            );
            let close = Rect::from_min_size(Pos2::new(bar.right() - 76.0, bar.top() + 7.0), Vec2::new(64.0, 26.0));
            if corner_button(ui, close, Id::unique("quire-diagram-full-close"), "Close", "Close (Escape)", pal) {
                res.close = true;
            }
            let frame = Rect::from_min_max(Pos2::new(area.left() + 24.0, bar.bottom() + 16.0), area.max - Vec2::splat(24.0));
            match out {
                Out::Ready(p) => {
                    // Fit the window at zoom 1.
                    let fit = (frame.width() / p.size.x).min(frame.height() / p.size.y).min(4.0);
                    let base = p.size * fit;
                    // Centred in the window at zoom 1.
                    let origin = frame.min + ((frame.size() - base) / 2.0).max(Vec2::ZERO);
                    let resp = ui.interact(frame, Id::unique("quire-diagram-full-area"), Sense::click_and_drag());
                    if resp.dragged() {
                        self.pan.offset += resp.drag_delta();
                    }
                    let (scroll, zoom, pointer) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
                    if resp.hovered() {
                        let at = pointer.map_or(base / 2.0, |p| p - origin);
                        let factor = zoom * (scroll / 400.0).exp();
                        if (factor - 1.0).abs() > f32::EPSILON {
                            self.pan.zoom_at(factor, at);
                        }
                    }
                    if resp.double_clicked() {
                        self.pan = Pan::default();
                    }
                    self.pan.keep_in(base, frame.size());
                    let shown = Rect::from_min_size(origin + self.pan.offset, base * self.pan.zoom);
                    let texture = sharp_texture(ctx, Id::unique("quire-diagram-full-sharp"), &p, &job, shown.width() / p.size.x.max(1.0));
                    ui.painter().with_clip_rect(frame).image(
                        texture,
                        shown,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                Out::Pending => {
                    ui.painter().text(
                        frame.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drawing diagram…",
                        egui::FontId::proportional(15.0),
                        pal.faint,
                    );
                }
                Out::Failed(e) => {
                    ui.painter().text(
                        frame.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("Diagram not drawn: {e}"),
                        egui::FontId::proportional(14.0),
                        pal.danger,
                    );
                }
            }
        });
        res
    }

    fn yields_to_capture(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp, reason = "zoom factors are exact here (2 and the cap)")]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut p = Pan::default();
        p.zoom_at(2.0, Vec2::new(100.0, 50.0));
        assert_eq!(p.zoom, 2.0);
        // The point that was at (100, 50) of the picture is still under the pointer.
        assert_eq!(p.offset + Vec2::new(100.0, 50.0) * 2.0, Vec2::new(100.0, 50.0));
        p.zoom_at(100.0, Vec2::ZERO);
        assert_eq!(p.zoom, MAX_ZOOM);
        let mut far = Pan { zoom: 1.0, offset: Vec2::new(5000.0, -5000.0) };
        far.keep_in(Vec2::new(300.0, 200.0), Vec2::new(300.0, 200.0));
        assert!(far.offset.x <= 260.0 && far.offset.y >= -160.0, "some of the picture stays in view: {far:?}");
    }

    /// 0.12: the stretched picture while zooming, a sharper one once the zoom rests.
    #[cfg(feature = "typeset")]
    #[test]
    fn a_zoomed_diagram_is_drawn_again_once_the_zoom_rests() {
        let ctx = egui::Context::default();
        let job = Job::Diagram { source: "graph TD\n  Zoom --> Rest".into(), dark: false };
        let frame = |t: f64, f: &mut dyn FnMut(&egui::Context)| {
            let mut out = ctx.run_pass(egui::RawInput { time: Some(t), ..egui::RawInput::default() }, |c| f(c));
            out.textures_delta.clear();
        };
        let start = std::time::Instant::now();
        let base = loop {
            let mut out = None;
            frame(0.0, &mut |c| out = Some(crate::typeset_ui::request(c, &job)));
            if let Some(Out::Ready(p)) = out {
                break p;
            }
            assert!(start.elapsed().as_secs() < 60, "rendering took too long");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let id = Id::unique("zoom-test");
        let at = |t: f64, factor: f32| {
            let mut tex = None;
            frame(t, &mut |c| tex = Some(sharp_texture(c, id, &base, &job, factor)));
            tex.unwrap()
        };
        assert_eq!(at(1.0, 1.2), base.texture.id(), "not zoomed past 1.5×: the picture as it is");
        assert_eq!(at(1.1, 4.0), base.texture.id(), "just zoomed: stretched");
        assert_eq!(at(1.2, 4.0), base.texture.id(), "still zooming");
        let mut t = 1.5;
        while at(t, 4.0) == base.texture.id() {
            assert!(start.elapsed().as_secs() < 60, "never drawn again");
            std::thread::sleep(std::time::Duration::from_millis(20));
            t += 0.05;
        }
        // Zooming on to another step: the sharp picture stays until the new one is drawn.
        let sharp = at(t, 4.0);
        assert_eq!(at(t + 0.01, 7.0), sharp);
    }
}
