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
use quire_base::time::now_ms;
use quire_vault::changes::{Delta, FileHash, Manifest};
use quire_vault::{NotePath, Vault};

use crate::chunk::chunk_note;
use crate::index::Index;
use crate::record::NoteRecord;
use crate::semantic::{SemanticIndex, build_lsa_index};
use crate::store::Store;

/// Wait this long after the last change before rebuilding the semantic index.
const SEMANTIC_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(700);

/// Notes cut into passages under one read lock while the semantic index is built.
const SEMANTIC_NOTES_PER_LOCK: usize = 256;

enum SemMsg {
    Rebuild,
    Flush(Sender<()>),
    Shutdown,
}

pub enum IndexMsg {
    /// Index every note in a vault (replaces current contents).
    Load(Vault),
    /// Index a vault through its manifest and the startup cache
    /// (`docs/design/0.14-index-startup.md` §3.6): notes whose cached record
    /// has the manifest's hash are not read; the others are parsed. From
    /// then on the index follows the manifest's scans, refuses a path the
    /// manifest doesn't list, and writes the cache when quiet.
    LoadCached {
        vault: Vault,
        manifest: Manifest,
    },
    Upsert {
        path: NotePath,
        source: String,
        /// The manifest hash of the bytes `source` was read from, when the
        /// sender has it; otherwise the hash of `source` itself.
        hash: Option<FileHash>,
    },
    Remove(NotePath),
    /// The text of a PDF or image (from the PDF-text or OCR job).
    UpsertAttachment(crate::attach::AttachmentText),
    RemoveAttachment(NotePath),
    /// 0.12: new typed-link settings (`[links]` in vault.toml).
    LinkConfig(crate::relations::LinkConfig),
    /// Replies once everything queued before it has been applied.
    Flush(Sender<()>),
    /// A folder was locked: delete the startup cache now; the next flush
    /// writes it again from what the index holds.
    PurgeCache,
    /// Write the startup cache now (if this process is its writer), then reply.
    FlushCache(Sender<()>),
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
    /// Notes in the index after the last batch, for per-frame UI.
    notes: AtomicUsize,
    semantic_version: AtomicU64,
    semantic_busy: AtomicBool,
    /// Notes read from disk and parsed by the last load.
    read: AtomicUsize,
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
                .spawn(move || {
                    crate::background::lower_priority();
                    run(&rx, &index, &shared, &sem_tx);
                })
                .expect("spawn index worker")
        };
        let sem_worker = {
            let index = Arc::clone(&index);
            let shared = Arc::clone(&shared);
            let semantic = Arc::clone(&semantic);
            let cfg = Arc::clone(&sem_config);
            std::thread::Builder::new()
                .name("quire-semantic".into())
                .spawn(move || {
                    crate::background::lower_priority();
                    run_semantic(&sem_rx, &index, &shared, &semantic, &cfg);
                })
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

    /// Whether an embedder other than the built-in LSA is set.
    pub fn has_semantic_builder(&self) -> bool {
        self.sem_config.read().unwrap_or_else(std::sync::PoisonError::into_inner).builder.is_some()
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

    /// Index a vault through its manifest and the startup cache
    /// ([`IndexMsg::LoadCached`]).
    pub fn load_cached(&self, vault: Vault, manifest: Manifest) {
        self.send(IndexMsg::LoadCached { vault, manifest });
    }

    pub fn upsert(&self, path: NotePath, source: String) {
        self.send(IndexMsg::Upsert { path, source, hash: None });
    }

    /// [`Self::upsert`] with the manifest hash of the bytes `source` was
    /// read from (`Manifest::note_written` gives it for a save).
    pub fn upsert_hashed(&self, path: NotePath, source: String, hash: FileHash) {
        self.send(IndexMsg::Upsert { path, source, hash: Some(hash) });
    }

    /// Delete the startup cache now (a folder was locked).
    pub fn purge_cache(&self) {
        self.send(IndexMsg::PurgeCache);
    }

    /// Write the startup cache now, if this process is its writer, and wait.
    pub fn flush_cache(&self) {
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        self.send(IndexMsg::FlushCache(done_tx));
        let _ = done_rx.recv();
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

    /// Use these typed-link settings. A `load` reads the vault's own, on
    /// the worker thread; this is for a change while the vault is open.
    pub fn set_link_config(&self, cfg: crate::relations::LinkConfig) {
        self.send(IndexMsg::LinkConfig(cfg));
    }

    /// The shared model, for background jobs that outlive a borrow of the
    /// service. Hold the read lock briefly (`docs/design/0.9.md` §9: at most 2 ms).
    pub fn handle(&self) -> Arc<RwLock<Index>> {
        Arc::clone(&self.index)
    }

    /// Read access to the current model. Hold briefly.
    pub fn read(&self) -> RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Read access if no batch is being applied right now (`None` while the
    /// worker holds the write lock): for per-frame UI that can show what it
    /// had a moment longer instead of waiting.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, Index>> {
        match self.index.try_read() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// How many notes are indexed, without taking the lock (0.13.1: for the
    /// status bar, which asked under a blocking read every frame).
    pub fn note_count(&self) -> usize {
        self.shared.notes.load(Ordering::Relaxed)
    }

    /// How many notes the last load read from disk and parsed: all of them
    /// without the startup cache, the changed ones with it.
    pub fn notes_read(&self) -> usize {
        self.shared.read.load(Ordering::Relaxed)
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

type Parsed = (NoteRecord, String, i64);

fn parse_parallel(vault: &Vault, paths: &[NotePath], shared: &Shared) -> Vec<Parsed> {
    let threads = std::thread::available_parallelism().map_or(2, std::num::NonZero::get).clamp(1, 8);
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        paths
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    crate::background::lower_priority();
                    part.iter()
                        .filter_map(|p| {
                            let src = vault.read(p).ok()?;
                            let rec = NoteRecord::from_source(p.clone(), &src);
                            let modified = vault
                                .storage()
                                .modified(p)
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                            shared.indexed.fetch_add(1, Ordering::Relaxed);
                            Some((rec, src, modified))
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
    /// A note, with the manifest hash of its text when known.
    Note(Box<Parsed>, Option<FileHash>),
    Remove(NotePath),
    Attachment(crate::attach::AttachmentText),
    RemoveAttachment(NotePath),
}

/// The manifest and the startup cache of the vault loaded with
/// [`IndexMsg::LoadCached`].
struct Cached {
    manifest: Manifest,
    deltas: Receiver<Arc<Delta>>,
    /// `None` for a storage with no place on disk.
    store: Option<Store>,
}

/// What the worker waits for next.
enum Next {
    Msg(Box<IndexMsg>),
    Delta(Arc<Delta>),
    FlushDue,
    Closed,
}

fn next(rx: &Receiver<IndexMsg>, cached: Option<&Cached>) -> Next {
    // A scan's changes first: what was sent after a scan returned (a flush
    // in a test, a save) then sees them applied.
    if let Some(d) = cached.and_then(|c| c.deltas.try_recv().ok()) {
        return Next::Delta(d);
    }
    let never = crossbeam_channel::never();
    let deltas = cached.map_or(&never, |c| &c.deltas);
    let timer = cached.and_then(|c| c.store.as_ref()?.due()).map_or_else(crossbeam_channel::never, crossbeam_channel::at);
    crossbeam_channel::select! {
        recv(rx) -> m => m.map_or(Next::Closed, |m| Next::Msg(Box::new(m))),
        recv(deltas) -> d => d.map_or(Next::FlushDue, Next::Delta),
        recv(timer) -> _ => Next::FlushDue,
    }
}

/// A note the index takes: Markdown, not a template, not in a locked folder.
fn indexable(vault: &Vault, p: &NotePath) -> bool {
    p.is_markdown() && !vault.is_template(p) && vault.locked_folder(p).is_none()
}

/// A scan's changes as work: changed notes read again (unless the index
/// has that text already), with the file's time; removed ones removed.
fn from_delta(delta: &Delta, vault: &Vault, ix: &Index) -> Vec<Prepared> {
    let mut out = Vec::new();
    for row in &delta.changed {
        if !indexable(vault, &row.path) {
            continue;
        }
        if ix.source(&row.path).is_some_and(|s| FileHash::of(s.as_bytes()) == row.hash) {
            continue;
        }
        if let Ok(source) = vault.read(&row.path) {
            let modified = Some(i64::try_from(row.mtime_ns / 1_000_000).unwrap_or(i64::MAX)).filter(|m| *m > 0).unwrap_or_else(now_ms);
            out.push(Prepared::Note(Box::new((NoteRecord::from_source(row.path.clone(), &source), source, modified)), None));
        }
    }
    out.extend(delta.removed.iter().filter(|p| ix.contains(p)).cloned().map(Prepared::Remove));
    out
}

/// Flush the startup cache, at most until `deadline`.
fn flush_cache(cached: Option<&mut Cached>, index: &RwLock<Index>, deadline: Option<std::time::Instant>) {
    let Some(c) = cached else { return };
    // The manifest is written on the same schedule (when quiet, at most
    // every 5 minutes, on clean exit): it is what the next start trusts.
    // Both are caches: one that can't be written costs speed at the next
    // start, never a result, and the index carries on in memory.
    #[expect(clippy::let_underscore_must_use, reason = "the manifest is a cache; a failed write is retried at the next flush")]
    let _ = c.manifest.flush();
    let Some(store) = c.store.as_mut() else { return };
    let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    #[expect(clippy::let_underscore_must_use, reason = "the cache is disposable; a failed flush is retried at the next one")]
    let _ = store.flush(&ix, deadline);
}

/// Load `vault` through `manifest` and the cache: install every note whose
/// cached record has the manifest's hash (keyword search is then complete
/// for them), and give back the notes still to parse
/// (`docs/design/0.14-index-startup.md` §3.6, startup).
fn load_cached(
    vault: &Vault,
    manifest: Manifest,
    link_config: crate::relations::LinkConfig,
    index: &RwLock<Index>,
    shared: &Shared,
) -> (Cached, Vec<NotePath>) {
    let deltas = manifest.subscribe();
    // The index scans only if no scan ran in this process yet.
    let listed = manifest.scanned() || manifest.scan().is_ok();
    let paths = vault.notes().unwrap_or_default();
    // Only regular files the manifest lists are indexed.
    let notes: Vec<(NotePath, FileHash)> =
        if listed { paths.iter().filter_map(|p| Some((p.clone(), manifest.get(p)?.hash))).collect() } else { Vec::new() };
    shared.total.store(notes.len(), Ordering::Relaxed);
    shared.indexed.store(0, Ordering::Relaxed);
    let mut store = Store::open(vault);
    let threads = std::thread::available_parallelism().map_or(2, std::num::NonZero::get).clamp(1, 8);
    let found = match store.as_mut() {
        Some(s) => s.load(&notes, threads),
        None => crate::store::Found { segments: Vec::new(), parse: notes.iter().map(|(p, _)| p.clone()).collect() },
    };
    let hashes: std::collections::HashMap<&NotePath, FileHash> = notes.iter().map(|(p, h)| (p, *h)).collect();
    let mut ix = Index::new();
    ix.set_link_config_quiet(link_config);
    for seg in found.segments {
        let table: Vec<&str> = seg.table.iter().map(String::as_str).collect();
        let map = ix.intern_all(&table);
        for (stored, src) in seg.notes {
            let p = stored.rec.path.clone();
            let Some(&hash) = hashes.get(&p) else { continue };
            let modified = manifest
                .get(&p)
                .map(|r| i64::try_from(r.mtime_ns / 1_000_000).unwrap_or(i64::MAX))
                .filter(|m| *m > 0)
                .or_else(|| store.as_ref().and_then(|s| s.row_modified(&p)))
                .unwrap_or(0);
            ix.insert_stored(stored, &map, src);
            ix.set_modified(&p, modified);
            if let Some(s) = store.as_mut() {
                s.note(&p, hash, modified);
            }
            shared.indexed.fetch_add(1, Ordering::Relaxed);
        }
    }
    ix.recount();
    ix.end_batch();
    shared.notes.store(ix.len(), Ordering::Relaxed);
    *index.write().unwrap_or_else(std::sync::PoisonError::into_inner) = ix;
    shared.version.fetch_add(1, Ordering::Release);
    (Cached { manifest, deltas, store }, found.parse)
}

#[expect(clippy::too_many_lines, reason = "one loop over the worker's messages; each arm is short")]
fn run(rx: &Receiver<IndexMsg>, index: &RwLock<Index>, shared: &Shared, sem_tx: &Sender<SemMsg>) {
    let read = || index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let write = || index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    // The vault last loaded: a note in one of its locked folders (0.11) is
    // never indexed, whoever sends it (a save, a watcher event, a job).
    let mut loaded: Option<Vault> = None;
    let mut cached: Option<Cached> = None;
    loop {
        let (first, from_scan) = match next(rx, cached.as_ref()) {
            Next::Msg(m) => (vec![*m], Vec::new()),
            Next::Delta(d) => (Vec::new(), loaded.as_ref().map(|v| from_delta(&d, v, &read())).unwrap_or_default()),
            Next::FlushDue => {
                flush_cache(cached.as_mut(), index, None);
                continue;
            }
            Next::Closed => break,
        };
        if first.is_empty() && from_scan.is_empty() {
            continue;
        }
        shared.busy.store(true, Ordering::Relaxed);
        // Drain whatever else is queued so a burst of saves costs one graph rebuild.
        let mut batch = first;
        batch.extend(rx.try_iter());
        let mut changed = !from_scan.is_empty();
        let mut prepared: Vec<Prepared> = from_scan;
        let mut reload: Option<Vec<Parsed>> = None;
        let mut shutdown = false;
        let mut waiters = Vec::new();
        let mut cache_waiters = Vec::new();
        let mut rebuild = false;
        let mut link_config: Option<crate::relations::LinkConfig> = None;
        // One scan per batch at most, for a path the manifest doesn't list yet.
        let mut rescanned = false;
        for msg in batch {
            match msg {
                IndexMsg::Load(vault) => {
                    changed = true;
                    cached = None;
                    loaded = Some(vault.clone());
                    // 0.12: read here, off the UI thread, with the notes.
                    link_config = Some(crate::relations::LinkConfig::load(&vault));
                    let paths = vault.notes().unwrap_or_default();
                    shared.total.store(paths.len(), Ordering::Relaxed);
                    shared.indexed.store(0, Ordering::Relaxed);
                    shared.read.store(paths.len(), Ordering::Relaxed);
                    reload = Some(parse_parallel(&vault, &paths, shared));
                    prepared.clear();
                }
                IndexMsg::LoadCached { vault, manifest } => {
                    changed = true;
                    reload = None;
                    prepared.clear();
                    loaded = Some(vault.clone());
                    let cfg = link_config.take().unwrap_or_else(|| crate::relations::LinkConfig::load(&vault));
                    let (c, parse) = load_cached(&vault, manifest, cfg, index, shared);
                    cached = Some(c);
                    shared.read.store(parse.len(), Ordering::Relaxed);
                    // Catch up: the notes the cache didn't have, through the
                    // per-note path; then one graph build for everything.
                    prepared.extend(parse_parallel(&vault, &parse, shared).into_iter().map(|p| Prepared::Note(Box::new(p), None)));
                    rebuild = true;
                }
                IndexMsg::Upsert { path, source, hash } => {
                    changed = true;
                    if loaded.as_ref().is_some_and(|v| v.locked_folder(&path).is_some()) {
                        prepared.push(Prepared::Remove(path));
                        continue;
                    }
                    // Only regular files the manifest lists are indexed: not a
                    // symlink, nothing through a symlinked folder
                    // (`docs/design/0.14-index-startup.md` §3.6). A path not
                    // listed yet (a new file seen by the watcher) costs one scan.
                    if let Some(c) = &cached
                        && !c.manifest.has(&path)
                        && (rescanned || {
                            rescanned = true;
                            c.manifest.scan().is_err() || !c.manifest.has(&path)
                        })
                    {
                        prepared.push(Prepared::Remove(path));
                        continue;
                    }
                    prepared.push(Prepared::Note(Box::new((NoteRecord::from_source(path, &source), source, now_ms())), hash));
                }
                IndexMsg::Remove(path) => {
                    changed = true;
                    prepared.push(Prepared::Remove(path));
                }
                // Attachment text isn't in the semantic index yet: no rebuild.
                IndexMsg::UpsertAttachment(text) => {
                    if loaded.as_ref().is_some_and(|v| v.locked_folder(&text.path).is_some()) {
                        prepared.push(Prepared::RemoveAttachment(text.path));
                    } else {
                        prepared.push(Prepared::Attachment(text));
                    }
                }
                IndexMsg::RemoveAttachment(path) => prepared.push(Prepared::RemoveAttachment(path)),
                IndexMsg::LinkConfig(cfg) => link_config = Some(cfg),
                IndexMsg::Flush(reply) => waiters.push(reply),
                IndexMsg::PurgeCache => {
                    if let Some(s) = cached.as_mut().and_then(|c| c.store.as_mut()) {
                        s.purge();
                    }
                }
                IndexMsg::FlushCache(reply) => cache_waiters.push(reply),
                IndexMsg::Shutdown => shutdown = true,
            }
        }
        // 0.13.1: the link graph is rebuilt only when the batch can change how
        // links resolve (a load, a note added or removed, new aliases, new
        // link settings). An edit to notes already indexed patches it in
        // place, and attachment text leaves it alone.
        rebuild |= reload.is_some();
        let mut patched: Vec<NotePath> = Vec::new();
        let mut store = cached.as_mut().and_then(|c| c.store.as_mut());
        {
            let mut ix = write();
            if let Some(all) = reload {
                let cfg = ix.link_config().clone();
                *ix = Index::new();
                // Without a refresh: the one below builds the graph.
                ix.set_link_config_quiet(link_config.take().unwrap_or(cfg));
                for (rec, src, modified) in all {
                    let path = rec.path.clone();
                    ix.insert_record(rec, src);
                    ix.set_modified(&path, modified);
                }
                ix.recount();
            }
            for item in prepared {
                match item {
                    Prepared::Note(note, hash) => {
                        let (rec, src, modified) = *note;
                        let path = rec.path.clone();
                        if !rebuild && ix.graph_patchable(&rec) {
                            patched.push(path.clone());
                        } else {
                            rebuild = true;
                        }
                        if let Some(s) = store.as_deref_mut() {
                            s.note(&path, hash.unwrap_or_else(|| FileHash::of(src.as_bytes())), modified);
                        }
                        ix.insert_record(rec, src);
                        ix.set_modified(&path, modified);
                    }
                    Prepared::Remove(path) => {
                        // A path that isn't indexed (a locked folder's note, a file that isn't a note) changes nothing.
                        rebuild |= ix.contains(&path);
                        ix.remove_no_refresh(&path);
                        if let Some(s) = store.as_deref_mut() {
                            s.gone(&path);
                        }
                    }
                    Prepared::Attachment(text) => ix.upsert_attachment(text),
                    Prepared::RemoveAttachment(path) => ix.remove_attachment(&path),
                }
            }
            if let Some(cfg) = link_config {
                rebuild |= cfg != *ix.link_config();
                ix.set_link_config_quiet(cfg);
            }
            if !rebuild && !patched.is_empty() {
                ix.patch_graph(&patched);
            }
            ix.end_batch();
            if let Some(s) = store {
                s.settle_after_load();
            }
            shared.notes.store(ix.len(), Ordering::Relaxed);
        }
        if rebuild {
            // The graph is built under a read lock (searches and the UI keep working) and
            // installed under a brief write. Only this thread writes, so the records cannot
            // change in between.
            let graph = read().build_graph();
            write().install_graph(graph);
        }
        shared.busy.store(false, Ordering::Relaxed);
        shared.version.fetch_add(1, Ordering::Release);
        if changed {
            let _ = sem_tx.send(SemMsg::Rebuild);
        }
        for w in waiters {
            let _ = w.send(());
        }
        if !cache_waiters.is_empty() || shutdown {
            // On clean exit the flush gets 2 s, then it is given up.
            let deadline = shutdown.then(|| std::time::Instant::now() + std::time::Duration::from_secs(2));
            flush_cache(cached.as_mut(), index, deadline);
        }
        for w in cache_waiters {
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
        // 0.13.1: passages are cut from the index's own text, a few hundred notes per
        // short read lock, instead of from a copy of every note made first (the copy,
        // as big as the vault, was freed after but stayed in the process's memory).
        let paths = index.read().unwrap_or_else(std::sync::PoisonError::into_inner).paths();
        let mut chunks: Vec<crate::chunk::Chunk> = Vec::new();
        for part in paths.chunks(SEMANTIC_NOTES_PER_LOCK) {
            let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            for p in part {
                if let Some(s) = ix.source(p) {
                    chunks.extend(chunk_note(p, s));
                }
            }
        }
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
#[path = "cache_tests.rs"]
mod cache_tests;

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
        assert_eq!(svc.note_count(), 3);
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

    /// 0.13.1: an edit patches the graph, a new note or new aliases rebuild it,
    /// attachment text leaves it alone; each gives the links a rebuild would.
    #[test]
    fn edits_patch_the_graph_and_attachments_leave_it() {
        let vault = Vault::in_memory(&[("A.md", "links [[B]]"), ("B.md", "# B\nbee"), ("C.md", "sea")]);
        let svc = IndexService::spawn();
        svc.load(vault);
        svc.flush();
        let b = NotePath::new("B.md");
        svc.upsert(NotePath::new("C.md"), "now [[B]] and [[Bee]]".into());
        svc.flush();
        let from: Vec<String> = svc.read().backlinks(&b).iter().map(|l| l.from.to_string()).collect();
        assert_eq!(from.len(), 2, "{from:?}");
        assert_eq!(svc.read().graph().unresolved().len(), 1, "[[Bee]] doesn't resolve yet");
        // New aliases change what links resolve to: a rebuild.
        svc.upsert(b.clone(), "---\naliases: [Bee]\n---\n# B".into());
        svc.flush();
        assert_eq!(svc.read().backlinks(&b).len(), 3);
        assert!(svc.read().graph().unresolved().is_empty());
        svc.upsert_attachment(crate::attach::AttachmentText {
            path: NotePath::new("x.pdf"),
            kind: crate::attach::AttachmentKind::Pdf,
            pages: vec!["[[A]]".into()],
            source_hash: "ab".into(),
            by: crate::attach::TextSource::Embedded,
        });
        svc.flush();
        assert_eq!(svc.read().backlinks(&b).len(), 3);
        // A new note: a rebuild, and the link that waited for it resolves.
        svc.upsert(NotePath::new("D.md"), "[[C]]".into());
        svc.upsert(NotePath::new("A.md"), "links [[D]]".into());
        svc.flush();
        assert_eq!(svc.read().backlinks(&NotePath::new("D.md")).len(), 1);
        assert_eq!(svc.read().backlinks(&b).len(), 2);
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
