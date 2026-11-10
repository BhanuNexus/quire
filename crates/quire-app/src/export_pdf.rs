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
        // 0.11: an Excalidraw drawing goes in as its SVG picture (as in HTML export).
        if let Some(d) = drawing(vault, t) {
            return crate::drawings::svg(vault, &d, &|_| true).map(String::into_bytes);
        }
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

/// The drawing `![[target]]` embeds: `Plan.excalidraw` is the file of
/// that name or the drawing note `Plan.excalidraw.md`.
fn drawing(vault: &Vault, target: &str) -> Option<NotePath> {
    let lower = target.to_ascii_lowercase();
    if !quire_canvas::excalidraw::is_drawing(&lower) {
        return None;
    }
    vault
        .resolve_file(target)
        .or_else(|| {
            let md = std::path::Path::new(&lower).extension().is_some_and(|e| e.eq_ignore_ascii_case("md"));
            (!md).then(|| vault.resolve_file(&format!("{target}.md"))).flatten()
        })
        .filter(|p| quire_canvas::excalidraw::is_drawing(p.as_str()))
}

impl Workspace {
    /// Start exporting the active note to `Exports/<title>.pdf`; a notice says when it's done.
    pub fn export_active_pdf(&mut self) {
        if crate::secure_state::refuse_export(self) {
            return;
        }
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

    /// 0.11: `![[Plan.excalidraw]]` is the drawing's picture, text and all.
    #[test]
    fn drawings_are_pictures_in_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        let drawing = include_str!("../../quire-canvas/testdata/excalidraw/Plain.excalidraw.md");
        vault.write(&NotePath::new("Sketches/Plan.excalidraw.md"), drawing).unwrap();
        let out = NotePath::new("Exports/Report.pdf");
        let path = export_pdf(&vault, "Report", "# Report\n\n![[Plan.excalidraw]]\n", &out).unwrap();
        let pdf = quire_pdf::PdfDoc::open(std::fs::read(path).unwrap()).unwrap();
        let text = pdf.page_text(0).unwrap().text();
        assert!(text.contains("Idea"), "the drawing's text is in the PDF: {text}");
        assert!(!text.contains("Plan.excalidraw"), "not the missing-picture box: {text}");
    }

    /// 0.12 verify: in the exported PDF, links inside the note (to a heading, to a
    /// paragraph id, `[[#Heading]]`, `[[ThisNote#Heading]]`) jump to the right page; links to
    /// other notes are plain text, and web links stay web links.
    #[test]
    fn links_inside_the_note_jump_to_their_page_in_the_pdf() {
        use quire_pdf::text::LinkTarget;
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        let filler = "Filler line to push the next heading to a later page.\n\n".repeat(60);
        let text = format!(
            "# Top\n\nJump to [[#Later part]], [[Report#Later part|the same]], [the heading](#later-part), [[#^marker]] and [[Report#^marker]]. \
             A [web link](https://example.com), a [[Some Other Note]] and [[Some Other Note#Part]].\n\n{filler}## Later part\n\n\
             The marked paragraph. ^marker\n\n[[#Top]]\n"
        );
        let out = NotePath::new("Exports/Report.pdf");
        let path = export_pdf(&vault, "Report", &text, &out).unwrap();
        let pdf = quire_pdf::PdfDoc::open(std::fs::read(path).unwrap()).unwrap();
        assert!(pdf.page_count() >= 2, "the later part is on a later page");
        let last = pdf.page_count() - 1;
        let first = pdf.page_links(0);
        let uris: Vec<&str> =
            first.iter().filter_map(|l| if let LinkTarget::Uri(u) = &l.target { Some(u.as_str()) } else { None }).collect();
        assert_eq!(uris, ["https://example.com"]);
        let pages: Vec<u32> = first.iter().filter_map(|l| if let LinkTarget::Page(p) = l.target { Some(p) } else { None }).collect();
        assert_eq!(pages.len(), 5, "five links inside the note, none to other notes: {first:?}");
        assert!(pages.iter().all(|p| *p as usize == last), "they jump to the later page ({last}): {pages:?}");
        // The link back to the top.
        let back = pdf.page_links(last);
        assert!(back.iter().any(|l| l.target == LinkTarget::Page(0)), "{back:?}");
        // Other notes are text: their names are in the page's text, with no link on them.
        assert!(pdf.page_text(0).unwrap().text().contains("Some Other Note"));
    }
}
