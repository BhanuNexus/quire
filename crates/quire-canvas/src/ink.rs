// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ink: hand-drawn strokes and shapes stored as SVG
//! (`attachments/<canvas>-ink-<n>.svg`, docs/formats/canvas.md).
//!
//! Each stroke is a filled `<path>` outline that any SVG viewer draws, with
//! the input points kept in `data-quire-points="x,y,p;…"` and the tool in
//! `data-quire-tool`, so Quire can edit strokes later. Shapes are plain SVG
//! elements marked `data-quire-shape`. Elements Quire didn't write are kept
//! as they are.
//!
//! Input goes through [`simplify`] (Ramer–Douglas–Peucker, ε 0.5) when a
//! stroke ends; drawing smooths the kept points with Catmull–Rom
//! ([`smooth`]) and builds a perfect-freehand-style outline whose width
//! follows the pressure ([`outline`]).

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Pen,
    Marker,
    Highlighter,
}

impl Tool {
    pub const ALL: [Self; 3] = [Self::Pen, Self::Marker, Self::Highlighter];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Pen => "pen",
            Self::Marker => "marker",
            Self::Highlighter => "highlighter",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The default width in canvas units.
    pub const fn default_size(self) -> f32 {
        match self {
            Self::Pen => 3.0,
            Self::Marker => 8.0,
            Self::Highlighter => 20.0,
        }
    }

    /// How much pressure thins the line (0 = not at all).
    pub const fn thinning(self) -> f32 {
        match self {
            Self::Pen => 0.6,
            Self::Marker | Self::Highlighter => 0.0,
        }
    }

    pub const fn opacity(self) -> f32 {
        match self {
            Self::Highlighter => 0.35,
            Self::Pen | Self::Marker => 1.0,
        }
    }
}

/// An input point: position and pen pressure (0–1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32, pressure: f32) -> Self {
        Self { x, y, pressure }
    }
}

/// `x,y,p;x,y,p…` with two decimals.
pub fn encode_points(points: &[Point]) -> String {
    points.iter().map(|p| format!("{:.2},{:.2},{:.2}", p.x, p.y, p.pressure)).collect::<Vec<_>>().join(";")
}

/// The inverse of [`encode_points`]; malformed points are skipped.
pub fn decode_points(s: &str) -> Vec<Point> {
    s.split(';')
        .filter_map(|t| {
            let mut it = t.split(',').map(|n| n.trim().parse::<f32>());
            Some(Point { x: it.next()?.ok()?, y: it.next()?.ok()?, pressure: it.next().and_then(Result::ok).unwrap_or(0.5) })
        })
        .filter(|p| p.x.is_finite() && p.y.is_finite())
        .collect()
}

/// Ramer–Douglas–Peucker: drop points within `epsilon` of the line through
/// their neighbours. The first and last points stay.
pub fn simplify(points: &[Point], epsilon: f32) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0, points.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        let (pa, pb) = ((points[a].x, points[a].y), (points[b].x, points[b].y));
        let mut far = (0.0, a);
        for (i, p) in points.iter().enumerate().take(b).skip(a + 1) {
            let d = crate::geom::dist_to_segment((p.x, p.y), pa, pb);
            if d > far.0 {
                far = (d, i);
            }
        }
        if far.0 > epsilon {
            keep[far.1] = true;
            stack.push((a, far.1));
            stack.push((far.1, b));
        }
    }
    points.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// Catmull–Rom through the points, `steps` segments between each pair
/// (pressure is interpolated linearly).
pub fn smooth(points: &[Point], steps: usize) -> Vec<Point> {
    if points.len() < 3 || steps < 2 {
        return points.to_vec();
    }
    let n = points.len();
    let at = |i: isize| points[i.clamp(0, n as isize - 1) as usize];
    let mut out = Vec::with_capacity(n * steps);
    for i in 0..n - 1 {
        let (p0, p1, p2, p3) = (at(i as isize - 1), at(i as isize), at(i as isize + 1), at(i as isize + 2));
        for s in 0..steps {
            let t = s as f32 / steps as f32;
            let (t2, t3) = (t * t, t * t * t);
            let f = |a: f32, b: f32, c: f32, d: f32| {
                0.5 * ((2.0 * b) + (-a + c) * t + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2 + (-a + 3.0 * b - 3.0 * c + d) * t3)
            };
            out.push(Point {
                x: f(p0.x, p1.x, p2.x, p3.x),
                y: f(p0.y, p1.y, p2.y, p3.y),
                pressure: p1.pressure + (p2.pressure - p1.pressure) * t,
            });
        }
    }
    out.push(points[n - 1]);
    out
}

/// The radius at a point: `size / 2`, thinned by low pressure.
fn radius(size: f32, thinning: f32, pressure: f32) -> f32 {
    let p = pressure.clamp(0.0, 1.0);
    (size * (0.5 - thinning * (0.5 - p))).max(size * 0.12).max(0.25)
}

/// One point of a stroke drawn as a ribbon: its centre, its left and right
/// edges and its radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RibbonPoint {
    pub center: (f32, f32),
    pub left: (f32, f32),
    pub right: (f32, f32),
    pub radius: f32,
}

/// A stroke as a ribbon (smoothed, repeated points dropped): what the
/// outline is made of, and what a UI draws as triangles plus round ends.
pub fn ribbon(points: &[Point], size: f32, thinning: f32) -> Vec<RibbonPoint> {
    let pts = smooth(points, 4);
    let mut p: Vec<Point> = Vec::with_capacity(pts.len());
    for q in pts {
        if p.last().is_none_or(|l: &Point| (l.x - q.x).hypot(l.y - q.y) > 0.01) {
            p.push(q);
        }
    }
    let n = p.len();
    (0..n)
        .map(|i| {
            let a = p[i.saturating_sub(1)];
            let b = p[(i + 1).min(n - 1)];
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let len = dx.hypot(dy);
            let (nx, ny) = if len > f32::EPSILON { (-dy / len, dx / len) } else { (0.0, 1.0) };
            let r = radius(size, thinning, p[i].pressure);
            RibbonPoint {
                center: (p[i].x, p[i].y),
                left: (p[i].x + nx * r, p[i].y + ny * r),
                right: (p[i].x - nx * r, p[i].y - ny * r),
                radius: r,
            }
        })
        .collect()
}

/// The filled outline of a stroke (a closed polygon), in the style of
/// perfect-freehand: each point offset left and right by its radius, with
/// round caps. The points are smoothed first.
pub fn outline(points: &[Point], size: f32, thinning: f32) -> Vec<(f32, f32)> {
    let rib = ribbon(points, size, thinning);
    let (Some(first), Some(last)) = (rib.first(), rib.last()) else { return Vec::new() };
    // Half a circle from angle `from`, turning towards the stroke's outside.
    let cap = |c: (f32, f32), r: f32, from: f32, out: &mut Vec<(f32, f32)>| {
        for k in 0..=8 {
            let a = from - std::f32::consts::PI * k as f32 / 8.0;
            out.push((c.0 + r * a.cos(), c.1 + r * a.sin()));
        }
    };
    let angle = |p: &RibbonPoint| (p.left.1 - p.center.1).atan2(p.left.0 - p.center.0);
    if rib.len() == 1 {
        let mut out = Vec::new();
        cap(first.center, first.radius, 0.0, &mut out);
        cap(first.center, first.radius, std::f32::consts::PI, &mut out);
        return out;
    }
    let mut out = Vec::with_capacity(2 * rib.len() + 18);
    out.extend(rib.iter().map(|p| p.left));
    // End cap: from the left edge round the tip to the right edge.
    cap(last.center, last.radius, angle(last), &mut out);
    out.extend(rib.iter().rev().map(|p| p.right));
    cap(first.center, first.radius, angle(first) + std::f32::consts::PI, &mut out);
    out
}

/// SVG path data for a closed outline: quadratic curves through midpoints.
pub fn path_data(poly: &[(f32, f32)]) -> String {
    let Some(&(x0, y0)) = poly.first() else { return String::new() };
    let mut d = format!("M{x0:.2} {y0:.2}");
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        let _ = write!(d, " Q{:.2} {:.2} {:.2} {:.2}", a.0, a.1, f32::midpoint(a.0, b.0), f32::midpoint(a.1, b.1));
    }
    d.push('Z');
    d
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub tool: Tool,
    /// `#rrggbb`.
    pub color: String,
    pub size: f32,
    pub points: Vec<Point>,
}

impl Stroke {
    pub fn outline(&self) -> Vec<(f32, f32)> {
        outline(&self.points, self.size, self.tool.thinning())
    }

    /// Whether a circle at `p` of radius `r` touches the stroke.
    pub fn hit(&self, p: (f32, f32), r: f32) -> bool {
        let reach = r + self.size / 2.0;
        match self.points.as_slice() {
            [] => false,
            [only] => (only.x - p.0).hypot(only.y - p.1) <= reach,
            pts => pts.windows(2).any(|w| crate::geom::dist_to_segment(p, (w[0].x, w[0].y), (w[1].x, w[1].y)) <= reach),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Rect,
    Ellipse,
    Line,
    Arrow,
}

impl ShapeKind {
    pub const ALL: [Self; 4] = [Self::Rect, Self::Ellipse, Self::Line, Self::Arrow];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Rect => "rect",
            Self::Ellipse => "ellipse",
            Self::Line => "line",
            Self::Arrow => "arrow",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// A shape between two corners (rect, ellipse) or two ends (line, arrow).
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub kind: ShapeKind,
    pub from: (f32, f32),
    pub to: (f32, f32),
    pub color: String,
    pub width: f32,
}

impl Shape {
    /// The two lines of an arrow's head at `to`.
    pub fn arrow_head(&self) -> [(f32, f32); 2] {
        let (dx, dy) = (self.to.0 - self.from.0, self.to.1 - self.from.1);
        let len = dx.hypot(dy).max(f32::EPSILON);
        let (ux, uy) = (dx / len, dy / len);
        let k = (4.0 * self.width).max(10.0);
        let back = (self.to.0 - ux * k, self.to.1 - uy * k);
        [(back.0 - uy * k * 0.5, back.1 + ux * k * 0.5), (back.0 + uy * k * 0.5, back.1 - ux * k * 0.5)]
    }

    pub fn hit(&self, p: (f32, f32), r: f32) -> bool {
        let reach = r + self.width / 2.0;
        let rect = crate::geom::Rect::from_corners(self.from, self.to);
        match self.kind {
            ShapeKind::Line | ShapeKind::Arrow => crate::geom::dist_to_segment(p, self.from, self.to) <= reach,
            ShapeKind::Rect => {
                let c = [(rect.x, rect.y), (rect.right(), rect.y), (rect.right(), rect.bottom()), (rect.x, rect.bottom())];
                (0..4).any(|i| crate::geom::dist_to_segment(p, c[i], c[(i + 1) % 4]) <= reach)
            }
            ShapeKind::Ellipse => {
                let (cx, cy) = rect.center();
                let (rx, ry) = ((rect.w / 2.0).max(0.5), (rect.h / 2.0).max(0.5));
                let k = ((p.0 - cx) / rx).hypot((p.1 - cy) / ry);
                (k - 1.0).abs() * rx.min(ry) <= reach
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Stroke(Stroke),
    Shape(Shape),
    /// An element Quire didn't write, kept verbatim.
    Raw(String),
}

impl Item {
    /// Bounds of what it draws (raw elements have none).
    pub fn bounds(&self) -> Option<crate::geom::Rect> {
        use crate::geom::Rect;
        match self {
            Self::Stroke(s) => {
                let r = s.size / 2.0;
                s.points.iter().map(|p| Rect::new(p.x - r, p.y - r, 2.0 * r, 2.0 * r)).reduce(Rect::union)
            }
            Self::Shape(s) => {
                let pad = s.width / 2.0 + if s.kind == ShapeKind::Arrow { (4.0 * s.width).max(10.0) } else { 0.0 };
                Some(Rect::from_corners(s.from, s.to).expand(pad))
            }
            Self::Raw(_) => None,
        }
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        match self {
            Self::Stroke(s) => s.points.iter_mut().for_each(|p| {
                p.x += dx;
                p.y += dy;
            }),
            Self::Shape(s) => {
                s.from = (s.from.0 + dx, s.from.1 + dy);
                s.to = (s.to.0 + dx, s.to.1 + dy);
            }
            Self::Raw(_) => {}
        }
    }

    pub fn hit(&self, p: (f32, f32), r: f32) -> bool {
        match self {
            Self::Stroke(s) => s.hit(p, r),
            Self::Shape(s) => s.hit(p, r),
            Self::Raw(_) => false,
        }
    }
}

/// A sketch: what one ink SVG holds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sketch {
    pub width: f32,
    pub height: f32,
    pub items: Vec<Item>,
}

/// The default ink colour (drawn light on a dark theme).
pub const INK: &str = "#1e1e1e";

impl Sketch {
    /// The bounds of everything drawn.
    pub fn content_bounds(&self) -> Option<crate::geom::Rect> {
        self.items.iter().filter_map(Item::bounds).reduce(crate::geom::Rect::union)
    }

    /// Grow (or shrink) the drawing area to its content plus `pad`, moving
    /// everything so it starts at `pad`. Returns how far the content moved,
    /// which is how far the node's position moves the other way.
    pub fn fit(&mut self, pad: f32) -> (f32, f32) {
        let Some(b) = self.content_bounds() else { return (0.0, 0.0) };
        let (dx, dy) = (pad - b.x, pad - b.y);
        for i in &mut self.items {
            i.translate(dx, dy);
        }
        self.width = (b.w + 2.0 * pad).ceil();
        self.height = (b.h + 2.0 * pad).ceil();
        (dx, dy)
    }
}

fn unesc(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&apos;", "'").replace("&amp;", "&")
}

/// A colour attribute value, or the default ink.
fn color_attr(c: &str) -> String {
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok { c.to_ascii_lowercase() } else { INK.into() }
}

/// The SVG file for a sketch.
pub fn to_svg(s: &Sketch) -> String {
    let (w, h) = (s.width.max(1.0), s.height.max(1.0));
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" data-quire-sketch=\"1\">\n"
    );
    for item in &s.items {
        match item {
            Item::Stroke(st) => {
                let color = color_attr(&st.color);
                let opacity = st.tool.opacity();
                let op = if opacity < 1.0 { format!(" fill-opacity=\"{opacity}\"") } else { String::new() };
                let _ = writeln!(
                    out,
                    "<path d=\"{}\" fill=\"{color}\"{op} data-quire-tool=\"{}\" data-quire-size=\"{}\" data-quire-points=\"{}\"/>",
                    path_data(&st.outline()),
                    st.tool.name(),
                    st.size,
                    encode_points(&st.points)
                );
            }
            Item::Shape(sh) => {
                let color = color_attr(&sh.color);
                let r = crate::geom::Rect::from_corners(sh.from, sh.to);
                let common = format!(
                    "fill=\"none\" stroke=\"{color}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" data-quire-shape=\"{}\" data-quire-coords=\"{:.2},{:.2},{:.2},{:.2}\"",
                    sh.width,
                    sh.kind.name(),
                    sh.from.0,
                    sh.from.1,
                    sh.to.0,
                    sh.to.1
                );
                let _ = match sh.kind {
                    ShapeKind::Rect => {
                        writeln!(out, "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" {common}/>", r.x, r.y, r.w, r.h)
                    }
                    ShapeKind::Ellipse => {
                        let (cx, cy) = r.center();
                        writeln!(out, "<ellipse cx=\"{cx:.2}\" cy=\"{cy:.2}\" rx=\"{:.2}\" ry=\"{:.2}\" {common}/>", r.w / 2.0, r.h / 2.0)
                    }
                    ShapeKind::Line => writeln!(
                        out,
                        "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" {common}/>",
                        sh.from.0, sh.from.1, sh.to.0, sh.to.1
                    ),
                    ShapeKind::Arrow => {
                        let [a, b] = sh.arrow_head();
                        writeln!(
                            out,
                            "<path d=\"M{:.2} {:.2} L{:.2} {:.2} M{:.2} {:.2} L{:.2} {:.2} L{:.2} {:.2}\" {common}/>",
                            sh.from.0, sh.from.1, sh.to.0, sh.to.1, a.0, a.1, sh.to.0, sh.to.1, b.0, b.1
                        )
                    }
                };
            }
            Item::Raw(raw) => {
                out.push_str(raw.trim());
                out.push('\n');
            }
        }
    }
    out.push_str("</svg>\n");
    out
}

/// A tag found by the scanner.
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, String)>,
    closing: bool,
    self_closing: bool,
    start: usize,
    end: usize,
}

impl Tag<'_> {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str())
    }

    fn num(&self, k: &str) -> Option<f32> {
        self.attr(k).and_then(|v| v.trim().trim_end_matches("px").parse().ok()).filter(|v: &f32| v.is_finite())
    }
}

/// The next tag at or after `from`; comments, declarations and processing
/// instructions are skipped. `None` at the end or on broken markup.
fn next_tag(s: &str, from: usize) -> Option<Tag<'_>> {
    let mut at = from;
    loop {
        let lt = at + s.get(at..)?.find('<')?;
        let rest = &s[lt..];
        if rest.starts_with("<!--") {
            at = lt + rest.find("-->")? + 3;
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            at = lt + rest.find('>')? + 1;
            continue;
        }
        let closing = rest.starts_with("</");
        let body_start = lt + if closing { 2 } else { 1 };
        // Find the end of the tag, skipping `>` inside quoted values.
        let mut quote = None;
        let mut gt = None;
        for (i, ch) in s[body_start..].char_indices() {
            match (quote, ch) {
                (Some(q), c) if c == q => quote = None,
                (None, '"' | '\'') => quote = Some(ch),
                (None, '>') => {
                    gt = Some(body_start + i);
                    break;
                }
                _ => {}
            }
        }
        let gt = gt?;
        let inner = &s[body_start..gt];
        let self_closing = inner.trim_end().ends_with('/');
        let inner = inner.trim_end().trim_end_matches('/');
        let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
        let name = &inner[..name_end];
        let mut attrs = Vec::new();
        let mut rest = inner[name_end..].trim_start();
        while let Some(eq) = rest.find('=') {
            let key = rest[..eq].trim();
            let after = rest[eq + 1..].trim_start();
            let q = after.chars().next()?;
            if q != '"' && q != '\'' {
                return None;
            }
            let close = after[1..].find(q)? + 1;
            attrs.push((key, unesc(&after[1..close])));
            rest = after[close + 1..].trim_start();
        }
        return Some(Tag { name, attrs, closing, self_closing, start: lt, end: gt + 1 });
    }
}

/// Read a sketch. Elements Quire doesn't recognise are kept as [`Item::Raw`].
pub fn parse_svg(text: &str) -> Result<Sketch, String> {
    let mut at = 0;
    let root = loop {
        let t = next_tag(text, at).ok_or("not an SVG file")?;
        at = t.end;
        if t.name == "svg" && !t.closing {
            break t;
        }
    };
    let view: Vec<f32> = root.attr("viewBox").map(|v| v.split([' ', ',']).filter_map(|n| n.parse().ok()).collect()).unwrap_or_default();
    let mut sketch = Sketch {
        width: root.num("width").or_else(|| view.get(2).copied()).unwrap_or(400.0),
        height: root.num("height").or_else(|| view.get(3).copied()).unwrap_or(300.0),
        items: Vec::new(),
    };
    if root.self_closing {
        return Ok(sketch);
    }
    loop {
        let Some(t) = next_tag(text, at) else { return Err("the SVG file is cut off".into()) };
        if t.closing {
            if t.name == "svg" {
                return Ok(sketch);
            }
            at = t.end;
            continue;
        }
        // The element's full text, including children.
        let end = if t.self_closing { t.end } else { element_end(text, &t).ok_or("the SVG file is cut off")? };
        let raw = || Item::Raw(text[t.start..end].to_string());
        let item = if let Some(tool) = t.attr("data-quire-tool").and_then(Tool::from_name) {
            let points = decode_points(t.attr("data-quire-points").unwrap_or_default());
            if points.is_empty() {
                raw()
            } else {
                Item::Stroke(Stroke {
                    tool,
                    color: color_attr(t.attr("fill").unwrap_or(INK)),
                    size: t.num("data-quire-size").unwrap_or_else(|| tool.default_size()),
                    points,
                })
            }
        } else if let Some(kind) = t.attr("data-quire-shape").and_then(ShapeKind::from_name) {
            let c: Vec<f32> = t.attr("data-quire-coords").unwrap_or_default().split(',').filter_map(|n| n.trim().parse().ok()).collect();
            match c.as_slice() {
                [x0, y0, x1, y1] => Item::Shape(Shape {
                    kind,
                    from: (*x0, *y0),
                    to: (*x1, *y1),
                    color: color_attr(t.attr("stroke").unwrap_or(INK)),
                    width: t.num("stroke-width").unwrap_or(2.0),
                }),
                _ => raw(),
            }
        } else {
            raw()
        };
        sketch.items.push(item);
        at = end;
    }
}

/// Where element `t`'s closing tag ends (nested elements of the same name counted).
fn element_end(s: &str, t: &Tag<'_>) -> Option<usize> {
    let mut depth = 1;
    let mut at = t.end;
    while depth > 0 {
        let n = next_tag(s, at)?;
        if n.name == t.name {
            if n.closing {
                depth -= 1;
            } else if !n.self_closing {
                depth += 1;
            }
        }
        at = n.end;
    }
    Some(at)
}

/// Samples per pixel side when rasterizing.
const S: usize = 4;

/// A polygon to fill: its points, colour and opacity.
type Fill = (Vec<(f32, f32)>, (u8, u8, u8), f32);

/// A thick line from `a` to `b` as a quad.
fn segment(a: (f32, f32), b: (f32, f32), w: f32) -> Vec<(f32, f32)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy).max(1e-3);
    let (nx, ny) = (-dy / len * w / 2.0, dx / len * w / 2.0);
    vec![(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)]
}

fn rgb(c: &str) -> (u8, u8, u8) {
    let h = c.trim_start_matches('#');
    let ch = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(30);
    (ch(0), ch(2), ch(4))
}

/// What a sketch fills, in sketch units.
fn fills(sk: &Sketch) -> Vec<Fill> {
    let mut out = Vec::new();
    for item in &sk.items {
        match item {
            Item::Stroke(s) => out.push((s.outline(), rgb(&s.color), s.tool.opacity())),
            Item::Shape(sh) => {
                let c = rgb(&sh.color);
                let r = crate::geom::Rect::from_corners(sh.from, sh.to);
                let lines: Vec<((f32, f32), (f32, f32))> = match sh.kind {
                    ShapeKind::Line => vec![(sh.from, sh.to)],
                    ShapeKind::Arrow => {
                        let [h1, h2] = sh.arrow_head();
                        vec![(sh.from, sh.to), (h1, sh.to), (h2, sh.to)]
                    }
                    ShapeKind::Rect => {
                        let p = [(r.x, r.y), (r.right(), r.y), (r.right(), r.bottom()), (r.x, r.bottom())];
                        (0..4).map(|i| (p[i], p[(i + 1) % 4])).collect()
                    }
                    ShapeKind::Ellipse => {
                        let (cx, cy) = r.center();
                        let pt = |k: usize| {
                            let t = std::f32::consts::TAU * k as f32 / 64.0;
                            (cx + r.w / 2.0 * t.cos(), cy + r.h / 2.0 * t.sin())
                        };
                        (0..64).map(|k| (pt(k), pt(k + 1))).collect()
                    }
                };
                for (a, b) in lines {
                    out.push((segment(a, b, sh.width), c, 1.0));
                }
            }
            Item::Raw(_) => {}
        }
    }
    out
}

/// Draw a sketch into RGBA pixels on white, `scale` pixels per unit
/// (anti-aliased, 4 × 4 samples a pixel). For showing sketches as pictures
/// (embedded in notes); the size is capped at 2,000 pixels a side.
pub fn rasterize(sk: &Sketch, scale: f32) -> (u32, u32, Vec<u8>) {
    let scale = scale.min(2000.0 / sk.width.max(sk.height).max(1.0)).max(0.01);
    let w = ((sk.width * scale).ceil() as u32).clamp(1, 2000);
    let h = ((sk.height * scale).ceil() as u32).clamp(1, 2000);
    let mut px = vec![255u8; (w * h * 4) as usize];
    let mut cover = vec![0u8; w as usize];
    for (poly, rgb, opacity) in fills(sk) {
        let rgb: [u8; 3] = rgb.into();
        if poly.len() < 3 {
            continue;
        }
        let pts: Vec<(f32, f32)> = poly.iter().map(|p| (p.0 * scale, p.1 * scale)).collect();
        let y0 = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor().max(0.0) as u32;
        let y1 = (pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil().max(0.0) as u32).min(h);
        for y in y0..y1 {
            cover.fill(0);
            let mut touched = false;
            for sy in 0..S {
                let fy = y as f32 + (sy as f32 + 0.5) / S as f32;
                // Crossings with their winding direction (non-zero rule).
                let mut xs: Vec<(f32, i32)> = Vec::new();
                for i in 0..pts.len() {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    if (a.1 <= fy) != (b.1 <= fy) {
                        let t = (fy - a.1) / (b.1 - a.1);
                        xs.push((a.0 + t * (b.0 - a.0), if b.1 > a.1 { 1 } else { -1 }));
                    }
                }
                xs.sort_by(|p, q| p.0.total_cmp(&q.0));
                let mut wind = 0;
                for k in 0..xs.len().saturating_sub(1) {
                    wind += xs[k].1;
                    if wind == 0 {
                        continue;
                    }
                    let (sx0, sx1) = (
                        (xs[k].0 * S as f32).round().max(0.0) as usize,
                        ((xs[k + 1].0 * S as f32).round().max(0.0) as usize).min(w as usize * S),
                    );
                    for sx in sx0..sx1 {
                        cover[sx / S] = cover[sx / S].saturating_add(1);
                        touched = true;
                    }
                }
            }
            if !touched {
                continue;
            }
            for (x, c) in cover.iter().enumerate() {
                if *c == 0 {
                    continue;
                }
                let a = opacity * f32::from(*c) / (S * S) as f32;
                let i = ((y * w) as usize + x) * 4;
                for (k, v) in rgb.into_iter().enumerate() {
                    let old = f32::from(px[i + k]);
                    px[i + k] = (old + (f32::from(v) - old) * a.min(1.0)).round() as u8;
                }
            }
        }
    }
    (w, h, px)
}

/// An empty sketch file (for "Insert sketch").
pub fn empty_svg(width: f32, height: f32) -> String {
    to_svg(&Sketch { width, height, items: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_round_trip() {
        let pts = [Point { x: 1.0, y: 2.5, pressure: 0.3 }, Point { x: -4.25, y: 0.0, pressure: 1.0 }];
        let s = encode_points(&pts);
        assert_eq!(s, "1.00,2.50,0.30;-4.25,0.00,1.00");
        assert_eq!(decode_points(&s), pts);
        assert_eq!(decode_points("1,2;bad;3,4,0.9").len(), 2, "pressure is optional, junk is skipped");
        assert_eq!(Tool::from_name("marker"), Some(Tool::Marker));
    }

    #[test]
    fn simplify_keeps_corners_and_drops_noise() {
        let mut line: Vec<Point> = (0..=100).map(|i| Point::new(i as f32, if i % 2 == 0 { 0.0 } else { 0.2 }, 0.5)).collect();
        line.push(Point::new(100.0, 50.0, 0.5));
        let s = simplify(&line, 0.5);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!((s[1].x, s[1].y), (100.0, 0.0), "the corner stays");
        let sm = smooth(&s, 4);
        assert_eq!(sm.len(), 9);
        assert_eq!(sm[0], s[0]);
        assert_eq!(*sm.last().unwrap(), s[2]);
    }

    #[test]
    fn outline_width_follows_pressure() {
        let pts: Vec<Point> = (0..=10).map(|i| Point::new(i as f32 * 10.0, 0.0, if i < 5 { 0.1 } else { 1.0 })).collect();
        let poly = outline(&pts, 10.0, 0.6);
        let half = |x: f32| poly.iter().filter(|p| (p.0 - x).abs() < 1.0).map(|p| p.1.abs()).fold(0.0, f32::max);
        assert!(half(20.0) < half(80.0), "light pressure is thinner: {} vs {}", half(20.0), half(80.0));
        assert!((half(80.0) - 8.0).abs() < 0.6, "full pressure is 0.8 × size, as in perfect-freehand");
        let even: Vec<Point> = (0..=10).map(|i| Point::new(i as f32 * 10.0, 0.0, 0.5)).collect();
        let poly2 = outline(&even, 10.0, 0.6);
        let max = poly2.iter().filter(|p| (p.0 - 50.0).abs() < 1.0).map(|p| p.1.abs()).fold(0.0, f32::max);
        assert!((max - 5.0).abs() < 0.01, "a mouse (constant pressure) draws size/2: {max}");
        let dot = outline(&[Point::new(5.0, 5.0, 0.5)], 4.0, 0.0);
        assert!(dot.iter().all(|p| ((p.0 - 5.0).hypot(p.1 - 5.0) - 2.0).abs() < 0.01), "a tap is a dot");
        let d = path_data(&poly);
        assert!(d.starts_with('M') && d.ends_with('Z'));
    }

    #[test]
    fn svg_round_trips_and_keeps_foreign_elements() {
        let mut sk = Sketch {
            width: 200.0,
            height: 100.0,
            items: vec![
                Item::Stroke(Stroke {
                    tool: Tool::Pen,
                    color: "#1e1e1e".into(),
                    size: 3.0,
                    points: vec![Point::new(10.0, 10.0, 0.5), Point::new(50.0, 40.0, 0.8)],
                }),
                Item::Shape(Shape { kind: ShapeKind::Arrow, from: (5.0, 5.0), to: (90.0, 60.0), color: "#E03131".into(), width: 2.0 }),
                Item::Raw("<g id=\"x\"><text>hi &amp; bye</text><g/></g>".into()),
                Item::Stroke(Stroke {
                    tool: Tool::Highlighter,
                    color: "#ffd43b".into(),
                    size: 20.0,
                    points: vec![Point::new(0.0, 80.0, 0.5), Point::new(150.0, 80.0, 0.5)],
                }),
            ],
        };
        let svg = to_svg(&sk);
        assert!(svg.contains("data-quire-tool=\"pen\"") && svg.contains("fill-opacity=\"0.35\""), "{svg}");
        let back = parse_svg(&svg).unwrap();
        if let Item::Shape(s) = &mut sk.items[1] {
            s.color = "#e03131".into();
        }
        assert_eq!(back, sk);
        assert_eq!(to_svg(&back), svg, "stable");
        let foreign = "<?xml version=\"1.0\"?>\n<!-- made elsewhere -->\n<svg viewBox=\"0 0 30 40\"><circle r=\"3\"/></svg>";
        let f = parse_svg(foreign).unwrap();
        assert_eq!((f.width, f.height), (30.0, 40.0));
        assert_eq!(f.items, vec![Item::Raw("<circle r=\"3\"/>".into())]);
        assert!(parse_svg("<svg><path d=\"M0 0\"").is_err());
        assert!(parse_svg("hello").is_err());
        assert_eq!(parse_svg(&empty_svg(300.0, 200.0)).unwrap().items.len(), 0);
    }

    #[test]
    fn rasterizes_strokes_and_shapes() {
        let sk = Sketch {
            width: 100.0,
            height: 50.0,
            items: vec![
                Item::Stroke(Stroke {
                    tool: Tool::Marker,
                    color: "#000000".into(),
                    size: 8.0,
                    points: vec![Point::new(10.0, 25.0, 0.5), Point::new(90.0, 25.0, 0.5)],
                }),
                Item::Shape(Shape { kind: ShapeKind::Rect, from: (2.0, 2.0), to: (98.0, 48.0), color: "#ff0000".into(), width: 2.0 }),
            ],
        };
        let (w, h, px) = rasterize(&sk, 2.0);
        assert_eq!((w, h), (200, 100));
        let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 3) as usize];
        assert_eq!(at(100, 50), [0, 0, 0], "inside the stroke");
        assert_eq!(at(100, 20), [255, 255, 255], "paper");
        assert!(at(4, 50)[0] > 200 && at(4, 50)[1] < 60, "on the red frame: {:?}", at(4, 50));
    }

    #[test]
    fn fit_and_hits() {
        let mut sk = Sketch { width: 10.0, height: 10.0, items: Vec::new() };
        sk.items.push(Item::Shape(Shape { kind: ShapeKind::Rect, from: (100.0, 50.0), to: (200.0, 150.0), color: INK.into(), width: 2.0 }));
        sk.items.push(Item::Shape(Shape { kind: ShapeKind::Ellipse, from: (0.0, 0.0), to: (40.0, 20.0), color: INK.into(), width: 2.0 }));
        let moved = sk.fit(10.0);
        assert_eq!(moved, (11.0, 11.0));
        assert_eq!((sk.width, sk.height), (222.0, 172.0));
        let Item::Shape(r) = &sk.items[0] else { unreachable!() };
        assert!(r.hit((111.0, 100.0), 1.0), "on the left side");
        assert!(!r.hit((160.0, 110.0), 1.0), "inside, not on the outline");
        let Item::Shape(e) = &sk.items[1] else { unreachable!() };
        assert!(e.hit((11.0, 21.0), 1.0), "the ellipse's leftmost point");
        assert!(!e.hit((31.0, 21.0), 1.0), "its centre");
    }
}
