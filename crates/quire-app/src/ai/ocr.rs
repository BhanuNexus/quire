// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text in images and scanned PDF pages (design §D.3).
//!
//! A background job that runs only when "Read text in images and scanned PDFs" is on and its
//! model is installed, and only while the person is idle. Results go to the
//! index as attachment text (source `Ocr`) and are cached in
//! `.quire/cache/ocr/<sha256>.json`, so each file is read once.
//!
//! PDFs: pages the PDF text extractor found empty (a scan) are read; pages
//! with text are kept as they are.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quire_ai::local::{LocalOcr, LocalSetup, ocr_cache_path, ocr_text, read_ocr_cache, write_ocr_cache};
use quire_index::attach::{AttachmentKind, AttachmentText, TextSource};
use quire_intel::chat::CancelFlag;
use quire_vault::{NotePath, VaultEvent};

use crate::jobs::{Budget, Job, JobCx, Needs, Step};
use crate::workspace::Workspace;

type Queue = Arc<Mutex<VecDeque<NotePath>>>;

#[derive(Default)]
pub(crate) struct OcrState {
    queue: Queue,
    /// The job was added for this vault.
    running: bool,
    scanned: bool,
}

fn wants(p: &NotePath) -> bool {
    let s = p.as_str().to_ascii_lowercase();
    let ext = |e: &str| std::path::Path::new(&s).extension().is_some_and(|x| x.eq_ignore_ascii_case(e));
    (crate::attach::is_image(&s) && !ext("svg")) || ext("pdf")
}

fn enabled(ws: &Workspace) -> bool {
    let a = &ws.settings.ai_local;
    a.ocr
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
        let files: Vec<NotePath> = ws.other_files().into_iter().filter(wants).collect();
        lock(&ws.features.ai.ocr.queue).extend(files);
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

/// New and changed images and PDFs are read again.
pub(crate) fn vault_event(ws: &Workspace, ev: &VaultEvent) {
    if let VaultEvent::AttachmentChanged(p) = ev
        && wants(p)
        && ws.settings.ai_local.ocr
    {
        lock(&ws.features.ai.ocr.queue).push_back(p.clone());
    }
}

/// A new vault: start over (the job of the old one stops with its context).
pub(crate) fn reset(ws: &mut Workspace) {
    ws.features.ai.ocr = OcrState::default();
}

fn lock(q: &Queue) -> std::sync::MutexGuard<'_, VecDeque<NotePath>> {
    q.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct OcrJob {
    queue: Queue,
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
        let Some(path) = lock(&self.queue).pop_front() else { return Step::Idle(Duration::from_secs(5)) };
        let file = self.root.join(path.as_str());
        let Ok(hash) = quire_ai::download::sha256_file(&file) else { return Step::Continue };
        if !self.seen.insert((path.clone(), hash.clone())) {
            return Step::Continue;
        }
        let cache = ocr_cache_path(&self.root.join(".quire").join("cache"), &hash);
        let is_pdf = path.as_str().to_ascii_lowercase().ends_with(".pdf");
        let cancel = CancelFlag::new();
        let pages = if let Some(p) = read_ocr_cache(&cache, &self.model) {
            p
        } else if is_pdf {
            // Only pages the PDF text extractor found empty.
            let existing = cx.index.read().unwrap_or_else(std::sync::PoisonError::into_inner).attachment(&path).cloned();
            let Some(existing) = existing.filter(|e| e.by == TextSource::Embedded && e.source_hash == hash) else { return Step::Continue };
            if existing.pages.iter().all(|p| !p.trim().is_empty()) {
                return Step::Continue;
            }
            let mut pages = existing.pages;
            for (i, page) in pages.iter_mut().enumerate() {
                if !page.trim().is_empty() || cx.cancel.is_cancelled() {
                    continue;
                }
                match self.ocr.pdf_page(&file, u32::try_from(i).unwrap_or(u32::MAX), &cancel) {
                    Ok(lines) => *page = ocr_text(&lines),
                    Err(_) => return self.failed(),
                }
            }
            let _ = write_ocr_cache(&cache, &self.model, &pages);
            pages
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
        let text = AttachmentText {
            path,
            kind: if is_pdf { AttachmentKind::Pdf } else { AttachmentKind::Image },
            pages,
            source_hash: hash,
            by: TextSource::Ocr,
        };
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
