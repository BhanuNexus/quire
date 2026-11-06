// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Excalidraw drawings as pictures (0.10): `![[Plan.excalidraw]]` in a
//! note and its preview, drawing files on a canvas, and the drawing itself
//! when it's opened. Read-only: the notice says to edit in Excalidraw.
//!
//! An egui image loader draws `.excalidraw` and `.excalidraw.md` files to
//! pixels in both editions: `quire_canvas::excalidraw` turns the drawing
//! into paths and text, `vello_cpu` (already part of egui) fills and
//! strokes them, and the text is the bundled fonts' outlines. Pictures are
//! cached by the file's content, so an unchanged drawing is drawn once.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context, RichText, Ui};
use quire_app::Workspace;
use quire_app::vault::NotePath;
use quire_canvas::excalidraw::{self, FileRef, Op, Rgba, Scene, Seg};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider};
use vello_cpu::kurbo::{Affine, BezPath, Cap, Join, Rect, Stroke};
use vello_cpu::peniko::Color;
use vello_cpu::{CompositeMode, Pixmap, RasterizerSettings, RenderContext, Resources};

use crate::panels::PanelOut;
use crate::theme::Palette;

/// Pixels per drawing unit (sharp on high-density screens).
const SCALE: f64 = 2.0;
/// The largest picture side, in pixels.
const MAX_SIDE: f64 = 4096.0;

/// What a reader is told about drawings.
pub(crate) const NOTICE: &str = "Excalidraw drawing, shown as a picture. Quire doesn't edit drawings: edit in Excalidraw.";

// ---------------------------------------------------------------- rendering

/// Glyph outlines into a path, flipping y and moving to the pen position.
struct Pen<'a> {
    path: &'a mut BezPath,
    x: f64,
    y: f64,
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to((self.x + f64::from(x), self.y - f64::from(y)));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to((self.x + f64::from(x), self.y - f64::from(y)));
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.path.quad_to((self.x + f64::from(cx0), self.y - f64::from(cy0)), (self.x + f64::from(x), self.y - f64::from(y)));
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.path.curve_to(
            (self.x + f64::from(cx0), self.y - f64::from(cy0)),
            (self.x + f64::from(cx1), self.y - f64::from(cy1)),
            (self.x + f64::from(x), self.y - f64::from(y)),
        );
    }
    fn close(&mut self) {
        self.path.close_path();
    }
}

/// A line of text as one path, its start at `x` (or centred / ended there by `anchor`).
fn text_path(font: &FontRef<'_>, text: &str, size: f64, x: f64, baseline: f64, anchor: &str) -> BezPath {
    let px = Size::new(size as f32);
    let loc = LocationRef::default();
    let (map, metrics, outlines) = (font.charmap(), font.glyph_metrics(px, loc), font.outline_glyphs());
    let glyphs: Vec<_> = text.chars().filter_map(|c| map.map(c).or_else(|| map.map('?'))).collect();
    let width: f64 = glyphs.iter().map(|g| f64::from(metrics.advance_width(*g).unwrap_or(0.0))).sum();
    let mut pen_x = match anchor {
        "middle" => x - width / 2.0,
        "end" => x - width,
        _ => x,
    };
    let mut path = BezPath::new();
    for g in glyphs {
        if let Some(outline) = outlines.get(g) {
            let _ = outline.draw(DrawSettings::unhinted(px, loc), &mut Pen { path: &mut path, x: pen_x, y: baseline });
        }
        pen_x += f64::from(metrics.advance_width(g).unwrap_or(0.0));
    }
    path
}

fn color(c: Rgba) -> Color {
    let [r, g, b, a] = c.0;
    Color::from_rgba8(r, g, b, a)
}

fn bez(segs: &[Seg]) -> BezPath {
    let mut p = BezPath::new();
    for s in segs {
        match *s {
            Seg::Move(x, y) => p.move_to((x, y)),
            Seg::Line(x, y) => p.line_to((x, y)),
            Seg::Cubic(a, b, c, d, x, y) => p.curve_to((a, b), (c, d), (x, y)),
            Seg::Close => p.close_path(),
        }
    }
    p
}

/// Paint `img` (straight RGBA) over premultiplied pixels, scaled into `dst`.
fn blit(px: &mut [u8], w: usize, h: usize, img: &image::RgbaImage, dst: (f64, f64, f64, f64)) {
    let (x0, y0, dw, dh) = dst;
    if dw < 1.0 || dh < 1.0 || img.width() == 0 || img.height() == 0 {
        return;
    }
    let (sw, sh) = (f64::from(img.width()), f64::from(img.height()));
    let ys = (y0.max(0.0) as usize)..((y0 + dh).min(h as f64).max(0.0) as usize);
    let xs = (x0.max(0.0) as usize)..((x0 + dw).min(w as f64).max(0.0) as usize);
    for y in ys {
        let sy = (((y as f64 + 0.5 - y0) / dh * sh) as u32).min(img.height() - 1);
        for x in xs.clone() {
            let sx = (((x as f64 + 0.5 - x0) / dw * sw) as u32).min(img.width() - 1);
            let [r, g, b, a] = img.get_pixel(sx, sy).0;
            let i = (y * w + x) * 4;
            let af = f32::from(a) / 255.0;
            for (k, v) in [r, g, b].into_iter().enumerate() {
                px[i + k] = (f32::from(v) * af + f32::from(px[i + k]) * (1.0 - af)).round() as u8;
            }
            px[i + 3] = (f32::from(a) + f32::from(px[i + 3]) * (1.0 - af)).round() as u8;
        }
    }
}

/// Draw a scene: (width, height, straight RGBA). `image` gives an
/// embedded picture by file id.
pub(crate) fn render(scene: &Scene, image: &dyn Fn(&str) -> Option<image::RgbaImage>) -> (usize, usize, Vec<u8>) {
    let (x0, y0, x1, y1) = excalidraw::bounds(scene).unwrap_or((0.0, 0.0, 100.0, 60.0));
    let (x0, y0) = (x0 - excalidraw::PAD, y0 - excalidraw::PAD);
    let (dw, dh) = (x1 - x0 + excalidraw::PAD, y1 - y0 + excalidraw::PAD);
    let scale = SCALE.min(MAX_SIDE / dw.max(dh).max(1.0));
    let (w, h) = (((dw * scale).ceil() as u16).max(1), ((dh * scale).ceil() as u16).max(1));
    let to_px = Affine::translate((-x0, -y0)).then_scale(scale);
    let fonts = (FontRef::new(crate::theme::INTER_REGULAR).ok(), FontRef::new(crate::theme::JETBRAINS_MONO).ok());
    let mut pixmap = Pixmap::new(w, h);
    let mut res = Resources::new();
    let over = RasterizerSettings { composite_mode: CompositeMode::SrcOver, ..RasterizerSettings::default() };
    let mut ctx = RenderContext::new(w, h);
    ctx.set_transform(to_px);
    ctx.set_paint(color(scene.background.unwrap_or(Rgba::WHITE)));
    ctx.fill_rect(&Rect::new(x0, y0, x0 + dw, y0 + dh));
    for op in excalidraw::ops(scene) {
        match op {
            Op::Path { segs, fill, stroke, width, dash } => {
                let path = bez(&segs);
                if let Some(f) = fill {
                    ctx.set_paint(color(f));
                    ctx.fill_path(&path);
                }
                if let Some(s) = stroke {
                    ctx.set_paint(color(s));
                    ctx.set_stroke(Stroke::new(width).with_caps(Cap::Round).with_join(Join::Round).with_dashes(0.0, dash));
                    ctx.stroke_path(&path);
                }
            }
            Op::Text { lines, size, color: c, anchor, mono } => {
                let font = if mono { fonts.1.as_ref().or(fonts.0.as_ref()) } else { fonts.0.as_ref() };
                let Some(font) = font else { continue };
                ctx.set_paint(color(c));
                for (t, x, y) in lines {
                    ctx.fill_path(&text_path(font, &t, size, x, y, anchor));
                }
            }
            Op::Image { file_id, x, y, w: iw, h: ih } => {
                // Draw what's below, then the picture over it, then go on.
                let Some(img) = image(&file_id) else { continue };
                ctx.flush();
                ctx.render_with(&mut pixmap, &mut res, over);
                ctx.reset();
                ctx.set_transform(to_px);
                let (px, py) = ((x - x0) * scale, (y - y0) * scale);
                blit(pixmap.data_as_u8_slice_mut(), usize::from(w), usize::from(h), &img, (px, py, iw * scale, ih * scale));
            }
        }
    }
    ctx.flush();
    ctx.render_with(&mut pixmap, &mut res, over);
    let rgba = pixmap.take_unpremultiplied().into_iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
    (usize::from(w), usize::from(h), rgba)
}

/// The open vault's folder, for the picture loader (which is given file
/// paths, not the workspace). Set every frame by the app.
static VAULT_ROOT: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// The open vault changed (or none is open).
pub(crate) fn set_vault_root(root: Option<std::path::PathBuf>) {
    let same = VAULT_ROOT.read().is_ok_and(|r| *r == root);
    if !same && let Ok(mut r) = VAULT_ROOT.write() {
        *r = root;
    }
}

/// A picture inside a drawing: a `data:` URL, or a file in the vault (for
/// `[[attachments/x.png]]`, next to the drawing or in a folder above it).
/// A vault file passes the same checks as everywhere else
/// (`quire_app::drawings::embedded_file_at`): a picture, inside the vault
/// even through symlinks, never in a hidden folder. A drawing outside the
/// open vault embeds no files.
fn load_file(f: &FileRef, drawing: &Path, root: Option<&Path>) -> Option<image::RgbaImage> {
    use base64::Engine as _;
    let bytes = match f {
        FileRef::DataUrl(u) => {
            let data = u.split_once(";base64,")?.1;
            base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?
        }
        FileRef::Vault(rel) => {
            let root = root?.canonicalize().ok()?;
            let at = drawing.canonicalize().ok()?;
            let rel_drawing = at.strip_prefix(&root).ok()?.to_str()?.replace('\\', "/");
            let found = quire_app::drawings::embedded_file_at(&root, &NotePath::new(rel_drawing), rel)?;
            std::fs::read(found).ok()?
        }
    };
    Some(image::load_from_memory(&bytes).ok()?.to_rgba8())
}

/// A drawing file as a picture.
pub(crate) fn picture(text: &str, path: &Path) -> Result<ColorImage, String> {
    let root = VAULT_ROOT.read().ok().and_then(|r| r.clone());
    picture_in(text, path, root.as_deref())
}

/// [`picture`] for a drawing in the vault at `root`.
pub(crate) fn picture_in(text: &str, path: &Path, root: Option<&Path>) -> Result<ColorImage, String> {
    let scene = excalidraw::parse(text)?;
    let files = scene.files.clone();
    let (w, h, rgba) = render(&scene, &|id| load_file(files.get(id)?, path, root));
    let mut img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    img.source_size = egui::Vec2::new(w as f32 / SCALE as f32, h as f32 / SCALE as f32);
    Ok(img)
}

// ---------------------------------------------------------------- the loader

#[derive(Default)]
pub(crate) struct DrawingLoader {
    /// Pictures by the drawing's content hash.
    cache: Mutex<HashMap<u64, Arc<ColorImage>>>,
}

/// Install the loader (once, at start).
pub(crate) fn install(ctx: &Context) {
    ctx.add_image_loader(Arc::new(DrawingLoader::default()));
}

impl ImageLoader for DrawingLoader {
    fn id(&self) -> &'static str {
        concat!(module_path!(), "::DrawingLoader")
    }

    fn load(&self, _ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        let Some(path) = uri.strip_prefix("file://").filter(|p| excalidraw::is_drawing(p)) else {
            return Err(LoadError::NotSupported);
        };
        let text = std::fs::read_to_string(path).map_err(|e| LoadError::Loading(e.to_string()))?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let key = h.finish();
        let mut cache = self.cache.lock().map_err(|_| LoadError::Loading("the drawing cache is unavailable".into()))?;
        if let Some(img) = cache.get(&key) {
            return Ok(ImagePoll::Ready { image: Arc::clone(img) });
        }
        let img = Arc::new(picture(&text, Path::new(path)).map_err(LoadError::Loading)?);
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&img));
        Ok(ImagePoll::Ready { image: img })
    }

    fn forget(&self, _uri: &str) {}

    fn forget_all(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }

    fn byte_size(&self) -> usize {
        self.cache.lock().map_or(0, |c| c.values().map(|i| i.pixels.len() * 4).sum())
    }
}

// ---------------------------------------------------------------- in the app

thread_local! {
    /// When each shown drawing last changed, to redraw it after an edit elsewhere.
    static SEEN: std::cell::RefCell<HashMap<String, Option<std::time::SystemTime>>> = std::cell::RefCell::new(HashMap::new());
}

/// The picture of a drawing file, at its own size or smaller to fit.
fn show_picture(ui: &mut Ui, abs: &Path, max_w: f32, max_h: f32) {
    let uri = format!("file://{}", abs.display());
    let modified = std::fs::metadata(abs).and_then(|m| m.modified()).ok();
    let changed = SEEN.with_borrow_mut(|seen| seen.insert(uri.clone(), modified).is_some_and(|old| old != modified));
    if changed {
        ui.ctx().forget_image(&uri);
    }
    ui.add(egui::Image::new(uri).fit_to_original_size(1.0).max_width(max_w).max_height(max_h));
}

/// A `.excalidraw.md` note is open: its picture above the text.
pub(crate) fn banner_available(ws: &Workspace) -> bool {
    ws.active_doc().and_then(|d| d.path.as_ref()).is_some_and(|p| excalidraw::is_drawing(p.as_str()))
}

pub(crate) fn banner(ui: &mut Ui, ws: &mut Workspace, _st: &mut crate::registry::FeatureUi, pal: &Palette) -> PanelOut {
    let Some(abs) = ws.active_doc().and_then(|d| d.path.clone()).and_then(|p| ws.vault()?.storage().absolute(&p)) else {
        return PanelOut::default();
    };
    ui.label(RichText::new(NOTICE).color(pal.muted));
    show_picture(ui, &abs, ui.available_width(), 360.0);
    PanelOut::default()
}

/// A `.excalidraw` file (plain JSON) opened from the file list.
pub(crate) struct DrawingSurface {
    path: NotePath,
}

impl DrawingSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self { path: path.clone() }
    }
}

impl crate::surfaces::Surface for DrawingSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(24, 16)).show(ui, |ui| {
            ui.label(RichText::new(self.path.file_name()).size(22.0).color(pal.strong));
            ui.label(RichText::new(NOTICE).color(pal.muted));
            ui.add_space(8.0);
            if let Some(abs) = ws.vault().and_then(|v| v.storage().absolute(&self.path)) {
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    show_picture(ui, &abs, ui.available_width(), f32::INFINITY);
                });
            } else {
                ui.label(RichText::new("This file can't be read.").color(pal.danger));
            }
        });
        PanelOut::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPES: &str = include_str!("../../../crates/quire-canvas/testdata/excalidraw/shapes.excalidraw");

    #[test]
    fn a_drawing_becomes_a_picture() {
        let img = picture(SHAPES, Path::new("/nowhere/shapes.excalidraw")).unwrap();
        let [w, h] = img.size;
        assert!(w > 800 && h > 600, "{w}×{h}: two pixels a unit");
        // Background, then the filled diamond's green, the dashed red ellipse, and dark ink.
        let at = |x: usize, y: usize| img.pixels[y * w + x];
        assert_eq!(at(2, 2), egui::Color32::from_rgb(255, 249, 219), "the background colour");
        let has = |pred: &dyn Fn(egui::Color32) -> bool| img.pixels.iter().any(|p| pred(*p));
        assert!(has(&|p| p.g() > 200 && p.r() < 200 && p.b() < 200), "the green diamond");
        assert!(has(&|p| p.r() > 200 && p.g() < 80 && p.b() < 80), "the red ellipse");
        assert!(has(&|p| p.r() < 60 && p.g() < 60 && p.b() < 60), "dark strokes and text");
        assert!(has(&|p| p.b() > 230 && p.r() < 100 && p.g() > 120 && p.g() < 160), "the embedded blue picture");
    }

    #[test]
    fn text_has_ink() {
        let font = FontRef::new(crate::theme::INTER_REGULAR).unwrap();
        let p = text_path(&font, "Hi", 20.0, 10.0, 30.0, "start");
        let b = vello_cpu::kurbo::Shape::bounding_box(&p);
        assert!(b.x0 >= 10.0 && b.y1 <= 30.5 && b.width() > 10.0, "{b:?}");
        let centred = text_path(&font, "Hi", 20.0, 10.0, 30.0, "middle");
        assert!(vello_cpu::kurbo::Shape::bounding_box(&centred).x0 < 10.0);
    }
}
