// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-G's entry point into the workspace: canvases, sketches, PDFs and
//! citations (design §G).
//!
//! `features.rs` calls the functions below for every Spatial-owned command,
//! every frame, every vault event and every decorated document. Canvas
//! state lives in `ws.features.canvas`; PDFs in `ws.features.pdf`
//! (`pdf_state.rs`), the bibliography in `ws.features.citations`
//! (`citations.rs`). The editors themselves are desktop surfaces.

/// The canvas model, for crates above this one (`quire-mcp`'s canvas tools).
pub use quire_canvas as canvas;
/// PDF text and annotations.
pub use quire_pdf as pdf;

use std::collections::HashMap;
use std::time::Instant;

use quire_markdown::cite::Formatter as _;
use quire_vault::{NotePath, VaultEvent, sanitize_title};

use crate::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// WS-G's canvas state, kept in `ws.features.canvas`.
#[derive(Debug, Default)]
pub struct CanvasState {
    /// Bumped per file when a canvas or sketch changes on disk, so an open
    /// editor can take the change in.
    pub disk_changes: HashMap<NotePath, u64>,
}

impl CanvasState {
    /// How many times `path` changed on disk since the vault opened.
    pub fn disk_generation(&self, path: &NotePath) -> u64 {
        self.disk_changes.get(path).copied().unwrap_or(0)
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    crate::pdf_state::tick(ws, now) | crate::citations::tick(ws, now)
}

/// The earliest instant `tick` has work to do (the watched bibliography).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    crate::citations::next_deadline(ws)
}

/// Whether a Spatial-owned command is ready to offer.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    let vault = ws.vault.is_some();
    match cmd {
        Command::NewCanvas | Command::ImportBibliography | Command::NewDrawing => vault,
        Command::InsertSketch | Command::InsertCitation | Command::EmbedNewDrawing => vault && ws.active.is_some(),
        _ => false,
    }
}

/// A Spatial-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::NewCanvas => {
            match new_canvas(ws) {
                Ok(p) => {
                    ws.notify(NoticeLevel::Info, format!("Created {}", p.file_name()));
                    ws.request_ui(UiRequest::OpenFile(p));
                }
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
            None
        }
        Command::InsertSketch => {
            match insert_sketch(ws) {
                Ok(p) => {
                    ws.notify(NoticeLevel::Info, format!("Draw in {}; it saves as you go", p.file_name()));
                    ws.request_ui(UiRequest::OpenFile(p));
                }
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
            None
        }
        Command::InsertCitation | Command::ImportBibliography => Some(UiRequest::Feature(cmd)),
        // 0.13: Excalidraw drawings open in the drawing editor.
        Command::NewDrawing | Command::EmbedNewDrawing => {
            let made = if cmd == Command::NewDrawing { crate::drawings::new_drawing(ws) } else { crate::drawings::embed_new_drawing(ws) };
            match made {
                Ok(p) => ws.request_ui(UiRequest::OpenFile(p)),
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
            None
        }
        _ => {
            ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…')));
            None
        }
    }
}

/// The folder new files go in: the active note's.
fn current_folder(ws: &Workspace) -> String {
    ws.active_doc().and_then(|d| d.path.as_ref()).map(|p| p.folder().to_string()).unwrap_or_default()
}

/// A path in `folder` named `stem.ext`, or `stem 1.ext`, … if taken.
fn unique(ws: &Workspace, folder: &str, stem: &str, ext: &str) -> Option<NotePath> {
    let vault = ws.vault.as_ref()?;
    (0..10_000)
        .map(|n| if n == 0 { format!("{stem}.{ext}") } else { format!("{stem} {n}.{ext}") })
        .map(|f| NotePath::join(folder, &f))
        .find(|p| !vault.exists(p))
}

/// Make an empty canvas next to the active note.
pub fn new_canvas(ws: &Workspace) -> Result<NotePath, String> {
    let folder = current_folder(ws);
    let path = unique(ws, &folder, "Untitled", "canvas").ok_or("No vault is open")?;
    let vault = ws.vault.as_ref().ok_or("No vault is open")?;
    vault.storage().write_atomic(&path, &quire_canvas::to_canonical(&quire_canvas::Canvas::default())).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Make an empty sketch in the attachments folder and embed it at the caret.
pub fn insert_sketch(ws: &mut Workspace) -> Result<NotePath, String> {
    let doc = ws.active_doc().ok_or("Open a note first")?;
    let stem = doc.path.as_ref().map_or_else(|| "Sketch".to_string(), |p| sanitize_title(p.stem()));
    let vault = ws.vault.clone().ok_or("No vault is open")?;
    let folder = vault.config.attachments_folder.clone();
    let path = (1..10_000)
        .map(|n| NotePath::join(&folder, &format!("{stem}-ink-{n}.svg")))
        .find(|p| !vault.exists(p))
        .ok_or("Too many sketches")?;
    vault.storage().write_atomic(&path, &quire_canvas::ink::empty_svg(600.0, 300.0)).map_err(|e| e.to_string())?;
    // On a line of its own.
    let doc = ws.active_doc().ok_or("Open a note first")?;
    let sel = doc.selection();
    let line_start = doc.buffer().line_to_char(doc.buffer().char_to_line(sel.start()));
    let at_line_start = sel.start() == line_start;
    let embed = format!("{}![[{}]]\n", if at_line_start { "" } else { "\n" }, path.file_name());
    ws.insert_text(&embed);
    Ok(path)
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let _ = (ws, doc, out);
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let _ = (ws, ev);
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    // 0.13: an open drawing editor takes in a change made elsewhere.
    let (VaultEvent::Changed(p) | VaultEvent::Removed(p) | VaultEvent::AttachmentChanged(p) | VaultEvent::AttachmentRemoved(p)) = ev;
    let plain = quire_vault::lock::plain_name(p).unwrap_or_else(|| p.clone());
    if quire_canvas::excalidraw::is_drawing(plain.as_str()) {
        *ws.features.canvas.disk_changes.entry(plain).or_default() += 1;
    }
    match ev {
        VaultEvent::AttachmentChanged(p) | VaultEvent::AttachmentRemoved(p) => {
            let ext = p.as_str().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "canvas" | "svg") {
                *ws.features.canvas.disk_changes.entry(p.clone()).or_default() += 1;
            }
            if matches!(ext.as_str(), "bib") {
                crate::citations::file_changed(ws, p);
            }
        }
        _ => {}
    }
    crate::pdf_state::vault_event(ws, ev);
}

/// A vault was opened: state starts fresh. Work that needs the vault runs
/// on the first tick (the vault is set right after this call).
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.canvas = CanvasState::default();
    crate::pdf_state::vault_opened(ws);
    crate::citations::vault_opened(ws);
}

/// Status-bar items: the reference under the caret.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    if ws.features.citations.library.is_empty() {
        return;
    }
    let Some((_, Some(r))) = crate::citations::at_caret(ws) else { return };
    let year = if r.year.is_empty() { "n.d." } else { r.year.as_str() };
    let title: String = r.title.chars().take(48).collect();
    out.push(StatusItem {
        owner: crate::Owner::Spatial,
        text: format!("📚 {} {year} · {title}", r.short_authors()),
        tooltip: format!("{}\n\nCtrl+click the citation to open it.", ws.features.citations.library.reference(&r.key)),
        command: Some(Command::InsertCitation),
        warn: false,
    });
}

/// A link to a file that isn't a note opens it in its editor.
///
/// `[[paper.pdf#page=3]]`, `[[Board.canvas]]`. Returns false when `target`
/// isn't such a file (the caller treats it as a note link).
pub fn open_file_link(ws: &mut Workspace, target: &str, heading: Option<&str>) -> bool {
    let t = target.trim();
    let Some((_, ext)) = t.rsplit_once('.') else { return false };
    let ext = ext.to_ascii_lowercase();
    if ext == "md" || ext.contains(' ') || ext.is_empty() {
        return false;
    }
    let Some(path) = ws.vault.as_ref().and_then(|v| v.resolve_file(t)).filter(|p| !p.is_markdown()) else { return false };
    if ext == "pdf" {
        crate::pdf_state::open_link(ws, &path, heading);
    } else {
        ws.request_ui(UiRequest::OpenFile(path));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::Vault;

    #[test]
    fn new_canvas_and_sketch_make_files_and_open_them() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Projects/A.md", "# A\n\ntext")]));
        ws.open_note(&"Projects/A.md".into());
        assert!(ws.command_available(Command::NewCanvas));
        assert!(run(&mut ws, Command::NewCanvas).is_none());
        let p = NotePath::new("Projects/Untitled.canvas");
        assert_eq!(ws.vault().unwrap().read(&p).unwrap(), "{\"nodes\":[\n],\"edges\":[\n]}\n");
        assert_eq!(ws.take_ui_requests(), vec![UiRequest::OpenFile(p)]);
        run(&mut ws, Command::NewCanvas);
        assert!(ws.vault().unwrap().exists(&"Projects/Untitled 1.canvas".into()));
        ws.take_ui_requests();
        let end = ws.active_doc().unwrap().buffer().len_chars();
        ws.active_doc_mut().unwrap().set_selection(quire_text::Selection::cursor(end));
        run(&mut ws, Command::InsertSketch);
        let svg = NotePath::new("attachments/A-ink-1.svg");
        assert!(ws.vault().unwrap().read(&svg).unwrap().contains("data-quire-sketch"));
        assert!(ws.active_doc().unwrap().text().ends_with("text\n![[A-ink-1.svg]]\n"), "{}", ws.active_doc().unwrap().text());
        assert_eq!(ws.take_ui_requests(), vec![UiRequest::OpenFile(svg)]);
        assert_eq!(run(&mut ws, Command::InsertCitation), Some(UiRequest::Feature(Command::InsertCitation)));
    }

    #[test]
    fn links_to_files_open_them_and_pdf_links_jump() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[
            ("N.md", "x"),
            ("papers/paper.pdf", "%PDF-1.7"),
            (
                "papers/paper (annotations).md",
                "> [!quote] p. 3 · [[paper.pdf#page=3&annot=h1]]\n> q\n^h1\n\n<!-- quire:pdf-anchor {\"p\":3,\"q\":[],\"t\":\"q\"} -->\n",
            ),
            ("B.canvas", "{}"),
        ]));
        assert!(!open_file_link(&mut ws, "N", None), "notes aren't files");
        assert!(!open_file_link(&mut ws, "missing.pdf", None));
        assert!(open_file_link(&mut ws, "B.canvas", None));
        assert_eq!(ws.take_ui_requests(), vec![UiRequest::OpenFile("B.canvas".into())]);
        assert!(open_file_link(&mut ws, "paper.pdf", Some("page=3&annot=h1")));
        if quire_pdf::CAN_RENDER {
            assert_eq!(
                ws.features.pdf.jump,
                Some(crate::pdf_state::Jump { path: "papers/paper.pdf".into(), page: Some(3), annot: Some("h1".into()) })
            );
            assert_eq!(ws.take_ui_requests(), vec![UiRequest::OpenFile("papers/paper.pdf".into())]);
        } else {
            // Lite: the annotation note, at the highlight.
            let d = ws.active_doc().unwrap();
            assert_eq!(d.path.as_ref().map(NotePath::as_str), Some("papers/paper (annotations).md"));
        }
    }
}
