// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sync through a folder that something else already syncs: Dropbox, iCloud
//! Drive, Google Drive, `OneDrive`, Syncthing, a NAS or a USB stick.
//!
//! There's no server to put revisions in order, so each device only ever
//! appends to **its own** log files and never touches anyone else's. File
//! sync tools make "conflicted copy" files when two computers change one file,
//! and this layout never gives them the chance:
//!
//! ```text
//! <chosen folder>/Quire Sync/<space>/
//!   space.json                          the wrapped vault key (written once)
//!   devices/<device>/000001.jsonl       one sealed revision per line, append-only
//!   shared/devices/<device>/000001.jsonl  vault settings (0.9), the same way
//! ```
//!
//! `shared/` is a **lane** of its own ([`FolderTransport::lane`]): vault
//! settings and review history sync there, with their own revisions and
//! state. Quire 0.8 reads only `devices/`, so it never sees those files and
//! can never pass on a delete of them, even when it already joined the
//! vault before a 0.9 device turned settings sync on.
//!
//! Each device reads everyone's logs and works out, for every note, the same
//! **current revision**: start at the note's first revision and keep following
//! its child, picking the child with the smallest id when two devices built on
//! the same revision before seeing each other's. That rule doesn't depend on
//! the order files arrive in, so every device agrees once it has the same files.
//!
//! A device whose revision loses that tie hears about it later than it would
//! from a server (when the other device's file arrives), and then merges its
//! change again on top of the winner, three-way from the revision both share
//! ([`Transport::fork_base`]). Nothing is overwritten and no text is dropped.
//!
//! Lines are only read once they end in a newline, so a log that is still
//! downloading (or was cut short by a crash) is read up to its last whole line.
//! Revisions whose earlier revision hasn't arrived yet wait for it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::compact::{Manifest, Node, Part, Policy, Stats};
use crate::crypto::WrappedKey;
use crate::keyring::{DeviceRecord, EpochRecord, KeySet, LaneRules};
use crate::protocol::{Content, DeviceId, FileId, LanePos, PushOutcome, RevId, Revision, Sealing, SpaceId};
use crate::transport::{Fork, Transport, TransportError};

/// The folder Quire keeps inside the one the user picks.
pub const FOLDER_NAME: &str = "Quire Sync";
/// A device starts a new log file past this size, so tools that upload whole
/// files on every change don't re-send a growing log.
pub(crate) const SEGMENT_LIMIT: u64 = 1 << 20;
/// The lane vault settings sync in (`<space>/shared/`), which 0.8 never reads.
pub const SHARED_LANE: &str = "shared";
/// The format new spaces are written in; 0.8 reads it.
const FORMAT: u32 = 1;
/// The newest format this build understands (0.9: `min_reader` exists;
/// 0.13: [`LEVEL_HARDENED`]).
pub const READER_FORMAT: u32 = 4;
/// The minimum reader of a 0.13 vault.
///
/// Revisions bind their whole header and key epoch, keys can change, and old revisions can be compacted
/// (`docs/design/0.13-sync.md`). Quire 0.9–0.12 refuse it with "made by a
/// newer Quire", 0.8 refuses its `format`.
pub const LEVEL_HARDENED: u32 = 4;
/// Words for a vault whose level went down in the storage.
pub(crate) const LOWERED: &str = "The vault's sync format was lowered in the sync storage (Quire never does that; a removed device or someone else with access to the storage may have). Sync stopped on this device so nothing is sealed the old way.";
/// Passes of at least [`TICK`] seconds apart a snapshot must also be seen
/// for before it settles, so a clock set forward doesn't settle it early.
pub const SETTLE_TICKS: u32 = 72;
/// See [`SETTLE_TICKS`].
const TICK: i64 = 600;

/// When a snapshot was first seen here, and how many passes since.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    /// Unix seconds: the clock when first seen, or the snapshot's own time if later.
    at: i64,
    ticks: u32,
    last: i64,
}

/// `space.json`. From 0.9 on a reader refuses a space only when
/// `min_reader` is above what it reads, and otherwise ignores a newer
/// `format`; 0.8 refuses any `format` above 1 (design §B.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpaceFile {
    format: u32,
    space: SpaceId,
    /// Unix seconds.
    created: i64,
    wrapped_key: WrappedKey,
    /// The oldest reader that may use this space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_reader: Option<u32>,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// A vault found in a sync folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSpaceInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    /// Devices that have written to it.
    pub devices: usize,
}

/// Where Quire's files go for a folder the user picked: a `Quire Sync`
/// folder inside it, unless they picked that folder itself.
pub fn sync_root(chosen: &Path) -> PathBuf {
    if chosen.file_name().is_some_and(|n| n == FOLDER_NAME) { chosen.to_path_buf() } else { chosen.join(FOLDER_NAME) }
}

fn io_err(what: &str, e: &std::io::Error) -> TransportError {
    TransportError::Offline(format!("{what}: {e}"))
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub(crate) fn is_segment_name(name: &str) -> bool {
    name.len() == 12 && name.is_char_boundary(6) && &name[6..] == ".jsonl" && name[..6].bytes().all(|b| b.is_ascii_digit())
}

/// Write a small file so readers see the old version or the new one, never half.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Make a new vault in `root` (see [`sync_root`]) that every Quire since
/// 0.8 reads.
pub fn create_space(root: &Path, space: &SpaceId, key: &WrappedKey) -> Result<(), TransportError> {
    create_space_at(root, space, key, 1)
}

/// Make a new vault whose minimum reader is `level` (0.13 makes new
/// vaults at [`LEVEL_HARDENED`]).
pub fn create_space_at(root: &Path, space: &SpaceId, key: &WrappedKey, level: u32) -> Result<(), TransportError> {
    let dir = root.join(&space.0);
    fs::create_dir_all(dir.join("devices")).map_err(|e| io_err("can't create the sync folder", &e))?;
    let file = SpaceFile {
        format: FORMAT.max(level),
        space: space.clone(),
        created: unix_now(),
        wrapped_key: key.clone(),
        min_reader: (level > 1).then_some(level),
        other: serde_json::Map::new(),
    };
    let json = serde_json::to_vec_pretty(&file).unwrap_or_default();
    write_atomically(&dir.join("space.json"), &json).map_err(|e| io_err("can't write to the sync folder", &e))
}

fn read_space(dir: &Path) -> Result<SpaceFile, TransportError> {
    let bytes = fs::read(dir.join("space.json")).map_err(|e| io_err("can't read the vault in the sync folder", &e))?;
    let f: SpaceFile = serde_json::from_slice(&bytes)
        .map_err(|_| TransportError::Refused("the vault's space.json is damaged or still downloading".into()))?;
    if f.min_reader.is_some_and(|m| m > READER_FORMAT) {
        return Err(TransportError::Refused("this vault was made by a newer Quire; update Quire on this device".into()));
    }
    Ok(f)
}

/// Start the lane `lane` of a space (other 0.9 devices then join it).
pub fn open_lane(root: &Path, space: &SpaceId, lane: &str) -> Result<(), TransportError> {
    fs::create_dir_all(root.join(&space.0).join(lane).join("devices")).map_err(|e| io_err("can't write to the sync folder", &e))
}

/// Whether some device started the lane `lane` of a space.
pub fn lane_exists(root: &Path, space: &SpaceId, lane: &str) -> bool {
    root.join(&space.0).join(lane).join("devices").is_dir()
}

/// The oldest Quire format that may read this space (1 unless raised).
pub fn min_reader(root: &Path, space: &SpaceId) -> Result<u32, TransportError> {
    let f = read_space(&root.join(&space.0))?;
    Ok(f.min_reader.unwrap_or(1))
}

/// Keep readers older than `min` out of this space: `format` and
/// `min_reader` become `min`, so Quire 0.8 refuses it (design §B.5). Other
/// fields are kept. It never lowers either.
pub fn raise_min_reader(root: &Path, space: &SpaceId, min: u32) -> Result<(), TransportError> {
    let dir = root.join(&space.0);
    let mut f = read_space(&dir)?;
    f.format = f.format.max(min);
    f.min_reader = Some(f.min_reader.unwrap_or(1).max(min));
    let json = serde_json::to_vec_pretty(&f).unwrap_or_default();
    write_atomically(&dir.join("space.json"), &json).map_err(|e| io_err("can't write to the sync folder", &e))
}

/// The vaults in `root`, oldest first.
pub fn list_spaces(root: &Path) -> Result<Vec<FolderSpaceInfo>, TransportError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(root).map_err(|e| io_err("can't read the sync folder", &e))?;
    let mut out: Vec<FolderSpaceInfo> = entries
        .flatten()
        .filter_map(|e| read_space(&e.path()).ok().map(|f| (e.path(), f)))
        .map(|(dir, f)| FolderSpaceInfo {
            space: f.space,
            created: f.created,
            devices: fs::read_dir(dir.join("devices")).map_or(0, |d| d.flatten().filter(|e| e.path().is_dir()).count()),
        })
        .collect();
    out.sort_by(|a, b| (a.created, &a.space).cmp(&(b.created, &b.space)));
    Ok(out)
}

/// A vault's wrapped key, to join it from a new device.
pub fn wrapped_key(root: &Path, space: &SpaceId) -> Result<WrappedKey, TransportError> {
    Ok(read_space(&root.join(&space.0))?.wrapped_key)
}

/// Where a revision's line is.
#[derive(Debug, Clone)]
struct Loc {
    path: Arc<PathBuf>,
    offset: u64,
    len: usize,
}

#[derive(Debug, Clone)]
struct Meta {
    file: FileId,
    parents: Vec<RevId>,
    device: DeviceId,
    loc: Loc,
}

/// The snapshot in use (0.13 compaction).
#[derive(Debug, Clone)]
struct Gen {
    manifest: Manifest,
}

/// What this device knows about the folder. Only ids and positions are kept
/// in memory; sealed bytes are read back from the files when needed.
#[derive(Debug, Default)]
struct Index {
    revs: HashMap<RevId, Meta>,
    /// Revisions by their first parent (also those only known from a
    /// later revision's chain, see `bridges`).
    children: HashMap<RevId, Vec<RevId>>,
    /// Each note's first revision(s).
    roots: HashMap<FileId, Vec<RevId>>,
    /// How far each log file has been read (always just after a newline),
    /// and how many whole lines that is.
    read_to: HashMap<PathBuf, (u64, u64)>,
    /// The revision handed to the engine for each note, and when.
    delivered: HashMap<FileId, (RevId, u64)>,
    seq: u64,
    /// Revisions this process wrote, to notice another computer writing as us.
    mine: HashSet<RevId>,
    scanned_once: bool,
    problem: Option<String>,
    waiting_for_download: usize,
    /// 0.13: the snapshot taken, if any.
    snap: Option<Gen>,
    /// Snapshots whose parts were checked against their manifest.
    checked: HashSet<String>,
    /// Revisions never received, by the first parent a later revision's
    /// chain says they have.
    bridges: HashMap<RevId, RevId>,
    /// Ancestors reached through revisions a snapshot dropped.
    links: HashMap<RevId, Vec<RevId>>,
    /// Where each note's current revision is found from, after a snapshot.
    starts: HashMap<FileId, RevId>,
    /// The furthest line read in each device's lane.
    lane_end: HashMap<DeviceId, LanePos>,
    /// Lines sealed under a key epoch this device doesn't have yet.
    waiting_key: bool,
    /// Devices whose lane has a line the rules refuse (for now): nothing
    /// after it is read ([`LaneRules::allows`]).
    held: BTreeSet<DeviceId>,
    /// Notes whose first revision never arrived: the oldest revisions their
    /// later ones name, to start from.
    dangling: HashMap<FileId, BTreeSet<RevId>>,
}

/// A space in a synced folder, seen from one device.
#[derive(Debug)]
pub struct FolderTransport {
    /// The space's folder (`space.json`).
    dir: PathBuf,
    /// The folder holding `devices/`: the space's, or a lane's inside it.
    logs: PathBuf,
    space: SpaceId,
    /// `""` for notes, else the lane's name.
    lane_name: String,
    device: DeviceId,
    /// Remembers the last order number handed out, across restarts.
    seq_file: Option<PathBuf>,
    /// When each snapshot was first seen here (Unix seconds), across restarts.
    seen_file: Option<PathBuf>,
    /// How long this device's own log files are, as it wrote them: more
    /// there is another computer (or a removed device) writing as this one.
    own_file: Option<PathBuf>,
    index: Mutex<Index>,
    /// The vault's keys: lines are checked before they're used, and
    /// snapshots are authenticated with them (0.13).
    keys: Mutex<Option<KeySet>>,
    /// Devices removed from the vault, and the last line still taken from each.
    removed: Mutex<BTreeMap<DeviceId, LanePos>>,
    /// The space's minimum reader, as last read.
    level: AtomicU32,
    /// The highest level this device knows the space had: lower in
    /// `space.json` stops sync ([`LOWERED`]).
    floor: AtomicU32,
    /// What other devices' lines and snapshots must be (0.13 §2).
    rules: Mutex<LaneRules>,
    /// How long a snapshot settles (tests shorten it).
    settle: i64,
    /// Check other devices' lines with the keys (off for a mirror whose
    /// lines were checked when fetched).
    check_lines: bool,
}

/// Whether `s` can be a device id (a folder name under `devices/`).
pub fn is_device_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn segment_number(path: &Path) -> u32 {
    path.file_name().and_then(|n| n.to_str()).filter(|n| is_segment_name(n)).and_then(|n| n[..6].parse().ok()).unwrap_or(0)
}

use crate::compact::sha256_hex;

impl FolderTransport {
    /// `root` is from [`sync_root`]. `state_dir` is a private folder on this
    /// device (the vault's `.quire/sync`) for one small counter.
    pub fn new(root: &Path, space: &SpaceId, device: DeviceId, state_dir: Option<&Path>) -> Self {
        let seq_file = state_dir.map(|d| d.join("folder-seq"));
        // Order numbers only ever grow on this device, even across restarts,
        // or the engine would ignore a note's new current revision.
        let saved = seq_file.as_ref().and_then(|f| fs::read_to_string(f).ok()).and_then(|s| s.trim().parse::<u64>().ok());
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        let seq = saved.unwrap_or(clock);
        let dir = root.join(&space.0);
        let level = read_space(&dir).ok().map_or(1, |f| f.min_reader.unwrap_or(1));
        Self {
            logs: dir.clone(),
            dir,
            space: space.clone(),
            lane_name: String::new(),
            device,
            seq_file,
            seen_file: state_dir.map(|d| d.join("snapshots-seen.json")),
            own_file: state_dir.map(|d| d.join("folder-own.json")),
            index: Mutex::new(Index { seq, ..Index::default() }),
            keys: Mutex::new(None),
            removed: Mutex::new(BTreeMap::new()),
            level: AtomicU32::new(level),
            floor: AtomicU32::new(0),
            rules: Mutex::new(LaneRules::default()),
            settle: crate::compact::SETTLE,
            check_lines: true,
        }
    }

    /// A lane of the space (`<space>/<lane>/devices/…`): the same layout and
    /// rules, in a folder older readers don't look at. `state_dir` keeps its
    /// own counter (`folder-seq-<lane>`).
    pub fn lane(root: &Path, space: &SpaceId, device: DeviceId, state_dir: Option<&Path>, lane: &str) -> Self {
        let dir = state_dir.map(|d| d.join(format!("lane-{lane}")));
        if let Some(d) = &dir {
            let _ = fs::create_dir_all(d);
        }
        let mut t = Self::new(root, space, device, dir.as_deref());
        t.logs = t.dir.join(lane);
        t.lane_name = lane.to_string();
        t
    }

    /// Check every line with the vault's keys before using it, and take
    /// snapshots authenticated with them (0.13). The app always does.
    #[must_use]
    pub fn with_keys(self, keys: impl Into<KeySet>) -> Self {
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = Some(keys.into());
        self
    }

    /// The vault's key changed (a device was removed): use these from now on.
    pub fn set_keys(&self, keys: KeySet) {
        let had_waiting = self.lock().waiting_key;
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = Some(keys);
        if had_waiting {
            // Lines skipped for want of the key are read again.
            self.reset();
        }
    }

    fn keyset(&self) -> Option<KeySet> {
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Devices removed from the vault, and the last of each one's lines in
    /// this lane still taken (`docs/design/0.13-sync.md` §2). Lines after it
    /// are ignored.
    pub fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>) {
        let changed = {
            let mut r = self.removed.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *r != removed;
            *r = removed;
            changed
        };
        if changed {
            self.reset();
        }
    }

    /// What other devices' lines and snapshots must be to be taken, now
    /// that the vault's key changed (`docs/design/0.13-sync.md` §2). Lines
    /// refused for now are read again when the rules change.
    pub fn set_rules(&self, rules: LaneRules) {
        let changed = {
            let mut r = self.rules.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *r != rules;
            *r = rules;
            changed
        };
        if changed {
            self.reset();
        }
    }

    fn rules(&self) -> LaneRules {
        self.rules.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The highest level this device has seen the space at (kept by the
    /// caller): a lower one in the storage stops sync instead of sealing
    /// the old way.
    pub fn set_floor(&self, level: u32) {
        self.floor.fetch_max(level, Ordering::Relaxed);
    }

    #[cfg(feature = "remote")]
    pub(crate) fn floor(&self) -> u32 {
        self.floor.load(Ordering::Relaxed)
    }

    /// How long a snapshot settles here, in seconds.
    pub const fn settle(&self) -> i64 {
        self.settle
    }

    /// Read everything again at the next scan (the remote transport
    /// rewrote its mirror).
    pub fn reload(&self) {
        self.reset();
    }

    /// For tests: snapshots settle after `secs` instead of a week.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_settle(mut self, secs: i64) -> Self {
        self.settle = secs;
        self
    }

    #[doc(hidden)]
    pub const fn set_settle(&mut self, secs: i64) {
        self.settle = secs;
    }

    /// Whether other devices' lines are checked with the vault's keys
    /// before they're used (on unless the lines were checked already).
    pub const fn set_check_lines(&mut self, on: bool) {
        self.check_lines = on;
    }

    /// Forget what was read; the next scan reads everything again (what
    /// was handed to the engine is kept, so nothing is handed out twice).
    fn reset(&self) {
        let mut ix = self.lock();
        let keep = Index {
            delivered: std::mem::take(&mut ix.delivered),
            seq: ix.seq,
            mine: std::mem::take(&mut ix.mine),
            scanned_once: false,
            problem: ix.problem.take(),
            checked: std::mem::take(&mut ix.checked),
            ..Index::default()
        };
        *ix = keep;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn own_dir(&self) -> PathBuf {
        self.logs.join("devices").join(&self.device.0)
    }

    fn check_folder(&self) -> Result<(), TransportError> {
        if self.dir.join("space.json").is_file() || self.dir.join("devices").is_dir() {
            Ok(())
        } else {
            Err(TransportError::Offline(format!(
                "the sync folder isn't available ({}); is the drive connected and the sync app running?",
                self.dir.display()
            )))
        }
    }

    /// Another computer is writing as this device; sync stops until the user
    /// sets it up again on one of them.
    pub fn problem(&self) -> Option<String> {
        self.lock().problem.clone()
    }

    /// Log files the sync app hasn't downloaded yet (iCloud evicts files to
    /// save space), as a note for the user.
    pub fn waiting(&self) -> Option<String> {
        let ix = self.lock();
        let n = ix.waiting_for_download;
        let mut out = Vec::new();
        if n > 0 {
            out.push(format!(
                "Waiting for {n} file(s) in the sync folder to download. In iCloud Drive, choose “Keep Downloaded” for the Quire Sync folder."
            ));
        }
        if ix.waiting_key {
            out.push("Some changes use the vault's new key, which this device doesn't have yet.".to_string());
        }
        if !ix.held.is_empty() {
            let who: Vec<&str> = ix.held.iter().map(|d| d.0.get(..8).unwrap_or(&d.0)).collect();
            out.push(format!(
                "Changes from device(s) {} use the vault's old key after that device took the new one; they wait until it syncs again (if a removed device wrote them, they're never taken).",
                who.join(", ")
            ));
        }
        (!out.is_empty()).then(|| out.join(" "))
    }

    /// Changes whenever any device's log changes; cheap enough to poll.
    pub fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for sub in ["devices", "snapshots"] {
            if let Ok(devs) = fs::read_dir(self.logs.join(sub)) {
                let mut files: Vec<(PathBuf, u64, Option<SystemTime>)> = devs
                    .flatten()
                    .filter_map(|d| fs::read_dir(d.path()).ok())
                    .flat_map(Iterator::flatten)
                    .filter_map(|f| f.metadata().ok().map(|m| (f.path(), m.len(), m.modified().ok())))
                    .collect();
                files.sort();
                for f in files {
                    f.hash(&mut h);
                }
            }
        }
        if let Ok(keys) = fs::read_dir(self.dir.join("keys")) {
            let mut names: Vec<String> = keys.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for n in &names {
                n.hash(&mut h);
            }
        }
        h.finish()
    }

    /// The space's minimum reader (1 unless raised; 4 for a 0.13 vault).
    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }

    /// Say what the space's minimum reader is (an S3 or WebDAV mirror has
    /// no `space.json` of its own).
    pub fn set_level(&self, level: u32) {
        self.level.store(level, Ordering::Relaxed);
    }

    /// [`Self::level`], read from `space.json` now.
    pub fn read_level(&self) -> Result<u32, TransportError> {
        let l = read_space(&self.dir)?.min_reader.unwrap_or(1);
        self.store_level(l)?;
        Ok(l)
    }

    /// Keep the level read from `space.json`, unless it's below the floor.
    fn store_level(&self, l: u32) -> Result<(), TransportError> {
        if l < self.floor.load(Ordering::Relaxed) {
            return Err(TransportError::Refused(LOWERED.into()));
        }
        self.level.store(l, Ordering::Relaxed);
        Ok(())
    }

    /// Raise the space's minimum reader (see [`raise_min_reader`]).
    pub fn raise_level(&self, level: u32) -> Result<(), TransportError> {
        let root = self.dir.parent().ok_or_else(|| TransportError::Refused("no sync folder".into()))?;
        raise_min_reader(root, &self.space, level)?;
        self.level.store(level.max(self.level()), Ordering::Relaxed);
        Ok(())
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.logs.join("snapshots")
    }

    /// Complete, authentic snapshots in the folder.
    ///
    /// Authentic: under the current key (a removed device doesn't have it),
    /// or one the newest key change names. Its cut never goes past what is
    /// in the folder (a synced folder; a mirror's cut was checked when fetched).
    fn snapshots(&self, ix: &mut Index) -> Vec<Manifest> {
        let Some(keys) = self.keyset() else { return Vec::new() };
        let Ok(rd) = fs::read_dir(self.snapshots_dir()) else { return Vec::new() };
        let rules = self.rules();
        let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            let Some((g, maker)) = crate::compact::parse_name(&name) else { continue };
            let Ok(bytes) = fs::read(e.path().join("manifest.json")) else { continue };
            let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else { continue };
            if m.generation != g || m.maker != maker || !m.verify(&keys, &self.space, &self.lane_name) {
                continue;
            }
            if !Self::takes(&rules, &removed, &keys, &m, &bytes) {
                continue;
            }
            if self.check_lines && !ix.checked.contains(&name) && !self.cut_is_here(&m) {
                continue;
            }
            // Every part there, whole, and as the manifest says.
            let ok = ix.checked.contains(&name)
                || m.parts.iter().all(|p| {
                    crate::compact::parse_part(&p.name)
                        && fs::read(e.path().join(&p.name)).is_ok_and(|b| b.len() as u64 == p.size && sha256_hex(&b) == p.sha256)
                });
            if !ok {
                continue;
            }
            ix.checked.insert(name);
            out.push(m);
        }
        out
    }

    /// Whether snapshot `m` (its manifest's bytes) may be taken: under the
    /// current key, or the newest key change names it; and not made by a
    /// removed device (unless named).
    fn takes(rules: &LaneRules, removed: &BTreeMap<DeviceId, LanePos>, keys: &KeySet, m: &Manifest, bytes: &[u8]) -> bool {
        let (name, sha) = (m.name(), sha256_hex(bytes));
        let pinned = rules.snapshots.iter().any(|(n, h)| *n == name && *h == sha);
        rules.takes_snapshot(keys.current(), m.epoch, &name, &sha) && (pinned || !removed.contains_key(&m.maker))
    }

    /// [`Self::takes`], with this transport's rules.
    #[cfg(feature = "remote")]
    pub(crate) fn takes_snapshot(&self, keys: &KeySet, m: &Manifest, bytes: &[u8]) -> bool {
        let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner).clone();
        Self::takes(&self.rules(), &removed, keys, m, bytes)
    }

    /// Whether every lane position `m`'s cut names is in the folder: its
    /// segment there, with at least that many lines.
    fn cut_is_here(&self, m: &Manifest) -> bool {
        m.cut.iter().all(|(d, c)| {
            if c.is_empty() {
                return true;
            }
            if c.n > 0 || c.seg == 0 || !is_device_name(&d.0) {
                return false;
            }
            let dir = self.logs.join("devices").join(&d.0);
            let last = fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().to_str().filter(|n| is_segment_name(n)).and_then(|n| n[..6].parse::<u32>().ok()))
                        .max()
                })
                .ok()
                .flatten();
            match last {
                Some(l) if l > c.seg => true,
                Some(l) if l == c.seg => {
                    fs::read(dir.join(format!("{l:06}.jsonl"))).is_ok_and(|b| b.iter().filter(|&&x| x == b'\n').count() as u64 >= c.line)
                }
                _ => false,
            }
        })
    }

    fn load_seen(&self) -> BTreeMap<String, Seen> {
        let Some(b) = self.seen_file.as_ref().and_then(|f| fs::read(f).ok()) else { return BTreeMap::new() };
        serde_json::from_slice(&b)
            .or_else(|_| {
                // Before 0.13.1: only when.
                serde_json::from_slice::<BTreeMap<String, i64>>(&b)
                    .map(|m| m.into_iter().map(|(k, at)| (k, Seen { at, ticks: 0, last: at })).collect())
            })
            .unwrap_or_default()
    }

    /// Note that snapshot `m` is in use: when it was first seen here (not
    /// before it was made, whatever the clock said), and one more pass if
    /// the last was a while ago.
    fn mark_seen(&self, m: &Manifest) -> Seen {
        let mut seen = self.load_seen();
        let now = unix_now();
        let e = seen.entry(m.name()).or_insert(Seen { at: now.max(m.made), ticks: 0, last: now });
        let mut changed = e.ticks == 0 && e.last == now;
        if now - e.last >= TICK {
            e.ticks = e.ticks.saturating_add(1);
            e.last = now;
            changed = true;
        }
        let out = *e;
        if changed && let Some(f) = &self.seen_file {
            let _ = write_atomically(f, &serde_json::to_vec(&seen).unwrap_or_default());
        }
        out
    }

    /// Whether a snapshot seen as `s` has settled: a week since, and
    /// [`SETTLE_TICKS`] passes (not needed when tests shorten the week).
    const fn settled(&self, s: &Seen, now: i64) -> bool {
        now - s.at >= self.settle && (self.settle == 0 || s.ticks >= SETTLE_TICKS)
    }

    /// Read whatever is new in every device's logs.
    fn scan(&self, ix: &mut Index) -> Result<(), TransportError> {
        self.check_folder()?;
        if let Ok(f) = read_space(&self.dir) {
            self.store_level(f.min_reader.unwrap_or(1))?;
        }
        // The snapshot to use: when it changes, everything is read again.
        let snaps = self.snapshots(ix);
        let win = crate::compact::winner(&snaps).cloned();
        if win.as_ref().map(Manifest::name) != ix.snap.as_ref().map(|g| g.manifest.name()) {
            let fresh = Index {
                delivered: std::mem::take(&mut ix.delivered),
                seq: ix.seq,
                mine: std::mem::take(&mut ix.mine),
                scanned_once: ix.scanned_once,
                problem: ix.problem.take(),
                checked: std::mem::take(&mut ix.checked),
                ..Index::default()
            };
            *ix = fresh;
            if let Some(m) = win {
                let dir = self.snapshots_dir().join(m.name());
                let keys = self.keyset();
                for p in &m.parts {
                    self.read_part(ix, &dir.join(&p.name), keys.as_ref());
                }
                for (a, b) in &m.links {
                    ix.links.entry(a.clone()).or_default().push(b.clone());
                }
                ix.starts = m.starts.iter().map(|(f, r)| (f.clone(), r.clone())).collect();
                ix.snap = Some(Gen { manifest: m });
            }
        }
        if let Some(g) = &ix.snap {
            self.mark_seen(&g.manifest);
        }
        ix.waiting_for_download = 0;
        ix.held.clear();
        let Ok(devs) = fs::read_dir(self.logs.join("devices")) else { return Ok(()) };
        let mut logs = Vec::new();
        for dev in devs.flatten() {
            let Some(name) = dev.file_name().to_str().map(str::to_string) else { continue };
            let own = name == self.device.0;
            let Ok(files) = fs::read_dir(dev.path()) else { continue };
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if fname.ends_with(".icloud") {
                    // iCloud evicted it to save space; it downloads on demand.
                    ix.waiting_for_download += 1;
                } else if Path::new(&fname).extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl")) {
                    if own && (!is_segment_name(&fname) || self.grew_elsewhere(&fname, f.metadata().map_or(0, |m| m.len()))) {
                        ix.problem = Some(Self::shared_identity());
                    }
                    logs.push((DeviceId(name.clone()), own, f.path()));
                }
            }
        }
        // A stable order, so a revision's parent tends to be read first.
        logs.sort();
        let keys = self.keyset();
        let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let rules = self.rules();
        for (dev, own, path) in logs {
            if ix.held.contains(&dev) {
                continue;
            }
            self.read_log(ix, &dev, own, &path, keys.as_ref(), removed.get(&dev), &rules);
        }
        ix.scanned_once = true;
        Ok(())
    }

    fn shared_identity() -> String {
        "Another computer is syncing as this one (a copy of this vault's .quire folder?). Turn sync off and on again on one of them.".into()
    }

    /// A snapshot's part: its revisions, each checked with the vault's
    /// keys like a lane line (one that doesn't open isn't used).
    fn read_part(&self, ix: &mut Index, path: &Path, keys: Option<&KeySet>) {
        let Ok(buf) = fs::read(path) else { return };
        let shared = Arc::new(path.to_path_buf());
        let mut start = 0;
        while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + nl];
            if let Ok(r) = serde_json::from_slice::<Revision>(line)
                && keys.is_none_or(|k| Content::open_set(k, &self.space, &r, true).is_ok())
            {
                Self::insert(ix, &r, Loc { path: Arc::clone(&shared), offset: start as u64, len: line.len() });
            }
            start += nl + 1;
        }
    }

    #[allow(clippy::too_many_arguments, reason = "what a lane line is checked against, each once")]
    fn read_log(
        &self,
        ix: &mut Index,
        dev: &DeviceId,
        own: bool,
        path: &Path,
        keys: Option<&KeySet>,
        cutoff: Option<&LanePos>,
        rules: &LaneRules,
    ) {
        let Ok(mut f) = File::open(path) else { return };
        let len = f.metadata().map_or(0, |m| m.len());
        let (mut from, mut lines) = ix.read_to.get(path).copied().unwrap_or((0, 0));
        if len < from {
            // Replaced by a shorter file; read it again (known lines are skipped).
            (from, lines) = (0, 0);
        }
        if len == from || f.seek(SeekFrom::Start(from)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.take(len - from).read_to_end(&mut buf).is_err() {
            return;
        }
        let seg = segment_number(path);
        // This device's own log: only as far as it wrote it.
        let noted = if own { self.own_len(path) } else { None };
        let cut = ix.snap.as_ref().and_then(|g| g.manifest.cut.get(dev)).copied();
        let shared = Arc::new(path.to_path_buf());
        let mut start = 0;
        while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + nl];
            let parsed = serde_json::from_slice::<Revision>(line).ok();
            let n = parsed.as_ref().map_or(0, |r| r.seq);
            let pos_line = lines + 1;
            // Folded into the snapshot, or written after this device was removed.
            let covered = cut.is_some_and(|c| c.covers(seg, pos_line, n));
            let after_removal = cutoff.is_some_and(|c| !c.covers(seg, pos_line, n));
            let foreign = noted.is_some_and(|l| from + start as u64 >= l);
            if let Some(r) = parsed.filter(|_| !covered && !after_removal && !foreign) {
                // Sealed under a key older than this lane may use here
                // (a removed device writing into it): nothing more is read
                // from this lane until its device vouches for the line.
                if !own && !rules.allows(dev, seg, pos_line, n, &r.sealed) {
                    ix.held.insert(dev.clone());
                    break;
                }
                // Checked with the vault's keys (not our own lines): a line
                // that doesn't open is damaged and skipped; one sealed under
                // a key this device doesn't have yet waits for it.
                let check = if own || !self.check_lines {
                    Ok(())
                } else {
                    keys.map_or(Ok(()), |k| Content::open_set(k, &self.space, &r, true).map(|_| ()))
                };
                if let Err(crate::crypto::CryptoError::UnknownEpoch(_)) = check {
                    ix.waiting_key = true;
                    break;
                }
                if check.is_ok() && r.device == *dev {
                    let loc = Loc { path: Arc::clone(&shared), offset: from + start as u64, len: line.len() };
                    if own && ix.scanned_once && !ix.mine.contains(&r.rev) && !ix.revs.contains_key(&r.rev) {
                        ix.problem = Some(Self::shared_identity());
                    }
                    Self::insert(ix, &r, loc);
                }
            }
            // A line that doesn't parse is damaged; it's skipped for good.
            let end = ix.lane_end.entry(dev.clone()).or_default();
            if n > 0 {
                end.n = end.n.max(n);
            } else if (seg, pos_line) > (end.seg, end.line) {
                (end.seg, end.line) = (seg, pos_line);
            }
            lines += 1;
            start += nl + 1;
        }
        ix.read_to.insert(path.to_path_buf(), (from + start as u64, lines));
    }

    fn insert(ix: &mut Index, r: &Revision, loc: Loc) {
        if ix.revs.contains_key(&r.rev) {
            return;
        }
        // Known now: no longer only from a chain.
        if let Some(p) = ix.bridges.remove(&r.rev)
            && let Some(c) = ix.children.get_mut(&p)
        {
            c.retain(|x| *x != r.rev);
        }
        match r.parents.first() {
            Some(p) => {
                let c = ix.children.entry(p.clone()).or_default();
                if !c.contains(&r.rev) {
                    c.push(r.rev.clone());
                }
            }
            None => ix.roots.entry(r.file.clone()).or_default().push(r.rev.clone()),
        }
        // What its chain says about revisions this device never got.
        let up: Vec<&RevId> = r.parents.first().into_iter().chain(r.chain.iter()).take(crate::protocol::CHAIN + 1).collect();
        for w in up.windows(2) {
            let (x, px) = (w[0], w[1]);
            if !ix.revs.contains_key(x) && !ix.bridges.contains_key(x) {
                ix.bridges.insert(x.clone(), px.clone());
                let c = ix.children.entry(px.clone()).or_default();
                if !c.contains(x) {
                    c.push(x.clone());
                }
            }
        }
        // The oldest revision it names, if it's not here: where the note
        // starts if its first revision never arrives.
        if let Some(top) = up.last().filter(|t| !ix.revs.contains_key(*t)) {
            let top = (*top).clone();
            ix.dangling.entry(r.file.clone()).or_default().insert(top);
        }
        if let Some(d) = ix.dangling.get_mut(&r.file) {
            d.remove(&r.rev);
        }
        ix.revs.insert(r.rev.clone(), Meta { file: r.file.clone(), parents: r.parents.clone(), device: r.device.clone(), loc });
    }

    /// Every note something is known of.
    fn files(ix: &Index) -> Vec<FileId> {
        let mut files: Vec<FileId> = ix.roots.keys().chain(ix.starts.keys()).chain(ix.dangling.keys()).cloned().collect();
        files.sort();
        files.dedup();
        files
    }

    /// A note's current revision: from its first revision (or where a
    /// snapshot says to start), keep following the child with the smallest
    /// id. A revision known only from a later one's chain is passed through.
    /// When the first revision never arrived, from the oldest one later
    /// revisions name (the same on every device with the same files).
    fn head(ix: &Index, file: &FileId) -> Option<RevId> {
        let mut cur = match ix.starts.get(file).filter(|s| ix.revs.contains_key(*s)) {
            Some(s) => s,
            None => match ix.roots.get(file).and_then(|r| r.iter().min()) {
                Some(r) => r,
                None => ix.dangling.get(file)?.iter().find(|d| !ix.revs.contains_key(*d))?,
            },
        };
        for _ in 0..=ix.revs.len() + ix.bridges.len() {
            match ix.children.get(cur).and_then(|c| c.iter().min()) {
                Some(next) => cur = next,
                None => break,
            }
        }
        ix.revs.contains_key(cur).then(|| cur.clone())
    }

    fn read_rev(ix: &Index, rev: &RevId) -> Result<Revision, TransportError> {
        let m = ix.revs.get(rev).ok_or_else(|| TransportError::Refused("a revision went missing from the sync folder".into()))?;
        let mut f = File::open(&*m.loc.path).map_err(|e| io_err("can't read the sync folder", &e))?;
        f.seek(SeekFrom::Start(m.loc.offset)).map_err(|e| io_err("can't read the sync folder", &e))?;
        let mut buf = vec![0; m.loc.len];
        f.read_exact(&mut buf).map_err(|e| io_err("can't read the sync folder", &e))?;
        serde_json::from_slice(&buf).map_err(|_| TransportError::Refused("a log file in the sync folder changed unexpectedly".into()))
    }

    fn next_seq(&self, ix: &mut Index) -> u64 {
        ix.seq += 1;
        if let Some(f) = &self.seq_file {
            let _ = write_atomically(f, ix.seq.to_string().as_bytes());
        }
        ix.seq
    }

    /// The revisions `r` builds on directly: parents, snapshot links, or
    /// what a chain says.
    fn ups(ix: &Index, r: &RevId) -> Vec<RevId> {
        let mut out: Vec<RevId> = match ix.revs.get(r) {
            Some(m) => m.parents.clone(),
            None => ix.bridges.get(r).cloned().into_iter().collect(),
        };
        out.extend(ix.links.get(r).into_iter().flatten().cloned());
        out
    }

    /// Every revision `rev` builds on, and itself.
    fn ancestry(ix: &Index, rev: &RevId) -> HashSet<RevId> {
        let mut seen = HashSet::new();
        let mut todo = vec![rev.clone()];
        while let Some(r) = todo.pop() {
            if seen.insert(r.clone()) {
                todo.extend(Self::ups(ix, &r).into_iter().filter(|p| !seen.contains(p)));
            }
        }
        seen
    }

    /// The log file this device appends to now.
    fn current_segment(&self) -> Result<PathBuf, TransportError> {
        let dir = self.own_dir();
        fs::create_dir_all(&dir).map_err(|e| io_err("can't write to the sync folder", &e))?;
        let last = fs::read_dir(&dir)
            .map_err(|e| io_err("can't read the sync folder", &e))?
            .flatten()
            .filter_map(|e| e.file_name().to_str().filter(|n| is_segment_name(n)).and_then(|n| n[..6].parse::<u32>().ok()))
            .max();
        let path = |n: u32| dir.join(format!("{n:06}.jsonl"));
        Ok(match last {
            Some(n) if fs::metadata(path(n)).map_or(0, |m| m.len()) < SEGMENT_LIMIT => path(n),
            Some(n) => path(n + 1),
            None => path(1),
        })
    }

    fn append(&self, ix: &mut Index, r: &Revision, n: u64) -> Result<(), TransportError> {
        let path = self.current_segment()?;
        let werr = |e: std::io::Error| io_err("can't write to the sync folder", &e);
        let mut f = OpenOptions::new().create(true).read(true).append(true).open(&path).map_err(werr)?;
        let mut offset = f.metadata().map_err(werr)?.len();
        let before = offset;
        let mut line = Vec::new();
        if offset > 0 {
            // A crash can leave half a line; start on a fresh one.
            let mut last = [0u8];
            f.seek(SeekFrom::Start(offset - 1)).map_err(werr)?;
            f.read_exact(&mut last).map_err(werr)?;
            if last[0] != b'\n' {
                line.push(b'\n');
                offset += 1;
            }
        }
        let json = serde_json::to_vec(&Revision { seq: n, ..r.clone() }).map_err(|e| TransportError::Refused(e.to_string()))?;
        line.extend_from_slice(&json);
        line.push(b'\n');
        // Noted before writing: a crash in between leaves the file shorter
        // than noted, never longer.
        self.note_own(&path, before + line.len() as u64);
        f.write_all(&line).map_err(werr)?;
        f.sync_data().map_err(werr)?;
        ix.mine.insert(r.rev.clone());
        Self::insert(ix, r, Loc { path: Arc::new(path), offset, len: json.len() });
        Ok(())
    }

    /// Note that this device's log file `path` is `len` bytes long.
    fn note_own(&self, path: &Path, len: u64) {
        let (Some(f), Some(name)) = (&self.own_file, path.file_name().and_then(|n| n.to_str())) else { return };
        let mut own: BTreeMap<String, u64> = fs::read(f).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        own.insert(name.to_string(), len);
        let _ = write_atomically(f, &serde_json::to_vec(&own).unwrap_or_default());
    }

    /// How long this device wrote its log file `path`, if it noted it (a
    /// synced folder; a mirror's own lines are written by its transport).
    fn own_len(&self, path: &Path) -> Option<u64> {
        if !self.check_lines {
            return None;
        }
        let name = path.file_name()?.to_str()?;
        let own: BTreeMap<String, u64> = fs::read(self.own_file.as_ref()?).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
        own.get(name).copied()
    }

    /// Whether this device's log file `name` is longer than it wrote it.
    fn grew_elsewhere(&self, name: &str, len: u64) -> bool {
        self.own_len(&self.own_dir().join(name)).is_some_and(|l| len > l)
    }

    /// [`Transport::push`], with the line numbered `n` (the object number of
    /// an S3 or WebDAV mirror; 0 in a synced folder).
    pub fn push_numbered(&self, rev: &Revision, n: u64) -> Result<PushOutcome, TransportError> {
        let mut ix = self.lock();
        self.check_folder()?;
        if let Some(p) = &ix.problem {
            return Err(TransportError::Refused(p.clone()));
        }
        match (Self::head(&ix, &rev.file), rev.parents.first()) {
            (None, None) => {}
            (Some(h), Some(p)) if h == *p => {}
            (Some(h), _) => {
                let mut head = Self::read_rev(&ix, &h)?;
                head.seq = ix.delivered.get(&rev.file).map_or(0, |(_, s)| *s);
                return Ok(PushOutcome::Conflict { head });
            }
            (None, Some(_)) => return Err(TransportError::Refused(crate::transport::LOST_PARENT.into())),
        }
        self.append(&mut ix, rev, n)?;
        let seq = self.next_seq(&mut ix);
        ix.delivered.insert(rev.file.clone(), (rev.rev.clone(), seq));
        Ok(PushOutcome::Accepted { seq })
    }

    /// The furthest line read in each device's lane (after a scan).
    pub fn lane_ends(&self) -> Result<BTreeMap<DeviceId, LanePos>, TransportError> {
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        let mut out: BTreeMap<DeviceId, LanePos> = ix.lane_end.iter().map(|(d, p)| (d.clone(), *p)).collect();
        // Past the snapshot's cut, a lane may have nothing more.
        if let Some(g) = &ix.snap {
            for (d, c) in &g.manifest.cut {
                let e = out.entry(d.clone()).or_default();
                if e.is_empty() || (c.n > 0 && e.n < c.n) {
                    *e = LanePos { seg: e.seg.max(c.seg), line: if e.seg > c.seg { e.line } else { e.line.max(c.line) }, n: e.n.max(c.n) };
                }
            }
        }
        Ok(out)
    }

    /// Where this device's own lane stands, as read here.
    pub fn own_end(&self) -> Result<LanePos, TransportError> {
        Ok(self.lane_ends()?.get(&self.device).copied().unwrap_or_default())
    }

    /// Every complete snapshot taken here: its name and the SHA-256 of its
    /// manifest (a key change names them, see [`LaneRules::snapshots`]).
    pub fn snapshots_taken(&self) -> Vec<(String, String)> {
        let mut ix = self.lock();
        let ms = self.snapshots(&mut ix);
        drop(ix);
        ms.iter()
            .filter_map(|m| {
                let bytes = fs::read(self.snapshots_dir().join(m.name()).join("manifest.json")).ok()?;
                Some((m.name(), sha256_hex(&bytes)))
            })
            .collect()
    }

    /// Devices with a lane here (they wrote revisions).
    pub fn writers(&self) -> Vec<DeviceId> {
        let mut out: Vec<DeviceId> = fs::read_dir(self.logs.join("devices"))
            .map(|rd| {
                rd.flatten().filter_map(|e| e.file_name().to_str().filter(|n| is_device_name(n)).map(|n| DeviceId(n.to_string()))).collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// The snapshot in use: its name and when it was first seen here.
    pub fn generation(&self) -> Option<(Manifest, i64)> {
        self.generation_settled().map(|(m, s, _)| (m, s))
    }

    /// [`Self::generation`], and whether it has settled here.
    pub fn generation_settled(&self) -> Option<(Manifest, i64, bool)> {
        let mut ix = self.lock();
        let _ = self.scan(&mut ix);
        let m = ix.snap.as_ref()?.manifest.clone();
        let now = unix_now();
        let seen = self.load_seen().get(&m.name()).copied().unwrap_or(Seen { at: now, ticks: 0, last: now });
        Some((m, seen.at, self.settled(&seen, now)))
    }

    /// Whether every lane was read without a gap up to where it was read
    /// (a sync tool may deliver a later log file before an earlier one),
    /// and nothing waits for a download. Compaction cuts only such lanes.
    fn lanes_whole(&self, ix: &Index) -> bool {
        if ix.waiting_for_download > 0 {
            return false;
        }
        ix.lane_end.iter().all(|(d, end)| {
            if end.seg == 0 {
                return true;
            }
            let base = ix.snap.as_ref().and_then(|g| g.manifest.cut.get(d)).map_or(1, |c| c.seg.max(1));
            let dir = self.logs.join("devices").join(&d.0);
            (base..end.seg).all(|s| {
                let p = dir.join(format!("{s:06}.jsonl"));
                let len = fs::metadata(&p).map_or(None, |m| Some(m.len()));
                // Read to its end (it can't grow: a newer segment exists).
                len.is_some_and(|l| ix.read_to.get(&p).is_some_and(|(r, _)| *r == l))
            })
        })
    }

    /// Fold old revisions into a snapshot (`docs/design/0.13-sync.md` §1).
    /// `cut` replaces the lanes' ends as read here (an S3 or WebDAV mirror
    /// passes its object numbers). With `force`, even when little would
    /// go (but never when nothing would). Returns the snapshot written, or why not.
    pub fn compact(&self, policy: Policy, cut: Option<BTreeMap<DeviceId, LanePos>>, force: bool) -> Result<Compacted, TransportError> {
        let keys = self.keyset().ok_or_else(|| TransportError::Refused("compaction needs the vault's keys".into()))?;
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        if self.level() < LEVEL_HARDENED {
            return Ok(Compacted::NotUpgraded);
        }
        if ix.waiting_key {
            return Ok(Compacted::Busy("some changes use a key this device doesn't have yet".into()));
        }
        if !self.lanes_whole(&ix) {
            return Ok(Compacted::Busy("some sync files haven't arrived yet".into()));
        }
        let now = unix_now();
        let generation = match &ix.snap {
            Some(g) => {
                let seen = self.load_seen().get(&g.manifest.name()).copied().unwrap_or(Seen { at: now, ticks: 0, last: now });
                if !self.settled(&seen, now) {
                    return Ok(Compacted::Settling { until: (seen.at + self.settle).max(now) });
                }
                g.manifest.generation + 1
            }
            None => 1,
        };
        // Someone already made this generation: let it settle.
        if fs::read_dir(self.snapshots_dir()).is_ok_and(|rd| {
            rd.flatten().any(|e| e.file_name().to_str().and_then(crate::compact::parse_name).is_some_and(|(g, _)| g >= generation))
        }) {
            return Ok(Compacted::Busy("another device is compacting".into()));
        }
        // One revision at a time: only ids, times and sizes stay in memory.
        let mut nodes = HashMap::new();
        let ids: Vec<RevId> = ix.revs.keys().cloned().collect();
        for id in ids {
            let r = Self::read_rev(&ix, &id)?;
            let opened = Content::open_set(&keys, &self.space, &r, true);
            let bytes = ix.revs.get(&id).map_or(0, |m| m.loc.len as u64);
            nodes.insert(
                id,
                Node { at: opened.as_ref().map_or(0, |c| c.at), bytes, opaque: opened.is_err(), file: r.file, parents: r.parents },
            );
        }
        let files = Self::files(&ix);
        let heads: BTreeMap<FileId, RevId> = files.iter().filter_map(|f| Some((f.clone(), Self::head(&ix, f)?))).collect();
        // A note with revisions but no current one: something's missing.
        if ix.revs.values().any(|m| !heads.contains_key(&m.file)) {
            return Ok(Compacted::Busy("some sync files haven't arrived yet".into()));
        }
        let links: HashMap<RevId, Vec<RevId>> = ix.links.clone();
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let plan = crate::compact::plan(&nodes, &heads, &links, policy, now_ms);
        // Nothing would go, or (unless asked) too little.
        if plan.stats.dropped == 0 || (!force && !crate::compact::worth_it(&plan.stats)) {
            return Ok(Compacted::NotWorthIt(plan.stats));
        }
        let mut cut = cut.unwrap_or_else(|| ix.lane_end.iter().map(|(d, p)| (d.clone(), *p)).collect());
        if let Some(g) = &ix.snap {
            for (d, c) in &g.manifest.cut {
                cut.entry(d.clone()).or_insert(*c);
            }
        }
        let name = crate::compact::name(generation, &self.device);
        let dir = self.snapshots_dir().join(&name);
        let tmp = self.snapshots_dir().join(format!(".{name}.tmp"));
        let werr = |e: std::io::Error| io_err("can't write to the sync folder", &e);
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).map_err(werr)?;
        // Kept revisions in log order of their files, so parents come first.
        let mut kept: Vec<(&RevId, &Meta)> = ix.revs.iter().filter(|(id, _)| plan.keep.contains(*id)).collect();
        kept.sort_by(|a, b| (&*a.1.loc.path, a.1.loc.offset).cmp(&(&*b.1.loc.path, b.1.loc.offset)));
        let mut parts = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut lines = 0;
        let flush = |buf: &mut Vec<u8>, lines: &mut u64, parts: &mut Vec<Part>| -> Result<(), TransportError> {
            if buf.is_empty() {
                return Ok(());
            }
            let pname = format!("part-{:06}.jsonl", parts.len() + 1);
            write_atomically(&tmp.join(&pname), buf).map_err(werr)?;
            parts.push(Part { name: pname, size: buf.len() as u64, sha256: sha256_hex(buf), lines: *lines });
            buf.clear();
            *lines = 0;
            Ok(())
        };
        for (id, _) in kept {
            let line = serde_json::to_vec(&Revision { seq: 0, ..Self::read_rev(&ix, id)? }).unwrap_or_default();
            buf.extend_from_slice(&line);
            buf.push(b'\n');
            lines += 1;
            if buf.len() >= crate::compact::PART_BYTES {
                flush(&mut buf, &mut lines, &mut parts)?;
            }
        }
        flush(&mut buf, &mut lines, &mut parts)?;
        let mut m = Manifest {
            format: crate::compact::FORMAT,
            space: self.space.clone(),
            lane: self.lane_name.clone(),
            generation,
            maker: self.device.clone(),
            made: now,
            epoch: 0,
            cut,
            parts,
            starts: plan.starts,
            links: plan.links,
            stats: plan.stats,
            mac: String::new(),
        };
        m.seal(&keys);
        // The manifest goes last: until it's there, the snapshot isn't used.
        write_atomically(&tmp.join("manifest.json"), &serde_json::to_vec_pretty(&m).unwrap_or_default()).map_err(werr)?;
        fs::rename(&tmp, &dir).map_err(werr)?;
        drop(ix);
        self.mark_seen(&m);
        Ok(Compacted::Done(Box::new(m)))
    }

    /// Delete what a settled snapshot made unnecessary: this device's own
    /// log files before its cut, and older snapshots. Also the logs of
    /// removed devices before the cut. Returns how many files went.
    pub fn tidy(&self) -> usize {
        let Some((m, _, settled)) = self.generation_settled() else { return 0 };
        if !settled {
            return 0;
        }
        let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut gone = 0;
        for (d, c) in &m.cut {
            if (*d != self.device && !removed.contains_key(d)) || c.seg == 0 {
                continue;
            }
            let Ok(rd) = fs::read_dir(self.logs.join("devices").join(&d.0)) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let s = segment_number(&p);
                // Whole segments before the cut's (the cut's own may go on).
                if s > 0 && s < c.seg && fs::remove_file(&p).is_ok() {
                    gone += 1;
                }
            }
        }
        if let Ok(rd) = fs::read_dir(self.snapshots_dir()) {
            for e in rd.flatten() {
                let Some(n) = e.file_name().to_str().map(str::to_string) else { continue };
                let older = crate::compact::parse_name(&n).is_some_and(|(g, _)| g < m.generation || (g == m.generation && n != m.name()));
                if older && fs::remove_dir_all(e.path()).is_ok() {
                    gone += 1;
                }
            }
        }
        gone
    }

    /// The vault's key records (0.13: a device was removed).
    pub fn key_records(&self) -> Vec<EpochRecord> {
        let Ok(rd) = fs::read_dir(self.dir.join("keys")) else { return Vec::new() };
        let mut out: Vec<EpochRecord> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice::<EpochRecord>(&b).ok())
            .filter(|r| r.space == self.space)
            .collect();
        out.sort_by(|a, b| (a.epoch, &a.maker).cmp(&(b.epoch, &b.maker)));
        out
    }

    /// Write a key record (by the device that makes it; never changed).
    pub fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        let dir = self.dir.join("keys");
        let werr = |e: std::io::Error| io_err("can't write to the sync folder", &e);
        fs::create_dir_all(&dir).map_err(werr)?;
        let path = dir.join(format!("{:06}-{}.json", r.epoch, r.maker));
        write_atomically(&path, &serde_json::to_vec_pretty(r).unwrap_or_default()).map_err(werr)
    }

    /// Every device's record (0.13 devices publish one).
    pub fn device_records(&self) -> Vec<DeviceRecord> {
        let Ok(rd) = fs::read_dir(self.dir.join("devices")) else { return Vec::new() };
        rd.flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let r: DeviceRecord = serde_json::from_slice(&fs::read(e.path().join("device.json")).ok()?).ok()?;
                (r.device.0 == name).then_some(r)
            })
            .collect()
    }

    /// Publish this device's record (in its own folder).
    pub fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        let dir = self.dir.join("devices").join(&self.device.0);
        let werr = |e: std::io::Error| io_err("can't write to the sync folder", &e);
        fs::create_dir_all(&dir).map_err(werr)?;
        write_atomically(&dir.join("device.json"), &serde_json::to_vec_pretty(r).unwrap_or_default()).map_err(werr)
    }
}

/// What [`FolderTransport::compact`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compacted {
    Done(Box<Manifest>),
    /// Too little would go.
    NotWorthIt(Stats),
    /// The last snapshot is still settling, until then (Unix seconds).
    Settling {
        until: i64,
    },
    /// The vault isn't upgraded to 0.13 sync yet.
    NotUpgraded,
    Busy(String),
}

impl Transport for FolderTransport {
    /// The current revision of every note that changed since it was last
    /// handed out, each with a new order number past `after`.
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        ix.seq = ix.seq.max(after);
        for f in Self::files(&ix) {
            let Some(h) = Self::head(&ix, &f) else { continue };
            if ix.delivered.get(&f).is_none_or(|(d, _)| *d != h) {
                let seq = self.next_seq(&mut ix);
                ix.delivered.insert(f, (h, seq));
            }
        }
        let mut due: Vec<(u64, RevId)> = ix.delivered.values().filter(|(_, s)| *s > after).map(|(r, s)| (*s, r.clone())).collect();
        due.sort();
        due.into_iter()
            .filter(|(_, rev)| ix.revs.contains_key(rev))
            .map(|(seq, rev)| {
                let mut r = Self::read_rev(&ix, &rev)?;
                r.seq = seq;
                Ok(r)
            })
            .collect()
    }

    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        self.push_numbered(rev, 0)
    }

    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        let mine: Vec<RevId> = ix.revs.iter().filter(|(_, m)| m.file == *file).map(|(r, _)| r.clone()).collect();
        mine.iter().map(|r| Self::read_rev(&ix, r).map(|r| Revision { seq: 0, ..r })).collect()
    }

    fn fork_base(&self, file: &FileId, ours: &RevId, theirs: &Revision) -> Option<Revision> {
        match self.fork(file, ours, theirs) {
            Fork::Base(b) => Some(b),
            _ => None,
        }
    }

    fn fork(&self, _file: &FileId, ours: &RevId, theirs: &Revision) -> Fork {
        let ix = self.lock();
        let theirs_has = Self::ancestry(&ix, &theirs.rev);
        if theirs_has.contains(ours) {
            return Fork::Builds;
        }
        // Walk back from ours to the nearest revision theirs also has, noting
        // whether anything on the way was written here, and whether the way
        // runs into revisions no longer here (folded into a snapshot).
        let (mut written_here, mut base, mut gap) = (false, None, false);
        let mut todo = VecDeque::from([ours.clone()]);
        let mut seen = HashSet::new();
        while let Some(r) = todo.pop_front() {
            if !seen.insert(r.clone()) {
                continue;
            }
            if theirs_has.contains(&r) {
                base = base.or(Some(r));
                continue;
            }
            match ix.revs.get(&r) {
                Some(m) => {
                    written_here |= m.device == self.device;
                    todo.extend(Self::ups(&ix, &r));
                }
                None if ix.bridges.contains_key(&r) => todo.extend(Self::ups(&ix, &r)),
                None => gap |= r != *ours,
            }
        }
        if !written_here {
            return Fork::Builds;
        }
        match base.and_then(|b| Self::read_rev(&ix, &b).ok()) {
            Some(b) => Fork::Base(b),
            None if gap || ix.snap.is_some() => Fork::Unknown,
            None => Fork::Builds,
        }
    }

    fn sealing(&self) -> Sealing {
        if self.level() >= LEVEL_HARDENED { Sealing::Epoch } else { Sealing::Plain }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{KdfParams, VaultKey};

    fn rev(file: &FileId, parents: &[&RevId], device: &DeviceId) -> Revision {
        Revision {
            file: file.clone(),
            rev: RevId::random().unwrap(),
            parents: parents.iter().map(|p| (*p).clone()).collect(),
            seq: 0,
            device: device.clone(),
            sealed: vec![1, 2, 3],
            chain: vec![],
        }
    }

    /// Quire 0.8's check of `space.json`, as shipped.
    fn readable_by_0_8(dir: &Path) -> bool {
        let v: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("space.json")).unwrap()).unwrap();
        v["format"].as_u64().unwrap() <= 1
    }

    #[test]
    fn a_lane_is_invisible_to_the_main_logs_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = sync_root(dir.path());
        let space = SpaceId::random().unwrap();
        let key = WrappedKey::wrap(&VaultKey::generate().unwrap(), "pw", KdfParams::TEST).unwrap();
        create_space(&root, &space, &key).unwrap();
        let d = DeviceId::random().unwrap();
        assert!(!lane_exists(&root, &space, SHARED_LANE));
        let lane = FolderTransport::lane(&root, &space, d.clone(), None, SHARED_LANE);
        let main = FolderTransport::new(&root, &space, d.clone(), None);
        let f = FileId::random().unwrap();
        let r = rev(&f, &[], &d);
        assert!(matches!(lane.push(&r).unwrap(), PushOutcome::Accepted { .. }));
        assert!(lane_exists(&root, &space, SHARED_LANE));
        assert!(root.join(&space.0).join("shared/devices").join(&d.0).join("000001.jsonl").is_file());
        // The main logs (all that 0.8 reads) don't have it.
        let other = FolderTransport::new(&root, &space, DeviceId::random().unwrap(), None);
        assert!(other.pull(0).unwrap().is_empty());
        assert!(main.pull(0).unwrap().is_empty());
        let main_rev = rev(&FileId::random().unwrap(), &[], &d);
        main.push(&main_rev).unwrap();
        let other_lane = FolderTransport::lane(&root, &space, DeviceId::random().unwrap(), None, SHARED_LANE);
        assert_eq!(other_lane.pull(0).unwrap().iter().map(|r| r.rev.clone()).collect::<Vec<_>>(), [r.rev]);
        assert_eq!(other.pull(0).unwrap().iter().map(|r| r.rev.clone()).collect::<Vec<_>>(), [main_rev.rev]);
    }

    #[test]
    fn format_fields_let_old_and_new_readers_choose() {
        let dir = tempfile::tempdir().unwrap();
        let root = sync_root(dir.path());
        let space = SpaceId::random().unwrap();
        let key = WrappedKey::wrap(&VaultKey::generate().unwrap(), "pw", KdfParams::TEST).unwrap();
        create_space(&root, &space, &key).unwrap();
        let sdir = root.join(&space.0);
        assert!(readable_by_0_8(&sdir), "0.9 keeps writing format 1");
        assert_eq!(min_reader(&root, &space).unwrap(), 1);
        // Unknown fields from a newer Quire survive a rewrite.
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(sdir.join("space.json")).unwrap()).unwrap();
        v["future"] = serde_json::json!({"x": 1});
        fs::write(sdir.join("space.json"), serde_json::to_vec(&v).unwrap()).unwrap();
        raise_min_reader(&root, &space, 2).unwrap();
        assert!(!readable_by_0_8(&sdir), "0.8 stops at a min-2 space");
        assert_eq!(min_reader(&root, &space).unwrap(), 2);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(sdir.join("space.json")).unwrap()).unwrap();
        assert_eq!((v["format"].clone(), v["min_reader"].clone(), v["future"]["x"].clone()), (2.into(), 2.into(), 1.into()));
        assert!(wrapped_key(&root, &space).is_ok());
        raise_min_reader(&root, &space, 1).unwrap();
        assert_eq!(min_reader(&root, &space).unwrap(), 2, "never lowered");
        // A future format that 0.9 may still read, and one it may not.
        let mut v = v;
        v["format"] = 3.into();
        fs::write(sdir.join("space.json"), serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(wrapped_key(&root, &space).is_ok(), "format 3 / min_reader 2: readable by 0.9");
        v["min_reader"] = (READER_FORMAT + 1).into();
        fs::write(sdir.join("space.json"), serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(wrapped_key(&root, &space).unwrap_err().to_string().contains("newer Quire"));
    }

    #[test]
    fn spaces_are_listed_and_their_keys_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = sync_root(dir.path());
        assert!(root.ends_with(FOLDER_NAME));
        assert_eq!(sync_root(&root), root);
        assert!(list_spaces(&root).unwrap().is_empty());
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        let wrapped = WrappedKey::wrap(&key, "correct horse", KdfParams::TEST).unwrap();
        create_space(&root, &space, &wrapped).unwrap();
        let listed = list_spaces(&root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].space, space);
        assert_eq!(wrapped_key(&root, &space).unwrap(), wrapped);
    }

    #[test]
    fn torn_lines_wait_and_ties_resolve_the_same_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let space = SpaceId::random().unwrap();
        let key = WrappedKey::wrap(&VaultKey::generate().unwrap(), "pass phrase", KdfParams::TEST).unwrap();
        create_space(dir.path(), &space, &key).unwrap();
        let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
        let a = FolderTransport::new(dir.path(), &space, da.clone(), None);
        let b = FolderTransport::new(dir.path(), &space, db.clone(), None);
        let file = FileId::random().unwrap();
        let root = rev(&file, &[], &da);
        assert!(matches!(a.push(&root).unwrap(), PushOutcome::Accepted { .. }));
        // Both build on the root before seeing each other's.
        assert_eq!(b.pull(0).unwrap().len(), 1);
        let ra = rev(&file, &[&root.rev], &da);
        let rb = rev(&file, &[&root.rev], &db);
        a.push(&ra).unwrap();
        b.push(&rb).unwrap();
        let winner = ra.rev.clone().min(rb.rev.clone());
        let ha = a.pull(0).unwrap().into_iter().find(|r| r.file == file).unwrap();
        let hb = b.pull(0).unwrap().into_iter().find(|r| r.file == file).unwrap();
        assert_eq!((&ha.rev, &hb.rev), (&winner, &winner));
        // Only the device that lost the tie is asked to merge again, from the root.
        let (loser, loser_rev) = if winner == ra.rev { (&b, &rb.rev) } else { (&a, &ra.rev) };
        let winner_t = if winner == ra.rev { &a } else { &b };
        assert_eq!(loser.fork_base(&file, loser_rev, &ha).map(|r| r.rev), Some(root.rev));
        assert_eq!(winner_t.fork_base(&file, &winner, &ha), None);

        // Half a line (a download in progress) isn't read until it's whole.
        let seg = dir.path().join(&space.0).join("devices").join(&da.0).join("000001.jsonl");
        let late = rev(&file, &[&winner], &da);
        let line = serde_json::to_vec(&late).unwrap();
        OpenOptions::new().append(true).open(&seg).unwrap().write_all(&line[..10]).unwrap();
        assert!(!b.pull(0).unwrap().iter().any(|r| r.rev == late.rev));
        let mut rest = line[10..].to_vec();
        rest.push(b'\n');
        OpenOptions::new().append(true).open(&seg).unwrap().write_all(&rest).unwrap();
        assert!(b.pull(0).unwrap().iter().any(|r| r.rev == late.rev));
    }

    #[test]
    fn a_second_computer_writing_as_this_device_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let space = SpaceId::random().unwrap();
        let key = WrappedKey::wrap(&VaultKey::generate().unwrap(), "pass phrase", KdfParams::TEST).unwrap();
        create_space(dir.path(), &space, &key).unwrap();
        let d = DeviceId::random().unwrap();
        let one = FolderTransport::new(dir.path(), &space, d.clone(), None);
        let two = FolderTransport::new(dir.path(), &space, d.clone(), None);
        one.pull(0).unwrap();
        two.pull(0).unwrap();
        two.push(&rev(&FileId::random().unwrap(), &[], &d)).unwrap();
        one.pull(0).unwrap();
        assert!(one.problem().is_some_and(|n| n.contains("Another computer")));
        assert!(matches!(one.push(&rev(&FileId::random().unwrap(), &[], &d)), Err(TransportError::Refused(_))));
    }

    #[test]
    fn a_missing_folder_is_offline_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let t = FolderTransport::new(&dir.path().join("unplugged"), &SpaceId::random().unwrap(), DeviceId::random().unwrap(), None);
        assert!(matches!(t.pull(0), Err(TransportError::Offline(_))));
    }
}
