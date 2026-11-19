// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vault manifest: one table of what is on disk, kept by one scanner,
//! that everything asking "what changed?" asks (`docs/design/0.14-architecture.md` §2).
//! Format: `docs/formats/manifest.md`.
//!
//! One [`Row`] per visible file of the [`Storage`]: its path, size, time and
//! the SHA-256 of its bytes as stored (for a locked folder, of the `.qenc`
//! file), the generation in which it last changed, and whether it is a
//! tombstone.
//!
//! **Stat is a hint, hash is the fact.** [`Manifest::scan`] lists the vault
//! once, with sizes and times from the listing itself
//! ([`Storage::list_stat`]). A row whose size and time are unchanged is
//! trusted; any other file is read and hashed, and its row changes only if
//! the hash did. A row whose time is within [`RACY_WINDOW`] of the scan
//! (a coarse clock, or an edit in the same tick as the scan) is never
//! trusted on the next scan. [`Manifest::verify`] trusts no row. A listing
//! error is an error, never "all gone", and changes no row.
//!
//! **Our own writes.** [`Manifest::note_written`] and [`Manifest::note_removed`]
//! update the row at once and never wait for a scan in progress (P9): while
//! the table is busy they queue, and [`Manifest::get`] and [`Manifest::has`]
//! already answer with them. Such a row is hashed again by the next scan.
//!
//! **Sharing.** A [`Manifest`] is a handle: clones share one table, so one
//! scanner serves a vault. Each scan's [`Delta`] goes to every
//! [`Manifest::subscribe`]r. The table persists through
//! [`quire_base::cache`] at `.quire/cache/manifest` when the storage has a
//! place on disk ([`Manifest::flush`]); a missing, damaged, newer or foreign
//! file only costs a scan that reads every file.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use quire_base::cache::{self, Kind};
use sha2::{Digest as _, Sha256};

use crate::{FileStat, NotePath, Result, Storage, Vault, VaultError};

/// Where the manifest is kept, vault-relative.
pub const CACHE_PATH: &str = ".quire/cache/manifest";
/// The layout of the manifest's body (`docs/formats/manifest.md`).
pub const FORMAT: u32 = 1;
/// What derives the rows: SHA-256 of the stored bytes, the racy rule.
pub const BUILDER: u32 = 1;
/// A row whose time is this close to its scan (or later) is hashed again
/// by the next scan.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);
/// Tombstones kept at least; above this and a quarter of the live rows,
/// the oldest go (see [`Manifest::since`]).
const MIN_TOMBSTONES: usize = 1024;

/// SHA-256 of a file's bytes as stored: no BOM stripping, no decoding.
///
/// Not [`crate::ContentHash`] (BOM-stripped, 128 bits) and not sync's
/// attachment fingerprint: a consumer that needs one of those stores it
/// next to this.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileHash(pub [u8; 32]);

impl FileHash {
    /// The hash of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Lower-case hex, 64 characters.
    pub fn to_hex(self) -> String {
        quire_base::enc::hex(&self.0)
    }
}

impl std::fmt::Debug for FileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileHash({})", self.to_hex())
    }
}

/// One file of the vault, or a tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Vault-relative, as the storage names it (`Journal/Day.md.qenc` in a
    /// locked folder).
    pub path: NotePath,
    /// Bytes on disk when it was hashed.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch as the listing
    /// gave it; 0 when unknown (never trusted).
    pub mtime_ns: u64,
    /// SHA-256 of the bytes; for a tombstone, of the last bytes seen.
    pub hash: FileHash,
    /// The generation in which this row last changed (hash, appearance or
    /// removal; a new size or time with the same hash is no change).
    pub generation: u64,
    /// The file was removed in that generation.
    pub gone: bool,
    /// Not to be trusted by the next scan: hashed again whatever its size
    /// and time say.
    racy: bool,
}

impl Row {
    /// Whether the next scan reads this file whatever its size and time.
    pub const fn is_racy(&self) -> bool {
        self.racy
    }

    /// Whether a listing's `stat` lets a scan keep this row without reading.
    fn trusts(&self, stat: Option<FileStat>) -> bool {
        !self.gone && !self.racy && self.mtime_ns != 0 && stat == Some(FileStat { size: self.size, mtime_ns: self.mtime_ns })
    }
}

/// What one scan changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delta {
    /// The manifest's epoch (see [`Manifest::epoch`]).
    pub epoch: [u8; 16],
    /// The manifest's generation after the scan.
    pub generation: u64,
    /// Files new or with new bytes, sorted by path.
    pub changed: Vec<Row>,
    /// Files no longer listed, sorted.
    pub removed: Vec<NotePath>,
    /// From [`Manifest::verify`] (no row was trusted).
    pub verified: bool,
}

impl Delta {
    /// Nothing changed.
    pub const fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

/// The clock a manifest reads for the racy rule (injected in tests, P10).
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// One of our own writes, waiting for the table.
#[derive(Debug, Clone)]
enum Own {
    Written { path: NotePath, size: u64, hash: FileHash },
    Removed(NotePath),
}

impl Own {
    const fn path(&self) -> &NotePath {
        match self {
            Self::Written { path, .. } | Self::Removed(path) => path,
        }
    }
}

/// The rows and their bookkeeping.
#[derive(Debug)]
struct Table {
    epoch: [u8; 16],
    generation: u64,
    /// Tombstones of this generation and older were dropped.
    floor: u64,
    rows: BTreeMap<NotePath, Row>,
    /// Bumped on every change to what is persisted.
    version: u64,
    /// A scan is between its listing and its result: own writes made
    /// meanwhile are in `touched`, and the scan leaves those rows alone.
    scanning: bool,
    touched: HashSet<NotePath>,
    /// Scans finished in this process.
    scans: u64,
}

impl Table {
    fn fresh(epoch: [u8; 16]) -> Self {
        Self { epoch, generation: 0, floor: 0, rows: BTreeMap::new(), version: 0, scanning: false, touched: HashSet::new(), scans: 0 }
    }

    fn apply_own(&mut self, own: Own) {
        if self.scanning {
            self.touched.insert(own.path().clone());
        }
        let next = self.generation + 1;
        match own {
            Own::Written { path, size, hash } => {
                match self.rows.get_mut(&path) {
                    Some(r) if !r.gone && r.hash == hash => {
                        r.size = size;
                        r.mtime_ns = 0;
                        r.racy = true;
                    }
                    _ => {
                        self.rows.insert(path.clone(), Row { path, size, mtime_ns: 0, hash, generation: next, gone: false, racy: true });
                        self.generation = next;
                    }
                }
                self.version += 1;
            }
            Own::Removed(path) => {
                if let Some(r) = self.rows.get_mut(&path)
                    && !r.gone
                {
                    r.gone = true;
                    r.generation = next;
                    self.generation = next;
                    self.version += 1;
                }
            }
        }
    }

    fn drain(&mut self, pending: &Mutex<Vec<Own>>) {
        let queued = std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner));
        for own in queued {
            self.apply_own(own);
        }
    }

    /// Fold a scan's result in: `listed` is every path the listing gave
    /// that was still there to read, `read` the rows of files it hashed.
    fn apply_scan(&mut self, listed: &HashSet<NotePath>, read: Vec<Row>, verified: bool) -> Delta {
        let next = self.generation + 1;
        let mut delta = Delta { verified, ..Delta::default() };
        for mut row in read {
            if self.touched.contains(&row.path) {
                continue;
            }
            match self.rows.get_mut(&row.path) {
                Some(old) if !old.gone && old.hash == row.hash => {
                    if (old.size, old.mtime_ns, old.racy) != (row.size, row.mtime_ns, row.racy) {
                        (old.size, old.mtime_ns, old.racy) = (row.size, row.mtime_ns, row.racy);
                        self.version += 1;
                    }
                }
                _ => {
                    row.generation = next;
                    delta.changed.push(row.clone());
                    self.rows.insert(row.path.clone(), row);
                }
            }
        }
        for (p, r) in &mut self.rows {
            if !r.gone && !listed.contains(p) && !self.touched.contains(p) {
                r.gone = true;
                r.generation = next;
                delta.removed.push(p.clone());
            }
        }
        if !delta.is_empty() {
            self.generation = next;
            self.version += 1;
        }
        self.prune_tombstones();
        self.scans += 1;
        delta.changed.sort_by(|a, b| a.path.cmp(&b.path));
        (delta.epoch, delta.generation) = (self.epoch, self.generation);
        delta
    }

    /// Drop the oldest tombstones when there are more than
    /// [`MIN_TOMBSTONES`] and a quarter of the live rows, down to half that.
    fn prune_tombstones(&mut self) {
        let tombs = self.rows.values().filter(|r| r.gone).count();
        let live = self.rows.len() - tombs;
        let limit = MIN_TOMBSTONES.max(live / 4);
        if tombs <= limit {
            return;
        }
        let mut gens: Vec<u64> = self.rows.values().filter(|r| r.gone).map(|r| r.generation).collect();
        gens.sort_unstable();
        let cut = gens[tombs - limit / 2 - 1];
        self.rows.retain(|_, r| !(r.gone && r.generation <= cut));
        self.floor = self.floor.max(cut);
        self.version += 1;
    }
}

/// Persisting the table.
#[derive(Debug, Default)]
struct Saved {
    /// The `version` last written (or loaded).
    version: u64,
    /// The file holds a newer format: the table lives in memory only.
    kept_newer: bool,
}

struct Shared {
    storage: Arc<dyn Storage>,
    /// The cache file, when the storage has a place on disk.
    file: Option<PathBuf>,
    /// SHA-256 of `file`'s path: a copy made for another vault, or for this
    /// vault at another place, is not used.
    origin: [u8; 32],
    clock: Clock,
    table: RwLock<Table>,
    pending: Mutex<Vec<Own>>,
    /// One scan at a time.
    scanning: Mutex<()>,
    subscribers: Mutex<Vec<Sender<Arc<Delta>>>>,
    saved: Mutex<Saved>,
}

/// The vault manifest (see the module docs). Clones share one table.
#[derive(Clone)]
pub struct Manifest {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Manifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.read();
        f.debug_struct("Manifest")
            .field("file", &self.shared.file)
            .field("generation", &t.generation)
            .field("rows", &t.rows.len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resets the scan marks of the table when a scan ends, however it ends.
struct ScanMark<'a>(&'a RwLock<Table>);

impl Drop for ScanMark<'_> {
    fn drop(&mut self) {
        let mut t = self.0.write().unwrap_or_else(PoisonError::into_inner);
        t.scanning = false;
        t.touched.clear();
    }
}

impl Manifest {
    /// The manifest of `vault`'s storage: the cached table at
    /// [`CACHE_PATH`] if it is valid, else an empty one with a new epoch.
    /// Reads no note. Rows from the cache are only as good as the last
    /// scan that wrote them: [`Self::scan`] before trusting them.
    pub fn open(vault: &Vault) -> Self {
        Self::open_with_clock(vault, Arc::new(SystemTime::now))
    }

    /// [`Self::open`] with the clock the racy rule reads.
    pub fn open_with_clock(vault: &Vault, clock: Clock) -> Self {
        let storage = Arc::clone(vault.storage());
        let file = storage.absolute(&NotePath::new(CACHE_PATH));
        let origin: [u8; 32] =
            Sha256::digest(file.as_ref().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default().as_bytes()).into();
        let loaded = file.as_ref().and_then(|f| cache::read(f, Kind::MANIFEST, FORMAT, BUILDER)).and_then(|b| decode(&b, &origin));
        let table = loaded.unwrap_or_else(|| Table::fresh(new_epoch(&origin)));
        let saved = Saved { version: table.version, kept_newer: false };
        Self {
            shared: Arc::new(Shared {
                storage,
                file,
                origin,
                clock,
                table: RwLock::new(table),
                pending: Mutex::new(Vec::new()),
                scanning: Mutex::new(()),
                subscribers: Mutex::new(Vec::new()),
                saved: Mutex::new(saved),
            }),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Table> {
        self.shared.table.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Table> {
        self.shared.table.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply queued own writes if the table is free now.
    fn settle(&self) {
        if lock(&self.shared.pending).is_empty() {
            return;
        }
        if let Ok(mut t) = self.shared.table.try_write() {
            t.drain(&self.shared.pending);
        }
    }

    /// List the vault and bring every row up to date (see the module docs).
    ///
    /// A listing error, or a file that is listed but can't be read for
    /// another reason than being gone, is an error and changes no row. A
    /// file gone between the listing and its read counts as not listed.
    /// Subscribers get the [`Delta`] when it isn't empty. Another scan in
    /// progress is waited for; own writes never are.
    pub fn scan(&self) -> Result<Delta> {
        self.run_scan(true)
    }

    /// [`Self::scan`] trusting no row: every file is read and hashed. For a
    /// change that kept a file's size and time (sync runs it when idle, at
    /// most once a day, and on `quire-cli sync once --verify`).
    pub fn verify(&self) -> Result<Delta> {
        self.run_scan(false)
    }

    fn run_scan(&self, trust_stat: bool) -> Result<Delta> {
        let sh = &*self.shared;
        let _one = lock(&sh.scanning);
        let start_ns = FileStat::nanos((sh.clock)());
        // From before the listing: an own write made from here on (even
        // one queued now) is newer than what the scan sees, so its row is
        // left to the next scan.
        {
            let mut t = self.write();
            t.scanning = true;
            t.touched.clear();
            t.drain(&sh.pending);
        }
        let mark = ScanMark(&sh.table);
        let listing = sh.storage.list_stat()?;
        let to_read: Vec<(NotePath, Option<FileStat>)> = {
            let t = self.read();
            listing.iter().filter(|(p, st)| !(trust_stat && t.rows.get(p).is_some_and(|r| r.trusts(*st)))).cloned().collect()
        };
        let mut read = Vec::with_capacity(to_read.len());
        let mut vanished = HashSet::new();
        for (path, stat) in to_read {
            match sh.storage.read_bytes(&path) {
                Ok(bytes) => {
                    let mtime_ns = stat.map_or(0, |s| s.mtime_ns);
                    let racy = mtime_ns == 0 || mtime_ns.saturating_add(RACY_NS) > start_ns;
                    let hash = FileHash::of(&bytes);
                    read.push(Row { path, size: bytes.len() as u64, mtime_ns, hash, generation: 0, gone: false, racy });
                }
                Err(VaultError::NotFound(_)) => {
                    vanished.insert(path);
                }
                Err(e) => return Err(e),
            }
        }
        let listed: HashSet<NotePath> = listing.into_iter().map(|(p, _)| p).filter(|p| !vanished.contains(p)).collect();
        let delta = {
            let mut t = self.write();
            let delta = t.apply_scan(&listed, read, !trust_stat);
            t.drain(&sh.pending);
            delta
        };
        drop(mark);
        if !delta.is_empty() {
            let shared = Arc::new(delta.clone());
            lock(&sh.subscribers).retain(|s| s.send(Arc::clone(&shared)).is_ok());
        }
        Ok(delta)
    }

    /// Record that Quire wrote `bytes` at `path` (the storage path and the
    /// bytes as stored: for a locked note, the `.qenc` path and its
    /// encrypted bytes), and give their hash.
    ///
    /// Call it after the write landed and before telling anyone else (the
    /// index), so the new file is listed first. Never waits for a scan in
    /// progress: the row is queued then, and [`Self::get`] and
    /// [`Self::has`] already see it. The row is hashed again by the next
    /// scan. Not passed to subscribers: the writer tells them itself.
    pub fn note_written(&self, path: &NotePath, bytes: &[u8]) -> FileHash {
        let hash = FileHash::of(bytes);
        self.own(Own::Written { path: path.clone(), size: bytes.len() as u64, hash });
        hash
    }

    /// Record that Quire removed `path` (or that a read found it gone). As
    /// [`Self::note_written`], never waits.
    pub fn note_removed(&self, path: &NotePath) {
        self.own(Own::Removed(path.clone()));
    }

    fn own(&self, own: Own) {
        match self.shared.table.try_write() {
            Ok(mut t) => {
                t.drain(&self.shared.pending);
                t.apply_own(own);
            }
            Err(_) => lock(&self.shared.pending).push(own),
        }
    }

    /// The row of a file that is there now (no tombstones), own writes
    /// still queued included (they report the next generation).
    pub fn get(&self, path: &NotePath) -> Option<Row> {
        self.settle();
        let queued = lock(&self.shared.pending).iter().rev().find(|o| o.path() == path).cloned();
        let t = self.read();
        match queued {
            Some(Own::Written { path, size, hash }) => {
                Some(Row { path, size, mtime_ns: 0, hash, generation: t.generation + 1, gone: false, racy: true })
            }
            Some(Own::Removed(_)) => None,
            None => t.rows.get(path).filter(|r| !r.gone).cloned(),
        }
    }

    /// Whether `path` is a regular file the manifest lists (not a symlink,
    /// nothing in a hidden folder). Before the first scan in this process
    /// the answer comes from the cached table.
    pub fn has(&self, path: &NotePath) -> bool {
        self.get(path).is_some()
    }

    /// Every row that changed after `generation`, tombstones included,
    /// sorted by path; `None` when tombstones that old were dropped, so the
    /// caller compares everything with [`Self::rows`] instead (as for
    /// another epoch). Own writes still queued come in a later generation.
    pub fn since(&self, generation: u64) -> Option<Vec<Row>> {
        self.settle();
        let t = self.read();
        (generation >= t.floor).then(|| t.rows.values().filter(|r| r.generation > generation).cloned().collect())
    }

    /// Every file there now (no tombstones), sorted by path.
    pub fn rows(&self) -> Vec<Row> {
        self.settle();
        self.read().rows.values().filter(|r| !r.gone).cloned().collect()
    }

    /// The current generation: bumped by every scan or own write that
    /// changed a row.
    pub fn generation(&self) -> u64 {
        self.settle();
        self.read().generation
    }

    /// 16 random bytes, new whenever the table was built from nothing. A
    /// persisted `(epoch, generation)` cursor is valid only while this matches.
    pub fn epoch(&self) -> [u8; 16] {
        self.read().epoch
    }

    /// Whether a scan finished in this process (rows before that come
    /// from the cache).
    pub fn scanned(&self) -> bool {
        self.read().scans > 0
    }

    /// Every later scan's [`Delta`] that changed something.
    pub fn subscribe(&self) -> Receiver<Arc<Delta>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        lock(&self.shared.subscribers).push(tx);
        rx
    }

    /// Whether the table changed since it was last written or loaded.
    pub fn is_dirty(&self) -> bool {
        self.settle();
        let saved = lock(&self.shared.saved);
        !saved.kept_newer && self.shared.file.is_some() && saved.version != self.read().version
    }

    /// Write the table to [`CACHE_PATH`] if it changed (whole-file replace,
    /// not flushed to disk; [`quire_base::cache::write`]). Does nothing for
    /// a storage with no place on disk, or once the file was found to hold
    /// a newer format (P3: the table then lives in memory). Two processes
    /// may replace each other's file; either is valid for the next start.
    /// When to call it (when quiet for 30 s, at most every 5 minutes while
    /// busy, on clean exit) is the owner's choice
    /// (`docs/design/0.14-architecture.md` §3).
    pub fn flush(&self) -> std::io::Result<()> {
        let Some(file) = &self.shared.file else { return Ok(()) };
        self.settle();
        let mut saved = lock(&self.shared.saved);
        let (body, version) = {
            let t = self.read();
            if saved.kept_newer || saved.version == t.version {
                return Ok(());
            }
            (encode(&t, &self.shared.origin), t.version)
        };
        match cache::write(file, Kind::MANIFEST, FORMAT, BUILDER, &body)? {
            cache::Written::Written => saved.version = version,
            cache::Written::KeptNewer => saved.kept_newer = true,
        }
        Ok(())
    }
}

const RACY_NS: u64 = RACY_WINDOW.as_nanos() as u64;

/// 16 random bytes; if the system has none, a value from the time, the
/// process and the vault (an epoch must differ from the last one, not be
/// secret).
fn new_epoch(origin: &[u8; 32]) -> [u8; 16] {
    quire_base::rand::bytes::<16>().unwrap_or_else(|_| {
        let mut h = Sha256::new();
        h.update(FileStat::nanos(SystemTime::now()).to_le_bytes());
        h.update(std::process::id().to_le_bytes());
        h.update(origin);
        let d = h.finalize();
        let mut e = [0u8; 16];
        e.copy_from_slice(&d[..16]);
        e
    })
}

// ------------------------------------------------------------------ the body

const GONE: u8 = 1;
const RACY: u8 = 2;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// The body (`docs/formats/manifest.md`).
fn encode(t: &Table, origin: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(80 + t.rows.len() * 80);
    out.extend_from_slice(origin);
    out.extend_from_slice(&t.epoch);
    out.extend_from_slice(&t.generation.to_le_bytes());
    out.extend_from_slice(&t.floor.to_le_bytes());
    out.extend_from_slice(&(t.rows.len() as u64).to_le_bytes());
    for r in t.rows.values() {
        let p = r.path.as_str().as_bytes();
        put_varint(&mut out, p.len() as u64);
        out.extend_from_slice(p);
        put_varint(&mut out, r.size);
        out.extend_from_slice(&r.mtime_ns.to_le_bytes());
        out.extend_from_slice(&r.hash.0);
        put_varint(&mut out, r.generation);
        out.push(if r.gone { GONE } else { 0 } | if r.racy { RACY } else { 0 });
    }
    out
}

/// Reads a body front to back; every method is `None` past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u64(&mut self) -> Option<u64> {
        self.array::<8>().map(u64::from_le_bytes)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            v |= u64::from(b & 0x7f).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

/// The table in `body`, `None` for anything doubtful (another origin,
/// a bad path, rows out of order, unknown flags, a generation from the
/// future, bytes left over).
fn decode(body: &[u8], origin: &[u8; 32]) -> Option<Table> {
    let mut r = Reader(body);
    if r.array::<32>()? != *origin {
        return None;
    }
    let mut t = Table::fresh(r.array::<16>()?);
    t.generation = r.u64()?;
    t.floor = r.u64()?;
    let count = r.u64()?;
    let mut last: Option<NotePath> = None;
    for _ in 0..count {
        let len = usize::try_from(r.varint()?).ok()?;
        let raw = std::str::from_utf8(r.take(len)?).ok()?;
        if raw.is_empty() || !quire_base::path::Rule::InsideRoot.accepts(raw) {
            return None;
        }
        let path = NotePath::new(raw);
        if path.as_str() != raw || last.as_ref().is_some_and(|l| *l >= path) {
            return None;
        }
        let size = r.varint()?;
        let mtime_ns = r.u64()?;
        let hash = FileHash(r.array::<32>()?);
        let generation = r.varint()?;
        let flags = r.take(1)?[0];
        if flags & !(GONE | RACY) != 0 || generation > t.generation {
            return None;
        }
        last = Some(path.clone());
        t.rows.insert(path.clone(), Row { path, size, mtime_ns, hash, generation, gone: flags & GONE != 0, racy: flags & RACY != 0 });
    }
    r.0.is_empty().then_some(t)
}

#[cfg(test)]
mod tests;
