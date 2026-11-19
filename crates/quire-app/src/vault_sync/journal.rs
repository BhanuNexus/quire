//! Sync's record on this device: a checkpoint, a journal of what changed
//! since, and the merge bases in pack files (`docs/design/0.14-sync-speed.md` §3.2, §3.3).
//! Format: `docs/formats/sync-state.md`.
//!
//! One folder per engine state, beside where Quire 0.13 kept one JSON
//! file: `.quire/sync/state.d/` (`state-partial.d/`, `state-shared.d/`).
//!
//! * A **commit** writes nothing when nothing changed; otherwise it flushes
//!   the pack it appended to, then appends one journal line naming what
//!   changed. Data first, then the record that names it, each flushed.
//! * The journal is folded into a new checkpoint once it passes a quarter
//!   of the checkpoint (at least 256 KiB).
//! * **Opening** takes the highest checkpoint that parses and its journal
//!   lines while each one's hash holds. Only then are leftovers tidied (a
//!   torn journal tail, pack bytes past what the record names, files it
//!   doesn't name). A folder that can't be read is an error; a damaged one
//!   is kept aside and sync starts afresh, saying so.
//! * A 0.13 `state.json` is **migrated** once (and again if an older Quire
//!   wrote a newer one meanwhile: the older file wins).
//!
//! Only the process holding the [`super::SyncLock`] writes here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use quire_sync::engine::is_attachment;
use quire_sync::{Bases, DeleteMark, FileId, Print, Skipped, SyncState, Tracked};
use serde::{Deserialize, Serialize};

use super::sync_dir;

/// The layout of checkpoints and journal lines this build writes.
const FORMAT: u32 = 1;
/// A pack is closed for appends past this size (small in tests, so they
/// see several).
const PACK_LIMIT: u64 = if cfg!(test) { 4 * 1024 } else { 16 * 1024 * 1024 };
/// The journal is folded into a checkpoint past this size (or a quarter
/// of the checkpoint, if larger).
const JOURNAL_MIN: u64 = if cfg!(test) { 2 * 1024 } else { 256 * 1024 };

/// What sync says when its record can't be read now.
fn cant_read(path: &Path, e: &std::io::Error) -> String {
    format!("Couldn't read what sync did last ({}): {e}", path.display())
}

/// What sync says when it started afresh over a damaged record.
fn started_afresh(aside: &Path) -> String {
    format!(
        "Sync's record of what it last did on this device was damaged, so it started afresh (the old record is kept as “{}”). Nothing is deleted; a note that differs from the synced version is kept next to it as a copy, like “Plan (2).md”.",
        aside.display()
    )
}

/// What sync says when part of its record was damaged and it went on
/// from what was good.
fn went_back(aside: &Path) -> String {
    format!(
        "Part of sync's record of what it last did on this device was damaged (kept as “{}”), so sync went on from the part before it. Nothing is deleted; a note that differs from the synced version is kept next to it as a copy, like “Plan (2).md”.",
        aside.display()
    )
}

const DAMAGED_BASE: &str = "Part of sync's record on this device (the last synced text of some notes) was damaged. Nothing is lost: where both this device and another changed such a note, both versions are kept, one as a copy like “Plan (conflict …).md”.";

const OLDER_RAN_ONCE: &str = "Sync found a newer record left by an older Quire (0.13 or earlier) on this device and went on from it.";

const OLDER_RUNS: &str = "An older Quire (0.13 or earlier) also syncs this vault on this device, so each of them starts sync afresh and keeps changed notes as copies. Update every Quire on this device (the app and `quire-cli`) together.";

// ------------------------------------------------------------------ faults

#[cfg(test)]
thread_local! {
    /// Tests: the disk step that fails (counting down to 0), and whether a
    /// failure is a crash (every later step fails too, and a write stops
    /// half-way).
    static FAULT: std::cell::Cell<(Option<usize>, bool, bool)> = const { std::cell::Cell::new((None, false, false)) };
}

/// One step that touches the disk; `Err` when a test says it fails.
#[cfg(test)]
fn step() -> std::io::Result<()> {
    FAULT.with(|f| {
        let (left, crash, crashed) = f.get();
        if crashed {
            return Err(std::io::Error::other("crashed"));
        }
        match left {
            Some(0) => {
                f.set((None, crash, crash));
                Err(std::io::Error::other("injected fault"))
            }
            Some(n) => {
                f.set((Some(n - 1), crash, false));
                Ok(())
            }
            None => Ok(()),
        }
    })
}

#[cfg(not(test))]
#[expect(clippy::unnecessary_wraps, reason = "tests inject failures here")]
#[inline]
const fn step() -> std::io::Result<()> {
    Ok(())
}

/// Tests: whether the fault that just fired was a crash (a write then
/// stops half-way).
#[cfg(test)]
fn crashed() -> bool {
    FAULT.with(|f| f.get().2)
}

#[cfg(not(test))]
const fn crashed() -> bool {
    false
}

/// Tests: make step `n` (0 first) fail, as a crash or an error; `None` clears it.
#[cfg(test)]
pub(super) fn inject(n: Option<usize>, crash: bool) {
    FAULT.with(|f| f.set((n, crash, false)));
}

/// Tests: whether the fault set by [`inject`] fired.
#[cfg(test)]
pub(super) fn fault_fired() -> bool {
    FAULT.with(|f| f.get().0.is_none())
}

/// Tests: what a power cut keeps. Each file this module appended to has the
/// length last flushed (`sync_data`), and a file made since its folder was
/// last flushed has no entry in it yet.
#[cfg(test)]
#[derive(Default)]
struct Disk {
    /// path → (bytes flushed, folder entry flushed)
    files: BTreeMap<PathBuf, (u64, bool)>,
}

#[cfg(test)]
thread_local! {
    static DISK: std::cell::RefCell<Disk> = std::cell::RefCell::default();
}

/// Open `path` to append to it (made if missing).
fn open_append(path: &Path) -> std::io::Result<File> {
    #[cfg(test)]
    {
        let len = std::fs::metadata(path).ok().map(|m| m.len());
        DISK.with(|d| {
            d.borrow_mut().files.entry(path.to_path_buf()).or_insert_with(|| (len.unwrap_or(0), len.is_some()));
        });
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Tests: a power cut now. Every file is cut back to what was flushed, and
/// a file whose folder entry wasn't flushed is gone; the fault is cleared.
#[cfg(test)]
pub(super) fn power_cut() {
    inject(None, false);
    for (path, (len, entry)) in DISK.with(|d| std::mem::take(&mut d.borrow_mut().files)) {
        if !entry {
            let _ = std::fs::remove_file(&path);
        } else if std::fs::metadata(&path).is_ok_and(|m| m.len() > len) {
            let _ = OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(len));
        }
    }
}

#[cfg(test)]
fn flushed(path: &Path, len: Option<u64>, entry: bool) {
    DISK.with(|d| {
        let mut d = d.borrow_mut();
        match len {
            Some(len) => {
                let e = d.files.entry(path.to_path_buf()).or_insert((0, false));
                e.0 = len;
                e.1 |= entry;
            }
            None => {
                d.files.remove(path);
            }
        }
    });
}

#[cfg(test)]
fn folder_flushed(dir: &Path) {
    DISK.with(|d| {
        for (p, (_, entry)) in &mut d.borrow_mut().files {
            if p.parent() == Some(dir) {
                *entry = true;
            }
        }
    });
}

fn write_all(f: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    if let Err(e) = step() {
        if crashed() {
            // A crash in the middle of a write: part of it landed.
            f.write_all(&bytes[..bytes.len() / 2])?;
        }
        return Err(e);
    }
    f.write_all(bytes)
}

fn sync_data(f: &File, path: &Path) -> std::io::Result<()> {
    step()?;
    f.sync_data()?;
    #[cfg(test)]
    flushed(path, Some(f.metadata()?.len()), false);
    #[cfg(not(test))]
    let _ = path;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    step()?;
    quire_base::fs::write_atomic(path, bytes, quire_base::fs::Options::STATE)?;
    // Flushed, renamed and its folder flushed (Options::STATE).
    #[cfg(test)]
    flushed(path, Some(bytes.len() as u64), true);
    Ok(())
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    step()?;
    #[cfg(test)]
    flushed(path, None, false);
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    step()?;
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn set_len(path: &Path, len: u64) -> std::io::Result<()> {
    step()?;
    OpenOptions::new().write(true).open(path)?.set_len(len)
}

/// Flush a folder's list of files, so a file made or removed in it stays
/// so after a power cut (Unix; Windows has no such call).
fn sync_folder(dir: &Path) -> std::io::Result<()> {
    step()?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    #[cfg(test)]
    folder_flushed(dir);
    Ok(())
}

#[expect(clippy::disallowed_methods, reason = "a damaged record is renamed aside whole, packs included, so nothing is copied")]
fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    step()?;
    #[cfg(test)]
    flushed(from, None, false);
    std::fs::rename(from, to)
}

/// `path` with `.unreadable` added, or `-2`, `-3`… after it if taken.
fn aside_name(path: &Path) -> PathBuf {
    let base = format!("{}.unreadable", path.display());
    (1..10_000)
        .map(|n| if n == 1 { PathBuf::from(&base) } else { PathBuf::from(format!("{base}-{n}")) })
        .find(|p| !p.exists())
        .unwrap_or_else(|| PathBuf::from(base))
}

// ------------------------------------------------------------------ framing

fn sha256_hex(s: &str) -> String {
    quire_base::enc::sha256_hex(s.as_bytes())
}

/// One journal line: `{"h":"<SHA-256 of d>","d":<d>}` and a newline.
fn frame(d: &str) -> String {
    format!("{{\"h\":\"{}\",\"d\":{d}}}\n", sha256_hex(d))
}

/// The `d` of a line (without its newline) whose hash holds.
fn unframe(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("{\"h\":\"")?;
    let (h, rest) = rest.split_at_checked(64)?;
    let d = rest.strip_prefix("\",\"d\":")?.strip_suffix('}')?;
    (sha256_hex(d) == h).then_some(d)
}

/// What a checkpoint file holds.
enum Checkpoint {
    Good(Box<Body>),
    Damaged,
    Newer,
}

/// A checkpoint file: `{"format":1,"h":"…","d":<body>}` and a newline.
fn checkpoint_bytes(body: &Body) -> Result<Vec<u8>, String> {
    let d = serde_json::to_string(body).map_err(|e| e.to_string())?;
    Ok(format!("{{\"format\":{FORMAT},\"h\":\"{}\",\"d\":{d}}}\n", sha256_hex(&d)).into_bytes())
}

fn parse_checkpoint(bytes: &[u8]) -> Checkpoint {
    let Ok(text) = std::str::from_utf8(bytes) else { return Checkpoint::Damaged };
    let Some(rest) = text.strip_prefix("{\"format\":") else { return Checkpoint::Damaged };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let Ok(format) = digits.parse::<u32>() else { return Checkpoint::Damaged };
    if format > FORMAT {
        return Checkpoint::Newer;
    }
    let Some(line) = rest[digits.len()..].strip_prefix(',').and_then(|r| r.strip_suffix('\n')) else { return Checkpoint::Damaged };
    match unframe(&format!("{{{line}")).map(serde_json::from_str::<Body>) {
        Some(Ok(b)) => Checkpoint::Good(Box::new(b)),
        _ => Checkpoint::Damaged,
    }
}

// ------------------------------------------------------------------ the record

/// Where a base's record is: pack number, offset, length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Place(u32, u64, u32);

/// Everything a checkpoint holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Body {
    state: SyncState,
    /// When every note was last read again (Unix ms; 0: never).
    #[serde(default)]
    verified_at: i64,
    /// The packs and how much of each the record names.
    #[serde(default)]
    packs: BTreeMap<u32, u64>,
    /// Where each base's text is.
    #[serde(default)]
    places: BTreeMap<Print, Place>,
    /// SHA-256 of the 0.13 `state.json` this record was made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migrated_from: Option<String>,
    /// How many times an older Quire's newer `state.json` replaced this record.
    #[serde(default, skip_serializing_if = "is_zero")]
    older_runs: u32,
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// One journal line's change: the entries that changed (`null`: removed),
/// and the other parts whole when they changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Delta {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    files: BTreeMap<FileId, Option<Tracked>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    left: Option<BTreeMap<String, quire_sync::engine::LeftCopy>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped: Option<BTreeMap<FileId, Skipped>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deletes_sent: Option<Vec<DeleteMark>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    packs: Option<BTreeMap<u32, u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    places: BTreeMap<Print, Option<Place>>,
}

impl Delta {
    fn between(old: &Body, new: &Body) -> Self {
        fn entries<K: Ord + Clone, V: PartialEq + Clone>(old: &BTreeMap<K, V>, new: &BTreeMap<K, V>) -> BTreeMap<K, Option<V>> {
            let mut out: BTreeMap<K, Option<V>> =
                new.iter().filter(|(k, v)| old.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
            out.extend(old.keys().filter(|k| !new.contains_key(*k)).map(|k| (k.clone(), None)));
            out
        }
        fn whole<T: PartialEq + Clone>(old: &T, new: &T) -> Option<T> {
            (old != new).then(|| new.clone())
        }
        let (o, n) = (&old.state, &new.state);
        Self {
            files: entries(&o.files, &n.files),
            cursor: whole(&o.cursor, &n.cursor),
            left: whole(&o.left, &n.left),
            skipped: whole(&o.skipped, &n.skipped),
            deletes_sent: whole(&o.deletes_sent, &n.deletes_sent),
            verified_at: whole(&old.verified_at, &new.verified_at),
            packs: whole(&old.packs, &new.packs),
            places: entries(&old.places, &new.places),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn apply(self, b: &mut Body) {
        fn entries<K: Ord, V>(map: &mut BTreeMap<K, V>, d: BTreeMap<K, Option<V>>) {
            for (k, v) in d {
                match v {
                    Some(v) => map.insert(k, v),
                    None => map.remove(&k),
                };
            }
        }
        entries(&mut b.state.files, self.files);
        entries(&mut b.places, self.places);
        if let Some(c) = self.cursor {
            b.state.cursor = c;
        }
        if let Some(l) = self.left {
            b.state.left = l;
        }
        if let Some(s) = self.skipped {
            b.state.skipped = s;
        }
        if let Some(d) = self.deletes_sent {
            b.state.deletes_sent = d;
        }
        if let Some(v) = self.verified_at {
            b.verified_at = v;
        }
        if let Some(p) = self.packs {
            b.packs = p;
        }
    }
}

/// The note texts `state` needs kept: every base, kept copy and left copy
/// of a note (attachments keep only a print; an empty text needs no record).
pub(super) fn live_prints(state: &SyncState) -> BTreeSet<Print> {
    let empty = Print::of("");
    let notes = state
        .files
        .values()
        .flat_map(|t| std::iter::once((t.path.as_str(), t.base)).chain(t.kept.iter().map(|k| (k.path.as_str(), k.base))))
        .chain(state.left.iter().filter_map(|(p, l)| l.base.map(|b| (p.as_str(), b))));
    notes.filter(|(p, b)| !is_attachment(p) && *b != empty).map(|(_, b)| b).collect()
}

// ------------------------------------------------------------------ packs

fn pack_name(n: u32) -> String {
    format!("bases-{n:06}.pack")
}

/// A record: `u32 length | print | codec | bytes`, the length of `bytes`
/// little-endian; codec 0 raw, 1 deflate.
fn encode_record(p: Print, text: &str) -> Vec<u8> {
    let raw = text.as_bytes();
    let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    let packed = z.write_all(raw).and_then(|()| z.finish()).ok().filter(|z| z.len() < raw.len());
    let (codec, bytes) = match &packed {
        Some(z) => (1u8, z.as_slice()),
        None => (0u8, raw),
    };
    let mut out = Vec::with_capacity(37 + bytes.len());
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&p.0);
    out.push(codec);
    out.extend_from_slice(bytes);
    out
}

/// The text in a whole record, if it is `p`'s.
fn decode_record(rec: &[u8], p: &Print) -> Option<String> {
    let len = usize::try_from(u32::from_le_bytes(rec.get(..4)?.try_into().ok()?)).ok()?;
    if rec.get(4..36)? != p.0 || rec.len() != 37 + len {
        return None;
    }
    let bytes = &rec[37..];
    let text = match rec[36] {
        0 => String::from_utf8(bytes.to_vec()).ok()?,
        1 => {
            let mut out = String::new();
            flate2::read::DeflateDecoder::new(bytes).read_to_string(&mut out).ok()?;
            out
        }
        _ => return None,
    };
    (Print::of(&text) == *p).then_some(text)
}

/// The packs of one state folder, shared by the engine (as its [`Bases`])
/// and the [`Journal`] that commits them.
#[derive(Debug)]
struct Packs {
    dir: PathBuf,
    places: BTreeMap<Print, Place>,
    /// Bytes in each pack (named or appended since).
    lens: BTreeMap<u32, u64>,
    /// The pack appended to.
    active: u32,
    /// Packs appended to since the last commit, still to be flushed.
    touched: BTreeSet<u32>,
    /// A pack file was made since the last commit (its folder entry is flushed too).
    made: bool,
    /// A damaged record was found (told once).
    damaged: bool,
    told: bool,
}

impl Packs {
    fn path(&self, n: u32) -> PathBuf {
        self.dir.join(pack_name(n))
    }

    fn read(&mut self, p: &Print) -> Result<Option<String>, String> {
        if *p == Print::of("") {
            return Ok(Some(String::new()));
        }
        let Some(Place(n, at, len)) = self.places.get(p).copied() else { return Ok(None) };
        let path = self.path(n);
        let mut f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.lost(p);
                return Ok(None);
            }
            Err(e) => return Err(cant_read(&path, &e)),
        };
        let mut rec = vec![0u8; len as usize];
        f.seek(SeekFrom::Start(at)).map_err(|e| cant_read(&path, &e))?;
        match f.read_exact(&mut rec) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.lost(p);
                return Ok(None);
            }
            Err(e) => return Err(cant_read(&path, &e)),
        }
        let text = decode_record(&rec, p);
        if text.is_none() {
            self.lost(p);
        }
        Ok(text)
    }

    /// `p`'s record is damaged: it names nothing now, so the text is
    /// written again if it comes back.
    fn lost(&mut self, p: &Print) {
        self.damaged = true;
        self.places.remove(p);
    }

    fn append(&mut self, p: Print, text: &str) -> Result<(), String> {
        let rec = encode_record(p, text);
        let at = self.lens.get(&self.active).copied().unwrap_or(0);
        if at > 0 && at + rec.len() as u64 > PACK_LIMIT {
            self.active = self.lens.keys().next_back().map_or(1, |n| n + 1).max(self.active + 1);
            return self.append(p, text);
        }
        let path = self.path(self.active);
        let fresh = at == 0;
        let res = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            let mut f = open_append(&path)?;
            if f.metadata()?.len() != at {
                // Bytes this process didn't name (a failed append): start a new pack.
                return Err(std::io::Error::other("pack length changed"));
            }
            write_all(&mut f, &rec)
        })();
        if let Err(e) = res {
            // When part of it landed, that part is past what any record
            // names: later records go to a new pack. When nothing did (a
            // full disk refusing the write), the same pack is tried again.
            if std::fs::metadata(&path).is_ok_and(|m| m.len() != at) {
                self.lens.entry(self.active).or_insert(0);
                self.active = self.lens.keys().next_back().map_or(1, |n| n + 1);
            }
            return Err(format!("Couldn't save a note's synced text in {}: {e}", path.display()));
        }
        self.made |= fresh;
        self.lens.insert(self.active, at + rec.len() as u64);
        self.touched.insert(self.active);
        self.places.insert(p, Place(self.active, at, u32::try_from(rec.len()).unwrap_or(u32::MAX)));
        Ok(())
    }

    /// Flush what was appended since the last commit.
    fn flush(&mut self) -> Result<(), String> {
        // Each pack leaves the list only once flushed: a failure keeps it,
        // and every one after it, for the next try.
        for n in self.touched.clone() {
            let path = self.path(n);
            let f = OpenOptions::new().append(true).open(&path).map_err(|e| e.to_string());
            if let Err(e) = f.and_then(|f| sync_data(&f, &path).map_err(|e| e.to_string())) {
                return Err(format!("Couldn't save sync's record in {}: {e}", path.display()));
            }
            self.touched.remove(&n);
        }
        if self.made {
            sync_folder(&self.dir).map_err(|e| e.to_string())?;
            self.made = false;
        }
        Ok(())
    }

    /// Copy pack `n`'s live records to the active pack (a new one if `n`
    /// is it). A record that is damaged is dropped (its base is missing).
    fn compact(&mut self, n: u32) -> Result<(), String> {
        if n == self.active {
            self.active = self.lens.keys().next_back().map_or(1, |m| m + 1);
        }
        let moving: Vec<Print> = self.places.iter().filter(|(_, pl)| pl.0 == n).map(|(p, _)| *p).collect();
        for p in moving {
            // Its place moves only once the copy landed.
            match self.read(&p)? {
                Some(text) => self.append(p, &text)?,
                None => {
                    self.places.remove(&p);
                }
            }
        }
        self.lens.remove(&n);
        Ok(())
    }

    /// Live bytes per pack.
    fn live_bytes(&self) -> BTreeMap<u32, u64> {
        let mut out = BTreeMap::new();
        for Place(n, _, len) in self.places.values() {
            *out.entry(*n).or_insert(0) += u64::from(*len);
        }
        out
    }
}

/// The merge bases of one state folder (see [`Bases`]). Clones share the packs.
#[derive(Debug, Clone)]
pub(super) struct PackBases(Arc<Mutex<Packs>>);

impl PackBases {
    fn lock(&self) -> std::sync::MutexGuard<'_, Packs> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn new(dir: &Path, body: &Body) -> Self {
        let lens = body.packs.clone();
        let active = lens.iter().next_back().map_or(1, |(n, len)| if *len >= PACK_LIMIT { n + 1 } else { *n });
        Self(Arc::new(Mutex::new(Packs {
            dir: dir.to_path_buf(),
            places: body.places.clone(),
            lens,
            active,
            touched: BTreeSet::new(),
            made: false,
            damaged: false,
            told: false,
        })))
    }

    /// A notice once, when a damaged base was found.
    pub(super) fn take_notice(&self) -> Option<String> {
        let mut p = self.lock();
        (p.damaged && !p.told).then(|| {
            p.told = true;
            DAMAGED_BASE.to_string()
        })
    }
}

impl Bases for PackBases {
    fn get(&self, p: &Print) -> Result<Option<String>, String> {
        self.lock().read(p)
    }

    fn put(&mut self, p: Print, text: &str) -> Result<(), String> {
        let mut packs = self.lock();
        if p == Print::of("") || packs.places.contains_key(&p) {
            return Ok(());
        }
        packs.append(p, text)
    }
}

// ------------------------------------------------------------------ the journal

/// One engine state's record on disk (see the module docs).
#[derive(Debug)]
pub(super) struct Journal {
    dir: PathBuf,
    /// The checkpoint's number; 0 before the first one is written.
    generation: u32,
    /// What the last commit wrote.
    committed: Body,
    log_len: u64,
    checkpoint_len: u64,
    /// The journal may end in a partly written line: fold it into a
    /// checkpoint at the next commit rather than append after it.
    broken: bool,
    /// Packs the record no longer names whose delete failed: tried again
    /// at each commit.
    unremoved: BTreeSet<u32>,
    bases: PackBases,
}

/// What [`Journal::open`] gives a session.
#[derive(Debug)]
pub(super) struct Opened {
    pub(super) journal: Journal,
    pub(super) state: SyncState,
    pub(super) bases: PackBases,
    pub(super) notices: Vec<String>,
}

/// The state folder of the 0.13 state file `name` (`state.json` →
/// `state.d`).
pub(super) fn state_dir(root: &Path, name: &str) -> PathBuf {
    sync_dir(root).join(format!("{}.d", name.trim_end_matches(".json")))
}

fn checkpoint_name(n: u32) -> String {
    format!("checkpoint-{n:06}.json")
}

fn journal_name(n: u32) -> String {
    format!("journal-{n:06}.jsonl")
}

/// The number in `<prefix><6 digits><suffix>`.
fn numbered(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    let n = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (n.len() == 6 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok()).flatten()
}

/// What a state folder holds, as found.
#[derive(Debug, Default)]
struct Found {
    body: Body,
    generation: u32,
    log_len: u64,
    checkpoint_len: u64,
    notices: Vec<String>,
    /// The journal had a bad line before good ones: write a checkpoint now.
    refold: bool,
    /// Files the record doesn't name, and lengths to cut files back to:
    /// tidied only once the record was accepted.
    stale: Vec<PathBuf>,
    cut: Vec<(PathBuf, u64)>,
    /// Damaged files to keep aside (renamed).
    aside: Vec<PathBuf>,
    /// The folder held checkpoints, none of which parses.
    all_damaged: bool,
    /// It held any checkpoint at all.
    any_checkpoint: bool,
}

/// Read the state folder `dir` without changing anything. `Ok(None)`: no
/// folder.
fn read_dir(dir: &Path) -> Result<Option<Found>, String> {
    // A file gone between the listing and its read: a checkpoint being
    // rotated by the process that syncs (a look from `sync status` while it
    // runs). Read again; never answer with an empty record for it.
    for _ in 0..10 {
        match read_dir_once(dir) {
            Err(e) if e == VANISHED => {}
            r => return r,
        }
    }
    read_dir_once(dir)
}

/// [`read_dir_once`] found a file gone that it had just listed.
const VANISHED: &str = "a file of the record went away while it was read";

fn read_dir_once(dir: &Path) -> Result<Option<Found>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(cant_read(dir, &e)),
    };
    let (mut checkpoints, mut journals, mut packs, mut others) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for e in entries {
        let e = e.map_err(|e| cant_read(dir, &e))?;
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = numbered(&name, "checkpoint-", ".json") {
            checkpoints.push(n);
        } else if let Some(n) = numbered(&name, "journal-", ".jsonl") {
            journals.push(n);
        } else if let Some(n) = numbered(&name, "bases-", ".pack") {
            packs.push(n);
        } else if quire_base::fs::is_leftover(&name) {
            others.push(e.path());
        }
    }
    checkpoints.sort_unstable();
    let mut found = Found { any_checkpoint: !checkpoints.is_empty(), stale: others, ..Found::default() };
    let mut chosen = None;
    for &n in checkpoints.iter().rev() {
        let path = dir.join(checkpoint_name(n));
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(VANISHED.into()),
            Err(e) => return Err(cant_read(&path, &e)),
        };
        match parse_checkpoint(&bytes) {
            Checkpoint::Good(b) => {
                found.checkpoint_len = bytes.len() as u64;
                found.body = *b;
                chosen = Some(n);
                break;
            }
            Checkpoint::Newer => {
                return Err("This vault's sync record was written by a newer Quire; update Quire on this device".into());
            }
            Checkpoint::Damaged => found.aside.push(path),
        }
    }
    let Some(g) = chosen else {
        found.all_damaged = found.any_checkpoint;
        return Ok(Some(found));
    };
    found.generation = g;
    if let Some(aside) = found.aside.first() {
        found.notices.push(went_back(&aside_name(aside)));
    }
    // The journal of that checkpoint, line by line while each holds.
    let jpath = dir.join(journal_name(g));
    let bytes = match std::fs::read(&jpath) {
        Ok(b) => b,
        // Rotated meanwhile (its checkpoint is gone too): read again.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dir.join(checkpoint_name(g)).exists() => return Err(VANISHED.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(cant_read(&jpath, &e)),
    };
    let mut at = 0usize;
    let lines: Vec<&[u8]> = bytes.split_inclusive(|b| *b == b'\n').collect();
    for (i, raw) in lines.iter().enumerate() {
        let good = raw.ends_with(b"\n")
            && std::str::from_utf8(&raw[..raw.len() - 1])
                .ok()
                .and_then(unframe)
                .and_then(|d| serde_json::from_str::<Delta>(d).ok())
                .map(|d| d.apply(&mut found.body))
                .is_some();
        if good {
            at += raw.len();
            continue;
        }
        let later =
            lines[i + 1..].iter().any(|l| l.ends_with(b"\n") && std::str::from_utf8(&l[..l.len() - 1]).ok().and_then(unframe).is_some());
        if later {
            // Damage, not a crash: what follows can't be trusted to build
            // on what is lost. Kept aside; the state up to here is used.
            found.aside.push(jpath.clone());
            found.notices.push(went_back(&aside_name(&jpath)));
            found.refold = true;
        } else {
            // A crash's torn last line: cut it off.
            found.cut.push((jpath, at as u64));
        }
        break;
    }
    found.log_len = at as u64;
    for n in checkpoints.into_iter().filter(|n| *n != g) {
        let p = dir.join(checkpoint_name(n));
        if !found.aside.contains(&p) {
            found.stale.push(p);
        }
    }
    found.stale.extend(journals.into_iter().filter(|n| *n != g).map(|n| dir.join(journal_name(n))));
    for n in packs {
        let p = dir.join(pack_name(n));
        match found.body.packs.get(&n) {
            Some(len) => found.cut.push((p, *len)),
            None => found.stale.push(p),
        }
    }
    Ok(Some(found))
}

/// Read a 0.13 state file's bytes: its state and bases, or a fresh one
/// with a notice when it is damaged (the file is kept aside first).
fn read_013(path: &Path, bytes: &[u8], bases: &mut dyn Bases) -> Result<(SyncState, Option<String>), String> {
    if let Ok(old) = serde_json::from_slice::<quire_sync::bases::State13>(bytes) {
        return Ok((old.into_state(bases)?, None));
    }
    let aside = crate::atomic::keep_unreadable(path).ok_or_else(|| format!("Couldn't keep a copy of the damaged {}", path.display()))?;
    Ok((SyncState::default(), Some(started_afresh(&aside))))
}

impl Journal {
    /// Open the record of the 0.13 state file `name` (`state.json`,
    /// `state-partial.json`, `state-shared.json`) for a sync session:
    /// migrate it if it is there, else open its folder, else start afresh.
    /// An error when it can't be read now: nothing is written then.
    pub(super) fn open(root: &Path, name: &str) -> Result<Opened, String> {
        let legacy = sync_dir(root).join(name);
        let dir = state_dir(root, name);
        match std::fs::read(&legacy) {
            Ok(bytes) => Self::migrate(&dir, &legacy, &bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::open_dir(dir),
            Err(e) => Err(cant_read(&legacy, &e)),
        }
    }

    /// Open the state folder `dir` (no 0.13 file beside it).
    fn open_dir(dir: PathBuf) -> Result<Opened, String> {
        let Some(mut found) = read_dir(&dir)? else { return Ok(Self::fresh(&dir, Vec::new())) };
        if found.all_damaged {
            // Kept aside whole (a rename: packs included), and a fresh start.
            let aside = aside_name(&dir);
            rename(&dir, &aside).map_err(|e| format!("Couldn't keep the damaged {} aside: {e}", dir.display()))?;
            return Ok(Self::fresh(&dir, vec![started_afresh(&aside)]));
        }
        if !found.any_checkpoint {
            // Never committed (a first pass or a migration that stopped
            // before its checkpoint): nothing in it is named.
            remove_dir_all(&dir).map_err(|e| e.to_string())?;
            return Ok(Self::fresh(&dir, Vec::new()));
        }
        // Accepted: now tidy.
        for p in &found.aside {
            if p.exists() {
                rename(p, &aside_name(p)).map_err(|e| e.to_string())?;
            }
        }
        for (p, len) in &found.cut {
            if std::fs::metadata(p).is_ok_and(|m| m.len() > *len) {
                set_len(p, *len).map_err(|e| e.to_string())?;
            }
        }
        for p in &found.stale {
            remove_file(p).map_err(|e| e.to_string())?;
        }
        let notices = std::mem::take(&mut found.notices);
        let bases = PackBases::new(&dir, &found.body);
        let state = found.body.state.clone();
        let mut journal = Self {
            dir,
            generation: found.generation,
            committed: found.body,
            log_len: found.log_len,
            checkpoint_len: found.checkpoint_len,
            broken: false,
            unremoved: BTreeSet::new(),
            bases: bases.clone(),
        };
        if found.refold {
            let body = journal.committed.clone();
            journal.checkpoint(body)?;
        }
        Ok(Opened { journal, state, bases, notices })
    }

    fn fresh(dir: &Path, notices: Vec<String>) -> Opened {
        let body = Body::default();
        let bases = PackBases::new(dir, &body);
        let journal = Self {
            dir: dir.to_path_buf(),
            generation: 0,
            committed: body,
            log_len: 0,
            checkpoint_len: 0,
            broken: false,
            unremoved: BTreeSet::new(),
            bases: bases.clone(),
        };
        Opened { journal, state: SyncState::default(), bases, notices }
    }

    /// A 0.13 state file is here (`docs/design/0.14-sync-speed.md` §3.7).
    fn migrate(dir: &Path, legacy: &Path, bytes: &[u8]) -> Result<Opened, String> {
        let hash = quire_base::enc::sha256_hex(bytes);
        let mut older_runs = 0;
        if let Some(found) = read_dir(dir)? {
            if found.body.migrated_from.as_deref() == Some(hash.as_str()) && !found.all_damaged && found.any_checkpoint {
                // A crash after the checkpoint and before the old file went.
                remove_file(legacy).map_err(|e| e.to_string())?;
                sync_folder(&sync_dir_of(legacy)).map_err(|e| e.to_string())?;
                return Self::open_dir(dir.to_path_buf());
            }
            if found.any_checkpoint {
                // Written later by an older Quire (a downgrade and back, or
                // two versions syncing here): the old file wins.
                older_runs = found.body.older_runs + 1;
            }
            remove_dir_all(dir).map_err(|e| format!("Couldn't clear {}: {e}", dir.display()))?;
        }
        let mut opened = Self::fresh(dir, Vec::new());
        let (state, notice) = read_013(legacy, bytes, &mut opened.bases)?;
        opened.notices.extend(notice);
        match older_runs {
            0 => {}
            1 => opened.notices.push(OLDER_RAN_ONCE.into()),
            _ => opened.notices.push(OLDER_RUNS.into()),
        }
        let mut body = Body { state: state.clone(), migrated_from: Some(hash), older_runs, ..Body::default() };
        {
            let mut packs = opened.bases.lock();
            packs.flush()?;
            body.packs.clone_from(&packs.lens);
            body.places.clone_from(&packs.places);
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        opened.journal.checkpoint(body)?;
        remove_file(legacy).map_err(|e| format!("Couldn't remove {}: {e}", legacy.display()))?;
        sync_folder(&sync_dir_of(legacy)).map_err(|e| e.to_string())?;
        opened.state = state;
        Ok(opened)
    }

    /// The record last committed: when every note was last read again.
    pub(super) const fn verified_at(&self) -> i64 {
        self.committed.verified_at
    }

    /// Commit `state` (see the module docs), with `verified_at`. `locked`
    /// says whether a note path is in a locked folder now: when the last
    /// committed record kept the text of a note there and `state` no
    /// longer does (its delete was sent, or the scrub cleared it), the
    /// text leaves every pack in this commit (`docs/design/0.11-secure.md`).
    /// Returns whether anything was written.
    pub(super) fn commit(&mut self, state: &SyncState, verified_at: i64, locked: &dyn Fn(&str) -> bool) -> Result<bool, String> {
        self.retry_removals();
        let live = live_prints(state);
        let purging = self.locked_prints_dropped(&live, locked);
        let mut packs = self.bases.lock();
        packs.places.retain(|p, _| live.contains(p));
        // Nothing new: nothing written, and nothing copied to find that out.
        if !purging
            && packs.touched.is_empty()
            && verified_at == self.committed.verified_at
            && packs.places == self.committed.places
            && *state == self.committed.state
        {
            return Ok(false);
        }
        // Compaction: a pack mostly dead (not the one appended to). On a
        // purge, every pack with any dead bytes, the one appended to
        // included: none may keep that text, nor an older version of it,
        // nor the text of a note deleted earlier
        // (docs/design/0.14-sync-speed.md §3.2).
        let live_bytes = packs.live_bytes();
        let due: BTreeSet<u32> = packs
            .lens
            .iter()
            .filter(|(n, len)| {
                let live = live_bytes.get(*n).copied().unwrap_or(0);
                if purging { live < **len } else { **n != packs.active && live * 2 < **len }
            })
            .map(|(n, _)| *n)
            .collect();
        let mut gone = Vec::new();
        for n in due {
            if packs.lens.contains_key(&n) {
                packs.compact(n)?;
                gone.push(n);
            }
        }
        packs.flush()?;
        let body = Body {
            state: state.clone(),
            verified_at,
            packs: packs.lens.iter().filter(|(_, l)| **l > 0).map(|(n, l)| (*n, *l)).collect(),
            places: packs.places.clone(),
            migrated_from: self.committed.migrated_from.clone(),
            older_runs: self.committed.older_runs,
        };
        drop(packs);
        let delta = Delta::between(&self.committed, &body);
        if delta.is_empty() && gone.is_empty() {
            return Ok(false);
        }
        if self.generation == 0 || self.broken || self.log_len > JOURNAL_MIN.max(self.checkpoint_len / 4) {
            self.checkpoint(body)?;
        } else {
            self.append(&delta)?;
            self.committed = body;
            if self.log_len > JOURNAL_MIN.max(self.checkpoint_len / 4) {
                let body = self.committed.clone();
                self.checkpoint(body)?;
            }
        }
        // Packs the record no longer names: gone now, or at the next commit.
        self.unremoved.extend(gone);
        self.retry_removals();
        if purging {
            self.purge_aside();
        }
        Ok(true)
    }

    /// Whether the last committed record kept the text of a note in a
    /// locked folder that `live` no longer has.
    fn locked_prints_dropped(&self, live: &BTreeSet<Print>, locked: &dyn Fn(&str) -> bool) -> bool {
        let empty = Print::of("");
        let st = &self.committed.state;
        st.files
            .values()
            .flat_map(|t| std::iter::once((t.path.as_str(), t.base)).chain(t.kept.iter().map(|k| (k.path.as_str(), k.base))))
            .chain(st.left.iter().filter_map(|(p, l)| l.base.map(|b| (p.as_str(), b))))
            .any(|(path, b)| b != empty && !is_attachment(path) && !live.contains(&b) && locked(path))
    }

    /// Delete the packs the record no longer names; a failure is tried
    /// again at the next commit (and the next open deletes them too).
    fn retry_removals(&mut self) {
        let named = &self.committed.packs;
        let dir = &self.dir;
        self.unremoved.retain(|n| !named.contains_key(n) && remove_file(&dir.join(pack_name(*n))).is_err());
    }

    /// The packs of records kept aside as damaged (`state.d.unreadable…`)
    /// hold old merge bases nothing reads any more: a purge deletes them,
    /// so they never keep a locked note's text. Their checkpoints and
    /// journals (prints only) stay for a look. So does a damaged 0.13
    /// record kept aside (`state.json.unreadable`): it holds note texts
    /// and nothing reads it, so a purge deletes it too.
    fn purge_aside(&self) {
        let (Some(parent), Some(name)) = (self.dir.parent(), self.dir.file_name()) else { return };
        let name = name.to_string_lossy();
        let legacy = format!("{}.json.unreadable", name.trim_end_matches(".d"));
        let prefix = format!("{name}.unreadable");
        for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(&legacy) && e.path().is_file() {
                #[expect(clippy::let_underscore_must_use, reason = "tried again at the next purge")]
                let _ = remove_file(&e.path());
                continue;
            }
            if !e.file_name().to_string_lossy().starts_with(&prefix) || !e.path().is_dir() {
                continue;
            }
            for f in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                if f.file_name().to_string_lossy().ends_with(".pack") {
                    #[expect(clippy::let_underscore_must_use, reason = "tried again at the next purge")]
                    let _ = remove_file(&f.path());
                }
            }
        }
    }

    fn append(&mut self, delta: &Delta) -> Result<(), String> {
        let line = frame(&serde_json::to_string(delta).map_err(|e| e.to_string())?);
        let path = self.dir.join(journal_name(self.generation));
        let fresh = self.log_len == 0;
        let res = (|| -> std::io::Result<()> {
            let mut f = open_append(&path)?;
            write_all(&mut f, line.as_bytes())?;
            sync_data(&f, &path)?;
            if fresh {
                sync_folder(&self.dir)?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            // Part of the line may be there: the next commit writes a
            // checkpoint instead of appending after it.
            self.broken = true;
            return Err(format!("Couldn't save what sync did in {}: {e}", path.display()));
        }
        self.log_len += line.len() as u64;
        Ok(())
    }

    /// Write `body` as the next checkpoint, then remove the older pair.
    fn checkpoint(&mut self, body: Body) -> Result<(), String> {
        let next = self.generation + 1;
        let bytes = checkpoint_bytes(&body)?;
        let path = self.dir.join(checkpoint_name(next));
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        write_atomic(&path, &bytes).map_err(|e| format!("Couldn't save what sync did in {}: {e}", path.display()))?;
        if self.generation == 0
            && let Some(parent) = self.dir.parent()
        {
            // A new record folder: its entry in `.quire/sync/` is flushed too.
            sync_folder(parent).map_err(|e| format!("Couldn't save what sync did in {}: {e}", parent.display()))?;
        }
        let old = self.generation;
        self.generation = next;
        self.committed = body;
        self.log_len = 0;
        self.checkpoint_len = bytes.len() as u64;
        self.broken = false;
        if old > 0 {
            for p in [self.dir.join(checkpoint_name(old)), self.dir.join(journal_name(old))] {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "an older checkpoint left behind is deleted when the record is next opened"
                )]
                let _ = remove_file(&p);
            }
        }
        Ok(())
    }
}

fn sync_dir_of(legacy: &Path) -> PathBuf {
    legacy.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The engine state as last committed for the 0.13 state file `name`, for
/// a look (`sync status`, history): nothing is changed, migrated or tidied.
/// A fresh state when there is none or it can't be read.
pub(super) fn peek(root: &Path, name: &str) -> SyncState {
    if let Ok(bytes) = std::fs::read(sync_dir(root).join(name)) {
        // Not migrated yet, or written by an older Quire since: it wins.
        return serde_json::from_slice::<quire_sync::bases::State13>(&bytes)
            .ok()
            .and_then(|s| s.into_state(&mut quire_sync::MemoryBases::default()).ok())
            .unwrap_or_default();
    }
    read_dir(&state_dir(root, name)).ok().flatten().filter(|f| f.any_checkpoint && !f.all_damaged).map(|f| f.body.state).unwrap_or_default()
}

/// Whether the record for the 0.13 state file `name` exists (in either form).
pub(super) fn exists(root: &Path, name: &str) -> bool {
    sync_dir(root).join(name).exists() || state_dir(root, name).exists()
}

/// Remove the record for `name`, in both forms.
pub(super) fn remove(root: &Path, name: &str) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "stopping sync removes what it can; a file left behind is never read as this setup's"
    )]
    let _ = std::fs::remove_file(sync_dir(root).join(name));
    #[expect(clippy::let_underscore_must_use, reason = "as above")]
    let _ = std::fs::remove_dir_all(state_dir(root, name));
}

/// Rename the record of `from` to `to` (a selection set or cleared), in
/// whichever form it is.
pub(super) fn rename_record(root: &Path, from: &str, to: &str) -> Result<(), String> {
    let dir = sync_dir(root);
    if !exists(root, from) {
        return Ok(());
    }
    // A record left under the new name (an older Quire's, or one from
    // before a selection was set and cleared) is not this one's: it goes.
    remove_file(&dir.join(to)).map_err(|e| e.to_string())?;
    remove_dir_all(&state_dir(root, to)).map_err(|e| e.to_string())?;
    if dir.join(from).exists() {
        rename(&dir.join(from), &dir.join(to)).map_err(|e| e.to_string())?;
    }
    if state_dir(root, from).exists() {
        rename(&state_dir(root, from), &state_dir(root, to)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Tests: the text of every whole record in every pack of `name`'s record.
#[cfg(test)]
pub(super) fn pack_texts(root: &Path, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(state_dir(root, name)).into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().ends_with(".pack") {
            continue;
        }
        let all = std::fs::read(e.path()).unwrap_or_default();
        let mut at = 0;
        while at + 37 <= all.len() {
            let len = u32::from_le_bytes([all[at], all[at + 1], all[at + 2], all[at + 3]]) as usize;
            let Some(rec) = all.get(at..at + 37 + len) else { break };
            let mut p = [0u8; 32];
            p.copy_from_slice(&rec[4..36]);
            out.extend(decode_record(rec, &Print(p)));
            at += 37 + len;
        }
    }
    out
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
