// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDFs in the vault (design §G.2). Owned by WS-G; kept in `ws.features.pdf`.
//!
//! * **Search.** A background job reads each PDF's text (cached in
//!   `.quire/cache/pdf-text/<sha256>.json`) and gives it to the index as an
//!   attachment, one string per page, so search can say "paper.pdf p. 3".
//!   Pages that are only a picture are queued for OCR when on-device OCR is
//!   on (`ai_local.ocr`); [`take_ocr_requests`] and [`ocr_result`] are the
//!   hand-over to the OCR engine.
//! * **Highlights** live in `<stem> (annotations).md` beside the PDF
//!   (`quire_pdf::annot`); [`add_highlight`] and friends edit that note.
//! * **Links** `[[paper.pdf#page=3&annot=h7f3a]]` open the viewer at the
//!   highlight (full edition) or the annotation note at `^h7f3a` (Lite).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use quire_index::{AttachmentKind, AttachmentText, TextSource};
use quire_pdf::annot::{self, PdfAnchor};
use quire_vault::{NotePath, Vault, VaultEvent};
use serde::{Deserialize, Serialize};

use crate::jobs::{Budget, Job, JobCx, Needs, Step};
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// The cache folder for extracted text.
pub const CACHE_DIR: &str = ".quire/cache/pdf-text";

/// What the cache keeps per PDF (docs/formats/pdf-annotations.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedText {
    pub format: u32,
    pub sha256: String,
    pub pages: Vec<String>,
    /// 1-based pages that are only a picture.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_only: Vec<u32>,
    /// 1-based pages whose text came from OCR.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ocr: Vec<u32>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

pub fn cache_path(sha256: &str) -> NotePath {
    NotePath::new(format!("{CACHE_DIR}/{sha256}.json"))
}

/// A page waiting for OCR: the PDF and the page (0-based), as the engine's
/// `ocr` request takes them (`quire_ai::protocol::PdfPage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrRequest {
    pub path: NotePath,
    pub page: u32,
    pub sha256: String,
}

/// Where to go when a PDF opens (a deep link), for the viewer to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jump {
    pub path: NotePath,
    /// 1-based.
    pub page: Option<u32>,
    pub annot: Option<String>,
}

/// State for this feature.
#[derive(Default)]
pub struct PdfState {
    queue_tx: Option<Sender<NotePath>>,
    done_rx: Option<Receiver<Done>>,
    /// Needs the vault, which is set after `vault_opened`: done on the next tick.
    needs_start: bool,
    /// PDFs and their text, as last sent to the index.
    pub texts: std::collections::HashMap<NotePath, CachedText>,
    pub ocr_queue: VecDeque<OcrRequest>,
    /// A deep link waiting for the viewer.
    pub jump: Option<Jump>,
    /// Bumped whenever an annotation note changes, so open viewers re-read it.
    pub annotations_generation: u64,
}

impl std::fmt::Debug for PdfState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfState").field("texts", &self.texts.len()).field("ocr_queue", &self.ocr_queue.len()).finish_non_exhaustive()
    }
}

/// A finished extraction.
struct Done {
    path: NotePath,
    text: Result<CachedText, String>,
}

pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.pdf = PdfState { needs_start: true, ..PdfState::default() };
}

fn is_pdf(p: &NotePath) -> bool {
    p.as_str().to_ascii_lowercase().ends_with(".pdf")
}

/// Start the text job and queue every PDF in the vault.
fn start(ws: &mut Workspace) {
    let Some(vault) = ws.vault.clone() else { return };
    let (qtx, qrx) = crossbeam_channel::unbounded();
    let (dtx, drx) = crossbeam_channel::unbounded();
    for p in vault.storage().list().unwrap_or_default().into_iter().filter(is_pdf) {
        let _ = qtx.send(p);
    }
    ws.jobs.add(Box::new(PdfTextJob { queue: qrx, pending: VecDeque::new(), done: dtx, current: None }));
    ws.features.pdf.queue_tx = Some(qtx);
    ws.features.pdf.done_rx = Some(drx);
}

pub(crate) fn tick(ws: &mut Workspace, _now: Instant) -> bool {
    if std::mem::take(&mut ws.features.pdf.needs_start) {
        start(ws);
    }
    let mut changed = false;
    let done: Vec<Done> = ws.features.pdf.done_rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
    for d in done {
        changed = true;
        match d.text {
            Ok(t) => {
                if ws.settings.ai_local.ocr {
                    for p in &t.image_only {
                        if !t.ocr.contains(p) {
                            ws.features.pdf.ocr_queue.push_back(OcrRequest { path: d.path.clone(), page: p - 1, sha256: t.sha256.clone() });
                        }
                    }
                }
                send_to_index(ws, &d.path, &t);
                ws.features.pdf.texts.insert(d.path, t);
            }
            Err(e) => ws.notify(NoticeLevel::Warn, format!("Couldn't read the text of {}: {e}", d.path)),
        }
    }
    changed
}

fn attachment(path: &NotePath, t: &CachedText) -> AttachmentText {
    AttachmentText {
        path: path.clone(),
        kind: AttachmentKind::Pdf,
        pages: t.pages.clone(),
        source_hash: t.sha256.clone(),
        by: if t.ocr.is_empty() { TextSource::Embedded } else { TextSource::Ocr },
    }
}

fn send_to_index(ws: &Workspace, path: &NotePath, t: &CachedText) {
    ws.index.upsert_attachment(attachment(path, t));
}

pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    match ev {
        VaultEvent::AttachmentChanged(p) if is_pdf(p) => {
            if let Some(tx) = &ws.features.pdf.queue_tx {
                let _ = tx.send(p.clone());
            }
        }
        VaultEvent::AttachmentRemoved(p) if is_pdf(p) => {
            ws.features.pdf.texts.remove(p);
            ws.features.pdf.ocr_queue.retain(|r| r.path != *p);
            ws.index.remove_attachment(p.clone());
        }
        VaultEvent::Changed(p) | VaultEvent::Removed(p) if p.stem().ends_with(" (annotations)") => {
            ws.features.pdf.annotations_generation += 1;
        }
        _ => {}
    }
}

/// Queue every scanned page not read yet (after on-device OCR is turned on).
pub fn queue_ocr(ws: &mut Workspace) {
    let mut q: Vec<OcrRequest> = Vec::new();
    for (path, t) in &ws.features.pdf.texts {
        for p in t.image_only.iter().filter(|p| !t.ocr.contains(p)) {
            q.push(OcrRequest { path: path.clone(), page: p - 1, sha256: t.sha256.clone() });
        }
    }
    q.sort_by(|a, b| a.path.cmp(&b.path).then(a.page.cmp(&b.page)));
    for r in q {
        if !ws.features.pdf.ocr_queue.contains(&r) {
            ws.features.pdf.ocr_queue.push_back(r);
        }
    }
}

/// Pages waiting for OCR (taken by the on-device OCR job).
pub fn take_ocr_requests(ws: &mut Workspace, max: usize) -> Vec<OcrRequest> {
    let n = max.min(ws.features.pdf.ocr_queue.len());
    ws.features.pdf.ocr_queue.drain(..n).collect()
}

/// Text recognized on a page: kept in the cache and sent to the index.
pub fn ocr_result(ws: &mut Workspace, req: &OcrRequest, text: &str) {
    let Some(vault) = ws.vault.clone() else { return };
    let Some(t) = ws.features.pdf.texts.get_mut(&req.path) else { return };
    if t.sha256 != req.sha256 {
        return; // the PDF changed meanwhile
    }
    let i = req.page as usize;
    if let Some(slot) = t.pages.get_mut(i) {
        slot.clear();
        slot.push_str(text.trim());
    }
    if !t.ocr.contains(&(req.page + 1)) {
        t.ocr.push(req.page + 1);
    }
    let t = t.clone();
    write_cache(&vault, &t);
    send_to_index(ws, &req.path, &t);
}

fn write_cache(vault: &Vault, t: &CachedText) {
    if let Ok(json) = serde_json::to_string(t) {
        let _ = vault.storage().write_atomic(&cache_path(&t.sha256), &json);
    }
}

fn read_cache(vault: &Vault, sha: &str) -> Option<CachedText> {
    let text = vault.storage().read(&cache_path(sha)).ok()?;
    let t: CachedText = serde_json::from_str(&text).ok()?;
    (t.format == 1 && t.sha256 == sha).then_some(t)
}

/// One PDF being read, page by page across steps.
struct Current {
    path: NotePath,
    sha: String,
    doc: quire_pdf::PdfDoc,
    pages: Vec<String>,
    image_only: Vec<u32>,
}

/// The `pdf-text` job: reads PDFs' text off the UI thread, a few pages a step.
struct PdfTextJob {
    queue: Receiver<NotePath>,
    pending: VecDeque<NotePath>,
    done: Sender<Done>,
    current: Option<Current>,
}

impl PdfTextJob {
    fn begin(&mut self, vault: &Vault, path: NotePath) -> Option<Done> {
        // Gone again (a temporary file): nothing to say.
        let Ok(bytes) = vault.storage().read_bytes(&path) else { return None };
        let sha = quire_pdf::sha256_hex(&bytes);
        if let Some(t) = read_cache(vault, &sha) {
            return Some(Done { path, text: Ok(t) });
        }
        match quire_pdf::PdfDoc::open(bytes) {
            Ok(doc) => {
                self.current = Some(Current { path, sha, doc, pages: Vec::new(), image_only: Vec::new() });
                None
            }
            Err(e) => Some(Done { path, text: Err(e.to_string()) }),
        }
    }
}

impl Job for PdfTextJob {
    fn id(&self) -> &'static str {
        "pdf-text"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 20_000, max_calls_per_day: 0, needs: Needs::Any }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        if cx.cancel.is_cancelled() {
            return Step::Done;
        }
        // Several changes to one file in a row: read it once.
        for p in self.queue.try_iter() {
            if !self.pending.contains(&p) {
                self.pending.push_back(p);
            }
        }
        if self.current.is_none() {
            let Some(path) = self.pending.pop_front() else { return Step::Idle(Duration::from_secs(2)) };
            if let Some(d) = self.begin(&cx.vault, path) {
                let _ = self.done.send(d);
            }
            return Step::Continue;
        }
        let start = Instant::now();
        let finished = {
            let Some(cur) = self.current.as_mut() else { return Step::Continue };
            while cur.pages.len() < cur.doc.page_count() && start.elapsed() < Duration::from_millis(15) {
                let i = cur.pages.len();
                let page = cur.doc.page_text(i).unwrap_or_default();
                if page.image_only {
                    cur.image_only.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
                }
                cur.pages.push(page.text());
            }
            cur.pages.len() >= cur.doc.page_count()
        };
        if finished && let Some(cur) = self.current.take() {
            let t = CachedText {
                format: 1,
                sha256: cur.sha,
                pages: cur.pages,
                image_only: cur.image_only,
                ocr: Vec::new(),
                other: serde_json::Map::new(),
            };
            write_cache(&cx.vault, &t);
            let _ = self.done.send(Done { path: cur.path, text: Ok(t) });
        }
        Step::Continue
    }
}

// ---------------------------------------------------------------- without a workspace

/// The text of the PDF at `path`, from the cache or read now (and cached).
/// `None` when the file is gone or isn't a PDF Quire can read.
pub fn read_text(vault: &Vault, path: &NotePath) -> Option<CachedText> {
    let bytes = vault.storage().read_bytes(path).ok()?;
    let sha = quire_pdf::sha256_hex(&bytes);
    if let Some(t) = read_cache(vault, &sha) {
        return Some(t);
    }
    let doc = quire_pdf::PdfDoc::open(bytes).ok()?;
    let mut pages = Vec::with_capacity(doc.page_count());
    let mut image_only = Vec::new();
    for i in 0..doc.page_count() {
        let page = doc.page_text(i).unwrap_or_default();
        if page.image_only {
            image_only.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
        }
        pages.push(page.text());
    }
    let t = CachedText { format: 1, sha256: sha, pages, image_only, ocr: Vec::new(), other: serde_json::Map::new() };
    write_cache(vault, &t);
    Some(t)
}

/// Give `index` the text of every PDF in `vault` (for the CLI and the MCP
/// server, which have no background jobs; the app uses the `pdf-text`
/// job). Returns how many PDFs were sent.
pub fn index_pdfs(vault: &Vault, index: &quire_index::IndexService) -> usize {
    let mut n = 0;
    for p in vault.storage().list().unwrap_or_default().into_iter().filter(is_pdf) {
        if let Some(t) = read_text(vault, &p) {
            index.upsert_attachment(attachment(&p, &t));
            n += 1;
        }
    }
    n
}

/// A PDF changed or went away: update `index` (the MCP server's watcher).
pub fn pdf_changed(vault: &Vault, index: &quire_index::IndexService, path: &NotePath) {
    if !is_pdf(path) {
        return;
    }
    match read_text(vault, path) {
        Some(t) => index.upsert_attachment(attachment(path, &t)),
        None => index.remove_attachment(path.clone()),
    }
}

// ---------------------------------------------------------------- annotation notes

/// Write `text` to the note at `path`: through the open document when there
/// is one (so Undo works and nothing races), else to the file.
pub fn write_note(ws: &mut Workspace, path: &NotePath, text: &str) -> Result<(), String> {
    if let Some(i) = ws.docs.iter().position(|d| d.path.as_ref() == Some(path)) {
        let doc = &mut ws.docs[i];
        let old = doc.text();
        if &*old == text {
            return Ok(());
        }
        // Replace only what differs, so the caret and undo stay sensible.
        let prefix = old.bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count();
        let prefix = (0..=prefix).rev().find(|i| old.is_char_boundary(*i) && text.is_char_boundary(*i)).unwrap_or(0);
        let max_suffix = old.len().min(text.len()) - prefix;
        let suffix = old.bytes().rev().zip(text.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
        let suffix = (0..=suffix).rev().find(|s| old.is_char_boundary(old.len() - s) && text.is_char_boundary(text.len() - s)).unwrap_or(0);
        let from = doc.byte_to_char(prefix);
        let to = doc.byte_to_char(old.len() - suffix);
        let tx = quire_text::Transaction::replace(from, to, text[prefix..text.len() - suffix].to_string());
        doc.apply(tx, quire_text::EditKind::Other).map_err(|_| "The note couldn't be changed".to_string())?;
        let id = doc.id;
        ws.save(id);
    } else {
        let vault = ws.vault.clone().ok_or("No vault is open")?;
        vault.write(path, text).map_err(|e| e.to_string())?;
        ws.index.upsert(path.clone(), text.to_string());
    }
    ws.features.pdf.annotations_generation += 1;
    Ok(())
}

/// The current text of a note: the open document's, else the file's.
pub fn read_note(ws: &Workspace, path: &NotePath) -> Option<String> {
    if let Some(d) = ws.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
        return Some(d.text().to_string());
    }
    ws.vault.as_ref()?.read(path).ok()
}

/// The highlights of a PDF (from its annotation note).
pub fn highlights(ws: &Workspace, pdf: &NotePath) -> Vec<annot::Entry> {
    read_note(ws, &annot::note_for(pdf)).map(|t| annot::entries(&t)).unwrap_or_default()
}

/// Add a highlight to the PDF's annotation note (made when missing).
/// Returns its id.
pub fn add_highlight(ws: &mut Workspace, pdf: &NotePath, a: &PdfAnchor, quote: &str, comment: &str) -> Result<String, String> {
    let note_path = annot::note_for(pdf);
    let current = read_note(ws, &note_path).unwrap_or_else(|| {
        let sha = ws.vault.as_ref().and_then(|v| v.storage().read_bytes(pdf).ok()).map(|b| quire_pdf::sha256_hex(&b)).unwrap_or_default();
        annot::new_note(pdf, &sha)
    });
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.as_nanos() as u64);
    let id = annot::new_id(&current, seed);
    let text = annot::add(&current, pdf, &id, a, quote, comment);
    write_note(ws, &note_path, &text)?;
    Ok(id)
}

/// Change a highlight's comment.
pub fn set_comment(ws: &mut Workspace, pdf: &NotePath, id: &str, comment: &str) -> Result<(), String> {
    let note_path = annot::note_for(pdf);
    let current = read_note(ws, &note_path).ok_or("The annotation note is missing")?;
    let text = annot::set_comment(&current, id, comment).ok_or("That highlight is no longer in the note")?;
    write_note(ws, &note_path, &text)
}

/// Remove a highlight.
pub fn remove_highlight(ws: &mut Workspace, pdf: &NotePath, id: &str) -> Result<(), String> {
    let note_path = annot::note_for(pdf);
    let current = read_note(ws, &note_path).ok_or("The annotation note is missing")?;
    let text = annot::remove(&current, id).ok_or("That highlight is no longer in the note")?;
    write_note(ws, &note_path, &text)
}

/// Re-anchor every highlight when the PDF changed since they were made.
/// Returns the ids that couldn't be found ("detached"), or `None` when
/// nothing needed doing.
pub fn reanchor_all(ws: &mut Workspace, pdf: &NotePath, sha256: &str, pages: &[quire_pdf::PageText]) -> Option<Vec<String>> {
    let note_path = annot::note_for(pdf);
    let current = read_note(ws, &note_path)?;
    if annot::pdf_sha(&current).is_some_and(|s| s == sha256) {
        return None;
    }
    let mut text = current.clone();
    let mut detached = Vec::new();
    for e in annot::entries(&current) {
        match quire_pdf::find::reanchor(&e.anchor, pages) {
            Some(a) if a == e.anchor => {}
            Some(a) => {
                if let Some(t) = annot::move_anchor(&text, pdf, &e.id, &a) {
                    text = t;
                }
            }
            None => detached.push(e.id.clone()),
        }
    }
    // The note says it matches this version only once every highlight was
    // found in it: detached ones keep their old place and are looked for
    // (and listed as detached) again next time.
    if detached.is_empty() {
        text = annot::set_pdf_sha(&text, sha256);
    }
    if text != current
        && let Err(e) = write_note(ws, &note_path, &text)
    {
        ws.notify(NoticeLevel::Warn, format!("Couldn't update the highlights of {pdf}: {e}"));
    }
    Some(detached)
}

/// `[[paper.pdf#page=3&annot=h7f3a]]`: open the PDF there. In Lite (no
/// viewer) the annotation note opens at the highlight, or the page is said.
pub fn open_link(ws: &mut Workspace, path: &NotePath, fragment: Option<&str>) {
    let (page, annot_id) = fragment.map(annot::parse_fragment).unwrap_or_default();
    if quire_pdf::CAN_RENDER {
        ws.features.pdf.jump = Some(Jump { path: path.clone(), page, annot: annot_id });
        ws.request_ui(UiRequest::OpenFile(path.clone()));
        return;
    }
    if let Some(id) = annot_id {
        let note = annot::note_for(path);
        if ws.vault.as_ref().is_some_and(|v| v.exists(&note)) && ws.open_note(&note).is_some() {
            ws.jump_to_heading(&format!("^{id}"));
            ws.request_ui(UiRequest::RevealCaret);
            return;
        }
    }
    if let Some(p) = page {
        ws.notify(NoticeLevel::Info, format!("{} opens on page 1 here; the link points to page {p}.", path.file_name()));
    }
    ws.request_ui(UiRequest::OpenFile(path.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    #[test]
    fn highlights_go_into_the_annotation_note() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("papers/rope.pdf", "%PDF-1.7 not really")]));
        let pdf = NotePath::new("papers/rope.pdf");
        let a = PdfAnchor {
            p: 2,
            q: vec![[1.0, 2.0, 3.0, 4.0]],
            t: "Ropes".into(),
            pre: String::new(),
            suf: String::new(),
            c: "yellow".into(),
            other: serde_json::Map::new(),
        };
        let id = add_highlight(&mut ws, &pdf, &a, "Ropes", "Nice").unwrap();
        let note = ws.vault().unwrap().read(&"papers/rope (annotations).md".into()).unwrap();
        assert!(note.starts_with("---\nannotates: \"[[rope.pdf]]\"\npdf_sha256: "), "{note}");
        assert!(note.contains(&format!("[[rope.pdf#page=2&annot={id}]]")));
        assert_eq!(highlights(&ws, &pdf)[0].comment, "Nice");
        // With the note open, edits go through the document.
        let doc = ws.open_note(&annot::note_for(&pdf)).unwrap();
        set_comment(&mut ws, &pdf, &id, "Better").unwrap();
        assert_eq!(annot::entries(&ws.doc(doc).unwrap().text())[0].comment, "Better");
        remove_highlight(&mut ws, &pdf, &id).unwrap();
        assert!(highlights(&ws, &pdf).is_empty());
        assert!(ws.vault().unwrap().read(&annot::note_for(&pdf)).unwrap().contains("# Highlights: rope"));
    }

    fn page(text: &str) -> quire_pdf::PageText {
        let mut p = quire_pdf::PageText::default();
        for (k, c) in text.chars().enumerate() {
            let x = 72.0 + 6.0 * k as f32;
            p.chars.push((c, [x, 700.0, x + 6.0, 710.0]));
        }
        p.lines.push(quire_pdf::text::TextLine { text: text.to_string(), quad: [72.0, 700.0, 400.0, 710.0] });
        p
    }

    #[test]
    fn review_reanchoring_keeps_the_old_sha_while_highlights_are_detached() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("rope.pdf", "%PDF-1.7 not really")]));
        let pdf = NotePath::new("rope.pdf");
        let a = PdfAnchor {
            p: 1,
            q: vec![[72.0, 700.0, 102.0, 710.0]],
            t: "Ropes".into(),
            pre: String::new(),
            suf: String::new(),
            c: "yellow".into(),
            other: serde_json::Map::new(),
        };
        let id = add_highlight(&mut ws, &pdf, &a, "Ropes", "").unwrap();
        let sha = |ws: &Workspace| annot::pdf_sha(&read_note(ws, &annot::note_for(&pdf)).unwrap());
        let old = sha(&ws);
        // The PDF changed and the highlight's text is gone from it.
        let gone = [page("Nothing about it here")];
        assert_eq!(reanchor_all(&mut ws, &pdf, "new", &gone), Some(vec![id.clone()]));
        assert_eq!(sha(&ws), old, "the note doesn't claim to match the new file");
        assert_eq!(reanchor_all(&mut ws, &pdf, "new", &gone), Some(vec![id]), "listed as detached again");
        // Once every highlight is found, the new version is recorded.
        assert_eq!(reanchor_all(&mut ws, &pdf, "new", &[page("Ropes are here")]), Some(vec![]));
        assert_eq!(sha(&ws).as_deref(), Some("new"));
        assert_eq!(reanchor_all(&mut ws, &pdf, "new", &[page("Ropes are here")]), None);
    }

    #[test]
    fn the_job_reads_text_once_and_caches_it() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../quire-pdf/tests/fixtures");
        let bytes = std::fs::read(dir.join("text.pdf")).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("paper.pdf"), &bytes).unwrap();
        let vault = Vault::open_local(tmp.path()).unwrap();
        let (qtx, qrx) = crossbeam_channel::unbounded();
        let (dtx, drx) = crossbeam_channel::unbounded();
        let mut job = PdfTextJob { queue: qrx, pending: VecDeque::new(), done: dtx, current: None };
        let index = quire_index::IndexService::spawn();
        let cx = JobCx {
            vault: vault.clone(),
            index: index.handle(),
            agent: quire_agent::AgentStore::open(
                &vault,
                quire_agent::DeviceInfo { id: "d".into(), name: "test".into() },
                std::sync::Arc::new(quire_agent::SystemClock),
            ),
            chat: None,
            user_active: std::sync::Arc::default(),
            cancel: quire_intel::chat::CancelFlag::new(),
        };
        qtx.send(NotePath::new("paper.pdf")).unwrap();
        qtx.send(NotePath::new("paper.pdf")).unwrap();
        for _ in 0..50 {
            if matches!(job.step(&cx), Step::Idle(_)) {
                break;
            }
        }
        let done: Vec<Done> = drx.try_iter().collect();
        assert_eq!(done.len(), 1, "the same file twice in a row is read once");
        let t = done[0].text.as_ref().unwrap();
        assert_eq!(t.pages.len(), 2);
        assert!(t.pages[0].contains("Ropes are balanced trees of strings."));
        assert!(vault.storage().exists(&cache_path(&t.sha256)), "cached");
        assert_eq!(read_cache(&vault, &t.sha256).as_ref(), Some(t));
        qtx.send(NotePath::new("paper.pdf")).unwrap();
        job.step(&cx);
        assert!(drx.try_recv().unwrap().text.is_ok(), "the second time comes from the cache");
    }
}
