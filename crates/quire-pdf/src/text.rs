// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text with positions, one [`PageText`] per page.

use crate::PdfError;

/// A rectangle in PDF points: `[x0, y0, x1, y1]`, origin bottom-left.
pub type Quad = [f32; 4];

#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub text: String,
    pub quad: Quad,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageText {
    /// Every character with its box, in reading order.
    pub chars: Vec<(char, Quad)>,
    pub lines: Vec<TextLine>,
}

impl PageText {
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

/// The text of every page.
pub fn extract(pdf: &[u8]) -> Result<Vec<PageText>, PdfError> {
    if !pdf.starts_with(b"%PDF-") {
        return Err(PdfError::Unreadable("not a PDF file".into()));
    }
    Err(PdfError::NotAvailable("Reading text from PDFs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_it_cant_do() {
        assert!(matches!(extract(b"hello"), Err(PdfError::Unreadable(_))));
        assert_eq!(extract(b"%PDF-1.7\n"), Err(PdfError::NotAvailable("Reading text from PDFs")));
    }
}
