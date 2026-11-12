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
    /// 0.12: which links to draw: all, typed only, or one type.
    filter: EdgeFilter,
}

/// Which links the graph draws (0.12).
#[derive(Clone, PartialEq, Eq, Default)]
enum EdgeFilter {
    #[default]
    All,
    Typed,
    Type(String),
}

impl EdgeFilter {
    fn keeps(&self, t: Option<&String>) -> bool {
        match self {
            Self::All => true,
            Self::Typed => t.is_some(),
            Self::Type(x) => t == Some(x),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::All => "All links".into(),
            Self::Typed => "Typed links".into(),
            Self::Type(t) => t.clone(),
        }
    }
}

/// A steady colour for a link type, by the type's place in the graph's
/// list (most used first), from the theme's distinct hues.
fn type_color(g: &Graph, t: &str, pal: &Palette) -> egui::Color32 {
    // 0.13: a colour set in Settings → Links wins.
    if let Some([r, gr, b]) = g.type_colors.get(t) {
        return egui::Color32::from_rgb(*r, *gr, *b);
    }
    let hues = [pal.accent, pal.tag, pal.warn, pal.added, pal.danger, pal.link, pal.quote];
    let i = g.types.iter().position(|x| x == t).unwrap_or(0);
    hues[i % hues.len()]
}

/// Distance from `p` to the segment `a`–`b`.
fn to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
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
            filter: EdgeFilter::All,
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
        if let EdgeFilter::Type(t) = &self.filter
            && !g.types.contains(t)
        {
            self.filter = EdgeFilter::All;
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
                // 0.12: draw every link, the typed ones, or one type.
                if !st.graph.types.is_empty() {
                    ui.add_space(8.0);
                    let types = st.graph.types.clone();
                    egui::ComboBox::from_id_salt("quire-graph-edge-filter").selected_text(st.filter.label()).show_ui(ui, |ui| {
                        ui.selectable_value(&mut st.filter, EdgeFilter::All, "All links");
                        ui.selectable_value(&mut st.filter, EdgeFilter::Typed, "Typed links");
                        for t in types {
                            let label = RichText::new(format!("● {t}")).color(type_color(&st.graph, &t, pal));
                            ui.selectable_value(&mut st.filter, EdgeFilter::Type(t), label);
                        }
                    });
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
            // 0.12: the typed link under the pointer (when no note is), for its label.
            let mut edge_hover: Option<(usize, f32)> = None;
            let mut labels: Vec<(Pos2, String, egui::Color32)> = Vec::new();
            for (k, &(a, b)) in st.graph.edges.iter().enumerate() {
                let t = st.graph.edge_types.get(k).and_then(Option::as_ref);
                if !st.filter.keeps(t) {
                    continue;
                }
                let (pa, pb) = (st.to_screen(rect, st.layout.pos[a]), st.to_screen(rect, st.layout.pos[b]));
                let on = hovered.is_some_and(|h| h == a || h == b);
                let typed = t.map(|t| type_color(&st.graph, t, pal));
                let color = match (on, typed) {
                    (true, Some(c)) => c,
                    (true, None) => pal.accent.gamma_multiply(0.8),
                    (false, Some(c)) if !dim => c.gamma_multiply(0.75),
                    _ if dim => pal.border.gamma_multiply(0.5),
                    _ => pal.faint.gamma_multiply(0.7),
                };
                painter.line_segment(
                    [pa, pb],
                    Stroke::new(
                        if on {
                            1.6
                        } else if typed.is_some() {
                            1.3
                        } else {
                            1.0
                        },
                        color,
                    ),
                );
                if let Some(t) = t {
                    if on {
                        labels.push((pa.lerp(pb, 0.5), t.clone(), color));
                    } else if hovered.is_none()
                        && let Some(p) = pointer
                    {
                        let d = to_segment(p, pa, pb);
                        if d < 5.0 && edge_hover.is_none_or(|(_, best)| d < best) {
                            edge_hover = Some((k, d));
                        }
                    }
                }
            }
            if let Some((k, _)) = edge_hover
                && let Some(t) = st.graph.edge_types[k].as_ref()
            {
                let (a, b) = st.graph.edges[k];
                let (pa, pb) = (st.to_screen(rect, st.layout.pos[a]), st.to_screen(rect, st.layout.pos[b]));
                let color = type_color(&st.graph, t, pal);
                painter.line_segment([pa, pb], Stroke::new(2.0, color));
                let words = format!("{} · {} — {}", t, st.graph.nodes[a].title, st.graph.nodes[b].title);
                labels.push((pa.lerp(pb, 0.5), words, color));
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
            // Link type labels, over everything.
            for (at, text, color) in labels {
                let galley = painter.layout_no_wrap(text, FontId::proportional(11.0), color);
                let bg = Rect::from_center_size(at, galley.size() + Vec2::new(8.0, 4.0));
                painter.rect_filled(bg, 3.0, pal.panel);
                painter.rect_stroke(bg, 3.0, Stroke::new(1.0, color.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                painter.galley(at - galley.size() / 2.0, galley, color);
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
