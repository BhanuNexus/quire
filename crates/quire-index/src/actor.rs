// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-writer actor around [`Index`].
//!
//! Writes are messages processed on a worker thread: parsing happens there,
//! outside the lock, and the write lock is held only to swap records in.
//! Readers share the model through an `RwLock` and use [`IndexService::version`]
//! to know when cached query results are stale.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use quire_base::time::now_ms;
use quire_vault::changes::{Delta, FileHash, Manifest};
use quire_vault::{NotePath, Vault};

use crate::fulltext::DocId;
use crate::index::Index;
use crate::index::SourceLoader;
use crate::record::NoteRecord;
use crate::scope::{FitSet, Scope, ScopeKey, ScopeRead, Scopes, Visible};
use crate::semantic::SemanticIndex;
use crate::store::{Found, Store, Texts};

/// Wait this long after the last change before updating the semantic index.
const SEMANTIC_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(700);

/// Notes cut into passages under one read lock while the semantic index is built.
const SEMANTIC_NOTES_PER_LOCK: usize = 256;

/// The refit rule for the built-in LSA (`docs/design/0.14-index-startup.md` §3.3): a refit
/// when passages added or changed since the last fit reach this share of all
/// passages (and at least [`REFIT_MIN`]),
const REFIT_SHARE: usize = 10;
const REFIT_MIN: usize = 256;
/// or when nothing changed for this long, something changed since the fit,
const REFIT_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// and the fit is older than this.
const REFIT_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// How often the semantic worker looks at the idle rule.
const REFIT_TICK: std::time::Duration = std::time::Duration::from_secs(30);

enum SemMsg {
    /// Build the whole index again (a load, another embedder).
    Rebuild,
    /// Notes stored or changed: their rows are made again with the current model.
    Changed(Vec<NotePath>),
    /// Notes gone: their rows go.
    Removed(Vec<NotePath>),
    /// A folder was locked, or a note went into one: the model was fitted on
    /// text now locked. It is marked tainted and refitted.
    Taint,
    /// Refit now (tests, and the person's "rebuild" action).
    Refit,
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
    /// The workspace is switching to another vault: let go of this one at
    /// once. Its manifest and cache store are dropped without a flush, so
    /// nothing of it is written after the switch, and the index is emptied.
    /// Updates are ignored until the next `Load` or `LoadCached`.
    Detach,
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
    semantic_fits: AtomicU64,
    /// Notes read from disk and parsed by the last load.
    read: AtomicUsize,
}

type SharedSemantic = Arc<RwLock<Option<Arc<SemanticIndex>>>>;

/// Makes the embedder to use instead of the built-in LSA, and its vector
/// cache, on the semantic worker (so files are read there, not by the caller).
pub type EmbedderSetup = Arc<dyn Fn() -> (Arc<dyn crate::semantic::Embedder>, crate::semantic::VectorCache) + Send + Sync>;

#[derive(Default)]
struct SemanticConfig {
    setup: Option<EmbedderSetup>,
    last_error: Option<String>,
    /// Called before each full build (tests hold a fit back with it).
    fit_hook: Option<Arc<dyn Fn() + Send + Sync>>,
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
    /// The agent views, beside the index (lock order: index, then views).
    scopes: Arc<Mutex<Scopes>>,
    /// Meaning search off (a test or bench server): [`Self::semantic`] gives none.
    meaning_off: Arc<AtomicBool>,
}

/// The index read through one caller's view: hold it for the length of one
/// search, as the index's read lock (which it holds).
///
/// It gives only what the view may give: ranked through the view's
/// statistics, members' records and text, and the meaning model only when
/// the view may use it. It does not dereference to the [`Index`], so a tool
/// can't make an unscoped search by mistake (Fable's review):
///
/// ```compile_fail
/// fn unscoped(sc: &quire_index::Scoped<'_>) {
///     let _ = sc.search_where("rope", 5, &|_| true);
/// }
/// ```
pub struct Scoped<'a> {
    ix: RwLockReadGuard<'a, Index>,
    scope: Arc<Scope>,
    /// The meaning model, only if the view may use it.
    semantic: Option<Arc<SemanticIndex>>,
}

impl Scoped<'_> {
    /// The meaning model if this view may use it, else `None`: the only
    /// one a caller with a view gets. The view may use it only if every note
    /// it was fitted on is one the view may read (`docs/design/0.14-agent-index.md` §3.11).
    pub fn semantic(&self) -> Option<&SemanticIndex> {
        self.semantic.as_deref()
    }

    /// How searches count through this view.
    pub(crate) fn scope(&self) -> ScopeRead<'_> {
        ScopeRead::View(&self.scope)
    }

    /// The whole index (this crate only: its scoped searches and tests).
    pub(crate) fn index(&self) -> &Index {
        &self.ix
    }

    /// The view itself (its statistics, no notes).
    pub fn view(&self) -> &Scope {
        &self.scope
    }

    /// Keyword search through the view ([`Index::search_in`]).
    pub fn search(&self, query: &str, limit: usize) -> Vec<crate::SearchHit> {
        self.ix.search_in(query, limit, self.scope())
    }

    /// How rare a (folded, stemmed) word is among the view's notes; `None`
    /// when none of them uses it.
    pub fn idf(&self, term: &str) -> Option<f32> {
        self.ix.idf_in(term, self.scope())
    }

    /// How many notes the view holds.
    pub fn len(&self) -> usize {
        self.ix.len_in(self.scope())
    }

    /// Whether the view holds no note.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `p` is one of the view's notes.
    pub fn has_note(&self, p: &NotePath) -> bool {
        self.scope().has_note(&self.ix, p)
    }

    /// The text of one of the view's notes.
    pub fn source(&self, p: &NotePath) -> Option<&str> {
        self.has_note(p).then(|| self.ix.source(p)).flatten()
    }

    /// The record of one of the view's notes.
    pub fn get(&self, p: &NotePath) -> Option<&NoteRecord> {
        self.has_note(p).then(|| self.ix.get(p)).flatten()
    }

    /// When one of the view's notes last changed (ms), as the index knows it.
    pub fn modified(&self, p: &NotePath) -> Option<i64> {
        self.has_note(p).then(|| self.ix.modified(p)).flatten()
    }
}

impl IndexService {
    pub fn spawn() -> Self {
        let index = Arc::new(RwLock::new(Index::new()));
        let shared = Arc::new(Shared::default());
        let semantic: SharedSemantic = Arc::new(RwLock::new(None));
        let sem_config: Arc<RwLock<SemanticConfig>> = Arc::default();
        let (tx, rx) = unbounded();
        let (sem_tx, sem_rx) = unbounded();
        let scopes: Arc<Mutex<Scopes>> = Arc::default();
        let worker = {
            let index = Arc::clone(&index);
            let shared = Arc::clone(&shared);
            let sem_tx = sem_tx.clone();
            let scopes = Arc::clone(&scopes);
            std::thread::Builder::new()
                .name("quire-index".into())
                .spawn(move || {
                    crate::background::lower_priority();
                    run(&rx, &index, &shared, &sem_tx, &scopes);
                })
                .expect("spawn index worker")
        };
        let sem_worker = {
            let index = Arc::clone(&index);
            let shared = Arc::clone(&shared);
            let semantic = Arc::clone(&semantic);
            let cfg = Arc::clone(&sem_config);
            let scopes = Arc::clone(&scopes);
            std::thread::Builder::new()
                .name("quire-semantic".into())
                .spawn(move || {
                    crate::background::lower_priority();
                    run_semantic(&sem_rx, &index, &shared, &semantic, &cfg, &scopes);
                })
                .expect("spawn semantic worker")
        };
        Self {
            index,
            tx,
            shared,
            worker: Some(worker),
            semantic,
            sem_tx,
            sem_worker: Some(sem_worker),
            sem_config,
            scopes,
            meaning_off: Arc::default(),
        }
    }

    /// Use a different embedder (e.g. a neural endpoint) for the semantic
    /// index, or `None` for the built-in LSA. Builds the index again in the
    /// background; the previous one serves meanwhile. If the embedder fails,
    /// the built-in LSA takes over ([`Self::semantic_error`] says why).
    pub fn set_embedder(&self, setup: Option<EmbedderSetup>) {
        let mut c = self.sem_config.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        c.setup = setup;
        c.last_error = None;
        drop(c);
        let _ = self.sem_tx.send(SemMsg::Rebuild);
    }

    /// Whether an embedder other than the built-in LSA is set.
    pub fn has_embedder(&self) -> bool {
        self.sem_config.read().unwrap_or_else(std::sync::PoisonError::into_inner).setup.is_some()
    }

    /// Refit the meaning model now, and wait for it (and anything queued
    /// before). Saves never refit: the refit rule does, in the background
    /// (`docs/design/0.14-index-startup.md` §3.3).
    pub fn refit_semantic(&self) {
        self.flush();
        let _ = self.sem_tx.send(SemMsg::Refit);
        self.flush_semantic();
    }

    /// How many full fits (builds) of the meaning model have finished.
    pub fn semantic_fits(&self) -> u64 {
        self.shared.semantic_fits.load(Ordering::Acquire)
    }

    /// Run `hook` before each full build of the meaning model (tests hold a fit back with it).
    #[cfg(test)]
    pub(crate) fn set_fit_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.sem_config.write().unwrap_or_else(std::sync::PoisonError::into_inner).fit_hook = hook;
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
        if self.meaning_off.load(Ordering::Relaxed) {
            return None;
        }
        self.semantic.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Turn meaning search off (`false`) or on: off, [`Self::semantic`] gives
    /// none (a test or bench server that compares keyword search only).
    pub fn set_meaning(&self, on: bool) {
        self.meaning_off.store(!on, Ordering::Relaxed);
    }

    /// The index read through the view for `key` (`docs/design/0.14-agent-index.md` §3.5).
    ///
    /// The view is built here, on the calling thread, if it doesn't exist
    /// for the index's current build: one `visible` call per note and
    /// attachment, under the index's read lock (so it agrees with the
    /// records; the worker patches it from then on). `key` must stand for
    /// `visible`: two callers with one key see the same notes.
    pub fn scoped(&self, key: ScopeKey, visible: Visible) -> Scoped<'_> {
        let ix = self.read();
        let lock = || self.scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = lock().get(&key, &ix, &visible);
        let scope = if let Some(s) = found {
            s
        } else {
            let built = Arc::new(Scope::build(&ix, visible));
            lock().insert(key, built)
        };
        // The meaning model only if every note it was fitted on is one this view may read. Under
        // the index's read lock: the worker can't change the notes or the fit set meanwhile.
        let semantic = self.semantic().filter(|m| {
            let mut reg = lock();
            if !reg.fit.as_ref().is_some_and(|f| f.is_for(m, &ix)) {
                // A model from before a load: what it was fitted on, against this build.
                reg.fit = Some(FitSet::of(m, &ix));
            }
            reg.fit.as_ref().is_some_and(|f| scope.may_use(f))
        });
        Scoped { ix, scope, semantic }
    }

    /// How many views are kept (at most [`crate::MAX_SCOPES`]).
    pub fn scope_count(&self) -> usize {
        self.scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    /// Increments each time the meaning index changes (a fit, or a per-note update).
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

    /// Let go of the vault loaded last, before another one is loaded
    /// ([`IndexMsg::Detach`]).
    pub fn detach(&self) {
        self.send(IndexMsg::Detach);
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

/// Read a plain note's bytes: its text and the hash of the bytes as
/// stored. A read that finds other bytes than the manifest's row is told
/// to the manifest ([`Manifest::observed`]), which passes it on to every
/// subscriber. `None` for a note in a locked folder or one that can't be read.
fn read_note(vault: &Vault, manifest: Option<&Manifest>, p: &NotePath) -> Option<(String, FileHash)> {
    if vault.locked_folder(p).is_some() {
        return None;
    }
    let bytes = vault.storage().read_bytes(p).ok()?;
    let hash = match manifest {
        Some(m) if m.get(p).is_none_or(|r| r.hash != FileHash::of(&bytes)) => m.observed(p, &bytes),
        _ => FileHash::of(&bytes),
    };
    // As `Vault::read` decodes a plain note: without a byte-order mark.
    let text = String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes)).into_owned();
    Some((text, hash))
}

/// [`parse_parallel`] through the manifest: each note with the hash of the
/// bytes it was read from.
fn parse_hashed(vault: &Vault, manifest: &Manifest, paths: &[NotePath], shared: &Shared) -> Vec<Prepared> {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get).max(1);
    crate::store::pool(paths, threads, |p| {
        let (src, hash) = read_note(vault, Some(manifest), p)?;
        let rec = NoteRecord::from_source(p.clone(), &src);
        shared.indexed.fetch_add(1, Ordering::Relaxed);
        Some(Prepared::Note(Box::new((rec, src, file_ms(vault, p))), Some(hash)))
    })
    .into_iter()
    .flatten()
    .flatten()
    .collect()
}

/// When a note's file last changed, Unix ms (0 if unknown).
fn file_ms(vault: &Vault, p: &NotePath) -> i64 {
    vault
        .storage()
        .modified(p)
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

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
                            let modified = file_ms(vault, p);
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

/// A manifest change (a scan's, or one row from [`Manifest::observed`]) as
/// work: changed notes read again unless the index has that text already
/// (the same change may arrive more than once: compared by hash), with the
/// file's time; removed ones removed. A file the scan couldn't read
/// (`Delta::unread`) keeps what the index has.
fn from_delta(
    delta: &Delta,
    vault: &Vault,
    manifest: &Manifest,
    known: &dyn Fn(&NotePath) -> Option<FileHash>,
    ix: &Index,
) -> Vec<Prepared> {
    let mut out = Vec::new();
    for row in &delta.changed {
        if !indexable(vault, &row.path) || known(&row.path) == Some(row.hash) {
            continue;
        }
        if let Some((source, hash)) = read_note(vault, Some(manifest), &row.path) {
            if known(&row.path) == Some(hash) {
                continue;
            }
            // The file's time; an observed row has none: asked of the file.
            let modified = Some(i64::try_from(row.mtime_ns / 1_000_000).unwrap_or(i64::MAX))
                .filter(|m| *m > 0)
                .or_else(|| Some(file_ms(vault, &row.path)).filter(|m| *m > 0))
                .unwrap_or_else(now_ms);
            out.push(Prepared::Note(Box::new((NoteRecord::from_source(row.path.clone(), &source), source, modified)), Some(hash)));
        }
    }
    out.extend(delta.removed.iter().filter(|p| ix.contains(p)).cloned().map(Prepared::Remove));
    out
}

/// A manifest delta as work for the index (see [`from_delta`]).
fn delta_work(d: &Delta, loaded: Option<&Vault>, cached: Option<&Cached>, index: &RwLock<Index>) -> Vec<Prepared> {
    let (Some(v), Some(c)) = (loaded, cached) else { return Vec::new() };
    let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    // What the index holds of each note, by hash: the cache's record of it,
    // else its text's.
    let known = |p: &NotePath| match c.store.as_ref().and_then(|s| s.hash_of(p)) {
        Some(h) => Some(h),
        None => ix.source(p).map(|s| FileHash::of(s.as_bytes())),
    };
    from_delta(d, v, &c.manifest, &known, &ix)
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
) -> (Cached, Vec<NotePath>, Pending) {
    // Subscribe first, then reconcile with the table as it is now (every
    // row, below): a change made in between arrives both ways, and the
    // worker compares by hash. Nothing is kept by generation or epoch.
    let deltas = manifest.subscribe();
    // Every load scans: the rows must be as new as the files, whatever was
    // saved since the last scan without telling the manifest (a reload
    // after an import or a vault switch runs long after the first scan).
    let scanned = manifest.scan().is_ok();
    let paths = vault.notes().unwrap_or_default();
    // Only regular files the manifest lists are indexed.
    let notes: Vec<(NotePath, FileHash)> = paths.iter().filter_map(|p| Some((p.clone(), manifest.get(p)?.hash))).collect();
    // A failed scan: no row can be trusted, so the cache is neither read nor
    // written in this load (its rows would all look dead), and every note
    // is read from its file.
    let mut store = if scanned { Store::open(vault) } else { None };
    shared.total.store(if scanned { notes.len() } else { paths.len() }, Ordering::Relaxed);
    shared.indexed.store(0, Ordering::Relaxed);
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get).max(1);
    let found = match store.as_mut() {
        Some(s) => s.load(&notes, threads),
        None if scanned => Found { segments: Vec::new(), parse: notes.iter().map(|(p, _)| p.clone()).collect() },
        None => Found { segments: Vec::new(), parse: paths },
    };
    let hashes: HashMap<&NotePath, FileHash> = notes.iter().map(|(p, h)| (p, *h)).collect();
    let mut ix = Index::new();
    ix.set_link_config_quiet(link_config);
    let mut texts: Pending = Vec::new();
    for seg in found.segments {
        let table: Vec<&str> = seg.table.iter().map(String::as_str).collect();
        let map = ix.intern_all(&table);
        let mut ranges = Vec::with_capacity(seg.notes.len());
        for (stored, range) in seg.notes {
            let p = stored.rec.path.clone();
            let Some(&hash) = hashes.get(&p) else { continue };
            let modified = manifest
                .get(&p)
                .map(|r| i64::try_from(r.mtime_ns / 1_000_000).unwrap_or(i64::MAX))
                .filter(|m| *m > 0)
                .or_else(|| store.as_ref().and_then(|s| s.row_modified(&p)))
                .unwrap_or(0);
            // The text comes in the background (`fill_texts`), or on demand.
            ix.insert_stored(stored, &map, None);
            ix.set_modified(&p, modified);
            if let Some(s) = store.as_mut() {
                s.note(&p, hash, modified);
            }
            shared.indexed.fetch_add(1, Ordering::Relaxed);
            ranges.push((p, range));
        }
        texts.push((seg.texts, ranges));
    }
    ix.set_source_loader(Some(loader(&texts)));
    ix.recount();
    ix.end_batch();
    shared.notes.store(ix.len(), Ordering::Relaxed);
    {
        let mut w = index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        *w = ix;
    }
    // Keyword search, quick open, tags, tasks and views are complete now.
    shared.version.fetch_add(1, Ordering::Release);
    (Cached { manifest, deltas, store }, found.parse, texts)
}

/// Every segment's texts, by note.
type Pending = Vec<(Arc<Texts>, Vec<(NotePath, std::ops::Range<usize>)>)>;

/// The text of a note installed without it, decoded from its segment on
/// demand (the segment last decoded is kept, as callers tend to ask for
/// notes of one folder together).
fn loader(texts: &Pending) -> SourceLoader {
    let at: HashMap<NotePath, (usize, std::ops::Range<usize>)> =
        texts.iter().enumerate().flat_map(|(i, (_, notes))| notes.iter().map(move |(p, r)| (p.clone(), (i, r.clone())))).collect();
    let segments: Vec<Arc<Texts>> = texts.iter().map(|(t, _)| Arc::clone(t)).collect();
    let last: Mutex<Option<(usize, Arc<Vec<u8>>)>> = Mutex::new(None);
    SourceLoader(Arc::new(move |p: &NotePath| {
        let (i, range) = at.get(p)?;
        let mut last = last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let all = match &*last {
            Some((j, all)) if j == i => Arc::clone(all),
            _ => {
                let all = Arc::new(segments.get(*i)?.inflate()?);
                *last = Some((*i, Arc::clone(&all)));
                all
            }
        };
        Texts::text(&all, range)
    }))
}

/// Phase 2 of a start from the cache: decode every segment's texts on a
/// pool, in the background (keyword search already answers), then put them
/// in place under a brief write lock. Gives the notes whose text couldn't be
/// decoded: they are parsed from their files.
fn fill_texts(texts: &Pending, index: &RwLock<Index>, threads: usize) -> Vec<NotePath> {
    type Decoded = (Vec<(NotePath, String, Vec<u32>)>, Vec<NotePath>);
    let decoded: Vec<Option<Decoded>> = crate::store::pool(texts, threads, |(t, notes)| {
        let Some(all) = t.inflate() else { return (Vec::new(), notes.iter().map(|(p, _)| p.clone()).collect()) };
        let mut ok = Vec::with_capacity(notes.len());
        let mut failed = Vec::new();
        for (p, r) in notes {
            match Texts::text(&all, r) {
                Some(text) => {
                    let starts = crate::index::line_starts(&text);
                    ok.push((p.clone(), text, starts));
                }
                None => failed.push(p.clone()),
            }
        }
        (ok, failed)
    });
    let mut filled = Vec::new();
    let mut failed = Vec::new();
    for (d, (_, notes)) in decoded.into_iter().zip(texts) {
        let (ok, bad) = d.unwrap_or_else(|| (Vec::new(), notes.iter().map(|(p, _)| p.clone()).collect()));
        filled.extend(ok);
        failed.extend(bad);
    }
    let mut ix = index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    ix.fill_sources(filled);
    ix.set_source_loader(None);
    failed
}

#[expect(clippy::too_many_lines, reason = "one loop over the worker's messages; each arm is short")]
fn run(rx: &Receiver<IndexMsg>, index: &RwLock<Index>, shared: &Shared, sem_tx: &Sender<SemMsg>, scopes: &Mutex<Scopes>) {
    let read = || index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let write = || index.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    // The vault last loaded: a note in one of its locked folders (0.11) is
    // never indexed, whoever sends it (a save, a watcher event, a job).
    let mut loaded: Option<Vault> = None;
    let mut cached: Option<Cached> = None;
    // Between a `Detach` and the next load: updates belong to no vault here.
    let mut detached = false;
    loop {
        let (first, from_scan) = match next(rx, cached.as_ref()) {
            Next::Msg(m) => (vec![*m], Vec::new()),
            Next::Delta(d) => (Vec::new(), delta_work(&d, loaded.as_ref(), cached.as_ref(), index)),
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
        // Changes the manifest sent before those messages (a save's
        // `observed`, then a flush): applied in this batch, not after it.
        let mut from_scan = from_scan;
        if let Some(c) = &cached {
            for d in c.deltas.try_iter() {
                from_scan.extend(delta_work(&d, loaded.as_ref(), Some(c), index));
            }
        }
        let mut sem_taint = false;
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
                    detached = false;
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
                    detached = false;
                    reload = None;
                    prepared.clear();
                    loaded = Some(vault.clone());
                    let cfg = link_config.take().unwrap_or_else(|| crate::relations::LinkConfig::load(&vault));
                    let (mut c, mut parse, texts) = load_cached(&vault, manifest, cfg, index, shared);
                    // Phase 2: the texts, while keyword search already answers.
                    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get).max(1);
                    let failed = fill_texts(&texts, index, threads);
                    if let Some(s) = c.store.as_mut() {
                        s.forget_rows(&failed);
                    }
                    parse.extend(failed);
                    shared.read.store(parse.len(), Ordering::Relaxed);
                    // Catch up: the notes the cache didn't have, through the
                    // per-note path; then one graph build for everything.
                    prepared.extend(parse_hashed(&vault, &c.manifest, &parse, shared));
                    cached = Some(c);
                    rebuild = true;
                }
                IndexMsg::Upsert { .. } | IndexMsg::Remove(_) | IndexMsg::UpsertAttachment(_) | IndexMsg::RemoveAttachment(_)
                    if detached => {}
                IndexMsg::Detach => {
                    detached = true;
                    // Dropped, not flushed: the store lets go of the lock and
                    // writes nothing more under the last vault.
                    cached = None;
                    loaded = None;
                    prepared.clear();
                    shared.total.store(0, Ordering::Relaxed);
                    shared.indexed.store(0, Ordering::Relaxed);
                    // An empty index in place of the last vault's.
                    reload = Some(Vec::new());
                }
                IndexMsg::Upsert { path, source, hash } => {
                    if loaded.as_ref().is_some_and(|v| v.locked_folder(&path).is_some()) {
                        // A note moved into a locked folder: the model may hold its text.
                        sem_taint = true;
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
                    // A folder was locked: the meaning model was fitted on its text.
                    sem_taint = true;
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
        let reloaded = reload.is_some();
        // The notes this batch stored or removed, for the meaning index (per note, not a refit).
        let (mut sem_changed, mut sem_removed) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
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
            // The documents this batch changed, for the agent views.
            let mut touched: Vec<DocId> = Vec::new();
            let mut pages_changed = false;
            for item in prepared {
                match item {
                    Prepared::Note(note, hash) => {
                        let (rec, src, modified) = *note;
                        let path = rec.path.clone();
                        sem_removed.remove(&path);
                        sem_changed.insert(path.clone());
                        touched.extend(ix.doc_id(&path));
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
                        touched.extend(ix.doc_id(&path));
                    }
                    Prepared::Remove(path) => {
                        sem_changed.remove(&path);
                        sem_removed.insert(path.clone());
                        // A path that isn't indexed (a locked folder's note, a file that isn't a note) changes nothing.
                        rebuild |= ix.contains(&path);
                        if let Some(id) = ix.doc_id(&path) {
                            touched.push(id);
                            // A note the meaning model was fitted on leaves: its text stays in the model.
                            if let Some(fit) = scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fit.as_mut() {
                                fit.removed(ix.build(), id, &path);
                            }
                        }
                        ix.remove_no_refresh(&path);
                        if let Some(s) = store.as_deref_mut() {
                            s.gone(&path);
                        }
                    }
                    Prepared::Attachment(text) => {
                        pages_changed = true;
                        ix.upsert_attachment(text);
                    }
                    Prepared::RemoveAttachment(path) => {
                        pages_changed = true;
                        ix.remove_attachment(&path);
                    }
                }
            }
            if let Some(cfg) = link_config {
                rebuild |= cfg != *ix.link_config();
                ix.set_link_config_quiet(cfg);
            }
            if !rebuild && !patched.is_empty() {
                ix.patch_graph(&patched);
            }
            // Every view of this build, in the same write as the notes and
            // before the batch's removed document ids are freed (a view of
            // an older build is built again on its next use).
            if !touched.is_empty() || pages_changed {
                touched.sort_unstable();
                touched.dedup();
                scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).patch(&ix, &touched, pages_changed);
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
        if reloaded {
            let _ = sem_tx.send(SemMsg::Rebuild);
        } else {
            if !sem_removed.is_empty() {
                let _ = sem_tx.send(SemMsg::Removed(sem_removed.into_iter().collect()));
            }
            if !sem_changed.is_empty() {
                let _ = sem_tx.send(SemMsg::Changed(sem_changed.into_iter().collect()));
            }
        }
        if sem_taint {
            let _ = sem_tx.send(SemMsg::Taint);
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

/// The semantic worker's own state beside the published index.
struct Sem {
    /// The rows of each note in the published index.
    at: HashMap<NotePath, Vec<usize>>,
    /// Passages added or changed since the last fit.
    since_fit: usize,
    fitted_at: std::time::Instant,
    last_change: std::time::Instant,
    /// The embedder in use and its cache (`None`: the built-in LSA).
    embedder: Option<(Arc<dyn crate::semantic::Embedder>, crate::semantic::VectorCache)>,
}

impl Sem {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self { at: HashMap::new(), since_fit: 0, fitted_at: now, last_change: now, embedder: None }
    }

    /// Whether the refit rule says to refit (`docs/design/0.14-index-startup.md` §3.3).
    fn refit_due(&self, live: usize, now: std::time::Instant) -> bool {
        self.since_fit > 0
            && (self.since_fit >= (live * REFIT_SHARE / 100).max(REFIT_MIN)
                || (now.duration_since(self.last_change) >= REFIT_IDLE && now.duration_since(self.fitted_at) >= REFIT_AGE))
    }
}

/// The refit rule on its own, for tests.
#[cfg(test)]
pub(crate) struct RefitProbe(Sem);

#[cfg(test)]
pub(crate) fn refit_probe(now: std::time::Instant) -> RefitProbe {
    let mut s = Sem::new();
    s.fitted_at = now;
    s.last_change = now;
    RefitProbe(s)
}

#[cfg(test)]
impl RefitProbe {
    pub(crate) fn since_fit(&mut self, n: usize) {
        self.0.since_fit = n;
    }
    pub(crate) fn changed_at(&mut self, t: std::time::Instant) {
        self.0.last_change = t;
    }
    pub(crate) fn due(&self, live: usize, now: std::time::Instant) -> bool {
        self.0.refit_due(live, now)
    }
}

/// The passages of `paths`, cut from the index's text a few hundred notes per short read lock.
fn chunks_of(index: &RwLock<Index>, paths: &[NotePath]) -> Vec<crate::chunk::Chunk> {
    let mut chunks = Vec::new();
    for part in paths.chunks(SEMANTIC_NOTES_PER_LOCK) {
        let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        for p in part {
            if let Some(s) = ix.source(p) {
                chunks.extend(crate::chunk::chunk_note(p, s));
            }
        }
    }
    chunks
}

/// Build the whole meaning index: fit the LSA on every passage, or embed
/// them with the configured embedder (the LSA if that fails).
fn build_full(index: &RwLock<Index>, cfg: &RwLock<SemanticConfig>, sem: &mut Sem, rebuild: bool) -> SemanticIndex {
    let hook = cfg.read().unwrap_or_else(std::sync::PoisonError::into_inner).fit_hook.clone();
    if let Some(h) = hook {
        h();
    }
    // 0.13.1: passages are cut from the index's own text, a few hundred notes per
    // short read lock, instead of from a copy of every note made first.
    let paths = index.read().unwrap_or_else(std::sync::PoisonError::into_inner).paths();
    let chunks = chunks_of(index, &paths);
    if rebuild {
        let setup = cfg.read().unwrap_or_else(std::sync::PoisonError::into_inner).setup.clone();
        sem.embedder = setup.map(|s| s());
    }
    let built = match sem.embedder.as_mut() {
        Some((e, cache)) => match SemanticIndex::try_with_cache(chunks.clone(), Arc::clone(e), cache) {
            Ok(ix) => {
                cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = None;
                ix
            }
            Err(e) => {
                cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(e);
                sem.embedder = None;
                SemanticIndex::fit_lsa(chunks)
            }
        },
        None => SemanticIndex::fit_lsa(chunks),
    };
    sem.at = rows_by_note(&built);
    sem.since_fit = 0;
    sem.fitted_at = std::time::Instant::now();
    built
}

fn rows_by_note(ix: &SemanticIndex) -> HashMap<NotePath, Vec<usize>> {
    let mut at: HashMap<NotePath, Vec<usize>> = HashMap::new();
    for (i, p) in ix.passages() {
        at.entry(p.path.clone()).or_default().push(i);
    }
    at
}

/// The rows of `changed` notes made again with the current model and those
/// of `removed` dropped: no refit, on the save path's background
/// (`docs/design/0.14-index-startup.md` §3.3). `None` if the embedder failed.
fn update(
    index: &RwLock<Index>,
    cur: &SemanticIndex,
    sem: &mut Sem,
    changed: &[NotePath],
    removed: &[NotePath],
) -> Result<SemanticIndex, String> {
    let gone: Vec<usize> = changed.iter().chain(removed).filter_map(|p| sem.at.get(p)).flatten().copied().collect();
    let chunks = chunks_of(index, changed);
    let added = chunks.len();
    let cache = sem.embedder.as_mut().map(|(_, c)| c);
    let (mut next, ids) = cur.updated(&gone, chunks.clone(), cache)?;
    for p in changed.iter().chain(removed) {
        sem.at.remove(p);
    }
    for (c, id) in chunks.iter().zip(ids) {
        sem.at.entry(c.path.clone()).or_default().push(id);
    }
    if let Some((compact, _)) = next.compacted() {
        next = compact;
        sem.at = rows_by_note(&next);
    }
    sem.since_fit += added;
    sem.last_change = std::time::Instant::now();
    Ok(next)
}

fn run_semantic(
    rx: &Receiver<SemMsg>,
    index: &RwLock<Index>,
    shared: &Shared,
    out: &SharedSemantic,
    cfg: &RwLock<SemanticConfig>,
    scopes: &Mutex<Scopes>,
) {
    let mut sem = Sem::new();
    let current = || out.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    // Publish `next`: with its fit set when it is a fit (a per-note update shares the fit's).
    let publish = |next: SemanticIndex, fit: bool| {
        let next = Arc::new(next);
        {
            // The model and what it was fitted on, together, while the worker can't change the notes.
            let ix = index.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            let set = fit.then(|| FitSet::of(&next, &ix));
            *out.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(next);
            if let Some(set) = set {
                scopes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fit = Some(set);
            }
        }
        if fit {
            shared.semantic_fits.fetch_add(1, Ordering::Release);
        }
        shared.semantic_version.fetch_add(1, Ordering::Release);
    };
    loop {
        // Waiting: for a message, or to look at the idle rule now and then.
        let first = match rx.recv_timeout(REFIT_TICK) {
            Ok(m) => Some(m),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        };
        let (mut rebuild, mut refit, mut taint) = (false, false, false);
        let (mut changed, mut removed) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        let mut waiters: Vec<Sender<()>> = Vec::new();
        let mut take = |m: SemMsg, waiters: &mut Vec<Sender<()>>| -> bool {
            match m {
                SemMsg::Shutdown => return false,
                SemMsg::Flush(r) => waiters.push(r),
                SemMsg::Rebuild => rebuild = true,
                SemMsg::Refit => refit = true,
                SemMsg::Taint => {
                    taint = true;
                    refit = true;
                }
                SemMsg::Changed(ps) => {
                    for p in ps {
                        removed.remove(&p);
                        changed.insert(p);
                    }
                }
                SemMsg::Removed(ps) => {
                    for p in ps {
                        changed.remove(&p);
                        removed.insert(p);
                    }
                }
            }
            true
        };
        if let Some(m) = first {
            if !take(m, &mut waiters) {
                return;
            }
            // Debounce: keep absorbing until things go quiet, or someone waits.
            while waiters.is_empty() {
                match rx.recv_timeout(SEMANTIC_DEBOUNCE) {
                    Ok(m) => {
                        if !take(m, &mut waiters) {
                            return;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
            }
            // Whatever else is queued already (a flush right behind the changes).
            for m in rx.try_iter().collect::<Vec<_>>() {
                if !take(m, &mut waiters) {
                    return;
                }
            }
        }
        let (changed, removed): (Vec<NotePath>, Vec<NotePath>) = (changed.into_iter().collect(), removed.into_iter().collect());
        shared.semantic_busy.store(true, Ordering::Relaxed);
        let cur = current();
        if rebuild || cur.is_none() && (refit || !changed.is_empty() || !removed.is_empty()) {
            let built = build_full(index, cfg, &mut sem, true);
            publish(built, true);
        } else if let Some(cur) = cur {
            let mut cur = cur;
            if taint && !cur.is_tainted() {
                let t = cur.tainted();
                publish(t, false);
                cur = current().unwrap_or(cur);
            }
            if !changed.is_empty() || !removed.is_empty() {
                match update(index, &cur, &mut sem, &changed, &removed) {
                    Ok(next) => {
                        publish(next, false);
                        cur = current().unwrap_or(cur);
                    }
                    Err(e) => {
                        // The embedder failed: the built-in LSA takes over.
                        cfg.write().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(e);
                        sem.embedder = None;
                        let built = build_full(index, cfg, &mut sem, false);
                        publish(built, true);
                        cur = current().unwrap_or(cur);
                    }
                }
            }
            // Only the built-in LSA is fitted on the vault, so only it is refitted.
            if cur.is_lsa() && (refit || sem.refit_due(cur.len(), std::time::Instant::now())) {
                let built = build_full(index, cfg, &mut sem, false);
                publish(built, true);
            } else if refit && !cur.is_lsa() {
                let built = build_full(index, cfg, &mut sem, false);
                publish(built, true);
            }
        }
        shared.semantic_busy.store(false, Ordering::Relaxed);
        for w in waiters {
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
        assert!(sem.passages().all(|(_, p)| p.path.as_str() != "A.md"));
        assert_eq!(svc.semantic_fits(), 1, "edits after the load update rows, they don't refit");
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
