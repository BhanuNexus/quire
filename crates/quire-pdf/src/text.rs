// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text with positions, one [`PageText`] per page (design §G.2).
//!
//! A `hayro_interpret::Device` collects every glyph's Unicode text and box
//! in PDF user space. Lines are rebuilt in content order: a glyph continues
//! the line when it sits on the same baseline and follows closely; a space
//! goes in where the gap is wider than a quarter of the font size. Pages
//! with no text whose images cover at least half the page are reported as
//! needing OCR.

use std::panic::{AssertUnwindSafe, catch_unwind};

use hayro_interpret::font::Glyph;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::{Array, Dict, MaybeRef, Name, Object};
use hayro_interpret::hayro_syntax::page::Page;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint, PathDrawMode, SoftMask,
    interpret_page,
};
use kurbo::{Affine, BezPath, Point};
use serde::{Deserialize, Serialize};

use crate::PdfError;

/// A rectangle in PDF points: `[x0, y0, x1, y1]`, origin bottom-left.
pub type Quad = [f32; 4];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    pub quad: Quad,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PageText {
    /// Every character of [`PageText::text`] with its box, in reading order
    /// (`\n` between lines has an empty box at the line's end).
    pub chars: Vec<(char, Quad)>,
    pub lines: Vec<TextLine>,
    /// No text, and images cover at least half the page: a scan.
    #[serde(default)]
    pub image_only: bool,
}

impl PageText {
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// The boxes of chars `range` (indices into [`PageText::text`]'s chars),
    /// merged per line.
    pub fn quads(&self, range: std::ops::Range<usize>) -> Vec<Quad> {
        let mut out: Vec<Quad> = Vec::new();
        let mut cur: Option<Quad> = None;
        for (c, q) in self.chars.get(range).unwrap_or_default() {
            if *c == '\n' {
                out.extend(cur.take());
                continue;
            }
            if q[2] <= q[0] {
                continue;
            }
            cur = Some(match cur {
                Some(a) => [a[0].min(q[0]), a[1].min(q[1]), a[2].max(q[2]), a[3].max(q[3])],
                None => *q,
            });
        }
        out.extend(cur);
        out
    }
}

/// A page's size and how to place PDF points on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageInfo {
    /// Displayed size in points (after rotation).
    pub width: f32,
    pub height: f32,
    /// Affine `[a, b, c, d, e, f]` from PDF user space to the displayed page
    /// (origin top-left, y down, in points).
    pub to_view: [f64; 6],
}

impl PageInfo {
    /// A point in PDF user space on the displayed page (points, y down).
    pub fn view_point(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.to_view;
        let (x, y) = (f64::from(x), f64::from(y));
        ((a * x + c * y + e) as f32, (b * x + d * y + f) as f32)
    }

    /// A PDF box on the displayed page, as `[left, top, right, bottom]`.
    pub fn view_rect(&self, q: Quad) -> [f32; 4] {
        let p = [self.view_point(q[0], q[1]), self.view_point(q[2], q[3]), self.view_point(q[0], q[3]), self.view_point(q[2], q[1])];
        let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in p {
            l = l.min(x);
            t = t.min(y);
            r = r.max(x);
            b = b.max(y);
        }
        [l, t, r, b]
    }

    /// The inverse of [`PageInfo::view_point`].
    pub fn pdf_point(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.to_view;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return (x, y);
        }
        let (x, y) = (f64::from(x) - e, f64::from(y) - f);
        (((d * x - c * y) / det) as f32, ((a * y - b * x) / det) as f32)
    }
}

/// Where a link on a page goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Uri(String),
    /// A page of this document, 0-based.
    Page(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageLink {
    pub quad: Quad,
    pub target: LinkTarget,
}

/// An open PDF: parse once, then read pages.
pub struct PdfDoc {
    pdf: Pdf,
}

impl std::fmt::Debug for PdfDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfDoc").field("pages", &self.page_count()).finish()
    }
}

/// Run hayro code, turning a panic into an error (odd PDFs must never crash Quire).
pub(crate) fn guard<T>(what: &str, f: impl FnOnce() -> T) -> Result<T, PdfError> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| PdfError::Unreadable(format!("{what} failed on this file")))
}

impl PdfDoc {
    pub fn open(bytes: Vec<u8>) -> Result<Self, PdfError> {
        if !bytes.starts_with(b"%PDF-") && !bytes.get(..1024).is_some_and(|h| h.windows(5).any(|w| w == b"%PDF-")) {
            return Err(PdfError::Unreadable("not a PDF file".into()));
        }
        let pdf = guard("Opening the PDF", || Pdf::new(bytes))?.map_err(|e| {
            PdfError::Unreadable(match e {
                hayro_interpret::hayro_syntax::LoadPdfError::Decryption(_) => "it is encrypted".into(),
                hayro_interpret::hayro_syntax::LoadPdfError::Invalid => "the file is damaged".into(),
            })
        })?;
        Ok(Self { pdf })
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    pub(crate) fn page(&self, i: usize) -> Result<&Page<'_>, PdfError> {
        self.pdf.pages().get(i).ok_or_else(|| PdfError::Unreadable(format!("there is no page {}", i + 1)))
    }

    pub fn page_info(&self, i: usize) -> Result<PageInfo, PdfError> {
        let page = self.page(i)?;
        guard("Reading the page size", || {
            let (width, height) = page.render_dimensions();
            PageInfo { width, height, to_view: page.initial_transform(true).as_coeffs() }
        })
    }

    /// The text of page `i` (0-based).
    pub fn page_text(&self, i: usize) -> Result<PageText, PdfError> {
        let page = self.page(i)?;
        guard("Reading text", || {
            let mut dev = Collector::default();
            let settings = InterpreterSettings { render_annotations: false, ..InterpreterSettings::default() };
            let cache = InterpreterCache::new();
            let mut cx = Context::new(Affine::IDENTITY, kurbo::Rect::new(0.0, 0.0, 1.0, 1.0), &cache, page.xref(), settings);
            interpret_page(page, &mut cx, &mut dev);
            let (w, h) = page.base_dimensions();
            let page_area = f64::from(w) * f64::from(h);
            let mut text = build(dev.glyphs);
            text.image_only = text.lines.is_empty() && page_area > 0.0 && dev.image_area / page_area >= 0.5;
            text
        })
    }

    /// The destination array a name stands for (`/Names /Dests` tree of the catalog, or the old
    /// `/Dests` dictionary).
    fn named_destination(&self, name: &[u8]) -> Option<Array<'_>> {
        let xref = self.pdf.xref();
        let root: Dict<'_> = xref.get(xref.root_id())?;
        if let Some(names) = root.get::<Dict<'_>>(b"Names".as_slice()).and_then(|n| n.get::<Dict<'_>>(b"Dests".as_slice())) {
            let flat = names.get::<Array<'_>>(b"Names".as_slice())?;
            let mut it = flat.flex_iter();
            while let Some(key) = it.next::<hayro_interpret::hayro_syntax::object::String<'_>>() {
                let value = it.next::<Array<'_>>();
                if key.as_bytes() == name {
                    return value;
                }
            }
            return None;
        }
        root.get::<Dict<'_>>(b"Dests".as_slice())?.get::<Array<'_>>(name)
    }

    /// Links on page `i`: web addresses and jumps to other pages.
    pub fn page_links(&self, i: usize) -> Vec<PageLink> {
        let Ok(page) = self.page(i) else { return Vec::new() };
        let pages = self.pdf.pages();
        guard("Reading links", || {
            let page_ids: Vec<_> = pages.iter().map(|p| p.raw().obj_id()).collect();
            let mut out = Vec::new();
            let Some(annots) = page.raw().get::<Array<'_>>(b"Annots".as_slice()) else { return out };
            for a in annots.iter::<Dict<'_>>() {
                if a.get::<Name<'_>>(b"Subtype".as_slice()).is_none_or(|s| s.as_ref() != b"Link") {
                    continue;
                }
                let Some(r) = a.get::<Array<'_>>(b"Rect".as_slice()) else { continue };
                let v: Vec<f32> = r.iter::<f32>().collect();
                let [x0, y0, x1, y1] = v[..] else { continue };
                let quad = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
                let action = a.get::<Dict<'_>>(b"A".as_slice());
                let uri = action
                    .as_ref()
                    .and_then(|d| d.get::<hayro_interpret::hayro_syntax::object::String<'_>>(b"URI".as_slice()))
                    .map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned());
                if let Some(u) = uri.filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:")) {
                    out.push(PageLink { quad, target: LinkTarget::Uri(u) });
                    continue;
                }
                let dest = a
                    .get::<Array<'_>>(b"Dest".as_slice())
                    .or_else(|| action.as_ref().and_then(|d| d.get::<Array<'_>>(b"D".as_slice())))
                    // 0.12: a named destination (Typst makes one for every heading and paragraph id it links to).
                    .or_else(|| {
                        let name = a
                            .get::<hayro_interpret::hayro_syntax::object::String<'_>>(b"Dest".as_slice())
                            .or_else(|| action.as_ref()?.get::<hayro_interpret::hayro_syntax::object::String<'_>>(b"D".as_slice()))?;
                        self.named_destination(name.as_bytes())
                    });
                let target = dest.and_then(|d| match d.raw_iter().next()? {
                    MaybeRef::Ref(r) => page_ids.iter().position(|id| id.is_some_and(|id| id.obj_number == r.obj_number)),
                    MaybeRef::NotRef(Object::Number(n)) => usize::try_from(n.as_i64()).ok(),
                    MaybeRef::NotRef(_) => None,
                });
                if let Some(p) = target {
                    out.push(PageLink { quad, target: LinkTarget::Page(u32::try_from(p).unwrap_or(0)) });
                }
            }
            out
        })
        .unwrap_or_default()
    }
}

/// The text of every page. A page that can't be read is empty.
pub fn extract(pdf: &[u8]) -> Result<Vec<PageText>, PdfError> {
    let doc = PdfDoc::open(pdf.to_vec())?;
    Ok((0..doc.page_count()).map(|i| doc.page_text(i).unwrap_or_default()).collect())
}

/// One glyph as drawn.
#[derive(Debug, Clone)]
struct RawGlyph {
    text: String,
    bbox: Quad,
    /// Baseline start and end in user space.
    origin: (f32, f32),
    end: (f32, f32),
    /// Font size in user space.
    size: f32,
}

#[derive(Default)]
struct Collector {
    glyphs: Vec<RawGlyph>,
    image_area: f64,
}

fn pt(a: Affine, x: f64, y: f64) -> (f32, f32) {
    let p = a * Point::new(x, y);
    (p.x as f32, p.y as f32)
}

impl Device<'_> for Collector {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'_>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'_>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'_>>, _: BlendMode) {}

    fn draw_glyph(&mut self, glyph: &Glyph<'_>, transform: Affine, glyph_transform: Affine, _: &Paint<'_>, _: &GlyphDrawMode) {
        let Some(u) = glyph.as_unicode() else { return };
        let text = match u {
            hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
            hayro_interpret::hayro_cmap::BfString::String(s) => s,
        };
        // Drop control characters; spaces are worked out from the gaps.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.trim().is_empty() {
            return;
        }
        let adv = match glyph {
            Glyph::Outline(o) => f64::from(o.advance_width().filter(|w| *w > 0.0).unwrap_or(500.0)),
            Glyph::Type3(_) => 500.0,
        };
        let t = transform * glyph_transform;
        let corners = [pt(t, 0.0, -200.0), pt(t, adv, -200.0), pt(t, adv, 800.0), pt(t, 0.0, 800.0)];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        let origin = pt(t, 0.0, 0.0);
        let top = pt(t, 0.0, 1000.0);
        let size = (top.0 - origin.0).hypot(top.1 - origin.1);
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) || size <= 0.0 {
            return;
        }
        self.glyphs.push(RawGlyph { text, bbox: [x0, y0, x1, y1], origin, end: pt(t, adv, 0.0), size });
    }

    fn draw_image(&mut self, image: Image<'_, '_>, transform: Affine) {
        // The transform maps the image's pixels to the page.
        let [a, b, c, d, _, _] = transform.as_coeffs();
        self.image_area += (a * d - b * c).abs() * f64::from(image.width()) * f64::from(image.height());
    }

    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Lines from glyphs in content order.
fn build(glyphs: Vec<RawGlyph>) -> PageText {
    struct Line {
        text: String,
        boxes: Vec<Quad>,
        dir: (f32, f32),
        end: (f32, f32),
        size: f32,
    }
    let mut lines: Vec<Line> = Vec::new();
    for g in glyphs {
        let (dx, dy) = (g.end.0 - g.origin.0, g.end.1 - g.origin.1);
        let len = dx.hypot(dy);
        let dir = if len > 0.0 { (dx / len, dy / len) } else { (1.0, 0.0) };
        let joined = lines.last_mut().is_some_and(|l| {
            // Distance along the line from its end, and across from its baseline.
            let (vx, vy) = (g.origin.0 - l.end.0, g.origin.1 - l.end.1);
            let along = vx * l.dir.0 + vy * l.dir.1;
            let across = (-vx * l.dir.1 + vy * l.dir.0).abs();
            let em = l.size.max(g.size);
            let same_dir = dir.0 * l.dir.0 + dir.1 * l.dir.1 > 0.9;
            if !(same_dir && across < 0.5 * em && along > -0.6 * em && along < 3.0 * em) {
                return false;
            }
            if along > 0.25 * em {
                l.text.push(' ');
                let prev = l.boxes.last().copied().unwrap_or(g.bbox);
                l.boxes.push([prev[2], prev[1].min(g.bbox[1]), g.bbox[0].max(prev[2]), prev[3].max(g.bbox[3])]);
            }
            true
        });
        if !joined {
            lines.push(Line { text: String::new(), boxes: Vec::new(), dir, end: g.origin, size: g.size });
        }
        let l = lines.last_mut().expect("a line was just ensured");
        let n = g.text.chars().count().max(1);
        let w = (g.bbox[2] - g.bbox[0]) / n as f32;
        for (k, c) in g.text.chars().enumerate() {
            l.text.push(c);
            l.boxes.push([g.bbox[0] + w * k as f32, g.bbox[1], g.bbox[0] + w * (k + 1) as f32, g.bbox[3]]);
        }
        l.end = g.end;
        l.size = l.size.max(g.size);
    }
    let mut out = PageText::default();
    for (i, l) in lines.into_iter().enumerate() {
        let quad =
            l.boxes.iter().copied().reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]).unwrap_or_default();
        if i > 0 {
            out.chars.push(('\n', [quad[0], quad[1], quad[0], quad[1]]));
        }
        out.chars.extend(l.text.chars().zip(l.boxes.iter().copied()));
        out.lines.push(TextLine { text: l.text, quad });
    }
    out
}

/// Plain text of every page, for search (`AttachmentText::pages`).
pub fn plain_pages(pages: &[PageText]) -> Vec<String> {
    pages.iter().map(PageText::text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, x: f32, y: f32, w: f32) -> RawGlyph {
        RawGlyph { text: text.into(), bbox: [x, y - 2.0, x + w, y + 8.0], origin: (x, y), end: (x + w, y), size: 10.0 }
    }

    #[test]
    fn rebuilds_lines_with_spaces_from_gaps() {
        let g = vec![
            glyph("H", 0.0, 100.0, 6.0),
            glyph("i", 6.0, 100.0, 3.0),
            glyph("y", 15.0, 100.0, 5.0),
            glyph("o", 20.0, 100.0, 5.0),
            // Next line.
            glyph("N", 0.0, 86.0, 7.0),
            glyph("fi", 7.0, 86.0, 6.0),
        ];
        let t = build(g);
        assert_eq!(t.text(), "Hi yo\nNfi");
        assert_eq!(t.chars.len(), t.text().chars().count());
        assert_eq!(t.chars[2].0, ' ');
        assert!(near(t.lines[1].quad, [0.0, 84.0, 13.0, 94.0]));
        let q = t.quads(0..5);
        assert!(q.len() == 1 && near(q[0], [0.0, 98.0, 25.0, 108.0]), "{q:?}");
        assert_eq!(t.quads(3..8).len(), 2, "two lines, two boxes");
    }

    fn near(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn says_what_it_cant_do() {
        assert!(matches!(extract(b"hello"), Err(PdfError::Unreadable(_))));
        assert!(matches!(extract(b"%PDF-1.7\n"), Err(PdfError::Unreadable(_)) | Ok(_)));
    }

    #[test]
    fn page_info_maps_both_ways() {
        let p = PageInfo { width: 612.0, height: 792.0, to_view: [1.0, 0.0, 0.0, -1.0, 0.0, 792.0] };
        assert_eq!(p.view_point(72.0, 720.0), (72.0, 72.0));
        assert_eq!(p.pdf_point(72.0, 72.0), (72.0, 720.0));
        assert!(near(p.view_rect([72.0, 700.0, 100.0, 720.0]), [72.0, 72.0, 100.0, 92.0]));
    }
}
