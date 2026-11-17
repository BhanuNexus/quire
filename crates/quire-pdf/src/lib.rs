// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDFs (design 0.9 §G.2): text with positions for search and highlights
//! (`text`, both editions), page images (`render`, full edition), and
//! annotations kept in a Markdown note next to the PDF.
//!
//! Everything that runs the PDF interpreter is wrapped in `catch_unwind`:
//! an odd PDF gives an error ("can't render this page"), never a crash.

pub mod annot;
pub mod find;
#[cfg(feature = "text")]
pub mod text;

#[cfg(feature = "text")]
pub use text::{PageInfo, PageText, PdfDoc};

/// Rendering pages is part of this build (the full edition).
pub const CAN_RENDER: bool = cfg!(feature = "render");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfError {
    /// This build or version can't do it.
    NotAvailable(&'static str),
    /// The file isn't a PDF Quire can read.
    Unreadable(String),
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAvailable(what) => write!(f, "{what} isn't available in this version of Quire"),
            Self::Unreadable(why) => write!(f, "can't read this PDF: {why}"),
        }
    }
}

impl std::error::Error for PdfError {}

/// A page image, RGBA8 (not premultiplied; opaque white background).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The largest side of a page image, in pixels.
pub const MAX_SIDE: f32 = 8000.0;

/// The most pixels in a page image (40 million: 160 MB of RGBA).
pub const MAX_PIXELS: f32 = 40_000_000.0;

/// The scale to render a `width` × `height` pt page at.
///
/// As close to `want` as the limits allow (a page under one pixel is scaled up to one): at most [`MAX_SIDE`] a side and [`MAX_PIXELS`] in
/// all, and at least 0.05. `None` for a page that can't be drawn: a size that
/// is zero, negative or not a number, or so thin that a side would be under
/// one pixel.
///
/// A page's size comes from the file (`/MediaBox`), so it is never trusted.
pub fn page_scale(want: f32, width: f32, height: f32) -> Option<f32> {
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return None;
    }
    let want = if want.is_finite() { want } else { 1.0 };
    let (w, h) = (f64::from(width), f64::from(height));
    let by_side = f64::from(MAX_SIDE) / w.max(h);
    let by_area = (f64::from(MAX_PIXELS) / (w * h)).sqrt();
    let scale = f64::from(want).max(0.05).max(1.0 / w.min(h)).min(by_side).min(by_area);
    (scale.is_finite() && w * scale >= 1.0 && h * scale >= 1.0).then_some(scale as f32)
}

/// Render page `page` (0-based) at `scale` pixels per point.
#[cfg(feature = "render")]
pub fn render_page(doc: &PdfDoc, page: usize, scale: f32) -> Result<PageImage, PdfError> {
    use hayro::{RenderCache, RenderSettings, render};
    use hayro_interpret::InterpreterSettings;

    let p = doc.page(page)?;
    text::guard("Showing this page", || {
        let (w, h) = p.render_dimensions();
        let scale = page_scale(scale, w, h).ok_or_else(|| PdfError::Unreadable("this page has no usable size".into()))?;
        let cache = RenderCache::new();
        let settings =
            RenderSettings { x_scale: scale, y_scale: scale, bg_color: hayro::vello_cpu::color::palette::css::WHITE, ..Default::default() };
        let pix = render(p, &cache, &InterpreterSettings::default(), &settings);
        let (width, height) = (u32::from(pix.width()), u32::from(pix.height()));
        let rgba: Vec<u8> = pix.take_unpremultiplied().into_iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
        Ok(PageImage { width, height, rgba })
    })?
}

/// Hex SHA-256 of a file's bytes (cache keys and `pdf_sha256`).
pub use quire_base::enc::sha256_hex;
