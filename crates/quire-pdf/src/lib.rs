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

/// Render page `page` (0-based) at `scale` pixels per point.
#[cfg(feature = "render")]
pub fn render_page(doc: &PdfDoc, page: usize, scale: f32) -> Result<PageImage, PdfError> {
    use hayro::{RenderCache, RenderSettings, render};
    use hayro_interpret::InterpreterSettings;

    let p = doc.page(page)?;
    text::guard("Showing this page", || {
        let (w, h) = p.render_dimensions();
        let scale = scale.clamp(0.05, MAX_SIDE / w.max(h).max(1.0));
        let cache = RenderCache::new();
        let settings =
            RenderSettings { x_scale: scale, y_scale: scale, bg_color: hayro::vello_cpu::color::palette::css::WHITE, ..Default::default() };
        let pix = render(p, &cache, &InterpreterSettings::default(), &settings);
        let (width, height) = (u32::from(pix.width()), u32::from(pix.height()));
        let rgba: Vec<u8> = pix.take_unpremultiplied().into_iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
        PageImage { width, height, rgba }
    })
}

/// Hex SHA-256 of a file's bytes (cache keys and `pdf_sha256`).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    sha2::Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
