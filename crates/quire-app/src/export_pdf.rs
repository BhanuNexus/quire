// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Export a note as a PDF (Typst underneath), off the UI thread.

use crossbeam_channel::Receiver;
use quire_vault::{NotePath, Vault};

use crate::Workspace;
use crate::workspace::NoticeLevel;

/// Typeset `text` and write it to `out` in the vault. Images embedded in
/// the note are read from the vault. Returns the file's full path.
pub fn export_pdf(vault: &Vault, title: &str, text: &str, out: &NotePath) -> Result<String, String> {
    let load = |target: &str| -> Option<Vec<u8>> {
        let t = target.split(['|', '#']).next().unwrap_or(target).trim();
        let p = vault.resolve_file(t).or_else(|| Some(NotePath::new(t.trim_start_matches("./"))))?;
        vault.storage().read_bytes(&p).ok()
    };
    // WS-G: `[@key]` citations and the reference list (Typst, embedded style).
    let (text, bibliography) = crate::citations::for_pdf(vault, text);
    let opts = quire_typeset::PdfOptions {
        title: title.to_string(),
        load_image: Box::new(load),
        bibliography,
        ..quire_typeset::PdfOptions::default()
    };
    let bytes = quire_typeset::markdown_pdf(&text, &opts).map_err(|e| e.to_string())?;
    vault.storage().write_bytes_atomic(out, &bytes).map_err(|e| e.to_string())?;
    Ok(vault.storage().absolute(out).map_or_else(|| out.to_string(), |p| p.display().to_string()))
}

impl Workspace {
    /// Start exporting the active note to `Exports/<title>.pdf`; a notice says when it's done.
    pub fn export_active_pdf(&mut self) {
        let (Some(doc), Some(vault)) = (self.active_doc(), self.vault.clone()) else { return };
        if self.pdf_job.is_some() {
            self.notify(NoticeLevel::Info, "Still making the last PDF…");
            return;
        }
        let title = doc.title();
        let text = doc.text().to_string();
        let out = NotePath::new(format!("Exports/{title}.pdf"));
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| export_pdf(&vault, &title, &text, &out)))
                .unwrap_or_else(|_| Err("the typesetter failed on this note".into()));
            let _ = tx.send(r);
        });
        self.pdf_job = Some(rx);
        self.notify(NoticeLevel::Info, "Making PDF…");
    }

    /// Report a finished export. Returns true if something changed.
    pub(crate) fn poll_pdf_job(&mut self) -> bool {
        let Some(rx) = &self.pdf_job else { return false };
        let Ok(r) = rx.try_recv() else { return false };
        self.pdf_job = None;
        match r {
            Ok(path) => self.notify(NoticeLevel::Info, format!("Exported to {path}")),
            Err(e) => self.notify(NoticeLevel::Error, format!("Couldn't make the PDF: {e}")),
        }
        true
    }
}

/// A pending export, for the workspace to poll.
pub(crate) type PdfJob = Option<Receiver<Result<String, String>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_a_note_with_an_image_to_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        // A 1×1 PNG.
        let png = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0,
            0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0, 0x05, 0x00,
            0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        vault.save_attachment("dot.png", &png).unwrap();
        let text = "# Report\n\n> [!tip] Good\n> Body with $x^2$\n\n![[dot.png]]\n\n$$\\sum_i i$$\n";
        let out = NotePath::new("Exports/Report.pdf");
        let path = export_pdf(&vault, "Report", text, &out).unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"%PDF-") && bytes.len() > 1000);
    }
}
