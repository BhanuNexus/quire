// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Graph view: notes as dots and links as lines, around the open note or
//! across the whole vault. Scroll to zoom, drag to pan or move a note,
//! click a note to open it.

use std::time::{Duration, Instant};

use egui::{Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::graph::{Graph, Layout, Scope};

use crate::theme::Palette;

pub(crate) struct GraphState {
    local: bool,
    depth: usize,
    orphans: bool,
    graph: Graph,
    layout: Layout,
    /// What the graph was built from: index version and scope.
    built: Option<(u64, Scope)>,
    pan: Vec2,
    zoom: f32,
    fitted: bool,
    /// Keep the whole graph in view while it settles, until the user zooms or pans.
    follow: bool,
    dragging: Option<usize>,
}

impl GraphState {
    pub(crate) fn new(ws: &Workspace) -> Self {
        let g = Graph::default();
        let layout = Layout::new(&g);
        // Around the open note when there is one; the whole vault otherwise.
        let local = ws.active_doc().and_then(|d| d.path.as_ref()).is_some();
        Self {
            local,
            depth: 2,
            orphans: false,
            graph: g,
            layout,
            built: None,
            pan: Vec2::ZERO,
            zoom: 1.0,
            fitted: false,
            follow: true,
            dragging: None,
        }
    }

    fn scope(&self, ws: &Workspace) -> Scope {
        match ws.active_doc().and_then(|d| d.path.clone()) {
            Some(focus) if self.local => Scope::Around { focus, depth: self.depth },
            _ => Scope::All { orphans: self.orphans },
        }
    }

    /// Rebuild when the vault or the scope changed, keeping known positions.
    fn refresh(&mut self, ws: &Workspace) {
        let key = (ws.index().version(), self.scope(ws));
        if self.built.as_ref() == Some(&key) {
            return;
        }
        let scope_changed = self.built.as_ref().is_none_or(|(_, s)| *s != key.1);
        let g = ws.link_graph(&key.1);
        let mut layout = Layout::new(&g);
        layout.carry_over(&self.graph, &self.layout, &g);
        if scope_changed {
            // Open (nearly) settled: run the first steps before showing it.
            layout.reheat(1.0);
            let start = Instant::now();
            while !layout.settled() && start.elapsed() < Duration::from_millis(120) {
                layout.step(&g);
            }
            self.fitted = false;
            self.follow = true;
        }
        self.graph = g;
        self.layout = layout;
        self.built = Some(key);
    }

    fn to_screen(&self, rect: Rect, p: [f32; 2]) -> Pos2 {
        rect.center() + (Vec2::new(p[0], p[1]) + self.pan) * self.zoom
    }

    fn to_world(&self, rect: Rect, s: Pos2) -> [f32; 2] {
        let v = (s - rect.center()) / self.zoom - self.pan;
        [v.x, v.y]
    }

    fn fit(&mut self, rect: Rect) {
        let (lo, hi) = self.layout.bounds();
        let size = Vec2::new(hi[0] - lo[0], hi[1] - lo[1]).max(Vec2::splat(1.0));
        self.zoom = ((rect.width() - 80.0) / size.x).min((rect.height() - 80.0) / size.y).clamp(0.08, 1.6);
        self.pan = -Vec2::new(f32::midpoint(lo[0], hi[0]), f32::midpoint(lo[1], hi[1]));
        self.fitted = true;
    }
}

fn radius(degree: usize, zoom: f32) -> f32 {
    (3.5 + (degree as f32).sqrt() * 1.8) * zoom.sqrt().clamp(0.6, 1.4)
}

/// Returns (close, opened a note).
pub(crate) fn show(ctx: &egui::Context, st: &mut GraphState, ws: &mut Workspace, pal: &Palette) -> (bool, bool) {
    st.refresh(ws);
    let (mut close, mut opened) = (false, false);
    let screen = ctx.content_rect();
    let frame_rect = screen.shrink2(Vec2::new(28.0, 24.0));
    let focus = ws.active_doc().and_then(|d| d.path.clone());
    egui::Area::new(egui::Id::unique("quire-graph")).order(egui::Order::Foreground).fixed_pos(frame_rect.min).show(ctx, |ui| {
        egui::Frame::window(ui.style()).fill(pal.bg).inner_margin(14).show(ui, |ui| {
            ui.set_width(frame_rect.width() - 28.0);
            ui.set_height(frame_rect.height() - 28.0);
            ui.horizontal(|ui| {
                ui.heading("Graph");
                ui.add_space(12.0);
                let has_note = focus.is_some();
                ui.add_enabled_ui(has_note, |ui| {
                    ui.selectable_value(&mut st.local, true, "Around this note");
                });
                ui.selectable_value(&mut st.local, false, "All notes");
                ui.add_space(8.0);
                if st.local && has_note {
                    ui.label(RichText::new("Links away").color(pal.muted).size(12.5));
                    for d in 1..=3 {
                        ui.selectable_value(&mut st.depth, d, d.to_string());
                    }
                } else {
                    ui.checkbox(&mut st.orphans, "Notes without links");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                    if ui.button("Fit").on_hover_text("Show the whole graph").clicked() {
                        st.fitted = false;
                        st.follow = true;
                    }
                    let n = st.graph.nodes.len();
                    let e = st.graph.edges.len();
                    ui.label(RichText::new(format!("{n} notes · {e} links")).color(pal.muted).size(12.5));
                });
            });
            ui.add_space(4.0);
            let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 8.0, pal.panel);
            if st.graph.nodes.is_empty() {
                let msg =
                    if st.local { "This note has no links yet." } else { "No linked notes yet. Link notes with [[double brackets]]." };
                painter.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(14.0), pal.muted);
                return;
            }
            if !st.fitted || (st.follow && !st.layout.settled()) {
                st.fit(rect);
            }

            // Pointer: hover, zoom around the pointer, drag a note or the view.
            let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
            let hovered = pointer.and_then(|p| {
                let mut best: Option<(usize, f32)> = None;
                for (i, n) in st.graph.nodes.iter().enumerate() {
                    let d = st.to_screen(rect, st.layout.pos[i]).distance(p);
                    if d <= radius(n.degree, st.zoom) + 4.0 && best.is_none_or(|(_, b)| d < b) {
                        best = Some((i, d));
                    }
                }
                best.map(|(i, _)| i)
            });
            if let Some(p) = pointer {
                let (scroll, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                let factor = zoom_delta * (scroll / 400.0).exp();
                if (factor - 1.0).abs() > 1e-4 {
                    let before = st.to_world(rect, p);
                    st.zoom = (st.zoom * factor).clamp(0.05, 4.0);
                    st.follow = false;
                    let after = st.to_world(rect, p);
                    st.pan += Vec2::new(after[0] - before[0], after[1] - before[1]);
                }
            }
            if resp.drag_started() {
                st.dragging = hovered;
                st.follow = false;
                st.layout.pinned = hovered;
            }
            if resp.dragged() {
                if let Some(i) = st.dragging {
                    if let Some(p) = pointer {
                        st.layout.pos[i] = st.to_world(rect, p);
                        st.layout.reheat(0.3);
                    }
                } else {
                    st.pan += resp.drag_delta() / st.zoom;
                }
            }
            if resp.drag_stopped() {
                st.dragging = None;
                st.layout.pinned = None;
            }
            if resp.clicked()
                && let Some(i) = hovered
            {
                let p = st.graph.nodes[i].path.clone();
                if ws.open_note(&p).is_some() {
                    opened = true;
                    close = true;
                }
            }
            if hovered.is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            // Simulate a little each frame until it settles, then stop repainting.
            if !st.layout.settled() {
                let start = Instant::now();
                while start.elapsed() < Duration::from_millis(6) && !st.layout.settled() {
                    st.layout.step(&st.graph);
                }
                ctx.request_repaint();
            }

            // Draw: links, then notes, then labels on top.
            let lit: Vec<usize> = hovered.map(|h| st.graph.neighbours(h).collect()).unwrap_or_default();
            let focus_i = focus.as_ref().and_then(|f| st.graph.nodes.iter().position(|n| n.path == *f));
            let dim = hovered.is_some();
            for &(a, b) in &st.graph.edges {
                let on = hovered.is_some_and(|h| h == a || h == b);
                let color = if on {
                    pal.accent.gamma_multiply(0.8)
                } else if dim {
                    pal.border.gamma_multiply(0.5)
                } else {
                    pal.faint.gamma_multiply(0.7)
                };
                painter.line_segment(
                    [st.to_screen(rect, st.layout.pos[a]), st.to_screen(rect, st.layout.pos[b])],
                    Stroke::new(if on { 1.6 } else { 1.0 }, color),
                );
            }
            // Small graphs: every title. Big ones: the hubs once zoomed in.
            let small = st.graph.nodes.len() <= 40;
            let zoomed = st.zoom > 0.75;
            for (i, n) in st.graph.nodes.iter().enumerate() {
                let c = st.to_screen(rect, st.layout.pos[i]);
                if !rect.expand(20.0).contains(c) {
                    continue;
                }
                let r = radius(n.degree, st.zoom);
                let is_focus = focus_i == Some(i);
                let near = hovered == Some(i) || lit.contains(&i);
                let fill = if is_focus || hovered == Some(i) {
                    pal.accent
                } else if near {
                    pal.accent.gamma_multiply(0.75)
                } else if dim || n.degree == 0 {
                    pal.faint
                } else {
                    pal.muted
                };
                if is_focus {
                    painter.circle_stroke(c, r + 3.5, Stroke::new(1.5, pal.accent.gamma_multiply(0.5)));
                }
                painter.circle_filled(c, r, fill);
                let label = hovered == Some(i) || is_focus || small || (!dim && zoomed && n.degree >= 2) || near;
                if label {
                    let color = if hovered == Some(i) || is_focus {
                        pal.strong
                    } else if dim && !near {
                        pal.faint
                    } else {
                        pal.text
                    };
                    let mut title = n.title.clone();
                    if title.chars().count() > 34 {
                        title = title.chars().take(32).collect::<String>() + "…";
                    }
                    let pos = c + Vec2::new(0.0, r + 3.0);
                    // A soft backing so labels stay readable over lines.
                    let galley = painter.layout_no_wrap(title, FontId::proportional(11.5), color);
                    let bg = Rect::from_center_size(pos + Vec2::new(0.0, galley.size().y / 2.0), galley.size() + Vec2::new(6.0, 2.0));
                    painter.rect_filled(bg, 3.0, pal.panel.gamma_multiply(0.85));
                    painter.galley(Pos2::new(pos.x - galley.size().x / 2.0, pos.y), galley, color);
                }
            }
            painter.text(
                rect.left_bottom() + Vec2::new(10.0, -8.0),
                Align2::LEFT_BOTTOM,
                "Scroll to zoom · drag to move · click a note to open it",
                FontId::proportional(11.0),
                pal.faint,
            );
        });
    });
    (close, opened)
}
