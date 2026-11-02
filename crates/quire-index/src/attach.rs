// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text of attachments (PDFs and images).
//!
//! The jobs that extract it send it in: PDF text (`quire-pdf`) and OCR
//! (`quire-ai`). Stored per path; search over it arrives with WS-C (design
//! §C.1, "attachment documents").

use quire_vault::NotePath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentKind {
    Pdf,
    Image,
}

/// Where the text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextSource {
    /// Text the file contains (a born-digital PDF).
    Embedded,
    /// Recognized from pixels.
    Ocr,
}

/// The text of one attachment, one string per page (an image is one page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentText {
    pub path: NotePath,
    pub kind: AttachmentKind,
    pub pages: Vec<String>,
    /// SHA-256 (hex) of the file the text was taken from, so a stale result can be spotted.
    pub source_hash: String,
    pub by: TextSource,
}
