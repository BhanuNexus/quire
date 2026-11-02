// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-writer actor around [`Index`].
//!
//! Writes are messages processed on a worker thread: parsing happens there,
//! outside the lock, and the write lock is held only to swap records in.
//! Readers share the model through an `RwLock` and use [`IndexService::version`]
//! to know when cached query results are stale.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use quire_vault::{NotePath, Vault};

use crate::chunk::chunk_note;
use crate::index::Index;
use crate::record::NoteRecord;
use crate::semantic::{SemanticIndex, build_lsa_index};

/// Wait this long after the last change before rebuilding the semantic index.
const SEMANTIC_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(700);

enum SemMsg {
    Rebuild,
    Flush(Sender<()>),
    Shutdown,
}

pub enum IndexMsg {
    /// Index every note in a vault (replaces current contents).
    Load(Vault),
    Upsert {
        path: NotePath,
        source: String,
    },
    Remove(NotePath),
    /// The text of a PDF or image (from the PDF-text or OCR job).
    UpsertAttachment(crate::attach::AttachmentText),
    RemoveAttachment(NotePath),
    /// Replies once everything queued before it has been applied.
    Flush(Sender<()>),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStatus {
    pub indexed: usize,
    pub total: usize,
    pub busy: bool,
}

#[derive(Default)]
struct Shared {
    version: AtomicU64,
    indexed: AtomicUsize,
    total: AtomicUsize,
    busy: AtomicBool,
    semantic_version: AtomicU64,
    semantic_busy: AtomicBool,
}

type SharedSemantic = Arc<RwLock<Option<Arc<SemanticIndex>>>>;

/// Builds a semantic index from passages; `Err` falls back to the built-in LSA.
pub type SemanticBuilder = Arc<dyn Fn(Vec<crate::chunk::Chunk>) -> Result<SemanticIndex, String> + Send + Sync>;

#[derive(Default)]
struct SemanticConfig {
    builder: Option<SemanticBuilder>,
    last_error: Option<String>,
}

pub struct IndexService {
    index: Arc<RwLock<Index>>,
    tx: Sender<IndexMsg>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    semantic: SharedSemantic,
    sem_tx: Sender<SemMsg>,
    sem_worker: Option<JoinHandle<()>>,
    sem_config: Arc<RwLock<SemanticConfig>>,
}

impl IndexService {
    pub fn spawn() -> Self {
        let index = Arc::new(RwLock::new(Index::new()));
        let shared = Arc::new(Shared::default());
        let semantic: SharedSemantic = Arc::new(RwLock::new(None));
        let sem_config: Arc<RwLock<SemanticConfig>> = Arc::default();
        let (tx, rx) = unbounded();
        let (sem_tx, sem_rx) = unbounded();
        let worker = {
            let index = Arc::clone(&index);
            let shared = Arc::clone(&shared);
            let sem_tx = sem_tx.clone();
            std::thread::Builder::new()
                .name("quire-index".into())
                .spawn(move || run(&rx, &index, &shared, &sem_tx))
                .expect("spawn index worker")
        };
        let sem_worker = {
            let index = Arc::clone(&index);
            let shared = Arc::clone(&shared);
            let semantic = Arc::clone(&semantic);
            let cfg = Arc::clone(&sem_config);
            std::thread::Builder::new()
                .name("quire-semantic".into())
                .spawn(move || run_semantic(&sem_rx, &index, &shared, &semantic, &cfg))
                .expect("spawn semantic worker")
        };
        Self { index, tx, shared, worker: Some(worker), semantic, sem_tx, sem_worker: Some(sem_worker), sem_config }
    }

    /// Use a different embedder (e.g. a neural endpoint) for the semantic
    /// index, or `None` for the built-in LSA. Triggers a rebuild.
    pub fn set_semantic_builder(&self, builder: Option<SemanticBuilder>) {
        let mut c = self.sem_config.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        c.builder = builder;
        c.last_error = None;
        drop(c);
        let _ = self.sem_tx.send(SemMsg::Rebuild);
    }

    /// Why the configured embedder couldn't be used last time, if it failed.
    pub fn semantic_error(&self) -> Option<String> {
        self.sem_config.read().unwrap_or_else(std::sync::PoisonError::into_inner).last_error.clone()
    }

    /// The latest semantic index, if one has been built.
    /// The index itself, for reading from another thread.
    pub fn shared(&self) -> Arc<RwLock<Index>> {
        Arc::clone(&self.index)
    }

    pub fn semantic(&self) -> Option<Arc<SemanticIndex>> {
        self.semantic.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Increments after each semantic rebuild.
    pub fn semantic_version(&self) -> u64 {
        self.shared.semantic_version.load(Ordering::Acquire)
    }

    pub fn semantic_busy(&self) -> bool {
        self.shared.semantic_busy.load(Ordering::Relaxed)
    }

    /// Block until pending index work *and* the semantic rebuild it triggers are done.
    pub fn flush_semantic(&self) {
        self.flush();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let _ = self.sem_tx.send(SemMsg::Flush(done_tx));
        let _ = done_rx.recv();
    }

    pub fn send(&self, msg: IndexMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn load(&self, vault: Vault) {
        self.send(IndexMsg::Load(vault));
    }

    pub fn upsert(&self, path: NotePath, source: String) {
        self.send(IndexMsg::Upsert { path, source });
    }

    pub fn remove(&self, path: NotePath) {
        self.send(IndexMsg::Remove(path));
    }

    /// Store the extracted text of a PDF or image (replacing older text).
    pub fn upsert_attachment(&self, text: crate::attach::AttachmentText) {
        self.send(IndexMsg::UpsertAttachment(text));
    }

    pub fn remove_attachment(&self, path: NotePath) {
        self.send(IndexMsg::RemoveAttachment(path));
    }

    /// The shared model, for background jobs that outlive a borrow of the
    /// service. Hold the read lock briefly (design §9: at most 2 ms).
    pub fn handle(&self) -> Arc<RwLock<Index>> {
        Arc::clone(&self.index)
    }

    /// Read access to the current model. Hold briefly.
    pub fn read(&self) -> RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Increments after every applied batch.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }

    pub fn status(&self) -> IndexStatus {
        IndexStatus {
            indexed: self.shared.indexed.load(Ordering::Relaxed),
            total: self.shared.total.load(Ordering::Relaxed),
            busy: self.shared.busy.load(Ordering::Relaxed),
        }
    }

    /// Block until every message sent so far is applied (tests, CLI).
    pub fn flush(&self) {
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        self.send(IndexMsg::Flush(done_tx));
        let _ = done_rx.recv();
    }
}

impl Drop for IndexService {
    fn drop(&mut self) {
        let _ = self.tx.send(IndexMsg::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        let _ = self.sem_tx.send(SemMsg::Shutdown);
        if let Some(w) = self.sem_worker.take() {
            let _ = w.join();
        }
    }
}

fn parse_parallel(vault: &Vault, paths: &[NotePath], shared: &Shared) -> Vec<(NoteRecord, String)> {
    let threads = std::thread::available_parallelism().map_or(2, std::num::NonZero::get).clamp(1, 8);
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        paths
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter_map(|p| {
                            let src = vault.read(p).ok()?;
                            let rec = NoteRecord::from_source(p.clone(), &src);
                            shared.indexed.fetch_add(1, Ordering::Relaxed);
                            Some((rec, src))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// A message's work, done before taking the write lock.
enum Prepared {
    Note(Box<(NoteRecord, String)>),
    Remove(NotePath),
    Attachment(crate::attach::AttachmentText),
    RemoveAttachment(NotePath),
}

fn run(rx: &Receiver<IndexMsg>, index: &RwLock<Index>, shared: &Shared, sem_tx: &Sender<SemMsg>) {
    let write = || index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    while let Ok(first) = rx.recv() {
        shared.busy.store(true, Ordering::Relaxed);
        // Drain whatever else is queued so a burst of saves costs one graph rebuild.
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let mut prepared: Vec<Prepared> = Vec::new();
        let mut reload: Option<Vec<(NoteRecord, String)>> = None;
        let mut shutdown = false;
        let mut waiters = Vec::new();
        let mut changed = false;
        for msg in batch {
            match msg {
                IndexMsg::Load(vault) => {
                    changed = true;
                    let paths = vault.notes().unwrap_or_default();
                    shared.total.store(paths.len(), Ordering::Relaxed);
                    shared.indexed.store(0, Ordering::Relaxed);
                    reload = Some(parse_parallel(&vault, &paths, shared));
                    prepared.clear();
                }
                IndexMsg::Upsert { path, source } => {
                    changed = true;
                    prepared.push(Prepared::Note(Box::new((NoteRecord::from_source(path, &source), source))));
                }
                IndexMsg::Remove(path) => {
                    changed = true;
                    prepared.push(Prepared::Remove(path));
                }
                // Attachment text isn't in the semantic index yet: no rebuild.
                IndexMsg::UpsertAttachment(text) => prepared.push(Prepared::Attachment(text)),
                IndexMsg::RemoveAttachment(path) => prepared.push(Prepared::RemoveAttachment(path)),
                IndexMsg::Flush(reply) => waiters.push(reply),
                IndexMsg::Shutdown => shutdown = true,
            }
        }
        {
            let mut ix = write();
            if let Some(all) = reload {
                *ix = Index::new();
                for (rec, src) in all {
                    ix.insert_record(rec, src);
                }
            }
            for item in prepared {
                match item {
                    Prepared::Note(note) => {
                        let (rec, src) = *note;
                        ix.insert_record(rec, src);
                    }
                    Prepared::Remove(path) => ix.remove_no_refresh(&path),
                    Prepared::Attachment(text) => ix.upsert_attachment(text),
                    Prepared::RemoveAttachment(path) => ix.remove_attachment(&path),
                }
            }
            ix.refresh();
        }
        shared.busy.store(false, Ordering::Relaxed);
        shared.version.fetch_add(1, Ordering::Release);
        if changed {
            let _ = sem_tx.send(SemMsg::Rebuild);
        }
        for w in waiters {
            let _ = w.send(());
        }
        if shutdown {
            break;
        }
    }
}

fn run_semantic(rx: &Receiver<SemMsg>, index: &RwLock<Index>, shared: &Shared, out: &SharedSemantic, cfg: &RwLock<SemanticConfig>) {
    let mut waiters: Vec<Sender<()>> = Vec::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            SemMsg::Shutdown => return,
            SemMsg::Flush(r) => {
                let _ = r.send(());
                continue;
            }
            SemMsg::Rebuild => {}
        }
        // Debounce: keep absorbing rebuild requests until things go quiet.
        loop {
            match rx.recv_timeout(SEMANTIC_DEBOUNCE) {
                Ok(SemMsg::Rebuild) => {}
                Ok(SemMsg::Flush(r)) => {
                    waiters.push(r);
                    break;
                }
                Ok(SemMsg::Shutdown) => return,
                Err(_) => break,
            }
        }
        shared.semantic_busy.store(true, Ordering::Relaxed);
        let docs: Vec<(NotePath, String)> = {
            let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            ix.paths().into_iter().filter_map(|p| ix.source(&p).map(|s| (p.clone(), s.to_string()))).collect()
        };
        let chunks: Vec<crate::chunk::Chunk> = docs.iter().flat_map(|(p, s)| chunk_note(p, s)).collect();
        let builder = cfg.read().unwrap_or_else(std::sync::PoisonError::into_inner).builder.clone();
        let built = match builder {
            Some(b) => match b(chunks.clone()) {
                Ok(ix) => {
                    cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = None;
                    ix
                }
                Err(e) => {
                    cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(e);
                    build_lsa_index(chunks)
                }
            },
            None => build_lsa_index(chunks),
        };
        let built = Arc::new(built);
        *out.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(built);
        shared.semantic_busy.store(false, Ordering::Relaxed);
        shared.semantic_version.fetch_add(1, Ordering::Release);
        for w in std::mem::take(&mut waiters) {
            let _ = w.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_updates_in_background() {
        let vault = Vault::in_memory(&[("A.md", "links [[B]]"), ("B.md", "# B\nbee"), ("C.md", "sea")]);
        let svc = IndexService::spawn();
        svc.load(vault);
        svc.flush();
        assert_eq!(svc.read().len(), 3);
        assert_eq!(svc.read().backlinks(&NotePath::new("B.md")).len(), 1);
        svc.upsert(NotePath::new("C.md"), "now links [[B]] too".into());
        svc.remove(NotePath::new("A.md"));
        svc.flush();
        let ix = svc.read();
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.backlinks(&NotePath::new("B.md"))[0].from.as_str(), "C.md");
        assert_eq!(svc.status().indexed, 3);
        drop(ix);
        svc.flush_semantic();
        let sem = svc.semantic().expect("semantic index built");
        assert!(sem.chunks.iter().all(|c| c.path.as_str() != "A.md"));
        assert_eq!(svc.semantic_version(), 1);
    }

    #[test]
    fn attachment_text_is_stored_and_removed() {
        use crate::attach::{AttachmentKind, AttachmentText, TextSource};
        let svc = IndexService::spawn();
        let p = NotePath::new("papers/rope.pdf");
        svc.upsert_attachment(AttachmentText {
            path: p.clone(),
            kind: AttachmentKind::Pdf,
            pages: vec!["page one".into(), "page two".into()],
            source_hash: "ab".into(),
            by: TextSource::Embedded,
        });
        svc.flush();
        assert_eq!(svc.read().attachment(&p).map(|a| a.pages.len()), Some(2));
        assert_eq!(svc.read().len(), 0, "attachments aren't notes");
        svc.remove_attachment(p.clone());
        svc.flush();
        assert!(svc.read().attachment(&p).is_none());
    }
}
