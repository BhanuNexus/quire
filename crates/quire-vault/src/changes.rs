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
//! trusted on the next scan. [`Manifest::verify`] trusts no row, and
//! [`Manifest::verify_where`] none of the files it is given. Files are
//! hashed through [`Storage::hash_file`], in pieces where the storage can
//! (a video is never read into memory whole). A listing
//! error is an error, never "all gone", and changes no row. A file that
//! can't be read (locked by another program, no permission, an I/O error)
//! keeps its row, racy and never removed, and is named in
//! [`Delta::unread`]: one such file never stops the others' updates.
//!
//! **Our own writes.** [`Manifest::note_written`] and [`Manifest::note_removed`]
//! update the row at once and never wait for a scan in progress (P9): while
//! the table is busy they queue, and [`Manifest::get`] and [`Manifest::has`]
//! already answer with them. Such a row is hashed again by the next scan.
//!
//! **Changes found by reading.** [`Manifest::observed`] is for bytes Quire
//! read but did not write (sync or the editor reading a note another
//! program changed): when the hash is new, the row changes at once and
//! every subscriber gets a [`Delta`] with that row, so a change found by
//! reading reaches the index as one found by a scan does.
//!
//! **Sharing.** A [`Manifest`] is a handle: clones share one table, so one
//! scanner serves a vault. Each scan's [`Delta`] that changed something,
//! and each observed change, goes to every [`Manifest::subscribe`]r. A
//! subscriber hears only what happens after it subscribed, and never our
//! own writes: it reconciles with [`Manifest::since`] at start. The table persists through
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
/// 2: tombstones carry no hash, size or time, and the epoch is not stored.
pub const FORMAT: u32 = 2;
/// What derives the rows: SHA-256 of the stored bytes, the racy rule.
pub const BUILDER: u32 = 1;
/// A row whose time is this close to its scan (or later) is hashed again
/// by the next scan.
pub const RACY_WINDOW: Duration = Duration::from_secs(2);
/// The buffer [`FileHash::of_reader`] (and so [`Storage::hash_file`] on
/// [`crate::LocalFs`]) reads a file through.
pub const HASH_BUFFER: usize = 64 * 1024;
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

    /// The hash of everything `reader` gives, and how many bytes that was,
    /// read in pieces of [`HASH_BUFFER`] bytes: the whole is never in
    /// memory at once.
    pub fn of_reader(mut reader: impl std::io::Read) -> std::io::Result<(Self, u64)> {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; HASH_BUFFER];
        let mut len = 0u64;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Ok((Self(hasher.finalize().into()), len)),
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    len += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
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
    /// Bytes on disk when it was hashed; 0 for a tombstone.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch as the listing
    /// gave it; 0 when unknown (never trusted) and for a tombstone.
    pub mtime_ns: u64,
    /// SHA-256 of the bytes; all zeros for a tombstone (no consumer needs
    /// it, and a locked note's plain-text hash must not stay behind).
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

    /// Make this row a tombstone of `generation`. A tombstone keeps only its
    /// path and generation: no hash, size or time (the hash of a note's
    /// plain text must not outlive it once its folder is locked).
    fn bury(&mut self, generation: u64) {
        (self.gone, self.generation) = (true, generation);
        (self.hash, self.size, self.mtime_ns, self.racy) = (FileHash([0; 32]), 0, 0, false);
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
    /// Files listed but not readable (locked by another program, no
    /// permission, an I/O error), sorted, with the error. Each keeps its
    /// row, marked racy so the next scan tries again, and is never taken
    /// for removed. Not a change: a [`Delta`] with only these is not sent
    /// to subscribers.
    pub unread: Vec<(NotePath, String)>,
    /// From [`Manifest::verify`] (no row was trusted) or
    /// [`Manifest::verify_where`] (no row of the files it was given).
    pub verified: bool,
}

impl Delta {
    /// Nothing changed ([`Self::unread`] is not a change).
    pub const fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

/// The clock a manifest reads for the racy rule (injected in tests, P10).
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// One of our own writes, or bytes a read found, waiting for the table.
#[derive(Debug, Clone)]
enum Own {
    /// `observed`: found by reading, not written by us, so a change is
    /// sent to subscribers ([`Manifest::observed`]).
    Written {
        path: NotePath,
        size: u64,
        hash: FileHash,
        observed: bool,
    },
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
    /// Deltas of observed changes not yet sent to subscribers: whoever
    /// applied them sends them once the table is free.
    outbox: Vec<Delta>,
}

impl Table {
    fn fresh(epoch: [u8; 16]) -> Self {
        Self {
            epoch,
            generation: 0,
            floor: 0,
            rows: BTreeMap::new(),
            version: 0,
            scanning: false,
            touched: HashSet::new(),
            scans: 0,
            outbox: Vec::new(),
        }
    }

    fn apply_own(&mut self, own: Own) {
        if self.scanning {
            self.touched.insert(own.path().clone());
        }
        let next = self.generation + 1;
        match own {
            Own::Written { path, size, hash, observed } => {
                match self.rows.get_mut(&path) {
                    // Bytes a read found that the row already has: nothing
                    // to record (the row's time and size stay the listing's).
                    Some(r) if observed && !r.gone && r.hash == hash => return,
                    Some(r) if !r.gone && r.hash == hash => {
                        r.size = size;
                        r.mtime_ns = 0;
                        r.racy = true;
                    }
                    _ => {
                        let row = Row { path: path.clone(), size, mtime_ns: 0, hash, generation: next, gone: false, racy: true };
                        if observed {
                            self.outbox.push(Delta { epoch: self.epoch, generation: next, changed: vec![row.clone()], ..Delta::default() });
                        }
                        self.rows.insert(path, row);
                        self.generation = next;
                    }
                }
                self.version += 1;
            }
            Own::Removed(path) => {
                if let Some(r) = self.rows.get_mut(&path)
                    && !r.gone
                {
                    r.bury(next);
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
    /// that was still there to read (unreadable ones included), `read` the
    /// rows of files it hashed, `unread` the files it could not read.
    fn apply_scan(&mut self, listed: &HashSet<NotePath>, read: Vec<Row>, mut unread: Vec<(NotePath, String)>, verified: bool) -> Delta {
        let next = self.generation + 1;
        for (p, _) in &unread {
            if let Some(r) = self.rows.get_mut(p)
                && !r.gone
                && !r.racy
                && !self.touched.contains(p)
            {
                r.racy = true;
                self.version += 1;
            }
        }
        unread.sort();
        let mut delta = Delta { unread, verified, ..Delta::default() };
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
                r.bury(next);
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
    /// [`CACHE_PATH`] if it is valid, else an empty one. Either way with a
    /// new epoch ([`Self::epoch`]). Reads no note. Rows from the cache are only as good as the last
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
        let mut table = loaded.unwrap_or_else(|| Table::fresh([0; 16]));
        // New on every open: after a crash, or with another process's file,
        // the same generation numbers can come back with other rows.
        table.epoch = new_epoch(&origin);
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
        let Ok(mut t) = self.shared.table.try_write() else { return };
        t.drain(&self.shared.pending);
        let out = std::mem::take(&mut t.outbox);
        drop(t);
        self.send(out);
    }

    /// List the vault and bring every row up to date (see the module docs).
    ///
    /// A listing error is an error and changes no row. A file that is
    /// listed but can't be read for another reason than being gone keeps
    /// its row (marked racy, never removed) and is named in
    /// [`Delta::unread`]; the other files are brought up to date. A file
    /// gone between the listing and its read counts as not listed.
    /// Subscribers get the [`Delta`] when it isn't empty. Another scan in
    /// progress is waited for; own writes never are.
    pub fn scan(&self) -> Result<Delta> {
        self.run_scan(&|_| true, false)
    }

    /// [`Self::scan`] trusting no row: every file is read and hashed. For a
    /// change that kept a file's size and time (sync runs it when idle, at
    /// most once a day, and on `quire-cli sync once --verify`). Reads the
    /// whole vault, videos included: a consumer that tracks only some files
    /// uses [`Self::verify_where`].
    pub fn verify(&self) -> Result<Delta> {
        self.run_scan(&|_| false, true)
    }

    /// [`Self::verify`] for the files `which` accepts: those are read and
    /// hashed whatever their size and time say; every other file is
    /// scanned as by [`Self::scan`]. Sync passes the notes it tracks, so a
    /// daily verify does not read every attachment. The [`Delta`] says
    /// `verified`.
    pub fn verify_where(&self, which: &dyn Fn(&NotePath) -> bool) -> Result<Delta> {
        self.run_scan(&|p| !which(p), true)
    }

    /// One scan: a listed file is kept without reading only when `trust`
    /// accepts its path and its row trusts the listing's stat.
    fn run_scan(&self, trust: &dyn Fn(&NotePath) -> bool, verified: bool) -> Result<Delta> {
        let sh = &*self.shared;
        let _one = lock(&sh.scanning);
        let start_ns = FileStat::nanos((sh.clock)());
        // Own writes queued before now are older than the listing: they are
        // applied first, unmarked, and the scan hashes them like any racy
        // row. From here on the table is marked as scanning, so an own write
        // made while the scan lists and reads (applied at once, or queued and
        // applied when the scan ends) goes into `touched`, and the scan
        // leaves that row to the next scan.
        let early = {
            let mut t = self.write();
            t.drain(&sh.pending);
            t.scanning = true;
            t.touched.clear();
            std::mem::take(&mut t.outbox)
        };
        self.send(early);
        let mark = ScanMark(&sh.table);
        let listing = sh.storage.list_stat()?;
        let to_read: Vec<(NotePath, Option<FileStat>)> = {
            let t = self.read();
            listing.iter().filter(|(p, st)| !(trust(p) && t.rows.get(p).is_some_and(|r| r.trusts(*st)))).cloned().collect()
        };
        let mut read = Vec::with_capacity(to_read.len());
        let mut vanished = HashSet::new();
        let mut unread = Vec::new();
        for (path, stat) in to_read {
            match sh.storage.hash_file(&path) {
                Ok((hash, size)) => {
                    let mtime_ns = stat.map_or(0, |s| s.mtime_ns);
                    let racy = mtime_ns == 0 || mtime_ns.saturating_add(RACY_NS) > start_ns;
                    read.push(Row { path, size, mtime_ns, hash, generation: 0, gone: false, racy });
                }
                Err(VaultError::NotFound(_)) => {
                    vanished.insert(path);
                }
                // Locked by another program, no permission, a bad sector:
                // this file only. Its row stays, racy, and is never taken
                // for removed (it is still listed).
                Err(e) => unread.push((path, e.to_string())),
            }
        }
        let listed: HashSet<NotePath> = listing.into_iter().map(|(p, _)| p).filter(|p| !vanished.contains(p)).collect();
        let (delta, late) = {
            let mut t = self.write();
            let delta = t.apply_scan(&listed, read, unread, verified);
            t.drain(&sh.pending);
            (delta, std::mem::take(&mut t.outbox))
        };
        drop(mark);
        if !delta.is_empty() {
            self.send(vec![delta.clone()]);
        }
        self.send(late);
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
        self.own(Own::Written { path: path.clone(), size: bytes.len() as u64, hash, observed: false });
        hash
    }

    /// Record that a read found `bytes` at `path` (the storage path and the
    /// bytes as stored), and give their hash: for bytes Quire did not write
    /// itself, such as a note another program changed that sync or the
    /// editor just read. When the hash differs from the row (or there is no
    /// row), the row changes as for [`Self::note_written`] and every
    /// subscriber gets a [`Delta`] with that one row; the same bytes again
    /// change nothing. The next scan reads the file again and, finding the
    /// same bytes, sends nothing more. Never waits: while a scan holds the
    /// table it is queued, and the [`Delta`] goes out when it is applied.
    pub fn observed(&self, path: &NotePath, bytes: &[u8]) -> FileHash {
        let hash = FileHash::of(bytes);
        self.own(Own::Written { path: path.clone(), size: bytes.len() as u64, hash, observed: true });
        hash
    }

    /// Record that Quire removed `path` (or that a read found it gone). As
    /// [`Self::note_written`], never waits.
    pub fn note_removed(&self, path: &NotePath) {
        self.own(Own::Removed(path.clone()));
    }

    fn own(&self, own: Own) {
        let Ok(mut t) = self.shared.table.try_write() else {
            lock(&self.shared.pending).push(own);
            return;
        };
        t.drain(&self.shared.pending);
        t.apply_own(own);
        let out = std::mem::take(&mut t.outbox);
        drop(t);
        self.send(out);
    }

    /// Send `deltas` to every subscriber, in order, forgetting the ones
    /// that hung up. Called with the table free.
    fn send(&self, deltas: Vec<Delta>) {
        if deltas.is_empty() {
            return;
        }
        let mut subs = lock(&self.shared.subscribers);
        for d in deltas {
            let d = Arc::new(d);
            subs.retain(|s| s.send(Arc::clone(&d)).is_ok());
        }
    }

    /// The row of a file that is there now (no tombstones), own writes
    /// still queued included (they report the next generation).
    pub fn get(&self, path: &NotePath) -> Option<Row> {
        self.settle();
        let queued = lock(&self.shared.pending).iter().rev().find(|o| o.path() == path).cloned();
        let t = self.read();
        match queued {
            // Observed bytes the row already has change nothing.
            Some(Own::Written { observed: true, hash, .. }) if t.rows.get(path).is_some_and(|r| !r.gone && r.hash == hash) => {
                t.rows.get(path).cloned()
            }
            Some(Own::Written { path, size, hash, .. }) => {
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
    /// another process). Own writes still queued come in a later generation.
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

    /// 16 random bytes, new on every [`Self::open`]: a generation means
    /// something only next to the epoch it came with, so only within this
    /// process. A consumer that persists what it saw compares by hash on
    /// its next start; it never keeps an `(epoch, generation)` cursor
    /// across processes.
    pub fn epoch(&self) -> [u8; 16] {
        self.read().epoch
    }

    /// Whether a scan finished in this process (rows before that come
    /// from the cache).
    pub fn scanned(&self) -> bool {
        self.read().scans > 0
    }

    /// Every later scan's [`Delta`] that changed something, and one for each
    /// [`Self::observed`] change. Own writes ([`Self::note_written`],
    /// [`Self::note_removed`]) are not sent: the writer tells others itself.
    ///
    /// Nothing from before the call is sent: a subscriber reconciles at
    /// start with [`Self::since`] (from the generation it knows, after
    /// subscribing; [`Self::rows`] when it knows none or `since` gives
    /// `None`), then applies the deltas. A change may then arrive both ways;
    /// compare by hash.
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
    out.extend_from_slice(&t.generation.to_le_bytes());
    out.extend_from_slice(&t.floor.to_le_bytes());
    out.extend_from_slice(&(t.rows.len() as u64).to_le_bytes());
    for r in t.rows.values() {
        let p = r.path.as_str().as_bytes();
        put_varint(&mut out, p.len() as u64);
        out.extend_from_slice(p);
        out.push(if r.gone { GONE } else { 0 } | if r.racy { RACY } else { 0 });
        put_varint(&mut out, r.generation);
        if !r.gone {
            put_varint(&mut out, r.size);
            out.extend_from_slice(&r.mtime_ns.to_le_bytes());
            out.extend_from_slice(&r.hash.0);
        }
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
/// a bad path, rows out of order, unknown flags, a racy tombstone, a
/// generation from the future, bytes left over). Its epoch is left for
/// the caller to set.
fn decode(body: &[u8], origin: &[u8; 32]) -> Option<Table> {
    let mut r = Reader(body);
    if r.array::<32>()? != *origin {
        return None;
    }
    let mut t = Table::fresh([0; 16]);
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
        let flags = r.take(1)?[0];
        let generation = r.varint()?;
        let (gone, racy) = (flags & GONE != 0, flags & RACY != 0);
        if flags & !(GONE | RACY) != 0 || (gone && racy) || generation > t.generation {
            return None;
        }
        let mut row = Row { path: path.clone(), size: 0, mtime_ns: 0, hash: FileHash([0; 32]), generation, gone, racy };
        if !gone {
            row.size = r.varint()?;
            row.mtime_ns = r.u64()?;
            row.hash = FileHash(r.array::<32>()?);
        }
        last = Some(path.clone());
        t.rows.insert(path, row);
    }
    r.0.is_empty().then_some(t)
}

#[cfg(test)]
mod tests;
