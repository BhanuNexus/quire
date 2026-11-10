// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The map view's world (0.11): an offline land outline and the pins.
//!
//! The outline is Natural Earth's 1:110m land polygons (public domain),
//! bundled as 21 KB (`assets/README.md`). Nothing is fetched: no tiles, no
//! network. Pins come from notes' `lat`/`lng` or `location: [lat, lng]`.

use std::sync::OnceLock;

use quire_vault::NotePath;

const LAND: &[u8] = include_bytes!("../assets/ne_110m_land.bin");

/// Land rings as (longitude, latitude) in degrees.
pub fn land() -> &'static [Vec<(f32, f32)>] {
    static RINGS: OnceLock<Vec<Vec<(f32, f32)>>> = OnceLock::new();
    RINGS.get_or_init(|| decode(LAND).unwrap_or_default())
}

fn decode(b: &[u8]) -> Option<Vec<Vec<(f32, f32)>>> {
    let u32_at = |at: usize| b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
    let i16_at = |at: usize| b.get(at..at + 2).map(|s| i16::from_le_bytes([s[0], s[1]]));
    if b.get(..4)? != b"QNE1" {
        return None;
    }
    let rings = u32_at(4)?;
    let mut at = 8;
    let mut out = Vec::with_capacity(rings);
    for _ in 0..rings {
        let n = u32_at(at)?;
        at += 4;
        let mut ring = Vec::with_capacity(n);
        for _ in 0..n {
            let (x, y) = (i16_at(at)?, i16_at(at + 2)?);
            ring.push((f32::from(x) / 100.0, f32::from(y) / 100.0));
            at += 4;
        }
        out.push(ring);
    }
    (at == b.len()).then_some(out)
}

/// Points and triangle indices.
pub type Mesh = (Vec<(f32, f32)>, Vec<u32>);

/// The land as triangles (worked out once): points as (longitude,
/// latitude) and indices, three per triangle. The map's projection is
/// linear in each, so the triangles stay right on screen.
pub fn land_mesh() -> &'static Mesh {
    static MESH: OnceLock<Mesh> = OnceLock::new();
    MESH.get_or_init(|| {
        let mut pts = Vec::new();
        let mut idx = Vec::new();
        for ring in land() {
            let mut r: Vec<(f32, f32)> = ring.clone();
            if r.len() > 1 && r.first() == r.last() {
                r.pop();
            }
            let base = pts.len() as u32;
            for t in triangulate(&r) {
                idx.extend(t.iter().map(|i| base + *i as u32));
            }
            pts.extend(r);
        }
        (pts, idx)
    })
}

fn cross(o: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// Ear clipping of a simple polygon: triangles as indices into `poly`.
pub fn triangulate(poly: &[(f32, f32)]) -> Vec<[usize; 3]> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let area: f32 = (0..n).map(|i| cross((0.0, 0.0), poly[i], poly[(i + 1) % n])).sum();
    let mut v: Vec<usize> = if area >= 0.0 { (0..n).collect() } else { (0..n).rev().collect() };
    let mut out = Vec::with_capacity(n - 2);
    let inside = |p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        cross(a, b, p) >= 0.0 && cross(b, c, p) >= 0.0 && cross(c, a, p) >= 0.0
    };
    let mut guard = 0;
    while v.len() > 3 && guard < 2 * n {
        let m = v.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (v[(i + m - 1) % m], v[i], v[(i + 1) % m]);
            let (a, b, c) = (poly[ia], poly[ib], poly[ic]);
            if cross(a, b, c) <= 0.0 {
                continue;
            }
            let blocked = v
                .iter()
                .any(|&j| j != ia && j != ib && j != ic && poly[j] != a && poly[j] != b && poly[j] != c && inside(poly[j], a, b, c));
            if blocked {
                continue;
            }
            out.push([ia, ib, ic]);
            v.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            // Not simple here (a pinch point): fan out what's left.
            guard += 1;
            let first = v[0];
            for w in v[1..].windows(2) {
                out.push([first, w[0], w[1]]);
            }
            v.clear();
        }
    }
    if v.len() == 3 {
        out.push([v[0], v[1], v[2]]);
    }
    out
}

/// A note on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    pub path: NotePath,
    pub title: String,
    pub lat: f64,
    pub lng: f64,
}

/// The pins of a view's rows (notes without a place are left out).
pub fn pins(ws: &crate::Workspace, rows: &[quire_index::views::ViewRow]) -> Vec<Pin> {
    // 0.12: a row may be a note of an unlocked folder.
    let mut out: Vec<Pin> = ws.with_notes(|ix| {
        rows.iter()
            .filter_map(|r| {
                let rec = ix.get(&r.path)?;
                let (lat, lng) = quire_index::views::location(rec)?;
                Some(Pin { path: r.path.clone(), title: r.title.clone(), lat, lng })
            })
            .collect()
    });
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// The view that shows every pin with a margin: (west, south, east, north)
/// in degrees; the whole world without pins.
pub fn bounds(pins: &[Pin]) -> (f64, f64, f64, f64) {
    if pins.is_empty() {
        return (-180.0, -60.0, 180.0, 85.0);
    }
    let (mut w, mut s, mut e, mut n) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
    for p in pins {
        w = w.min(p.lng);
        e = e.max(p.lng);
        s = s.min(p.lat);
        n = n.max(p.lat);
    }
    // At least 20° across, and a fifth more on each side.
    let (cx, cy) = (f64::midpoint(w, e), f64::midpoint(s, n));
    let half_w = ((e - w) / 2.0 * 1.4).max(10.0);
    let half_h = ((n - s) / 2.0 * 1.4).max(6.0);
    ((cx - half_w).max(-180.0), (cy - half_h).max(-90.0), (cx + half_w).min(180.0), (cy + half_h).min(90.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outline_is_bundled_and_sane() {
        let rings = land();
        assert_eq!(rings.len(), 128);
        assert_eq!(rings.iter().map(Vec::len).sum::<usize>(), 5_129);
        assert!(rings.iter().flatten().all(|(x, y)| (-180.0..=180.0).contains(x) && (-90.0..=90.0).contains(y)));
        assert!(decode(b"QNE1\x01\x00\x00\x00").is_none(), "cut short");
        assert!(decode(b"NOPE").is_none());
    }

    #[test]
    fn land_triangles_cover_the_land() {
        let area =
            |r: &[(f32, f32)]| (0..r.len()).map(|i| f64::from(cross((0.0, 0.0), r[i], r[(i + 1) % r.len()]))).sum::<f64>().abs() / 2.0;
        let (pts, idx) = land_mesh();
        assert_eq!(idx.len() % 3, 0);
        let tri: f64 = idx.chunks(3).map(|t| area(&[pts[t[0] as usize], pts[t[1] as usize], pts[t[2] as usize]])).sum();
        let rings: f64 = land().iter().map(|r| area(&r[..r.len() - 1])).sum();
        assert!((tri - rings).abs() / rings < 0.01, "{tri} vs {rings}");
        // A concave L.
        let l = [(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (1.0, 1.0), (1.0, 2.0), (0.0, 2.0)];
        let t = triangulate(&l);
        assert_eq!(t.len(), 4);
        assert!((t.iter().map(|t| area(&[l[t[0]], l[t[1]], l[t[2]]])).sum::<f64>() - 3.0).abs() < 1e-6);
    }

    #[test]
    fn bounds_fit_the_pins() {
        let p = |lat, lng| Pin { path: NotePath::new("a.md"), title: String::new(), lat, lng };
        assert_eq!(bounds(&[]), (-180.0, -60.0, 180.0, 85.0));
        let (w, s, e, n) = bounds(&[p(38.7, -9.1), p(35.0, 135.8)]);
        assert!(w < -9.1 && e > 135.8 && s < 35.0 && n > 38.7);
        let (w, _, e, _) = bounds(&[p(51.5, -0.1)]);
        assert!((e - w - 20.0).abs() < 1e-9, "one pin: 20° across");
    }
}
