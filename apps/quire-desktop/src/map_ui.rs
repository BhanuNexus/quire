// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The map view (0.11): notes with a place as pins on an offline world
//! outline (Natural Earth 1:110m land, bundled). Drag to pan, scroll or
//! pinch (or + and −) to zoom, click a pin to open its note. Nothing is
//! fetched from the network.
//!
//! Owned by the 0.11 tasks workstream.

use std::fmt::Write as _;

use egui::{Color32, Id, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2};
use quire_app::Workspace;
use quire_app::geo::{self, Pin};
use quire_app::index::views::ViewResult;
use quire_app::vault::NotePath;

use crate::theme::Palette;

/// Where the map looks: centre (lng, lat) and degrees of longitude per point.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Camera {
    lng: f64,
    lat: f64,
    deg_per_px: f64,
}

/// Equirectangular, with latitude stretched by 1/cos(centre) so the area
/// in view looks right.
fn project(c: &Camera, rect: Rect, lng: f64, lat: f64) -> Pos2 {
    let k = 1.0 / c.lat.to_radians().cos().clamp(0.3, 1.0);
    let x = f64::from(rect.center().x) + (lng - c.lng) / c.deg_per_px;
    let y = f64::from(rect.center().y) - (lat - c.lat) * k / c.deg_per_px;
    Pos2::new(x as f32, y as f32)
}

fn unproject(c: &Camera, rect: Rect, p: Pos2) -> (f64, f64) {
    let k = 1.0 / c.lat.to_radians().cos().clamp(0.3, 1.0);
    let lng = c.lng + f64::from(p.x - rect.center().x) * c.deg_per_px;
    let lat = c.lat - f64::from(p.y - rect.center().y) * c.deg_per_px / k;
    (lng, lat)
}

/// The camera that shows `bounds` in `rect`.
fn fit(bounds: (f64, f64, f64, f64), rect: Rect) -> Camera {
    let (w, s, e, n) = bounds;
    let lat = f64::midpoint(s, n);
    let k = 1.0 / lat.to_radians().cos().clamp(0.3, 1.0);
    let dx = (e - w) / f64::from(rect.width().max(1.0));
    let dy = (n - s) * k / f64::from(rect.height().max(1.0));
    Camera { lng: f64::midpoint(w, e), lat, deg_per_px: dx.max(dy).max(1e-4) }
}

/// Draw the pins of `r`'s rows. Returns a note to open.
pub(crate) fn show(ui: &mut Ui, ws: &Workspace, pal: &Palette, r: &ViewResult, key: Id) -> Option<NotePath> {
    let pins = geo::pins(ws, &r.rows);
    let missing = r.rows.len().saturating_sub(pins.len());
    ui.horizontal(|ui| {
        let mut s = format!("Map · {} pin{}", pins.len(), if pins.len() == 1 { "" } else { "s" });
        if missing > 0 {
            let _ = write!(s, " · {missing} without a place (lat and lng, or location: [lat, lng])");
        }
        ui.label(RichText::new(s).size(11.0).color(pal.muted).strong());
    });
    map(ui, pal, &pins, key)
}

/// The map itself, over `pins`.
pub(crate) fn map(ui: &mut Ui, pal: &Palette, pins: &[Pin], key: Id) -> Option<NotePath> {
    let w = ui.available_width().max(200.0);
    let h = (w * 0.55).clamp(220.0, 520.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click_and_drag());
    let mem = key.with("camera");
    let mut cam: Camera = ui
        .data(|d| d.get_temp::<(f64, f64, f64)>(mem))
        .map_or_else(|| fit(geo::bounds(pins), rect), |(lng, lat, d)| Camera { lng, lat, deg_per_px: d });
    // Pan.
    if resp.dragged() {
        let d = resp.drag_delta();
        let (lng0, lat0) = unproject(&cam, rect, rect.center());
        let (lng1, lat1) = unproject(&cam, rect, rect.center() - d);
        cam.lng += lng1 - lng0;
        cam.lat = (cam.lat + lat1 - lat0).clamp(-80.0, 80.0);
    }
    // Zoom about the pointer.
    let mut zoom = 1.0f64;
    if resp.hovered() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        zoom *= f64::from(pinch);
        if scroll.abs() > 0.0 {
            zoom *= f64::from((scroll / 400.0).exp());
        }
    }
    let painter = ui.painter_at(rect);
    let sea = if pal.dark { Color32::from_rgb(0x15, 0x1d, 0x26) } else { Color32::from_rgb(0xe8, 0xf0, 0xf7) };
    let land_fill = if pal.dark { Color32::from_rgb(0x2a, 0x30, 0x36) } else { Color32::from_rgb(0xf6, 0xf3, 0xea) };
    let coast = if pal.dark { Color32::from_rgb(0x55, 0x60, 0x6b) } else { Color32::from_rgb(0xa8, 0xb4, 0xbf) };
    painter.rect_filled(rect, 6.0, sea);
    // Graticule every 30°.
    for g in (-150..=180).step_by(30) {
        let a = project(&cam, rect, f64::from(g), -85.0);
        let b = project(&cam, rect, f64::from(g), 85.0);
        painter.line_segment([a, b], Stroke::new(0.5, coast.gamma_multiply(0.35)));
    }
    for g in (-60..=60).step_by(30) {
        let a = project(&cam, rect, -180.0, f64::from(g));
        let b = project(&cam, rect, 180.0, f64::from(g));
        painter.line_segment([a, b], Stroke::new(0.5, coast.gamma_multiply(0.35)));
    }
    // Land: triangles (worked out once), then the coast.
    let (pts, idx) = geo::land_mesh();
    let mut mesh = egui::Mesh::default();
    for (x, y) in pts {
        mesh.colored_vertex(project(&cam, rect, f64::from(*x), f64::from(*y)), land_fill);
    }
    mesh.indices.clone_from(idx);
    painter.add(Shape::mesh(mesh));
    for ring in geo::land() {
        let pts: Vec<Pos2> = ring.iter().map(|(x, y)| project(&cam, rect, f64::from(*x), f64::from(*y))).collect();
        if Rect::from_points(&pts).intersects(rect) {
            painter.add(Shape::line(pts, Stroke::new(1.0, coast)));
        }
    }
    // Pins.
    let pointer = ui.ctx().pointer_hover_pos().filter(|p| rect.contains(*p));
    let mut hovered: Option<&Pin> = None;
    for p in pins {
        let at = project(&cam, rect, p.lng, p.lat);
        if !rect.expand(10.0).contains(at) {
            continue;
        }
        let near = pointer.is_some_and(|q| q.distance(at - Vec2::new(0.0, 9.0)) < 9.0);
        if near {
            hovered = Some(p);
        }
        let c = if near { pal.danger } else { pal.accent };
        painter.circle_filled(at - Vec2::new(0.0, 10.0), 6.5, c);
        painter.add(Shape::convex_polygon(vec![at, at - Vec2::new(4.5, 7.0), at - Vec2::new(-4.5, 7.0)], c, Stroke::NONE));
        painter.circle_filled(at - Vec2::new(0.0, 10.0), 2.5, pal.panel);
    }
    let mut open = None;
    if let Some(p) = hovered {
        let at = project(&cam, rect, p.lng, p.lat);
        let text = format!("{}  ({:.2}, {:.2})", p.title, p.lat, p.lng);
        let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), pal.text);
        let size = galley.size() + Vec2::new(10.0, 6.0);
        // Left of the pin when it would run off the map.
        let dx = if at.x + 10.0 + size.x > rect.right() { -10.0 - size.x } else { 10.0 };
        let bg = Rect::from_min_size(at + Vec2::new(dx, -34.0), size);
        painter.rect(bg, 4.0, pal.raised, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
        painter.galley(bg.min + Vec2::new(5.0, 3.0), galley, pal.text);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            open = Some(p.path.clone());
        }
    }
    // Zoom buttons, and the credit.
    let btn = |ui: &mut Ui, at: Pos2, label: &str| {
        let r = Rect::from_min_size(at - Vec2::new(10.0, 0.0), Vec2::new(32.0, 22.0));
        ui.put(r, egui::Button::new(RichText::new(label).size(11.0)).small()).clicked()
    };
    if btn(ui, rect.right_top() + Vec2::new(-30.0, 8.0), "+") {
        zoom *= 1.6;
    }
    if btn(ui, rect.right_top() + Vec2::new(-30.0, 34.0), "−") {
        zoom /= 1.6;
    }
    if btn(ui, rect.right_top() + Vec2::new(-30.0, 60.0), "fit") {
        cam = fit(geo::bounds(pins), rect);
    }
    painter.text(
        rect.left_bottom() + Vec2::new(6.0, -4.0),
        egui::Align2::LEFT_BOTTOM,
        "Made with Natural Earth",
        egui::FontId::proportional(10.0),
        pal.faint,
    );
    if (zoom - 1.0).abs() > 1e-6 {
        let anchor = pointer.unwrap_or_else(|| rect.center());
        let (alng, alat) = unproject(&cam, rect, anchor);
        cam.deg_per_px = (cam.deg_per_px / zoom).clamp(0.0005, 1.0);
        // Keep the point under the pointer still.
        let (blng, blat) = unproject(&cam, rect, anchor);
        cam.lng += alng - blng;
        cam.lat = (cam.lat + alat - blat).clamp(-80.0, 80.0);
    }
    ui.data_mut(|d| d.insert_temp(mem, (cam.lng, cam.lat, cam.deg_per_px)));
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_round_trips_and_fits() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(600.0, 300.0));
        let cam = fit((-10.0, 35.0, 140.0, 45.0), rect);
        for (lng, lat) in [(-9.1, 38.7), (135.8, 35.0), (0.0, 40.0)] {
            let p = project(&cam, rect, lng, lat);
            assert!(rect.expand(1.0).contains(p), "{lng},{lat} → {p:?}");
            let (x, y) = unproject(&cam, rect, p);
            assert!((x - lng).abs() < 1e-3 && (y - lat).abs() < 1e-3);
        }
    }
}
