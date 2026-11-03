// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text in images and scanned PDF pages (design §D.3).
//!
//! A background job that runs only when "Read text in images and scanned PDFs" is on and its
//! model is installed, and only while the person is idle. Results go to the
//! index as attachment text (source `Ocr`) and are cached in
//! `.quire/cache/ocr/<sha256>.json`, so each file is read once.
//!
//! PDFs: WS-G's PDF text job lists the pages that are only a picture
//! (`pdf_state::take_ocr_requests`); this job reads them and hands the text
//! back (`pdf_state::ocr_result`), which caches it with the PDF's text and
//! sends the whole PDF to the index again. Pages with text stay as they are.

use std::collections::{HashSet, VecDeque};

use crossbeam_channel::{Receiver, Sender};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quire_ai::local::{LocalOcr, LocalSetup, ocr_cache_path, ocr_text, read_ocr_cache, write_ocr_cache};
use quire_index::attach::{AttachmentKind, AttachmentText, TextSource};
use quire_intel::chat::CancelFlag;
use quire_vault::{NotePath, VaultEvent};

use crate::jobs::{Budget, Job, JobCx, Needs, Step};
use crate::pdf_state::OcrRequest;
use crate::workspace::Workspace;

/// Something to read: an image, or one page of a PDF.
enum Item {
    Image(NotePath),
    Page(OcrRequest),
}

type Queue = Arc<Mutex<VecDeque<Item>>>;

pub(crate) struct OcrState {
    queue: Queue,
    /// Recognized PDF pages, for `pdf_state::ocr_result` on the UI thread.
    pages_tx: Sender<(OcrRequest, String)>,
    pages_rx: Receiver<(OcrRequest, String)>,
    /// The job was added for this vault.
    running: bool,
    scanned: bool,
}

impl Default for OcrState {
    fn default() -> Self {
        let (pages_tx, pages_rx) = crossbeam_channel::unbounded();
        Self { queue: Queue::default(), pages_tx, pages_rx, running: false, scanned: false }
    }
}

/// PDF pages handed to the job at a time (more wait in `pdf_state`).
const PAGES_AHEAD: usize = 4;

fn wants(p: &NotePath) -> bool {
    let s = p.as_str().to_ascii_lowercase();
    crate::attach::is_image(&s) && !std::path::Path::new(&s).extension().is_some_and(|x| x.eq_ignore_ascii_case("svg"))
}

fn enabled(ws: &Workspace) -> bool {
    let a = &ws.settings.ai_local;
    a.ocr_on()
        && super::catalog().of(quire_ai::catalog::Purpose::Ocr).next().is_some_and(super::installed)
        && super::engine_installed()
        && ws.vault_root().is_some()
}

/// Start the job and queue the vault's images once OCR is on and installed.
pub(crate) fn tick(ws: &mut Workspace) {
    if !enabled(ws) {
        return;
    }
    if !ws.features.ai.ocr.scanned {
        ws.features.ai.ocr.scanned = true;
        let files: Vec<Item> = ws.other_files().into_iter().filter(wants).map(Item::Image).collect();
        lock(&ws.features.ai.ocr.queue).extend(files);
        // Scanned pages of PDFs read before OCR was on.
        crate::pdf_state::queue_ocr(ws);
    }
    // Text read from PDF pages goes back to the PDF's text.
    let done: Vec<(OcrRequest, String)> = ws.features.ai.ocr.pages_rx.try_iter().collect();
    for (req, text) in done {
        crate::pdf_state::ocr_result(ws, &req, &text);
    }
    let waiting = lock(&ws.features.ai.ocr.queue).iter().filter(|i| matches!(i, Item::Page(_))).count();
    if waiting < PAGES_AHEAD {
        let more = crate::pdf_state::take_ocr_requests(ws, PAGES_AHEAD - waiting);
        lock(&ws.features.ai.ocr.queue).extend(more.into_iter().map(Item::Page));
    }
    if !ws.features.ai.ocr.running {
        let (Some(root), Some(setup), Some(entry)) = (
            ws.vault_root().map(PathBuf::from),
            super::setup(&ws.settings),
            super::catalog().of(quire_ai::catalog::Purpose::Ocr).next().cloned(),
        ) else {
            return;
        };
        ws.features.ai.ocr.running = true;
        let job = OcrJob {
            queue: Arc::clone(&ws.features.ai.ocr.queue),
            pages: ws.features.ai.ocr.pages_tx.clone(),
            ocr: LocalOcr::new(super::engine(&ws.settings), &entry, &setup),
            model: entry.id.clone(),
            root,
            _setup: setup,
            failures: 0,
            seen: HashSet::new(),
        };
        ws.jobs.add(Box::new(job));
    }
}

/// New and changed images are read again (changed PDFs come back through
/// `pdf_state` once their text is read).
pub(crate) fn vault_event(ws: &Workspace, ev: &VaultEvent) {
    if let VaultEvent::AttachmentChanged(p) = ev
        && wants(p)
        && ws.settings.ai_local.ocr_on()
    {
        lock(&ws.features.ai.ocr.queue).push_back(Item::Image(p.clone()));
    }
}

/// A new vault: start over (the job of the old one stops with its context).
pub(crate) fn reset(ws: &mut Workspace) {
    ws.features.ai.ocr = OcrState::default();
}

fn lock(q: &Queue) -> std::sync::MutexGuard<'_, VecDeque<Item>> {
    q.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct OcrJob {
    queue: Queue,
    pages: Sender<(OcrRequest, String)>,
    ocr: LocalOcr,
    model: String,
    root: PathBuf,
    _setup: LocalSetup,
    failures: u32,
    /// (path, hash) already handled while this vault is open.
    seen: HashSet<(NotePath, String)>,
}

impl Job for OcrJob {
    fn id(&self) -> &'static str {
        "ocr"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 10_000, max_calls_per_day: 0, needs: Needs::Idle }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        let Some(item) = lock(&self.queue).pop_front() else { return Step::Idle(Duration::from_secs(5)) };
        let cancel = CancelFlag::new();
        let path = match item {
            Item::Image(p) => p,
            Item::Page(req) => {
                if cx.cancel.is_cancelled() {
                    return Step::Done;
                }
                let file = self.root.join(req.path.as_str());
                return match self.ocr.pdf_page(&file, req.page, &cancel) {
                    Ok(lines) => {
                        self.failures = 0;
                        let _ = self.pages.send((req, ocr_text(&lines)));
                        Step::Continue
                    }
                    Err(quire_ai::engine::EngineError::NotInstalled) => Step::Done,
                    Err(_) => {
                        // Try the page again later.
                        lock(&self.queue).push_back(Item::Page(req));
                        self.failed()
                    }
                };
            }
        };
        let file = self.root.join(path.as_str());
        let Ok(hash) = quire_ai::download::sha256_file(&file) else { return Step::Continue };
        if !self.seen.insert((path.clone(), hash.clone())) {
            return Step::Continue;
        }
        let cache = ocr_cache_path(&self.root.join(".quire").join("cache"), &hash);
        let pages = if let Some(p) = read_ocr_cache(&cache, &self.model) {
            p
        } else {
            match self.ocr.image(&file, &cancel) {
                Ok(lines) => {
                    let pages = vec![ocr_text(&lines)];
                    let _ = write_ocr_cache(&cache, &self.model, &pages);
                    pages
                }
                Err(quire_ai::engine::EngineError::NotInstalled) => return Step::Done,
                Err(_) => return self.failed(),
            }
        };
        self.failures = 0;
        let text = AttachmentText { path, kind: AttachmentKind::Image, pages, source_hash: hash, by: TextSource::Ocr };
        cx.index.write().unwrap_or_else(std::sync::PoisonError::into_inner).upsert_attachment(text);
        Step::Continue
    }
}

impl OcrJob {
    /// After three failures in a row, wait ten minutes (a broken model or
    /// engine shouldn't spin).
    fn failed(&mut self) -> Step {
        self.failures += 1;
        if self.failures >= 3 {
            self.failures = 0;
            return Step::Idle(Duration::from_mins(10));
        }
        Step::Continue
    }
}
