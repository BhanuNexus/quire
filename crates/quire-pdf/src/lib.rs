// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDFs (design 0.9 §G.2): text with positions for search and highlights
//! (`text`, both editions), page images (`render`, full edition), and
//! annotations kept in a Markdown note next to the PDF.
//!
//! Extraction and rendering arrive with WS-G; until then they report
//! [`PdfError::NotAvailable`]. The annotation format is fixed here.

pub mod annot;
#[cfg(feature = "text")]
pub mod text;

/// Rendering pages is part of this build (the full edition).
pub const CAN_RENDER: bool = cfg!(feature = "render");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfError {
    /// This build or version can't do it yet.
    NotAvailable(&'static str),
    /// The file isn't a PDF Quire can read.
    Unreadable(String),
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAvailable(what) => write!(f, "{what} isn't available yet"),
            Self::Unreadable(why) => write!(f, "can't read this PDF: {why}"),
        }
    }
}

impl std::error::Error for PdfError {}

/// A page image, RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Render page `page` (0-based) at `dpi`.
#[cfg(feature = "render")]
pub fn render_page(pdf: &[u8], page: u32, dpi: f32) -> Result<PageImage, PdfError> {
    let _ = (pdf, page, dpi);
    Err(PdfError::NotAvailable("Showing PDF pages"))
}
