// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The index's startup cache under `<vault>/.quire/cache/index/`
//! (`docs/design/0.14-index-startup.md` §3.6; format: `docs/formats/index-cache.md`).
//!
//! `records.idx` has one row per note: its path, the manifest hash of the
//! text the record was made from, the segment that holds it and when the
//! note last changed. A segment `r-<16 hex>.seg` holds up to
//! 512 records (section A) and their sources (section B), each
//! deflated; it is written once and never changed in place.
//!
//! The rules this module keeps:
//! - **The cache never decides by itself.** A row is used only when its hash
//!   equals the manifest's row for the note now; anything else is parsed.
//! - **Any doubt is "no cache".** A file that is missing, of another
//!   version, fails its hash or doesn't decode costs parses, never an error.
//! - **One writer.** Only the process holding `writer.lock` writes or deletes
//!   segments; the lock is never waited for.
//! - **No locked text.** Every path is checked against the locked folders
//!   just before each write and again just after; if one is locked, the
//!   file just written is deleted. A load that finds a locked path in
//!   `records.idx` deletes every cache file before reading a segment.
//! - **Removed text leaves at the next flush.** A segment holding a note
//!   that is gone or was replaced is rewritten without it, and every segment
//!   the new `records.idx` doesn't reference is deleted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quire_base::cache::{self, Kind};
use quire_vault::changes::FileHash;
use quire_vault::{NotePath, Vault};

use crate::codec::{Reader, Stored, Writer, decode_record, encode_record};
use crate::index::Index;

/// The version of what derives a record from a note.
///
/// That is `extract`, the tokenizer and stemmer and the BM25 weights. Bump
/// it (and the golden hash in `tests/derive_version.rs`) when any of them
/// changes what it gives: every cache made before is then discarded, never
/// migrated.
pub const DERIVE_VERSION: u32 = 1;
/// The layout of `records.idx` and of a segment.
/// 2: records in the hand-written codec (1 held them as JSON, never released).
pub(crate) const FORMAT: u32 = 2;
/// Where the cache lives, vault-relative.
pub const DIR: &str = ".quire/cache/index";
const INDEX_FILE: &str = "records.idx";
const LOCK_FILE: &str = "writer.lock";
/// Notes in one segment at most.
pub(crate) const SEGMENT_NOTES: usize = 512;
/// Above this many segments a flush rewrites them all into full ones.
const MAX_SEGMENTS: usize = 256;
/// A temporary file older than this was left by a crash.
const LEFTOVER_AGE: Duration = Duration::from_mins(1);

/// One row of `records.idx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) hash: FileHash,
    pub(crate) segment: u64,
    pub(crate) slot: u32,
    /// When the note last changed, Unix ms (`Index::modified`).
    pub(crate) modified: i64,
}

fn segment_name(id: u64) -> String {
    format!("r-{id:016x}.seg")
}

fn segment_id(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("r-")?.strip_suffix(".seg")?;
    (hex.len() == 16).then(|| u64::from_str_radix(hex, 16).ok()).flatten()
}

/// A path as the vault names a note: not empty, relative, no `.` or `..`
/// segment, nothing hidden.
fn plausible(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && !s.starts_with('.'))
}

pub(crate) fn encode_index(rows: &BTreeMap<NotePath, Row>) -> Vec<u8> {
    let mut w = Writer::default();
    w.usize(rows.len());
    for (p, r) in rows {
        w.str(p.as_str());
        w.buf.extend_from_slice(&r.hash.0);
        w.buf.extend_from_slice(&r.segment.to_le_bytes());
        w.u64(u64::from(r.slot));
        w.i64(r.modified);
    }
    w.buf
}

/// The rows of `records.idx`, `None` on any doubt (paths out of order or
/// not of a note, a slot past a segment's size, bytes left over).
pub(crate) fn decode_index(body: &[u8]) -> Option<BTreeMap<NotePath, Row>> {
    let mut r = Reader::new(body);
    let n = r.count()?;
    let mut rows = BTreeMap::new();
    let mut last: Option<&str> = None;
    for _ in 0..n {
        let p = r.str()?;
        if !plausible(p) || last.is_some_and(|l| l >= p) {
            return None;
        }
        last = Some(p);
        let hash = FileHash(r.take(32)?.try_into().ok()?);
        let segment = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
        let slot = u32::try_from(r.u64()?).ok().filter(|s| (*s as usize) < SEGMENT_NOTES)?;
        let modified = r.i64()?;
        rows.insert(NotePath::new(p), Row { hash, segment, slot, modified });
    }
    r.is_empty().then_some(rows)
}

fn deflate(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::with_capacity(b.len() / 3), flate2::Compression::new(1));
    // Writing into memory has no failure.
    if e.write_all(b).is_err() {
        return Vec::new();
    }
    e.finish().unwrap_or_default()
}

/// Exactly `len` bytes inflated from `b`, or `None` (also for a stream that
/// would give more: nothing past `len` is ever inflated).
fn inflate(b: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).ok()?;
    let mut d = flate2::read::DeflateDecoder::new(b);
    (&mut d).take(len as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() == len).then_some(out)
}

/// What a segment holds for one note, ready to encode.
pub(crate) struct Entry<'a> {
    pub(crate) rec: &'a crate::NoteRecord,
    pub(crate) content: Vec<(&'a str, u32)>,
    pub(crate) bm25: Vec<(&'a str, f32)>,
    pub(crate) len: f32,
    pub(crate) source: &'a str,
}

/// The entry of an indexed note, from the index's own data.
pub(crate) fn entry<'a>(ix: &'a Index, p: &NotePath) -> Option<Entry<'a>> {
    let rec = ix.get(p)?;
    let source = ix.source(p)?;
    let ft = ix.fulltext();
    let doc = ft.doc_id(p)?;
    let bm25 = ft.doc_terms(doc).iter().map(|t| (ft.term(*t), ft.weight(*t, doc).unwrap_or(0.0))).collect();
    let content = rec.terms.iter().map(|(t, n)| (ft.term(*t), *n)).collect();
    Some(Entry { rec, content, bm25, len: ft.doc_len(doc)?, source })
}

/// A segment's body: its term table, then section A (records) and section
/// B (sources), each deflated.
pub(crate) fn encode_segment(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut table: Vec<&str> =
        entries.iter().flat_map(|e| e.content.iter().map(|(t, _)| *t).chain(e.bm25.iter().map(|(t, _)| *t))).collect();
    table.sort_unstable();
    table.dedup();
    let ids: HashMap<&str, u32> = table.iter().enumerate().map(|(i, t)| (*t, u32::try_from(i).unwrap_or(u32::MAX))).collect();
    let mut a = Writer::default();
    let mut b = Vec::new();
    for e in entries {
        let content: Vec<(u32, u32)> = e.content.iter().map(|(t, n)| (ids[t], *n)).collect();
        let bm25: Vec<(u32, f32)> = e.bm25.iter().map(|(t, f)| (ids[t], *f)).collect();
        encode_record(&mut a, e.rec, &content, &bm25, e.len);
        a.usize(e.source.len());
        b.extend_from_slice(e.source.as_bytes());
    }
    let mut w = Writer::default();
    w.usize(table.len());
    for t in &table {
        w.str(t);
    }
    w.usize(entries.len());
    for section in [&a.buf, &b] {
        w.usize(section.len());
        w.bytes(&deflate(section));
    }
    w.buf
}

/// A segment's section B (the notes' texts), still deflated: decoded after
/// keyword search is ready, or on demand for one note before that.
#[derive(Debug)]
pub(crate) struct Texts {
    deflated: Vec<u8>,
    len: usize,
}

impl Texts {
    /// Every text of the segment, one after another.
    pub(crate) fn inflate(&self) -> Option<Vec<u8>> {
        inflate(&self.deflated, self.len)
    }

    /// One note's text out of [`Self::inflate`]'s bytes.
    pub(crate) fn text(all: &[u8], range: &std::ops::Range<usize>) -> Option<String> {
        std::str::from_utf8(all.get(range.clone())?).ok().map(str::to_string)
    }
}

/// A decoded segment: its term table, each note's record and the range of
/// its text in section B, and section B itself, still deflated.
pub(crate) struct Segment {
    pub(crate) table: Vec<String>,
    pub(crate) notes: Vec<(Stored, std::ops::Range<usize>)>,
    pub(crate) texts: std::sync::Arc<Texts>,
}

/// Section A of a segment decoded; section B checked for its length only.
pub(crate) fn decode_segment(body: &[u8]) -> Option<Segment> {
    let mut r = Reader::new(body);
    let n_terms = r.count()?;
    let table = (0..n_terms).map(|_| r.str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    let n = r.usize().filter(|n| (1..=SEGMENT_NOTES).contains(n))?;
    let a_len = r.usize()?;
    let a = inflate(r.bytes()?, a_len)?;
    let b_len = r.usize()?;
    let texts = Texts { deflated: r.bytes()?.to_vec(), len: b_len };
    if !r.is_empty() {
        return None;
    }
    let mut ra = Reader::new(&a);
    let mut at = 0usize;
    let mut notes = Vec::with_capacity(n);
    for _ in 0..n {
        let stored = decode_record(&mut ra, table.len())?;
        let len = ra.usize()?;
        let end = at.checked_add(len)?;
        notes.push((stored, at..end));
        at = end;
    }
    (ra.is_empty() && at == b_len).then(|| Segment { table, notes, texts: std::sync::Arc::new(texts) })
}

/// `f` over `jobs` on up to `threads` threads (at least one), each taking
/// the next job until none is left; results in job order, `None` where a
/// thread failed.
pub(crate) fn pool<T: Sync, R: Send>(jobs: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut slots: Vec<Option<R>> = (0..jobs.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.clamp(1, jobs.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    crate::background::lower_priority();
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break done };
                        done.push((i, f(job)));
                    }
                })
            })
            .collect();
        for w in workers {
            for (i, r) in w.join().unwrap_or_default() {
                slots[i] = Some(r);
            }
        }
    });
    slots
}

/// Whether `path` holds a cache container of the records index in a newer
/// `format` than this build's (judged by its header alone).
fn newer_format(path: &Path) -> bool {
    let mut h = [0u8; 14];
    let Ok(mut f) = File::open(path) else { return false };
    f.read_exact(&mut h).is_ok()
        && h[..8] == cache::MAGIC
        && u16::from_le_bytes([h[8], h[9]]) == Kind::RECORDS_INDEX.0
        && u32::from_le_bytes([h[10], h[11], h[12], h[13]]) > FORMAT
}

/// Whether any of `paths` is in a locked folder now. Asks once per folder.
fn any_locked<'a>(vault: &Vault, paths: impl IntoIterator<Item = &'a NotePath>) -> bool {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    paths.into_iter().any(|p| {
        let folder = p.as_str().rsplit_once('/').map_or("", |(f, _)| f);
        seen.insert(folder) && vault.locked_folder(p).is_some()
    })
}

#[cfg(test)]
thread_local! {
    /// Fault injection: cache writes allowed before the next one fails, as
    /// if the process stopped there (`docs/design/0.14-index-startup.md` §6).
    pub(crate) static WRITES_LEFT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// One cache file written, or the injected crash.
fn put(path: &Path, kind: Kind, body: &[u8]) -> std::io::Result<cache::Written> {
    #[cfg(test)]
    if let Some(n) = WRITES_LEFT.get() {
        if n == 0 {
            return Err(std::io::Error::other("stopped by the test"));
        }
        WRITES_LEFT.set(Some(n - 1));
    }
    cache::write(path, kind, FORMAT, DERIVE_VERSION, body)
}

/// What a load found in the cache.
#[derive(Default)]
pub(crate) struct Found {
    /// Segments with the notes to use from them.
    pub(crate) segments: Vec<Segment>,
    /// Notes to read and parse.
    pub(crate) parse: Vec<NotePath>,
}

/// The cache of one vault, as the index worker keeps it.
pub(crate) struct Store {
    dir: PathBuf,
    vault: Vault,
    /// `writer.lock`, once this process got it.
    lock: Option<File>,
    /// `records.idx` as this process last read or wrote it.
    rows: BTreeMap<NotePath, Row>,
    /// Every indexed note's hash and time.
    current: HashMap<NotePath, (FileHash, i64)>,
    /// Notes whose record isn't in the cache as it is now.
    dirty: BTreeSet<NotePath>,
    /// When the first change since the last flush was made, and the last.
    first_change: Option<Instant>,
    last_change: Option<Instant>,
    /// A note was removed or replaced: its text must leave within 60 s.
    removal: Option<Instant>,
    /// A newer build's file was found: the cache is left to it (P3).
    kept_newer: bool,
    /// Segments holding a copy of a note whose row was dropped (damage):
    /// written again at the next flush, without that copy.
    rewrite: BTreeSet<u64>,
}

impl Store {
    /// The cache of `vault`, if its storage has a place on disk. Sweeps
    /// what a crashed write left.
    pub(crate) fn open(vault: &Vault) -> Option<Self> {
        let dir = vault.storage().absolute(&NotePath::new(DIR))?;
        quire_base::fs::sweep_leftovers(&dir, LEFTOVER_AGE);
        Some(Self {
            dir,
            vault: vault.clone(),
            lock: None,
            rows: BTreeMap::new(),
            current: HashMap::new(),
            dirty: BTreeSet::new(),
            first_change: None,
            last_change: None,
            removal: None,
            kept_newer: false,
            rewrite: BTreeSet::new(),
        })
    }

    /// Delete every cache file (not the lock: another process may hold it).
    pub(crate) fn purge(&mut self) {
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if e.file_name() != LOCK_FILE {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "a file already gone is what was wanted; the rest are retried at the next purge"
                )]
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.rows.clear();
        self.dirty.extend(self.current.keys().cloned());
        self.touch();
    }

    /// Read the cache for `notes` (with the manifest's hash of each now).
    /// Uses `threads` threads for the segments.
    pub(crate) fn load(&mut self, notes: &[(NotePath, FileHash)], threads: usize) -> Found {
        let index_file = self.dir.join(INDEX_FILE);
        let Some(rows) = cache::read(&index_file, Kind::RECORDS_INDEX, FORMAT, DERIVE_VERSION).and_then(|b| decode_index(&b)) else {
            // Without a usable `records.idx` no segment can be checked
            // against the locked folders: they all go, unless a newer
            // build's cache is there (P3). Only the writer deletes: if
            // another process holds the lock (its first flush, another
            // derive version), its files are its own, and it deletes what
            // its `records.idx` doesn't reference at its next flush. This
            // process reads none of them.
            if !newer_format(&index_file) && self.writer() {
                self.purge();
            }
            return Found { segments: Vec::new(), parse: notes.iter().map(|(p, _)| p.clone()).collect() };
        };
        if any_locked(&self.vault, rows.keys()) {
            // A folder was locked while no process here could purge (by
            // 0.13, or with Quire closed): nothing of it is read.
            self.purge();
            return Found { segments: Vec::new(), parse: notes.iter().map(|(p, _)| p.clone()).collect() };
        }
        let mut wanted: BTreeMap<u64, Vec<(u32, NotePath)>> = BTreeMap::new();
        let mut parse = Vec::new();
        for (p, hash) in notes {
            match rows.get(p) {
                Some(r) if r.hash == *hash => wanted.entry(r.segment).or_default().push((r.slot, p.clone())),
                _ => parse.push(p.clone()),
            }
        }
        let jobs: Vec<(u64, Vec<(u32, NotePath)>)> = wanted.into_iter().collect();
        // Section A of each segment on a small pool, back in segment order:
        // the same install order, and so the same term ids, on every run.
        let dir = &self.dir;
        let slots = pool(&jobs, threads, |(id, want)| read_segment(dir, *id, want));
        let mut segments = Vec::new();
        let mut rows = rows;
        for (slot, (_, want)) in slots.into_iter().zip(&jobs) {
            let (seg, missed) = slot.unwrap_or_else(|| (None, want.iter().map(|(_, p)| p.clone()).collect()));
            // A row whose segment failed is no row: the note is written
            // again, and its segment without the old copy.
            for p in &missed {
                if let Some(r) = rows.remove(p) {
                    self.rewrite.insert(r.segment);
                }
            }
            parse.extend(missed);
            segments.extend(seg);
        }
        self.rows = rows;
        if !self.rewrite.is_empty() {
            self.removal.get_or_insert_with(Instant::now);
            self.touch();
        }
        Found { segments, parse }
    }

    /// The note `p` is indexed with text of `hash`.
    pub(crate) fn note(&mut self, p: &NotePath, hash: FileHash, modified: i64) {
        let was = self.current.insert(p.clone(), (hash, modified)).map(|(h, _)| h);
        // The previous text leaves at the next flush, which this schedules.
        if self.rows.get(p).map(|r| r.hash) != Some(hash) {
            self.dirty.insert(p.clone());
        }
        if was != Some(hash) {
            self.touch();
        }
    }

    /// The note `p` left the index.
    pub(crate) fn gone(&mut self, p: &NotePath) {
        self.current.remove(p);
        self.dirty.remove(p);
        if self.rows.contains_key(p) {
            self.removal.get_or_insert_with(Instant::now);
            self.touch();
        }
    }

    /// After a load: rows of paths that are no longer notes are dead and
    /// leave within 60 s, as a removal does.
    pub(crate) fn settle_after_load(&mut self) {
        if self.rows.keys().any(|p| !self.current.contains_key(p)) {
            self.removal.get_or_insert_with(Instant::now);
            self.touch();
        }
    }

    /// The hash of the text the index holds for `p`.
    pub(crate) fn hash_of(&self, p: &NotePath) -> Option<FileHash> {
        self.current.get(p).map(|(h, _)| *h)
    }

    /// Notes whose text couldn't be decoded: their rows go, so they are
    /// parsed and written again.
    pub(crate) fn forget_rows(&mut self, paths: &[NotePath]) {
        for p in paths {
            if let Some(r) = self.rows.remove(p) {
                self.rewrite.insert(r.segment);
                self.removal.get_or_insert_with(Instant::now);
            }
        }
    }

    /// When the cache says the note last changed (Unix ms).
    pub(crate) fn row_modified(&self, p: &NotePath) -> Option<i64> {
        self.rows.get(p).map(|r| r.modified)
    }

    fn touch(&mut self) {
        let now = Instant::now();
        self.first_change.get_or_insert(now);
        self.last_change = Some(now);
    }

    /// When the next flush is due: 30 s after the last change, at most 5
    /// minutes after the first, within 60 s of a removal.
    pub(crate) fn due(&self) -> Option<Instant> {
        if self.kept_newer {
            return None;
        }
        let last = self.last_change? + Duration::from_secs(30);
        let first = self.first_change? + Duration::from_mins(5);
        let due = last.min(first);
        Some(self.removal.map_or(due, |r| due.min(r + Duration::from_mins(1))))
    }

    /// Whether this process holds the writer lock (taking it if free).
    fn writer(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return false;
        }
        let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join(LOCK_FILE)) else {
            return false;
        };
        if file.try_lock().is_ok() {
            self.lock = Some(file);
            true
        } else {
            false
        }
    }

    /// Write the cache for what `ix` holds now (see the module docs).
    /// `Ok(false)`: another process writes, or the cache is a newer
    /// build's; nothing was written. Gives up before replacing
    /// `records.idx` once `deadline` passed (what it wrote is then
    /// unreferenced and deleted by the next flush).
    pub(crate) fn flush(&mut self, ix: &Index, deadline: Option<Instant>) -> std::io::Result<bool> {
        let done = self.try_flush(ix, deadline);
        if !matches!(done, Ok(true)) {
            // Not written (another process writes, or a write failed): try
            // again at the next flush, not at once.
            let now = Instant::now();
            if self.first_change.is_some() || !self.dirty.is_empty() {
                (self.first_change, self.last_change) = (Some(now), Some(now));
            }
            self.removal = self.removal.map(|_| now);
        }
        done
    }

    fn try_flush(&mut self, ix: &Index, deadline: Option<Instant>) -> std::io::Result<bool> {
        if self.kept_newer || !self.writer() {
            return Ok(false);
        }
        // A newer build's `records.idx` is left alone (P3), so no segment is
        // written either: nothing would ever reference it.
        if newer_format(&self.dir.join(INDEX_FILE)) {
            self.kept_newer = true;
            return Ok(false);
        }
        // Rows still true: the note is indexed with that text and isn't
        // about to be written again.
        let keep = |p: &NotePath, r: &Row, s: &Self| !s.dirty.contains(p) && s.current.get(p).is_some_and(|(h, _)| *h == r.hash);
        let dead: BTreeSet<u64> =
            self.rows.iter().filter(|(p, r)| !keep(p, r, self)).map(|(_, r)| r.segment).chain(self.rewrite.iter().copied()).collect();
        let mut kept: BTreeMap<NotePath, Row> = self.rows.iter().filter(|(p, r)| keep(p, r, self)).map(|(p, r)| (p.clone(), *r)).collect();
        let segments: BTreeSet<u64> = kept.values().map(|r| r.segment).collect();
        let rewrite_all = segments.len() > MAX_SEGMENTS;
        // Live notes in a segment with a dead one are written again without it.
        let mut write: BTreeSet<NotePath> = self.dirty.iter().filter(|p| self.current.contains_key(*p)).cloned().collect();
        kept.retain(|p, r| {
            let again = rewrite_all || dead.contains(&r.segment);
            if again {
                write.insert(p.clone());
            }
            !again
        });
        let write: Vec<NotePath> = write.into_iter().filter(|p| ix.get(p).is_some() && ix.source(p).is_some()).collect();
        if write.is_empty() && self.rewrite.is_empty() && kept.len() == self.rows.len() && self.dir.join(INDEX_FILE).exists() {
            // Nothing changed since the last write.
            (self.first_change, self.last_change, self.removal) = (None, None, None);
            return Ok(true);
        }
        let mut rows = kept;
        for part in write.chunks(SEGMENT_NOTES) {
            let entries: Vec<Entry<'_>> = part.iter().filter_map(|p| entry(ix, p)).collect();
            if entries.is_empty() {
                continue;
            }
            // A random name, never one already there.
            let (id, file) = loop {
                let id = u64::from_le_bytes(
                    quire_base::rand::bytes::<8>().map_err(|e| std::io::Error::other(format!("a segment name: {e:?}")))?,
                );
                let file = self.dir.join(segment_name(id));
                if !file.exists() {
                    break (id, file);
                }
            };
            let paths: Vec<&NotePath> = entries.iter().map(|e| &e.rec.path).collect();
            if !self.checked_put(&file, Kind::RECORDS_SEGMENT, &encode_segment(&entries), &paths)? {
                return Ok(false);
            }
            for (slot, e) in entries.iter().enumerate() {
                let (hash, modified) = self.current[&e.rec.path];
                rows.insert(e.rec.path.clone(), Row { hash, segment: id, slot: u32::try_from(slot).unwrap_or(u32::MAX), modified });
            }
            if deadline.is_some_and(|d| Instant::now() > d) {
                return Ok(false);
            }
        }
        let paths: Vec<&NotePath> = rows.keys().collect();
        if !self.checked_put(&self.dir.join(INDEX_FILE), Kind::RECORDS_INDEX, &encode_index(&rows), &paths)? {
            return Ok(false);
        }
        let live: BTreeSet<u64> = rows.values().map(|r| r.segment).collect();
        self.rows = rows;
        self.dirty.clear();
        self.rewrite.clear();
        (self.first_change, self.last_change, self.removal) = (None, None, None);
        self.delete_unreferenced(&live);
        Ok(true)
    }

    /// Write one file, with the locked-folder check just before and just
    /// after (`docs/design/0.14-index-startup.md` §3.6). `Ok(false)` when it
    /// was not kept.
    fn checked_put(&mut self, file: &Path, kind: Kind, body: &[u8], paths: &[&NotePath]) -> std::io::Result<bool> {
        if any_locked(&self.vault, paths.iter().copied()) {
            self.purge();
            return Ok(false);
        }
        match put(file, kind, body)? {
            cache::Written::KeptNewer => {
                self.kept_newer = true;
                return Ok(false);
            }
            cache::Written::Written => {}
        }
        if any_locked(&self.vault, paths.iter().copied()) {
            // A folder was locked while the file was written (here or in
            // another process): the file goes, and the rest with it.
            #[expect(clippy::let_underscore_must_use, reason = "the purge below removes it again if this failed")]
            let _ = std::fs::remove_file(file);
            self.purge();
            return Ok(false);
        }
        Ok(true)
    }

    fn delete_unreferenced(&self, live: &BTreeSet<u64>) {
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if segment_id(&name).is_some_and(|id| !live.contains(&id)) {
                #[expect(clippy::let_underscore_must_use, reason = "a segment that can't be deleted now is deleted by the next flush")]
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// The notes `want` (slot, path) of segment `id`: the segment with only
/// those notes, and the paths it couldn't give.
fn read_segment(dir: &Path, id: u64, want: &[(u32, NotePath)]) -> (Option<Segment>, Vec<NotePath>) {
    let all_missed = || want.iter().map(|(_, p)| p.clone()).collect();
    let Some(mut seg) =
        cache::read(&dir.join(segment_name(id)), Kind::RECORDS_SEGMENT, FORMAT, DERIVE_VERSION).and_then(|b| decode_segment(&b))
    else {
        return (None, all_missed());
    };
    let mut slots: Vec<Option<(Stored, std::ops::Range<usize>)>> = seg.notes.drain(..).map(Some).collect();
    let mut missed = Vec::new();
    for (slot, p) in want {
        match slots.get_mut(*slot as usize).and_then(Option::take) {
            Some(n) if n.0.rec.path == *p => seg.notes.push(n),
            _ => missed.push(p.clone()),
        }
    }
    (Some(seg), missed)
}
