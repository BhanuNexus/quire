// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Geometry for the canvas editor, without a UI toolkit: rectangles, edge
//! curves, hit tests, snapping, alignment and groups. Coordinates are canvas
//! units (y grows downwards, as in JSON Canvas).

use crate::model::{Canvas, Edge};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle between two corners, in any order.
    pub fn from_corners(a: (f32, f32), b: (f32, f32)) -> Self {
        Self::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
    }

    pub fn right(self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    pub fn center(self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 <= self.right() && p.1 >= self.y && p.1 <= self.bottom()
    }

    /// Whether `other` lies entirely inside.
    pub fn contains_rect(self, other: Self) -> bool {
        other.x >= self.x && other.y >= self.y && other.right() <= self.right() && other.bottom() <= self.bottom()
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x <= other.right() && other.x <= self.right() && self.y <= other.bottom() && other.y <= self.bottom()
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self::new(x, y, self.right().max(other.right()) - x, self.bottom().max(other.bottom()) - y)
    }

    #[must_use]
    pub fn expand(self, by: f32) -> Self {
        Self::new(self.x - by, self.y - by, self.w + 2.0 * by, self.h + 2.0 * by)
    }

    #[must_use]
    pub fn translate(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// A side of a node, where an edge attaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    pub const ALL: [Self; 4] = [Self::Top, Self::Right, Self::Bottom, Self::Left];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|side| side.name() == s)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
        }
    }

    /// The middle of this side of `r`.
    pub fn anchor(self, r: Rect) -> (f32, f32) {
        let (cx, cy) = r.center();
        match self {
            Self::Top => (cx, r.y),
            Self::Right => (r.right(), cy),
            Self::Bottom => (cx, r.bottom()),
            Self::Left => (r.x, cy),
        }
    }

    /// The outward direction.
    pub const fn normal(self) -> (f32, f32) {
        match self {
            Self::Top => (0.0, -1.0),
            Self::Right => (1.0, 0.0),
            Self::Bottom => (0.0, 1.0),
            Self::Left => (-1.0, 0.0),
        }
    }
}

/// The sides to connect two rectangles by when the file doesn't say:
/// facing sides along the axis where they are further apart.
pub fn auto_sides(from: Rect, to: Rect) -> (Side, Side) {
    let (a, b) = (from.center(), to.center());
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    if dx.abs() >= dy.abs() {
        if dx >= 0.0 { (Side::Right, Side::Left) } else { (Side::Left, Side::Right) }
    } else if dy >= 0.0 {
        (Side::Bottom, Side::Top)
    } else {
        (Side::Top, Side::Bottom)
    }
}

/// A cubic Bézier: start, two control points, end.
pub type Curve = [(f32, f32); 4];

/// The curve an edge is drawn along, leaving each node straight out of its side.
pub fn curve(from: Rect, from_side: Option<Side>, to: Rect, to_side: Option<Side>) -> Curve {
    let (auto_from, auto_to) = auto_sides(from, to);
    let fs = from_side.unwrap_or(auto_from);
    let ts = to_side.unwrap_or(auto_to);
    let p0 = fs.anchor(from);
    let p3 = ts.anchor(to);
    let dist = (p3.0 - p0.0).hypot(p3.1 - p0.1);
    let k = (dist * 0.5).clamp(30.0, 150.0);
    let (n0, n3) = (fs.normal(), ts.normal());
    [p0, (p0.0 + n0.0 * k, p0.1 + n0.1 * k), (p3.0 + n3.0 * k, p3.1 + n3.1 * k), p3]
}

/// The curve of `e` on `c`, if both its nodes exist.
pub fn edge_curve(c: &Canvas, e: &Edge) -> Option<Curve> {
    let from = c.node(&e.from_node)?.rect();
    let to = c.node(&e.to_node)?.rect();
    Some(curve(from, e.from_side.as_deref().and_then(Side::parse), to, e.to_side.as_deref().and_then(Side::parse)))
}

pub fn bezier_point(c: &Curve, t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, cc, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (a * c[0].0 + b * c[1].0 + cc * c[2].0 + d * c[3].0, a * c[0].1 + b * c[1].1 + cc * c[2].1 + d * c[3].1)
}

/// The curve as a polyline of `n + 1` points.
pub fn flatten(c: &Curve, n: usize) -> Vec<(f32, f32)> {
    (0..=n).map(|i| bezier_point(c, i as f32 / n.max(1) as f32)).collect()
}

/// Distance from `p` to the segment `a`–`b`.
pub fn dist_to_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f32::EPSILON { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
    (p.0 - qx).hypot(p.1 - qy)
}

/// Distance from `p` to a curve (sampled; good to a fraction of a unit).
pub fn dist_to_curve(c: &Curve, p: (f32, f32)) -> f32 {
    flatten(c, 24).windows(2).map(|w| dist_to_segment(p, w[0], w[1])).fold(f32::INFINITY, f32::min)
}

/// The direction of the curve at its end (for the arrow head), normalized.
pub fn end_direction(c: &Curve) -> (f32, f32) {
    let (dx, dy) = (c[3].0 - c[2].0, c[3].1 - c[2].1);
    let len = dx.hypot(dy);
    if len <= f32::EPSILON { (1.0, 0.0) } else { (dx / len, dy / len) }
}

/// Round `v` to the grid (`grid` ≤ 0 means no snapping).
pub fn snap(v: f32, grid: f32) -> f32 {
    if grid <= 0.0 { v } else { (v / grid).round() * grid }
}

/// The topmost node at `p` (last in z-order wins). Groups are hit only on
/// their label bar unless `groups_anywhere`, so nodes inside stay clickable.
pub fn node_at(c: &Canvas, p: (f32, f32), groups_anywhere: bool) -> Option<usize> {
    let hit = |i: &usize| {
        let n = &c.nodes[*i];
        let r = n.rect();
        if n.kind == "group" && !groups_anywhere {
            // The label sits above the group's top edge, as in Obsidian.
            return Rect::new(r.x, r.y - 30.0, r.w.min(320.0), 30.0).contains(p) || (r.contains(p) && !inside_child(c, *i, p));
        }
        r.contains(p)
    };
    let mut idx: Vec<usize> = (0..c.nodes.len()).collect();
    // Non-groups before groups, each topmost first.
    idx.sort_by_key(|i| (c.nodes[*i].kind == "group", std::cmp::Reverse(*i)));
    idx.into_iter().find(hit)
}

fn inside_child(c: &Canvas, group: usize, p: (f32, f32)) -> bool {
    let g = c.nodes[group].rect();
    c.nodes.iter().enumerate().any(|(i, n)| i != group && g.contains_rect(n.rect()) && n.rect().contains(p))
}

/// The edge whose curve passes within `tol` of `p`.
pub fn edge_at(c: &Canvas, p: (f32, f32), tol: f32) -> Option<usize> {
    c.edges
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((i, dist_to_curve(&edge_curve(c, e)?, p))))
        .filter(|(_, d)| *d <= tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Nodes inside group `g` (fully contained), by index.
pub fn group_members(c: &Canvas, g: usize) -> Vec<usize> {
    let r = c.nodes[g].rect();
    c.nodes.iter().enumerate().filter(|(i, n)| *i != g && r.contains_rect(n.rect())).map(|(i, _)| i).collect()
}

/// Nodes a marquee touches.
pub fn nodes_in(c: &Canvas, r: Rect) -> Vec<usize> {
    c.nodes.iter().enumerate().filter(|(_, n)| r.intersects(n.rect())).map(|(i, _)| i).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    CenterX,
    Right,
    Top,
    CenterY,
    Bottom,
}

/// Line up nodes `ids` (indices) on their common edge or centre.
pub fn align(c: &mut Canvas, ids: &[usize], how: Align) {
    let Some(b) = ids.iter().filter_map(|i| c.nodes.get(*i)).map(crate::model::Node::rect).reduce(Rect::union) else { return };
    for i in ids {
        let Some(n) = c.nodes.get_mut(*i) else { continue };
        let r = n.rect();
        let (x, y) = match how {
            Align::Left => (b.x, r.y),
            Align::CenterX => (b.center().0 - r.w / 2.0, r.y),
            Align::Right => (b.right() - r.w, r.y),
            Align::Top => (r.x, b.y),
            Align::CenterY => (r.x, b.center().1 - r.h / 2.0),
            Align::Bottom => (r.x, b.bottom() - r.h),
        };
        n.set_rect(Rect::new(x, y, r.w, r.h));
    }
}

/// Space nodes evenly between the first and last along an axis
/// (`horizontal` or vertical), keeping equal gaps.
pub fn distribute(c: &mut Canvas, ids: &[usize], horizontal: bool) {
    let mut ids: Vec<usize> = ids.iter().copied().filter(|i| *i < c.nodes.len()).collect();
    if ids.len() < 3 {
        return;
    }
    let key = |c: &Canvas, i: usize| if horizontal { c.nodes[i].rect().x } else { c.nodes[i].rect().y };
    ids.sort_by(|a, b| key(c, *a).total_cmp(&key(c, *b)));
    let size = |c: &Canvas, i: usize| if horizontal { c.nodes[i].rect().w } else { c.nodes[i].rect().h };
    let first = ids[0];
    let last = ids[ids.len() - 1];
    let start = key(c, first);
    let end = key(c, last) + size(c, last);
    let total: f32 = ids.iter().map(|i| size(c, *i)).sum();
    let gap = (end - start - total) / (ids.len() - 1) as f32;
    let mut at = start;
    for i in ids {
        let s = size(c, i);
        let n = &mut c.nodes[i];
        let r = n.rect();
        n.set_rect(if horizontal { Rect::new(at, r.y, r.w, r.h) } else { Rect::new(r.x, at, r.w, r.h) });
        at += s + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind};

    fn canvas() -> Canvas {
        let mut c = Canvas::default();
        c.nodes.push(Node::new("g", NodeKind::Group, 0, 0, 500, 300));
        c.nodes.push(Node::new("a", NodeKind::Text, 10, 10, 100, 50));
        c.nodes.push(Node::new("b", NodeKind::Text, 300, 200, 100, 50));
        c.nodes.push(Node::new("c", NodeKind::Text, 700, 0, 100, 50));
        c.edges.push(Edge::new("e", "a", "b"));
        c
    }

    #[test]
    fn hits_nodes_groups_and_edges() {
        let c = canvas();
        assert_eq!(node_at(&c, (20.0, 20.0), false), Some(1), "the text node, not its group");
        assert_eq!(node_at(&c, (200.0, 150.0), false), Some(0), "empty part of the group");
        assert_eq!(node_at(&c, (20.0, -10.0), false), Some(0), "the group's label bar");
        assert_eq!(node_at(&c, (650.0, 150.0), false), None);
        assert_eq!(group_members(&c, 0), vec![1, 2]);
        let curve = edge_curve(&c, &c.edges[0]).unwrap();
        assert_eq!(curve[0], (110.0, 35.0), "leaves a's right side");
        assert_eq!(curve[3], (300.0, 225.0), "enters b's left side");
        let mid = bezier_point(&curve, 0.5);
        assert_eq!(edge_at(&c, mid, 4.0), Some(0));
        assert_eq!(edge_at(&c, (600.0, 10.0), 4.0), None);
        assert_eq!(auto_sides(Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(0.0, 100.0, 10.0, 10.0)), (Side::Bottom, Side::Top));
        assert_eq!(nodes_in(&c, Rect::from_corners((750.0, -10.0), (290.0, 210.0))), vec![0, 2, 3]);
    }

    #[test]
    fn align_and_distribute() {
        let mut c = canvas();
        align(&mut c, &[1, 2, 3], Align::Top);
        assert!(c.nodes[1..].iter().all(|n| n.y == 0));
        align(&mut c, &[1, 2], Align::Right);
        assert_eq!((c.nodes[1].x, c.nodes[2].x), (300, 300));
        let mut c = canvas();
        c.nodes[2].x = 150;
        distribute(&mut c, &[3, 1, 2], true);
        assert_eq!((c.nodes[1].x, c.nodes[2].x, c.nodes[3].x), (10, 355, 700));
        assert!((snap(13.0, 20.0) - 20.0).abs() < f32::EPSILON);
        assert!((snap(13.0, 0.0) - 13.0).abs() < f32::EPSILON);
    }
}
