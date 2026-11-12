// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Drawing the scene in the editor: each element's shapes are tessellated
//! once into a mesh (per element version and zoom) and kept; a frame only
//! moves the meshes of the elements in view. While the zoom changes, the
//! kept meshes are scaled, and tessellated again once it rests.

use std::collections::HashMap;
use std::sync::Arc;

use egui::epaint::{Mesh, PathShape, PathStroke, Shape, TessellationOptions, Tessellator, TextShape, Vertex};
use egui::{Align2, Color32, FontFamily, FontId, Painter, Pos2, Rect, Stroke, TextureHandle, Vec2};
use quire_canvas::drawing::{self, El};
use quire_canvas::excalidraw::{self, Element, Op, Rgba, Seg};

/// One element, ready to draw.
pub(super) struct Cached {
    /// `version` and `versionNonce`: a change makes a new one.
    key: (u64, u64),
    pub el: Option<Element>,
    /// The axis-aligned box in scene units (with the stroke).
    pub aabb: Rect,
    ops: Vec<Op>,
    /// Paths tessellated at `zoom`, in scene units × `zoom`.
    mesh: Option<(f32, Arc<Mesh>)>,
}

/// The tessellated elements, by id.
#[derive(Default)]
pub(super) struct Cache {
    map: HashMap<String, Cached>,
}

fn key(e: &El) -> (u64, u64) {
    (e.num("version").unwrap_or(0.0).to_bits(), e.num("versionNonce").unwrap_or(0.0).to_bits() ^ e.num("updated").unwrap_or(0.0).to_bits())
}

fn color(c: Rgba) -> Color32 {
    let [r, g, b, a] = c.0;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// A font for Excalidraw's font family numbers.
pub(super) fn family(n: i64) -> FontFamily {
    match n {
        // Cascadia, Comic Shanns: code.
        3 | 8 => FontFamily::Monospace,
        _ => FontFamily::Proportional,
    }
}

impl Cache {
    /// The element's entry, made again when it changed.
    pub(super) fn get(&mut self, e: &El) -> Option<&mut Cached> {
        let id = e.str("id")?;
        let k = key(e);
        let fresh = self.map.get(id).is_none_or(|c| c.key != k);
        if fresh {
            let el = drawing::to_element(e);
            let mut ops = Vec::new();
            if let Some(el) = &el
                && el.kind != "text"
            {
                excalidraw::element_ops(el, &mut ops);
            }
            let (x, y, w, h) = drawing::aabb(e);
            let pad = e.num("strokeWidth").unwrap_or(2.0) as f32 + 12.0;
            let aabb = Rect::from_min_size(Pos2::new(x as f32, y as f32), Vec2::new(w as f32, h as f32)).expand(pad);
            self.map.insert(id.to_string(), Cached { key: k, el, aabb, ops, mesh: None });
        }
        self.map.get_mut(id)
    }

    /// Forget elements no longer in the drawing.
    pub(super) fn retain(&mut self, ids: &std::collections::HashSet<&str>) {
        if self.map.len() > ids.len() + 64 {
            self.map.retain(|k, _| ids.contains(k.as_str()));
        }
    }

    pub(super) fn clear(&mut self) {
        self.map.clear();
    }
}

/// Flatten a path into polylines (one per sub-path) in scene units × `z`.
fn flatten(segs: &[Seg], z: f32) -> Vec<(Vec<Pos2>, bool)> {
    let mut out: Vec<(Vec<Pos2>, bool)> = Vec::new();
    let mut cur: Vec<Pos2> = Vec::new();
    let p = |x: f64, y: f64| Pos2::new(x as f32 * z, y as f32 * z);
    for s in segs {
        match *s {
            Seg::Move(x, y) => {
                if cur.len() > 1 {
                    out.push((std::mem::take(&mut cur), false));
                }
                cur = vec![p(x, y)];
            }
            Seg::Line(x, y) => cur.push(p(x, y)),
            Seg::Cubic(a, b, c, d, x, y) => {
                let Some(&p0) = cur.last() else { continue };
                let (p1, p2, p3) = (p(a, b), p(c, d), p(x, y));
                let len = p0.distance(p1) + p1.distance(p2) + p2.distance(p3);
                let n = ((len / 6.0).ceil() as usize).clamp(2, 48);
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let u = 1.0 - t;
                    let q = p0.to_vec2() * (u * u * u)
                        + p1.to_vec2() * (3.0 * u * u * t)
                        + p2.to_vec2() * (3.0 * u * t * t)
                        + p3.to_vec2() * (t * t * t);
                    cur.push(q.to_pos2());
                }
            }
            Seg::Close => {
                if cur.len() > 1 {
                    if cur.first() == cur.last() {
                        cur.pop();
                    }
                    out.push((std::mem::take(&mut cur), true));
                }
            }
        }
    }
    if cur.len() > 1 {
        out.push((cur, false));
    }
    out
}

fn is_convex(pts: &[Pos2]) -> bool {
    let n = pts.len();
    if n < 4 {
        return true;
    }
    let mut sign = 0.0_f32;
    for i in 0..n {
        let (a, b, c) = (pts[i], pts[(i + 1) % n], pts[(i + 2) % n]);
        let cross = (b - a).x * (c - b).y - (b - a).y * (c - b).x;
        if cross.abs() > 1e-4 {
            if sign != 0.0 && (cross.signum() - sign).abs() > 1.0 {
                return false;
            }
            sign = cross.signum();
        }
    }
    true
}

/// A concave polygon's fill, by ear clipping (closed lines drawn by hand).
fn fill_concave(pts: &[Pos2], c: Color32, out: &mut Mesh) {
    let n = pts.len();
    if n < 3 {
        return;
    }
    let area: f32 = (0..n).map(|i| pts[i].x * pts[(i + 1) % n].y - pts[(i + 1) % n].x * pts[i].y).sum();
    let base = out.vertices.len() as u32;
    out.vertices.extend(pts.iter().map(|p| Vertex { pos: *p, uv: egui::epaint::WHITE_UV, color: c }));
    let mut idx: Vec<usize> = (0..n).collect();
    let ccw = area > 0.0;
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, cc) = (pts[ia], pts[ib], pts[ic]);
            let cross = (b - a).x * (cc - b).y - (b - a).y * (cc - b).x;
            if (cross > 0.0) != ccw || cross.abs() < 1e-6 {
                continue;
            }
            let inside = idx.iter().any(|&j| j != ia && j != ib && j != ic && in_triangle(pts[j], a, b, cc));
            if inside {
                continue;
            }
            out.indices.extend([base + ia as u32, base + ib as u32, base + ic as u32]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break;
        }
    }
    if idx.len() == 3 {
        out.indices.extend(idx.iter().map(|&i| base + i as u32));
    }
}

fn in_triangle(p: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    let d = |p1: Pos2, p2: Pos2, p3: Pos2| (p1.x - p3.x) * (p2.y - p3.y) - (p2.x - p3.x) * (p1.y - p3.y);
    let (d1, d2, d3) = (d(p, a, b), d(p, b, c), d(p, c, a));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// The element's paths as one mesh, at zoom `z` (positions: scene × `z`).
fn tessellate(ops: &[Op], z: f32, tess: &mut Tessellator) -> Mesh {
    let mut mesh = Mesh::default();
    for op in ops {
        let Op::Path { segs, fill, stroke, width, dash } = op else { continue };
        let lines = flatten(segs, z);
        if let Some(f) = fill {
            let c = color(*f);
            for (pts, closed) in &lines {
                if !*closed && pts.len() < 3 {
                    continue;
                }
                if is_convex(pts) {
                    tess.tessellate_path(&PathShape::convex_polygon(pts.clone(), c, PathStroke::NONE), &mut mesh);
                } else {
                    fill_concave(pts, c, &mut mesh);
                }
            }
        }
        if let Some(s) = stroke {
            let st = Stroke::new((*width as f32 * z).max(0.6), color(*s));
            for (pts, closed) in &lines {
                if dash.is_empty() {
                    let shape = if *closed { PathShape::closed_line(pts.clone(), st) } else { PathShape::line(pts.clone(), st) };
                    tess.tessellate_path(&shape, &mut mesh);
                } else {
                    let mut path = pts.clone();
                    if *closed && let Some(&f) = pts.first() {
                        path.push(f);
                    }
                    let (d, g) = (dash[0] as f32 * z, dash.get(1).copied().unwrap_or(dash[0]) as f32 * z);
                    for sh in Shape::dashed_line(&path, st, d.max(1.0), g.max(1.0)) {
                        tess.tessellate_shape(sh, &mut mesh);
                    }
                }
            }
        }
    }
    mesh
}

/// Append `src` to `dst`, scaled by `s` and moved by `o`.
fn append(dst: &mut Mesh, src: &Mesh, s: f32, o: Vec2) {
    let base = dst.vertices.len() as u32;
    dst.vertices.extend(src.vertices.iter().map(|v| Vertex { pos: (v.pos.to_vec2() * s + o).to_pos2(), uv: v.uv, color: v.color }));
    dst.indices.extend(src.indices.iter().map(|i| i + base));
}

/// The view: scene units to screen points.
#[derive(Clone, Copy)]
pub(super) struct View {
    pub rect: Rect,
    /// The scene point at the view's top-left corner.
    pub offset: Vec2,
    pub zoom: f32,
}

impl View {
    pub(super) fn to_screen(self, p: (f64, f64)) -> Pos2 {
        self.rect.min + (Vec2::new(p.0 as f32, p.1 as f32) - self.offset) * self.zoom
    }

    pub(super) fn to_scene(self, s: Pos2) -> (f64, f64) {
        let v = self.offset + (s - self.rect.min) / self.zoom;
        (f64::from(v.x), f64::from(v.y))
    }

    /// The scene area in view.
    pub(super) fn visible(self) -> Rect {
        Rect::from_min_size(self.offset.to_pos2(), self.rect.size() / self.zoom)
    }
}

/// What a frame drew (for tests and the frame-time readout).
#[derive(Default, Debug, Clone, Copy)]
pub(super) struct Drawn {
    pub elements: usize,
    pub tessellated: usize,
}

/// Draw the elements in view. `rest_zoom`: the zoom has settled, so meshes
/// at another zoom are made again (else they're scaled). `image`: a picture's
/// texture by file id. `faded`: elements drawn lighter (about to be erased).
pub(super) fn draw(
    painter: &Painter,
    els: &[El],
    cache: &mut Cache,
    view: View,
    rest_zoom: bool,
    image: &mut dyn FnMut(&str) -> Option<TextureHandle>,
    faded: &[String],
    skip: Option<&str>,
) -> Drawn {
    let ctx = painter.ctx();
    let mut tess = Tessellator::new(
        ctx.pixels_per_point(),
        TessellationOptions::default(),
        ctx.fonts(
            #[expect(clippy::redundant_closure_for_method_calls, reason = "the method's lifetime doesn't fit the closure's")]
            |f| f.font_image_size(),
        ),
        Vec::new(),
    );
    let visible = view.visible();
    let z = view.zoom;
    let origin = view.rect.min.to_vec2() - view.offset * z;
    let mut run = Mesh::default();
    let mut drawn = Drawn::default();
    let flush = |run: &mut Mesh| {
        if !run.is_empty() {
            painter.add(Shape::mesh(std::mem::take(run)));
        }
    };
    for e in drawing::live(els) {
        let Some(Cached { el, aabb, ops, mesh, .. }) = cache.get(e) else { continue };
        if !aabb.intersects(visible) {
            continue;
        }
        let Some(el) = el.as_ref() else { continue };
        if skip.is_some_and(|s| s == el.id) {
            continue;
        }
        drawn.elements += 1;
        let fade = faded.contains(&el.id);
        let needs = match mesh {
            None => true,
            Some((mz, _)) => rest_zoom && (*mz - z).abs() > f32::EPSILON,
        };
        if needs && ops.iter().any(|o| matches!(o, Op::Path { .. })) {
            *mesh = Some((z, Arc::new(tessellate(ops, z, &mut tess))));
            drawn.tessellated += 1;
        }
        if let Some((mz, m)) = mesh {
            let before = run.vertices.len();
            append(&mut run, m, z / *mz, origin);
            if fade {
                for v in &mut run.vertices[before..] {
                    v.color = v.color.gamma_multiply(0.3);
                }
            }
        }
        for op in ops.iter() {
            match op {
                Op::Text { lines, size, color: col, .. } => {
                    flush(&mut run);
                    for (t, x, y) in lines {
                        let pos = view.to_screen((*x, *y - size * 0.8));
                        painter.text(pos, Align2::LEFT_TOP, t, FontId::proportional((*size as f32 * z).max(1.0)), color(*col));
                    }
                }
                Op::Image { file_id, x, y, w, h } => {
                    flush(&mut run);
                    let r = Rect::from_min_max(view.to_screen((*x, *y)), view.to_screen((x + w, y + h)));
                    if let Some(tex) = image(file_id) {
                        let mut m = Mesh::with_texture(tex.id());
                        let tint = Color32::WHITE.gamma_multiply(el.opacity as f32 * if fade { 0.3 } else { 1.0 });
                        m.add_rect_with_uv(r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
                        if el.angle != 0.0 {
                            let c = r.center();
                            let (s, co) = (el.angle as f32).sin_cos();
                            for v in &mut m.vertices {
                                let d = v.pos - c;
                                v.pos = c + Vec2::new(d.x * co - d.y * s, d.x * s + d.y * co);
                            }
                        }
                        painter.add(Shape::mesh(m));
                    } else {
                        painter.rect_filled(r, 2.0, Color32::from_gray(225));
                        painter.text(r.center(), Align2::CENTER_CENTER, "picture", FontId::proportional(11.0), Color32::from_gray(120));
                    }
                }
                Op::Path { .. } => {}
            }
        }
        if el.kind == "text" {
            flush(&mut run);
            draw_text(painter, el, view, fade);
        }
    }
    flush(&mut run);
    drawn
}

/// A text element: each line at its place, rotated with the element.
pub(super) fn draw_text(painter: &Painter, el: &Element, view: View, fade: bool) {
    let z = view.zoom;
    let size = (el.font_size as f32 * z).max(1.0);
    let font = FontId::new(size, family(el.font_family));
    let col = color(el.stroke.unwrap_or(Rgba::BLACK)).gamma_multiply(el.opacity as f32 * if fade { 0.3 } else { 1.0 });
    let lh = (el.font_size * el.line_height.max(0.5)) as f32 * z;
    let n = el.text.lines().count().max(1) as f32;
    let (x, y, w, h) = (el.x as f32, el.y as f32, el.width as f32, el.height as f32);
    let top = match el.vertical_align.as_str() {
        "middle" => (h * z - lh * n) / 2.0,
        "bottom" => h * z - lh * n,
        _ => 0.0,
    };
    let c = view.to_screen(((x + w / 2.0).into(), (y + h / 2.0).into()));
    let corner = view.to_screen((x.into(), y.into()));
    let (s, co) = (el.angle as f32).sin_cos();
    let rot = |p: Pos2| -> Pos2 {
        let d = p - c;
        c + Vec2::new(d.x * co - d.y * s, d.x * s + d.y * co)
    };
    for (i, line) in el.text.lines().enumerate() {
        let galley = painter.layout_no_wrap(line.to_string(), font.clone(), col);
        let gw = galley.size().x;
        let lx = match el.text_align.as_str() {
            "center" => (w * z - gw) / 2.0,
            "right" => w * z - gw,
            _ => 0.0,
        };
        let ly = top + lh * i as f32 + (lh - galley.size().y) / 2.0;
        let pos = rot(corner + Vec2::new(lx, ly));
        painter.add(TextShape::new(pos, galley, col).with_angle(el.angle as f32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concave_fills_cover_the_polygon_once() {
        // An L shape.
        let pts = [(0.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0), (10.0, 20.0), (0.0, 20.0)].map(|(x, y)| Pos2::new(x, y));
        assert!(!is_convex(&pts));
        let mut m = Mesh::default();
        fill_concave(&pts, Color32::RED, &mut m);
        assert_eq!(m.indices.len(), 4 * 3, "n − 2 triangles");
        let area: f32 = m
            .indices
            .chunks(3)
            .map(|t| {
                let (a, b, c) = (m.vertices[t[0] as usize].pos, m.vertices[t[1] as usize].pos, m.vertices[t[2] as usize].pos);
                ((b - a).x * (c - a).y - (b - a).y * (c - a).x).abs() / 2.0
            })
            .sum();
        assert!((area - 300.0).abs() < 1e-3, "{area}");
        assert!(is_convex(&[Pos2::ZERO, Pos2::new(10.0, 0.0), Pos2::new(10.0, 10.0), Pos2::new(0.0, 10.0)]));
    }

    #[test]
    fn curves_flatten_into_lines() {
        let segs = [Seg::Move(0.0, 0.0), Seg::Cubic(0.0, 50.0, 100.0, 50.0, 100.0, 0.0), Seg::Close];
        let l = flatten(&segs, 2.0);
        assert_eq!(l.len(), 1);
        assert!(l[0].1, "closed");
        assert!(l[0].0.len() > 8);
        assert_eq!(l[0].0.last().copied(), Some(Pos2::new(200.0, 0.0)));
    }
}
