// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Excalidraw drawings as pictures (read-only).
//!
//! Reads `.excalidraw` files (Excalidraw's JSON) and Obsidian's
//! `.excalidraw.md` notes, whose `## Drawing` section holds the same JSON,
//! plain (```` ```json ````) or LZ-string compressed (```` ```compressed-json ````),
//! with pictures listed under `## Embedded Files`.
//!
//! The drawing becomes a list of [`Op`]s (paths, text, images) in a clean
//! style (straight lines, no hand-drawn wobble); [`to_svg`] writes them as
//! SVG for export, and the desktop draws the same ops to pixels.
//! Element types: rectangle, ellipse, diamond, line, arrow, freedraw,
//! text, image and frame.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

/// The drawing, read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scene {
    pub elements: Vec<Element>,
    /// `appState.viewBackgroundColor` (white when missing).
    pub background: Option<Rgba>,
    /// Pictures by file id: a `data:` URL, or a vault path (`.excalidraw.md`).
    pub files: BTreeMap<String, FileRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRef {
    /// `data:image/png;base64,…`
    DataUrl(String),
    /// `[[picture.png]]` in a note's `## Embedded Files`.
    Vault(String),
}

/// A colour with alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const BLACK: Self = Self([30, 30, 30, 255]);
    pub const WHITE: Self = Self([255, 255, 255, 255]);

    /// `#rgb`, `#rrggbb`, `#rrggbbaa`; `transparent` and anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.trim().strip_prefix('#')?;
        let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
        match h.len() {
            3 => {
                let n = |i: usize| u8::from_str_radix(h.get(i..=i)?, 16).ok().map(|v| v * 17);
                Some(Self([n(0)?, n(1)?, n(2)?, 255]))
            }
            6 => Some(Self([byte(0)?, byte(2)?, byte(4)?, 255])),
            8 => Some(Self([byte(0)?, byte(2)?, byte(4)?, byte(6)?])),
            _ => None,
        }
    }

    fn with_alpha(self, a: f64) -> Self {
        let [r, g, b, a0] = self.0;
        Self([r, g, b, (f64::from(a0) * a.clamp(0.0, 1.0)).round() as u8])
    }

    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2])
    }

    fn opacity(self) -> f64 {
        f64::from(self.0[3]) / 255.0
    }
}

/// One element, with the fields Quire draws.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub kind: String,
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Radians, clockwise, around the element's centre.
    pub angle: f64,
    pub stroke: Option<Rgba>,
    pub fill: Option<Rgba>,
    /// `solid`, `hachure`, `cross-hatch`, `zigzag`.
    pub fill_style: String,
    pub stroke_width: f64,
    /// `solid`, `dashed`, `dotted`.
    pub stroke_style: String,
    /// 0–1.
    pub opacity: f64,
    pub rounded: bool,
    /// Lines, arrows and freedraw: points relative to (x, y).
    pub points: Vec<(f64, f64)>,
    pub start_arrow: Option<String>,
    pub end_arrow: Option<String>,
    pub text: String,
    pub font_size: f64,
    /// 1 Virgil (hand), 2 Helvetica, 3 Cascadia (code), 5 Excalifont, …
    pub font_family: i64,
    pub text_align: String,
    pub vertical_align: String,
    pub line_height: f64,
    pub file_id: Option<String>,
    /// Frames: their name.
    pub name: Option<String>,
}

fn num(v: &Value, k: &str, default: f64) -> f64 {
    v.get(k).and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(default)
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn element(v: &Value) -> Option<Element> {
    if v.get("isDeleted").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let kind = text(v, "type");
    let points = v
        .get("points")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let arrow = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "none").map(str::to_string);
    Some(Element {
        id: text(v, "id"),
        x: num(v, "x", 0.0),
        y: num(v, "y", 0.0),
        width: num(v, "width", 0.0),
        height: num(v, "height", 0.0),
        angle: num(v, "angle", 0.0),
        stroke: Rgba::parse(&text(v, "strokeColor")).or(Some(Rgba::BLACK)).filter(|_| text(v, "strokeColor") != "transparent"),
        fill: Rgba::parse(&text(v, "backgroundColor")),
        fill_style: v.get("fillStyle").and_then(Value::as_str).unwrap_or("solid").to_string(),
        stroke_width: num(v, "strokeWidth", 2.0),
        stroke_style: v.get("strokeStyle").and_then(Value::as_str).unwrap_or("solid").to_string(),
        opacity: num(v, "opacity", 100.0) / 100.0,
        rounded: v.get("roundness").is_some_and(|r| !r.is_null()),
        points,
        start_arrow: arrow("startArrowhead"),
        end_arrow: arrow("endArrowhead"),
        text: v.get("text").or_else(|| v.get("originalText")).and_then(Value::as_str).unwrap_or_default().to_string(),
        font_size: num(v, "fontSize", 20.0),
        font_family: v.get("fontFamily").and_then(Value::as_i64).unwrap_or(1),
        text_align: v.get("textAlign").and_then(Value::as_str).unwrap_or("left").to_string(),
        vertical_align: v.get("verticalAlign").and_then(Value::as_str).unwrap_or("top").to_string(),
        line_height: num(v, "lineHeight", 1.25),
        file_id: v.get("fileId").and_then(Value::as_str).map(str::to_string),
        name: v.get("name").and_then(Value::as_str).map(str::to_string),
        kind,
    })
}

/// Read Excalidraw JSON (the `.excalidraw` file, or a note's drawing).
pub fn parse_json(json: &str) -> Result<Scene, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("the drawing can't be read: {e}"))?;
    let elements = v.get("elements").and_then(Value::as_array).ok_or("the drawing has no elements")?;
    let background = v.pointer("/appState/viewBackgroundColor").and_then(Value::as_str).and_then(Rgba::parse);
    let mut files = BTreeMap::new();
    if let Some(fs) = v.get("files").and_then(Value::as_object) {
        for (id, f) in fs {
            if let Some(url) = f.get("dataURL").and_then(Value::as_str) {
                files.insert(id.clone(), FileRef::DataUrl(url.to_string()));
            }
        }
    }
    Ok(Scene { elements: elements.iter().filter_map(element).collect(), background, files })
}

/// The fenced block after `## Drawing` in an `.excalidraw.md` note: (language, body).
fn drawing_block(md: &str) -> Option<(&str, String)> {
    let at = md.find("## Drawing")?;
    let rest = &md[at..];
    let open = rest.find("```")?;
    let after = &rest[open + 3..];
    let nl = after.find('\n')?;
    let lang = after[..nl].trim();
    let body = &after[nl + 1..];
    let close = body.find("```")?;
    Some((lang, body[..close].to_string()))
}

/// Read a `.excalidraw.md` note or a `.excalidraw` file.
pub fn parse(text: &str) -> Result<Scene, String> {
    if text.trim_start().starts_with('{') {
        return parse_json(text);
    }
    let (lang, body) = drawing_block(text).ok_or("this note has no ## Drawing section")?;
    let json = match lang {
        "compressed-json" => lz_decompress_base64(&body).ok_or("the compressed drawing can't be read")?,
        _ => body,
    };
    let mut scene = parse_json(&json)?;
    // `## Embedded Files`: `<id>: [[picture.png]]`.
    if let Some(at) = text.find("## Embedded Files") {
        for line in text[at..].lines().skip(1) {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with("%%") {
                break;
            }
            if let Some((id, target)) = line.split_once(": ")
                && let Some(link) = target.trim().strip_prefix("[[").and_then(|t| t.strip_suffix("]]"))
            {
                let link = link.split('|').next().unwrap_or(link);
                scene.files.entry(id.trim().to_string()).or_insert_with(|| FileRef::Vault(link.to_string()));
            }
        }
    }
    Ok(scene)
}

/// Is this a drawing, by its name?
pub fn is_drawing(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".excalidraw") || p.ends_with(".excalidraw.md") || p.ends_with(".excalidraw.json")
}

// ---------------------------------------------------------------- LZ-string

/// LZ-string's `decompressFromBase64` (what Excalidraw and its Obsidian
/// plugin use); whitespace in the input is ignored.
pub fn lz_decompress_base64(input: &str) -> Option<String> {
    const KEY: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
    let vals: Vec<u32> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .map(|b| KEY.iter().position(|k| *k == b).map(|i| i as u32))
        .collect::<Option<_>>()?;
    if vals.is_empty() {
        return Some(String::new());
    }
    let units = lz_decompress(&vals, 32)?;
    String::from_utf16(&units).ok()
}

fn lz_decompress(data: &[u32], reset: u32) -> Option<Vec<u16>> {
    struct Bits<'a> {
        data: &'a [u32],
        val: u32,
        pos: u32,
        index: usize,
        reset: u32,
    }
    impl Bits<'_> {
        fn read(&mut self, n: u32) -> u32 {
            let mut bits = 0;
            for i in 0..n {
                let b = self.val & self.pos;
                self.pos >>= 1;
                if self.pos == 0 {
                    self.pos = self.reset;
                    self.val = self.data.get(self.index).copied().unwrap_or(0);
                    self.index += 1;
                }
                if b > 0 {
                    bits |= 1 << i;
                }
            }
            bits
        }
    }
    let mut bits = Bits { data, val: data[0], pos: reset, index: 1, reset };
    let mut dict: Vec<Vec<u16>> = vec![Vec::new(), Vec::new(), Vec::new()];
    let mut enlarge: u32 = 4;
    let mut num_bits: u32 = 3;
    let first = match bits.read(2) {
        0 => bits.read(8) as u16,
        1 => bits.read(16) as u16,
        _ => return Some(Vec::new()),
    };
    dict.push(vec![first]);
    let mut w = vec![first];
    let mut out = w.clone();
    loop {
        if bits.index > data.len() + 1 {
            return None;
        }
        let mut c = bits.read(num_bits) as usize;
        match c {
            0 | 1 => {
                let v = bits.read(if c == 0 { 8 } else { 16 }) as u16;
                dict.push(vec![v]);
                c = dict.len() - 1;
                enlarge -= 1;
            }
            2 => return Some(out),
            _ => {}
        }
        if enlarge == 0 {
            enlarge = 1 << num_bits;
            num_bits += 1;
        }
        let entry = match c.cmp(&dict.len()) {
            std::cmp::Ordering::Less => dict[c].clone(),
            std::cmp::Ordering::Equal => {
                let mut e = w.clone();
                e.push(*w.first()?);
                e
            }
            std::cmp::Ordering::Greater => return None,
        };
        out.extend_from_slice(&entry);
        let mut next = w;
        next.push(*entry.first()?);
        dict.push(next);
        enlarge -= 1;
        w = entry;
        if enlarge == 0 {
            enlarge = 1 << num_bits;
            num_bits += 1;
        }
        if out.len() > 64 << 20 {
            return None;
        }
    }
}

// ---------------------------------------------------------------- drawing ops

/// A path command, in drawing units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// What to draw, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Path {
        segs: Vec<Seg>,
        fill: Option<Rgba>,
        stroke: Option<Rgba>,
        width: f64,
        /// Dash and gap lengths; empty for a solid line.
        dash: Vec<f64>,
    },
    Text {
        /// Each line's text and the point its baseline starts from (or is
        /// centred on / ends at, by `anchor`).
        lines: Vec<(String, f64, f64)>,
        size: f64,
        color: Rgba,
        /// `start`, `middle`, `end`.
        anchor: &'static str,
        mono: bool,
    },
    Image {
        file_id: String,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    },
}

/// The area the drawing covers: (min x, min y, max x, max y).
pub fn bounds(scene: &Scene) -> Option<(f64, f64, f64, f64)> {
    let mut b: Option<(f64, f64, f64, f64)> = None;
    let mut add = |x: f64, y: f64| {
        b = Some(b.map_or((x, y, x, y), |(a, c, d, e)| (a.min(x), c.min(y), d.max(x), e.max(y))));
    };
    for e in &scene.elements {
        if e.points.is_empty() {
            add(e.x, e.y);
            add(e.x + e.width, e.y + e.height);
        } else {
            for (px, py) in &e.points {
                add(e.x + px, e.y + py);
            }
        }
        if e.kind == "frame" {
            add(e.x, e.y - 24.0);
        }
    }
    b
}

/// Rotate a point around a centre.
fn rot(p: (f64, f64), c: (f64, f64), a: f64) -> (f64, f64) {
    if a == 0.0 {
        return p;
    }
    let (s, co) = a.sin_cos();
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    (c.0 + dx * co - dy * s, c.1 + dx * s + dy * co)
}

fn rotate_segs(segs: &mut [Seg], c: (f64, f64), a: f64) {
    if a == 0.0 {
        return;
    }
    for s in segs {
        *s = match *s {
            Seg::Move(x, y) => {
                let p = rot((x, y), c, a);
                Seg::Move(p.0, p.1)
            }
            Seg::Line(x, y) => {
                let p = rot((x, y), c, a);
                Seg::Line(p.0, p.1)
            }
            Seg::Cubic(a1, b1, a2, b2, x, y) => {
                let (p1, p2, p) = (rot((a1, b1), c, a), rot((a2, b2), c, a), rot((x, y), c, a));
                Seg::Cubic(p1.0, p1.1, p2.0, p2.1, p.0, p.1)
            }
            Seg::Close => Seg::Close,
        };
    }
}

fn ellipse(cx: f64, cy: f64, rx: f64, ry: f64) -> Vec<Seg> {
    const K: f64 = 0.552_284_749_8;
    let (kx, ky) = (rx * K, ry * K);
    vec![
        Seg::Move(cx + rx, cy),
        Seg::Cubic(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry),
        Seg::Cubic(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy),
        Seg::Cubic(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry),
        Seg::Cubic(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy),
        Seg::Close,
    ]
}

fn rect(x: f64, y: f64, w: f64, h: f64, r: f64) -> Vec<Seg> {
    if r <= 0.0 {
        return vec![Seg::Move(x, y), Seg::Line(x + w, y), Seg::Line(x + w, y + h), Seg::Line(x, y + h), Seg::Close];
    }
    let k = r * (1.0 - 0.552_284_749_8);
    vec![
        Seg::Move(x + r, y),
        Seg::Line(x + w - r, y),
        Seg::Cubic(x + w - k, y, x + w, y + k, x + w, y + r),
        Seg::Line(x + w, y + h - r),
        Seg::Cubic(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h),
        Seg::Line(x + r, y + h),
        Seg::Cubic(x + k, y + h, x, y + h - k, x, y + h - r),
        Seg::Line(x, y + r),
        Seg::Cubic(x, y + k, x + k, y, x + r, y),
        Seg::Close,
    ]
}

fn dash_for(e: &Element) -> Vec<f64> {
    let w = e.stroke_width.max(1.0);
    match e.stroke_style.as_str() {
        "dashed" => vec![8.0 + w, 8.0 + w],
        "dotted" => vec![1.5 * w, 6.0 + w],
        _ => Vec::new(),
    }
}

/// Fill strength for Excalidraw's fill styles, drawn flat.
fn fill_for(e: &Element) -> Option<Rgba> {
    let a = match e.fill_style.as_str() {
        "hachure" => 0.35,
        "cross-hatch" | "zigzag" => 0.5,
        _ => 1.0,
    };
    e.fill.map(|f| f.with_alpha(a * e.opacity))
}

fn arrowhead(kind: &str, tip: (f64, f64), from: (f64, f64), width: f64, color: Rgba) -> Option<Op> {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let size = (12.0 + width * 2.0).min(len * 0.8).max(4.0);
    let back = (tip.0 - ux * size, tip.1 - uy * size);
    let side = |s: f64| (back.0 - uy * size * 0.5 * s, back.1 + ux * size * 0.5 * s);
    let (l, r) = (side(1.0), side(-1.0));
    Some(match kind {
        "triangle" | "triangle_outline" => Op::Path {
            segs: vec![Seg::Move(tip.0, tip.1), Seg::Line(l.0, l.1), Seg::Line(r.0, r.1), Seg::Close],
            fill: (kind == "triangle").then_some(color),
            stroke: Some(color),
            width,
            dash: Vec::new(),
        },
        "dot" | "circle" | "circle_outline" => {
            let rad = size * 0.3;
            Op::Path {
                segs: ellipse(tip.0 - ux * rad, tip.1 - uy * rad, rad, rad),
                fill: (kind != "circle_outline").then_some(color),
                stroke: Some(color),
                width,
                dash: Vec::new(),
            }
        }
        "bar" => {
            let (a, b) = ((tip.0 - uy * size * 0.5, tip.1 + ux * size * 0.5), (tip.0 + uy * size * 0.5, tip.1 - ux * size * 0.5));
            Op::Path { segs: vec![Seg::Move(a.0, a.1), Seg::Line(b.0, b.1)], fill: None, stroke: Some(color), width, dash: Vec::new() }
        }
        _ => Op::Path {
            segs: vec![Seg::Move(l.0, l.1), Seg::Line(tip.0, tip.1), Seg::Line(r.0, r.1)],
            fill: None,
            stroke: Some(color),
            width,
            dash: Vec::new(),
        },
    })
}

/// The drawing as ops, in drawing units (elements' own coordinates).
pub fn ops(scene: &Scene) -> Vec<Op> {
    let mut out = Vec::new();
    for e in &scene.elements {
        let c = (e.x + e.width / 2.0, e.y + e.height / 2.0);
        let stroke = e.stroke.map(|s| s.with_alpha(e.opacity));
        let width = e.stroke_width.max(0.5);
        let mut push_path = |mut segs: Vec<Seg>, fill: Option<Rgba>, stroke: Option<Rgba>, dash: Vec<f64>| {
            rotate_segs(&mut segs, c, e.angle);
            out.push(Op::Path { segs, fill, stroke, width, dash });
        };
        match e.kind.as_str() {
            "rectangle" => {
                let r = if e.rounded { (e.width.min(e.height) * 0.25).min(32.0) } else { 0.0 };
                push_path(rect(e.x, e.y, e.width, e.height, r), fill_for(e), stroke, dash_for(e));
            }
            "ellipse" => push_path(ellipse(c.0, c.1, e.width / 2.0, e.height / 2.0), fill_for(e), stroke, dash_for(e)),
            "diamond" => {
                let segs = vec![
                    Seg::Move(c.0, e.y),
                    Seg::Line(e.x + e.width, c.1),
                    Seg::Line(c.0, e.y + e.height),
                    Seg::Line(e.x, c.1),
                    Seg::Close,
                ];
                push_path(segs, fill_for(e), stroke, dash_for(e));
            }
            "frame" | "magicframe" => {
                push_path(rect(e.x, e.y, e.width, e.height, 0.0), None, Some(Rgba([140, 140, 140, 255])), Vec::new());
                let name = e.name.clone().unwrap_or_else(|| "Frame".into());
                out.push(Op::Text {
                    lines: vec![(name, e.x, e.y - 8.0)],
                    size: 14.0,
                    color: Rgba([120, 120, 120, 255]),
                    anchor: "start",
                    mono: false,
                });
            }
            "line" | "arrow" | "freedraw" => {
                let pts: Vec<(f64, f64)> = e.points.iter().map(|(px, py)| rot((e.x + px, e.y + py), c, e.angle)).collect();
                let Some(first) = pts.first() else { continue };
                let mut segs = vec![Seg::Move(first.0, first.1)];
                segs.extend(pts.iter().skip(1).map(|p| Seg::Line(p.0, p.1)));
                let closed = e.kind == "line" && pts.len() > 2 && pts.first() == pts.last();
                let fill = if closed { fill_for(e) } else { None };
                if closed {
                    segs.push(Seg::Close);
                }
                let dash = if e.kind == "freedraw" { Vec::new() } else { dash_for(e) };
                out.push(Op::Path { segs, fill, stroke, width, dash });
                if e.kind == "arrow"
                    && let Some(col) = stroke
                {
                    let n = pts.len();
                    if let (Some(kind), true) = (&e.end_arrow, n > 1) {
                        out.extend(arrowhead(kind, pts[n - 1], pts[n - 2], width, col));
                    }
                    if let (Some(kind), true) = (&e.start_arrow, n > 1) {
                        out.extend(arrowhead(kind, pts[0], pts[1], width, col));
                    }
                }
            }
            "text" => {
                let color = e.stroke.unwrap_or(Rgba::BLACK).with_alpha(e.opacity);
                let lh = e.font_size * e.line_height.max(0.5);
                let n = e.text.lines().count().max(1) as f64;
                let top = match e.vertical_align.as_str() {
                    "middle" => e.y + (e.height - lh * n) / 2.0,
                    "bottom" => e.y + e.height - lh * n,
                    _ => e.y,
                };
                let (anchor, ax) = match e.text_align.as_str() {
                    "center" => ("middle", e.x + e.width / 2.0),
                    "right" => ("end", e.x + e.width),
                    _ => ("start", e.x),
                };
                // The baseline sits about 0.8 of the font size below each line's top.
                let lines = e
                    .text
                    .lines()
                    .enumerate()
                    .map(|(i, l)| {
                        let p = rot((ax, top + lh * i as f64 + (lh - e.font_size) / 2.0 + e.font_size * 0.8), c, e.angle);
                        (l.to_string(), p.0, p.1)
                    })
                    .collect();
                out.push(Op::Text { lines, size: e.font_size, color, anchor, mono: e.font_family == 3 });
            }
            "image" => {
                if let Some(id) = &e.file_id {
                    out.push(Op::Image { file_id: id.clone(), x: e.x, y: e.y, w: e.width, h: e.height });
                }
            }
            _ => {}
        }
    }
    out
}

/// Margin around the drawing in pictures.
pub const PAD: f64 = 16.0;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn path_d(segs: &[Seg]) -> String {
    let mut d = String::new();
    for s in segs {
        let _ = match s {
            Seg::Move(x, y) => write!(d, "M{x:.2} {y:.2}"),
            Seg::Line(x, y) => write!(d, "L{x:.2} {y:.2}"),
            Seg::Cubic(a, b, c, e, x, y) => write!(d, "C{a:.2} {b:.2} {c:.2} {e:.2} {x:.2} {y:.2}"),
            Seg::Close => write!(d, "Z"),
        };
    }
    d
}

/// The drawing as an SVG document. `image` gives a picture's `href` (a
/// `data:` URL) by file id; pictures without one are left out.
pub fn to_svg(scene: &Scene, image: &dyn Fn(&str) -> Option<String>) -> String {
    let (x0, y0, x1, y1) = bounds(scene).unwrap_or((0.0, 0.0, 100.0, 60.0));
    let (x0, y0, w, h) = (x0 - PAD, y0 - PAD, x1 - x0 + 2.0 * PAD, y1 - y0 + 2.0 * PAD);
    let bg = scene.background.unwrap_or(Rgba::WHITE);
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{x0:.2} {y0:.2} {w:.2} {h:.2}\" width=\"{w:.0}\" height=\"{h:.0}\">\n<rect x=\"{x0:.2}\" y=\"{y0:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" fill=\"{}\"/>\n",
        bg.hex()
    );
    for op in ops(scene) {
        match op {
            Op::Path { segs, fill, stroke, width, dash } => {
                let fill = fill.map_or_else(|| "none".to_string(), |f| format!("{}\" fill-opacity=\"{:.3}", f.hex(), f.opacity()));
                let stroke = stroke.map_or_else(
                    || "none\"".to_string(),
                    |c| format!("{}\" stroke-opacity=\"{:.3}\" stroke-width=\"{width:.2}\"", c.hex(), c.opacity()),
                );
                let dash = if dash.is_empty() {
                    String::new()
                } else {
                    format!(" stroke-dasharray=\"{}\"", dash.iter().map(|d| format!("{d:.1}")).collect::<Vec<_>>().join(" "))
                };
                let _ = writeln!(
                    s,
                    "<path d=\"{}\" fill=\"{fill}\" stroke=\"{stroke} stroke-linecap=\"round\" stroke-linejoin=\"round\"{dash}/>",
                    path_d(&segs)
                );
            }
            Op::Text { lines, size, color, anchor, mono } => {
                let family = if mono { "ui-monospace, monospace" } else { "Inter, Helvetica, Arial, sans-serif" };
                for (t, x, y) in lines {
                    let _ = writeln!(
                        s,
                        "<text x=\"{x:.2}\" y=\"{y:.2}\" font-size=\"{size:.1}\" font-family=\"{family}\" fill=\"{}\" fill-opacity=\"{:.3}\" text-anchor=\"{anchor}\">{}</text>",
                        color.hex(),
                        color.opacity(),
                        esc(&t)
                    );
                }
            }
            Op::Image { file_id, x, y, w, h } => {
                if let Some(href) = image(&file_id) {
                    let _ = writeln!(
                        s,
                        "<image x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" preserveAspectRatio=\"none\" href=\"{}\"/>",
                        esc(&href)
                    );
                }
            }
        }
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPES: &str = include_str!("../testdata/excalidraw/shapes.excalidraw");
    const COMPRESSED: &str = include_str!("../testdata/excalidraw/Sketch.excalidraw.md");
    const PLAIN: &str = include_str!("../testdata/excalidraw/Plain.excalidraw.md");

    #[test]
    fn reads_every_element_type() {
        let s = parse(SHAPES).unwrap();
        let kinds: Vec<&str> = s.elements.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["frame", "rectangle", "ellipse", "diamond", "line", "arrow", "freedraw", "text", "image"]);
        assert!(!s.elements.iter().any(|e| e.id == "gone"), "deleted elements are skipped");
        assert_eq!(s.background, Some(Rgba([255, 249, 219, 255])));
        assert!(matches!(s.files.get("pic1"), Some(FileRef::DataUrl(u)) if u.starts_with("data:image/png;base64,")));
        let text = s.elements.iter().find(|e| e.kind == "text").unwrap();
        assert_eq!((text.text.as_str(), text.text_align.as_str()), ("Plan\nship it", "center"));
        let arrow = s.elements.iter().find(|e| e.kind == "arrow").unwrap();
        assert_eq!((arrow.start_arrow.as_deref(), arrow.end_arrow.as_deref()), (None, Some("arrow")));
        assert_eq!(arrow.points, [(0.0, 0.0), (120.0, 40.0)]);
    }

    #[test]
    fn reads_obsidian_notes_compressed_or_not() {
        let a = parse(COMPRESSED).unwrap();
        let b = parse(PLAIN).unwrap();
        assert_eq!(a.elements.len(), 3);
        assert_eq!(a.elements, b.elements, "the same drawing either way");
        assert_eq!(a.files.get("img7"), Some(&FileRef::Vault("attachments/dot.png".into())));
        assert!(parse("# Just a note").is_err());
        assert!(is_drawing("Sketches/Plan.excalidraw.md") && is_drawing("x.excalidraw") && !is_drawing("x.md"));
    }

    #[test]
    fn lz_string_round_trips_reference_output() {
        // `LZString.compressToBase64("Hello, Quire! ☃")` from lz-string 1.5.0.
        assert_eq!(lz_decompress_base64("BIUwNmD2A0AECKBXAlgJxAQloYDIg===").as_deref(), Some("Hello, Quire! ☃"));
        assert_eq!(lz_decompress_base64(""), Some(String::new()));
        assert_eq!(lz_decompress_base64("!!"), None);
    }

    #[test]
    fn svg_has_one_mark_per_element() {
        let s = parse(SHAPES).unwrap();
        let svg = to_svg(&s, &|id| (id == "pic1").then(|| "data:image/png;base64,AAAA".to_string()));
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox="));
        assert_eq!(svg.matches("<text").count(), 3, "two text lines and the frame's name");
        assert_eq!(svg.matches("<image").count(), 1);
        assert!(svg.contains("stroke-dasharray"), "the dashed ellipse");
        assert!(svg.contains("fill=\"#fff9db\""), "the background");
        assert!(svg.contains(">Plan</text>") && svg.contains(">ship it</text>"));
        // Rectangle, ellipse, diamond, line, arrow (+ head), freedraw, frame.
        assert_eq!(svg.matches("<path").count(), 8);
        let (x0, y0, x1, y1) = bounds(&s).unwrap();
        assert!(x0 <= 0.0 && y0 < 0.0 && x1 >= 420.0 && y1 >= 260.0, "{x0} {y0} {x1} {y1}");
    }
}
