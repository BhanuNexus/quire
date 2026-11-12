// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sync through object storage: S3-compatible buckets and WebDAV folders
//! (design 0.12, `docs/design/0.12-sync.md`).
//!
//! It is folder sync ([`crate::folder`]) with the storage in place of the
//! synced folder. The rules that keep revisions safe there hold here too:
//!
//! * **Each device writes only its own objects**, and an object is written
//!   once and never changed. Two devices syncing at once never write the
//!   same name, so no revision can be overwritten, on any storage, whether
//!   or not it supports conditional writes.
//! * Each device reads every device's revisions and works out each note's
//!   current revision with the folder transport's rule (follow the child
//!   with the smallest id), which doesn't depend on timing or order.
//!
//! ```text
//! <bucket>/<prefix>/quire-sync/<space>/
//!   space.json                     the wrapped vault key (written once)
//!   tips/<device>                  {"n": 42, "w": …}: that device has 42 revisions
//!   devices/<device>/000000000042.rev   its 42nd revision (never changed)
//! ```
//!
//! A device's **tip** is the only object ever overwritten, and only by its
//! own device. Listing `tips/` is one request, so checking for changes
//! costs one request however many notes or devices there are.
//!
//! **The mirror.** Revisions are kept on this device in the folder
//! transport's own layout (`.quire/sync/remote/mirror/`), and a
//! [`FolderTransport`] over that mirror does the rest. A revision pushed
//! here is first appended to the mirror (durable on this device), then
//! uploaded by [`RemoteTransport::flush`] or the next pass: an upload cut
//! short resumes where it stopped, and no revision is lost offline.
//!
//! **Conditional writes** (`If-None-Match: *`) are used where the storage
//! has them, to make `space.json` once and to catch a second computer
//! writing as this device. Without them those checks are a read before the
//! write, which can miss a race only in that misconfiguration.
//!
//! Nothing read from the storage is trusted: only names that are ids are
//! used, every reply has a size limit, XML has no DTDs, redirects are
//! refused, and a revision must say it's from the device whose folder it
//! is in. With the vault's keys ([`RemoteTransport::with_keys`]) every
//! revision is also decrypted before it's mirrored, and revisions are
//! sealed with their whole header bound ([`crate::Content::seal_bound`]),
//! so the storage can't change which revision one builds on.
//!
//! **A lost or damaged object** doesn't stop sync. A reader skips it once a
//! later object of that device is there, records it, says so
//! ([`RemoteTransport::waiting`]), and tries it again whenever that device
//! syncs; the device that wrote it checks its own objects once per session
//! and uploads again any that are missing or cut short.

pub mod net;
pub mod s3;
pub mod sigv4;
pub mod webdav;
pub mod xml;

#[cfg(feature = "mock-server")]
pub mod mock;

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::compact::{Manifest, Policy};
use crate::crypto::{CryptoError, WrappedKey};
use crate::folder::{Compacted, FolderTransport};
use crate::keyring::{DeviceRecord, EpochRecord, KeySet};
use crate::protocol::{Content, DeviceId, FileId, LanePos, PushOutcome, RevId, Revision, Sealing, SpaceId};
use crate::transport::{Fork, Transport, TransportError};

pub use s3::{S3Config, S3Credentials, S3Store};
pub use webdav::{WebDavConfig, WebDavCredentials, WebDavStore};
/// Secrets in memory are wiped when dropped.
pub use zeroize::Zeroizing;

/// The folder Quire keeps in the bucket or WebDAV folder.
pub const ROOT: &str = "quire-sync";
/// The format of remote spaces; Quire 0.12 is the first to read it.
pub const FORMAT: u32 = 3;
/// The minimum reader of a 0.13 vault in the storage (key epochs, revision
/// chains, compaction; `docs/design/0.13-sync.md`). 0.12 refuses it with
/// "made by a newer Quire".
pub const LEVEL_HARDENED: u32 = 4;
/// The newest minimum reader this build reads.
pub const READER: u32 = 4;
/// A revision object, at most: a 10 MB attachment is about 14 MB sealed.
pub const MAX_OBJECT: u64 = 64 * 1024 * 1024;
/// `space.json` and tips are small.
const MAX_SMALL: u64 = 256 * 1024;
/// Downloads and uploads at once.
const PARALLEL: usize = 8;
/// Revisions fetched in one pass, at most (the rest come next pass).
const MAX_FETCH_PER_PASS: u64 = 50_000;
/// Objects fetched before they're written to the mirror (each at most
/// [`MAX_OBJECT`], usually a few KB), so memory stays bounded.
const BATCH: u64 = 32;
/// Bytes fetched in one pass, at most; the rest come in the next pass.
const PASS_BYTES: u64 = 256 * 1024 * 1024;
/// Skipped revisions of a device tried again each time it syncs.
const RETRY_LOST: usize = 16;
/// How long a revision a tip announces may be missing before the status
/// calls it missing instead of "waiting".
const MISSING_AFTER: Duration = Duration::from_mins(10);
/// A lost mirror is restored only if the storage holds about as many of
/// this device's objects as its tip says (a tip far ahead is refused).
const RESTORE_SLACK: u64 = 64;
/// Devices in one vault, at most.
const MAX_DEVICES: usize = 1000;
/// The first line of a revision object.
const MAGIC: &[u8] = b"quire-rev/1\n";

/// What went wrong talking to the storage.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// No network, a timeout, or the storage is down. Worth retrying.
    #[error("{0}")]
    Offline(String),
    /// Wrong or expired credentials, or not allowed.
    #[error("{0}")]
    Auth(String),
    /// This computer's clock is too far off for signed requests.
    #[error("{0}")]
    ClockSkew(String),
    /// A wrong address, bucket, region or folder.
    #[error("{0}")]
    Config(String),
    /// The storage doesn't take `If-None-Match: *`.
    #[error("this storage doesn't support conditional writes")]
    NoConditional,
    /// Anything else, including replies Quire won't accept.
    #[error("{0}")]
    Refused(String),
}

impl From<StoreError> for TransportError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Offline(m) => Self::Offline(m),
            e => Self::Refused(e.to_string()),
        }
    }
}

/// One name in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Not yet checked: callers accept only the names they expect.
    pub name: String,
    pub dir: bool,
    /// The storage's `ETag`; empty when it has none.
    pub etag: String,
    /// Size and time, for servers without `ETag`s.
    pub stamp: String,
    /// The size in bytes, if the listing gave it.
    pub size: Option<u64>,
}

/// How a write went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Done,
    /// `create_only`, and the object was already there.
    Exists,
}

/// The few operations sync needs from a bucket or a WebDAV folder. Keys
/// are `/`-separated, relative to the profile's prefix or folder.
pub trait ObjectStore: Send + Sync + std::fmt::Debug {
    /// The object, or `None` if there's none. Fails over `limit` bytes.
    fn get(&self, key: &str, limit: u64) -> Result<Option<Vec<u8>>, StoreError>;
    fn exists(&self, key: &str) -> Result<bool, StoreError>;
    /// Write `body`. With `create_only`, only if nothing is there yet
    /// ([`Put::Exists`] otherwise, or [`StoreError::NoConditional`] when the
    /// storage can't promise that).
    fn put(&self, key: &str, body: &[u8], create_only: bool) -> Result<Put, StoreError>;
    /// What's directly in the folder `dir` (ends in `/`); empty if it doesn't exist.
    fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// "S3 bucket notes at https://…", for status lines.
    fn describe(&self) -> String;
    /// Whether a listing's `ETag` changes whenever an object's bytes do
    /// (S3: it's their MD5). WebDAV servers often derive it from the time
    /// in whole seconds and the size, so two writes of a tip within a
    /// second can keep it; their tips are then always read.
    fn etag_tracks_content(&self) -> bool {
        false
    }
}

/// A prefix or folder as typed (`/notes/`, `a/b`), checked: no `.`, `..`,
/// empty parts, backslashes or control characters.
pub fn clean_prefix(p: &str) -> Result<String, StoreError> {
    let t = p.trim().trim_matches('/');
    if t.is_empty() {
        return Ok(String::new());
    }
    if t.contains('\\') || t.chars().any(char::is_control) || t.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(StoreError::Config(format!("“{p}” isn't a usable folder name in the bucket")));
    }
    Ok(t.to_string())
}

/// Whether `s` is an id Quire made (32 lower-case hex digits). Every name
/// read from a listing is checked with this before it's used.
pub fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn space_dir(space: &SpaceId) -> String {
    format!("{ROOT}/{space}/")
}

fn object_key(space: &SpaceId, device: &DeviceId, n: u64) -> String {
    format!("{ROOT}/{space}/devices/{device}/{n:012}.rev")
}

fn tip_key(space: &SpaceId, device: &DeviceId) -> String {
    format!("{ROOT}/{space}/tips/{device}")
}

/// `space.json` of a remote space.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpaceFile {
    format: u32,
    space: SpaceId,
    created: i64,
    wrapped_key: WrappedKey,
    min_reader: u32,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// A vault found in the storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpaceInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    pub devices: usize,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Write `key` only if it isn't there: conditionally where the storage can,
/// else after checking (and remembering that it can't).
fn put_new(store: &dyn ObjectStore, key: &str, body: &[u8], conditional: &AtomicBool) -> Result<Put, StoreError> {
    if conditional.load(Ordering::Relaxed) {
        match store.put(key, body, true) {
            Err(StoreError::NoConditional) => conditional.store(false, Ordering::Relaxed),
            r => return r,
        }
    }
    if store.exists(key)? {
        return Ok(Put::Exists);
    }
    store.put(key, body, false)
}

/// Make a new vault in the storage that Quire 0.12 reads.
pub fn create_space(store: &dyn ObjectStore, space: &SpaceId, key: &WrappedKey) -> Result<(), TransportError> {
    create_space_at(store, space, key, FORMAT)
}

/// Make a new vault whose minimum reader is `level` (0.13 makes new vaults
/// at [`LEVEL_HARDENED`]).
pub fn create_space_at(store: &dyn ObjectStore, space: &SpaceId, key: &WrappedKey, level: u32) -> Result<(), TransportError> {
    let file = SpaceFile {
        format: level.max(FORMAT),
        space: space.clone(),
        created: unix_now(),
        wrapped_key: key.clone(),
        min_reader: level.max(FORMAT),
        other: serde_json::Map::new(),
    };
    let json = serde_json::to_vec_pretty(&file).unwrap_or_default();
    match put_new(store, &format!("{}space.json", space_dir(space)), &json, &AtomicBool::new(true))? {
        Put::Done => Ok(()),
        Put::Exists => Err(TransportError::Refused("a vault with this id already exists in the storage".into())),
    }
}

fn read_space(store: &dyn ObjectStore, space: &SpaceId) -> Result<SpaceFile, TransportError> {
    let bytes = store
        .get(&format!("{}space.json", space_dir(space)), MAX_SMALL)?
        .ok_or_else(|| TransportError::Refused("this vault isn't in the storage".into()))?;
    let f: SpaceFile =
        serde_json::from_slice(&bytes).map_err(|_| TransportError::Refused("the vault's space.json in the storage is damaged".into()))?;
    if f.space != *space {
        return Err(TransportError::Refused("the vault's space.json in the storage names another vault".into()));
    }
    if f.min_reader > READER {
        return Err(TransportError::Refused("this vault was made by a newer Quire; update Quire on this device".into()));
    }
    Ok(f)
}

/// The vault's minimum reader ([`LEVEL_HARDENED`] once upgraded).
pub fn level(store: &dyn ObjectStore, space: &SpaceId) -> Result<u32, TransportError> {
    Ok(read_space(store, space)?.min_reader)
}

/// Keep Quire older than `level` out of this vault (it refuses it with
/// "made by a newer Quire"). Other fields are kept; it never lowers.
pub fn raise_level(store: &dyn ObjectStore, space: &SpaceId, level: u32) -> Result<(), TransportError> {
    let mut f = read_space(store, space)?;
    if f.min_reader >= level {
        return Ok(());
    }
    f.format = f.format.max(level);
    f.min_reader = level;
    let json = serde_json::to_vec_pretty(&f).unwrap_or_default();
    store.put(&format!("{}space.json", space_dir(space)), &json, false)?;
    Ok(())
}

/// A vault's wrapped key, to join it from a new device.
pub fn wrapped_key(store: &dyn ObjectStore, space: &SpaceId) -> Result<WrappedKey, TransportError> {
    Ok(read_space(store, space)?.wrapped_key)
}

/// The vaults in the storage, oldest first.
pub fn list_spaces(store: &dyn ObjectStore) -> Result<Vec<RemoteSpaceInfo>, TransportError> {
    let mut out = Vec::new();
    for e in store.list(&format!("{ROOT}/"))?.into_iter().filter(|e| e.dir && is_id(&e.name)).take(100) {
        let space = SpaceId(e.name);
        let Ok(f) = read_space(store, &space) else { continue };
        let devices = store.list(&format!("{}tips/", space_dir(&space)))?.iter().filter(|t| !t.dir && is_id(&t.name)).count();
        out.push(RemoteSpaceInfo { space, created: f.created, devices });
    }
    out.sort_by(|a, b| (a.created, &a.space).cmp(&(b.created, &b.space)));
    Ok(out)
}

/// What "Test connection" found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The storage refuses a second `If-None-Match: *` write.
    pub conditional_writes: bool,
    /// Quire vaults already there.
    pub vaults: usize,
}

impl Probe {
    /// One line for the person.
    pub fn summary(&self) -> String {
        let vaults = match self.vaults {
            0 => "no Quire vaults yet".to_string(),
            1 => "1 Quire vault".to_string(),
            n => format!("{n} Quire vaults"),
        };
        let cond = if self.conditional_writes {
            "conditional writes supported"
        } else {
            "no conditional writes (fine: each device writes only its own files)"
        };
        format!("Connected: {vaults}; {cond}.")
    }
}

/// Check that Quire can list, write, read back and delete in the storage,
/// and whether it supports conditional writes. Writes and removes one small
/// object (`quire-sync/.probe-…`).
pub fn probe(store: &dyn ObjectStore) -> Result<Probe, StoreError> {
    let listed = store.list(&format!("{ROOT}/"))?;
    let id = crate::crypto::hex(&crate::crypto::random_bytes::<8>().map_err(|e| StoreError::Refused(e.to_string()))?);
    let key = format!("{ROOT}/.probe-{id}");
    let body = format!("quire probe {id}\n");
    let first = match store.put(&key, body.as_bytes(), true) {
        Err(StoreError::NoConditional) => store.put(&key, body.as_bytes(), false).map(|_| None),
        r => r.map(Some),
    }?;
    let conditional_writes = match first {
        // The same bytes again: a storage that ignores the header just
        // writes them a second time.
        Some(Put::Done) => store.put(&key, body.as_bytes(), true) == Ok(Put::Exists),
        _ => false,
    };
    let back = store.get(&key, MAX_SMALL)?;
    let _ = store.delete(&key);
    if back.as_deref() != Some(body.as_bytes()) {
        return Err(StoreError::Refused("the storage didn't give back what Quire wrote".into()));
    }
    Ok(Probe { conditional_writes, vaults: listed.iter().filter(|e| e.dir && is_id(&e.name)).count() })
}

/// A device's tip: how many revisions it has uploaded, and which process
/// wrote it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Tip {
    n: u64,
    #[serde(default)]
    w: String,
    /// 0.13: the snapshot this device made or uses (`<generation>-<maker>`),
    /// so the others notice a new one without listing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    s: String,
    /// 0.13: the key epoch this device uses.
    #[serde(default, skip_serializing_if = "is_zero")]
    k: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A revision as stored: `quire-rev/1`, its ids as JSON, then the sealed
/// bytes raw (half the size of the hex in the folder logs).
pub fn encode_object(r: &Revision) -> Vec<u8> {
    let head = serde_json::to_vec(&Revision { seq: 0, sealed: Vec::new(), ..r.clone() }).unwrap_or_default();
    let mut out = Vec::with_capacity(MAGIC.len() + head.len() + 1 + r.sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&head);
    out.push(b'\n');
    out.extend_from_slice(&r.sealed);
    out
}

pub fn decode_object(b: &[u8]) -> Option<Revision> {
    let rest = b.strip_prefix(MAGIC)?;
    let nl = rest.iter().position(|&c| c == b'\n')?;
    let mut r: Revision = serde_json::from_slice(&rest[..nl]).ok()?;
    let ids_ok = is_id(&r.file.0)
        && is_id(&r.rev.0)
        && is_id(&r.device.0)
        && r.parents.iter().chain(r.chain.iter()).all(|p| is_id(&p.0))
        && r.parents.len() <= 2
        && r.chain.len() <= crate::protocol::CHAIN;
    if !ids_ok || !r.sealed.is_empty() {
        return None;
    }
    r.sealed = rest[nl + 1..].to_vec();
    r.seq = 0;
    Some(r)
}

/// A folder that is removed when dropped (the mirror of a transport with
/// no state folder).
#[derive(Debug)]
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// How far another device's objects have been mirrored (`progress.json`
/// in the mirror; without it, the number of lines in its mirrored log).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Progress {
    /// Objects `1..=done` are mirrored, or skipped as lost.
    done: u64,
    /// The numbers skipped as missing or damaged, as inclusive ranges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lost: Vec<(u64, u64)>,
}

impl Progress {
    fn lose(&mut self, from: u64, to: u64) {
        if from > to {
            return;
        }
        match self.lost.last_mut() {
            Some(last) if last.1 + 1 == from => last.1 = to,
            _ => self.lost.push((from, to)),
        }
    }

    fn found(&mut self, n: u64) {
        let Some(i) = self.lost.iter().position(|&(a, b)| (a..=b).contains(&n)) else { return };
        let (a, b) = self.lost.remove(i);
        let parts = [(a, n.saturating_sub(1)), (n + 1, b)];
        for (j, (x, y)) in parts.into_iter().filter(|(x, y)| x <= y).enumerate() {
            self.lost.insert(i + j, (x, y));
        }
    }

    fn lost_count(&self) -> u64 {
        self.lost.iter().map(|(a, b)| b - a + 1).sum()
    }

    /// The first `k` lost numbers.
    fn lost_numbers(&self, k: usize) -> Vec<u64> {
        self.lost.iter().flat_map(|&(a, b)| a..=b).take(k).collect()
    }
}

/// A line in this device's own mirrored log for an own revision that was
/// gone from the storage when the mirror was restored: it keeps the
/// numbering. The folder transport skips it as a damaged line.
#[derive(Debug, Serialize, Deserialize)]
struct LostLine {
    lost: u64,
}

/// What a fetched object turned out to be.
enum Got {
    Missing,
    Bad,
    /// Sealed under a key epoch this device doesn't have yet: fetched again later.
    NoKey,
    Rev(Revision),
}

/// The first 8 digits of a device id, for messages.
fn short(d: &DeviceId) -> &str {
    d.0.get(..8).unwrap_or(&d.0)
}

/// The object numbers listed in a device's folder.
fn numbers(entries: &[Entry]) -> HashMap<u64, Option<u64>> {
    entries
        .iter()
        .filter(|e| !e.dir)
        .filter_map(|e| {
            let n = e.name.strip_suffix(".rev").filter(|n| n.len() == 12 && n.bytes().all(|b| b.is_ascii_digit()))?;
            Some((n.parse().ok()?, e.size))
        })
        .collect()
}

#[derive(Debug, Default)]
struct Mirror {
    started: bool,
    /// How far each other device is mirrored.
    fetched: HashMap<DeviceId, Progress>,
    /// The writer (`w`) of each device's tip when its lost revisions were
    /// last tried, so they're tried again when it syncs again.
    tried: HashMap<DeviceId, String>,
    /// Devices whose revision `n` a tip announces but isn't there, since when.
    stuck: HashMap<DeviceId, (u64, Instant)>,
    /// The pass stopped at its byte budget: more to fetch now.
    more: bool,
    /// The tips' `ETag`s when last read in full.
    seen: HashMap<String, String>,
    /// Revisions in this device's own log here.
    own: u64,
    /// Of those, uploaded (the tip says so, or will).
    uploaded: u64,
    /// The tip this process last wrote.
    tip_written: Option<u64>,
    /// Own revisions not yet uploaded, by number.
    outbox: Vec<(u64, Revision)>,
    /// The vault's minimum reader, read once per session.
    level: u32,
    /// Snapshots mirrored here (0.13).
    snaps: HashSet<String>,
    /// The snapshot and key epoch this device's tip last said.
    tip_meta: (String, u32),
    /// A tip names a key epoch newer than this device has: read the key records.
    keys_news: bool,
    /// Old objects were tidied away this session (or couldn't be).
    tidied: bool,
    /// Why old objects couldn't be deleted.
    tidy_note: Option<String>,
    /// Revisions sealed under a key this device doesn't have yet.
    no_key: bool,
    problem: Option<String>,
    /// Why the outbox couldn't be uploaded, last time.
    upload_error: Option<String>,
}

/// A vault in object storage, seen from one device.
#[derive(Debug)]
pub struct RemoteTransport {
    store: Arc<dyn ObjectStore>,
    space: SpaceId,
    device: DeviceId,
    /// `<mirror>/<space>/`: the folder layout the [`FolderTransport`] reads.
    mirror: PathBuf,
    folder: FolderTransport,
    state: Mutex<Mirror>,
    /// Who wrote a tip: this process (random), to notice another computer.
    nonce: String,
    conditional: AtomicBool,
    /// The vault's keys: every fetched revision is decrypted before it's
    /// mirrored, so a damaged one is skipped instead of stopping the engine.
    keys: Mutex<Option<KeySet>>,
    _temp: Option<TempDir>,
}

fn io_err(what: &str, e: &std::io::Error) -> TransportError {
    TransportError::Refused(format!("{what}: {e}"))
}

fn shared_identity() -> String {
    "Another computer is syncing as this one (a copy of this vault's .quire folder?). Turn sync off and on again on one of them.".into()
}

/// The lines of a device's mirrored logs, in order, with their object
/// numbers: a revision, or `None` for a [`LostLine`]. A line's number is
/// its `seq` (0.13 mirrors number every line), else one more than the
/// line before. Torn or damaged lines are skipped, as the folder transport
/// skips them.
fn read_logs(dir: &Path) -> Vec<(u64, Option<Revision>)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    files.sort();
    let mut out = Vec::new();
    let mut n = 0;
    for f in files {
        let Ok(bytes) = fs::read(&f) else { continue };
        for line in bytes.split(|&b| b == b'\n') {
            if let Ok(r) = serde_json::from_slice::<Revision>(line) {
                n = if r.seq > 0 { r.seq } else { n + 1 };
                out.push((n, Some(r)));
            } else if let Ok(l) = serde_json::from_slice::<LostLine>(line) {
                n = l.lost.max(n + 1);
                out.push((n, None));
            }
        }
    }
    out
}

/// A mirrored line for object `n`.
fn mirror_line(r: &Revision, n: u64) -> Vec<u8> {
    serde_json::to_vec(&Revision { seq: n, ..r.clone() }).unwrap_or_default()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_data()?;
    fs::rename(&tmp, path)
}

/// Append whole lines to a device's mirrored log, starting a fresh line
/// after a torn one, and a new segment past 1 MiB. If the write fails, the
/// file is cut back to where it was, so no part of the batch stays.
fn append_lines(dir: &Path, revs: &[(u64, Revision)]) -> Result<(), TransportError> {
    let lines: Vec<Vec<u8>> = revs.iter().map(|(n, r)| mirror_line(r, *n)).collect();
    append_raw(dir, &lines)
}

fn append_raw(dir: &Path, lines: &[Vec<u8>]) -> Result<(), TransportError> {
    if lines.is_empty() {
        return Ok(());
    }
    let werr = |e: std::io::Error| io_err("can't write the sync mirror", &e);
    fs::create_dir_all(dir).map_err(werr)?;
    let last = fs::read_dir(dir)
        .map_err(werr)?
        .flatten()
        .filter_map(|e| e.file_name().to_str().filter(|n| crate::folder::is_segment_name(n)).and_then(|n| n[..6].parse::<u32>().ok()))
        .max();
    let path = |n: u32| dir.join(format!("{n:06}.jsonl"));
    let seg = match last {
        Some(n) if fs::metadata(path(n)).map_or(0, |m| m.len()) < crate::folder::SEGMENT_LIMIT => path(n),
        Some(n) => path(n + 1),
        None => path(1),
    };
    let mut f = OpenOptions::new().create(true).read(true).append(true).open(&seg).map_err(werr)?;
    let len = f.metadata().map_err(werr)?.len();
    let mut buf = Vec::new();
    if len > 0 {
        let mut last = [0u8];
        f.seek(SeekFrom::Start(len - 1)).map_err(werr)?;
        f.read_exact(&mut last).map_err(werr)?;
        if last[0] != b'\n' {
            buf.push(b'\n');
        }
    }
    for l in lines {
        buf.extend_from_slice(l);
        buf.push(b'\n');
    }
    if let Err(e) = f.write_all(&buf).and_then(|()| f.sync_data()) {
        let _ = f.set_len(len);
        return Err(werr(e));
    }
    Ok(())
}

/// Run `f` over `items` on up to [`PARALLEL`] threads; results in order.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 {
        return items.iter().map(&f).collect();
    }
    let chunk = items.len().div_ceil(PARALLEL);
    std::thread::scope(|s| {
        let handles: Vec<_> = items.chunks(chunk).map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<R>>())).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
    })
}

impl RemoteTransport {
    /// `state_dir` is a private folder on this device (the vault's
    /// `.quire/sync/remote`) for the mirror and a counter; without one the
    /// mirror is a temporary folder, fetched again each time.
    pub fn new(store: Arc<dyn ObjectStore>, space: &SpaceId, device: DeviceId, state_dir: Option<&Path>) -> Result<Self, TransportError> {
        let (root, temp) = if let Some(d) = state_dir {
            (d.join("mirror"), None)
        } else {
            let id = crate::crypto::hex(&crate::crypto::random_bytes::<8>().map_err(|e| TransportError::Refused(e.to_string()))?);
            let t = std::env::temp_dir().join(format!("quire-remote-{id}"));
            (t.clone(), Some(TempDir(t)))
        };
        let mirror = root.join(&space.0);
        fs::create_dir_all(mirror.join("devices")).map_err(|e| io_err("can't make the sync mirror", &e))?;
        let folder = FolderTransport::new(&root, space, device.clone(), state_dir);
        let nonce = crate::crypto::hex(&crate::crypto::random_bytes::<8>().map_err(|e| TransportError::Refused(e.to_string()))?);
        Ok(Self {
            store,
            space: space.clone(),
            device,
            mirror,
            folder,
            state: Mutex::new(Mirror::default()),
            nonce,
            conditional: AtomicBool::new(true),
            keys: Mutex::new(None),
            _temp: temp,
        })
    }

    /// Decrypt every revision fetched with the vault's keys before it's
    /// mirrored: one that doesn't is skipped as damaged, so it can't stop
    /// the engine. Revisions are sealed with their header bound anyway
    /// ([`Transport::binds_header`]); without keys only their shape is checked.
    #[must_use]
    pub fn with_keys(self, keys: impl Into<KeySet>) -> Self {
        let keys = keys.into();
        self.folder.set_keys(keys.clone());
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = Some(keys);
        self
    }

    /// For tests: snapshots settle after `secs` instead of a week.
    #[doc(hidden)]
    #[must_use]
    pub fn with_settle(mut self, secs: i64) -> Self {
        self.folder.set_settle(secs);
        self
    }

    /// The vault's key changed (a device was removed): use these from now on.
    pub fn set_keys(&self, keys: KeySet) {
        self.folder.set_keys(keys.clone());
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = Some(keys);
        let mut m = self.lock();
        if m.no_key {
            // Objects that waited for the key are fetched again.
            m.no_key = false;
            m.seen.clear();
        }
    }

    fn keyset(&self) -> Option<KeySet> {
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Where the mirror of `space` is, under a state folder (for reading
    /// history while the worker syncs: a [`FolderTransport`] over it).
    pub fn mirror_root(state_dir: &Path) -> PathBuf {
        state_dir.join("mirror")
    }

    fn lock(&self) -> MutexGuard<'_, Mirror> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn device_dir(&self, d: &DeviceId) -> PathBuf {
        self.mirror.join("devices").join(&d.0)
    }

    pub fn space(&self) -> &SpaceId {
        &self.space
    }

    pub fn store(&self) -> &dyn ObjectStore {
        &*self.store
    }

    /// Another computer is writing as this device; sync stops.
    pub fn problem(&self) -> Option<String> {
        self.lock().problem.clone().or_else(|| self.folder.problem())
    }

    /// Other devices' revisions that aren't all here, and why: still
    /// uploading, missing for a while, or skipped as missing or damaged
    /// (naming the device and the revision numbers).
    pub fn waiting(&self) -> Option<String> {
        let m = self.lock();
        let now = Instant::now();
        let mut out = Vec::new();
        let mut stuck: Vec<(&DeviceId, &(u64, Instant))> = m.stuck.iter().collect();
        stuck.sort_by_key(|(d, _)| *d);
        let slow = stuck.iter().filter(|(_, (_, since))| now.duration_since(*since) < MISSING_AFTER).count();
        if slow > 0 {
            out.push(format!("Waiting for {slow} device(s)' latest changes to finish uploading to the storage."));
        }
        for (d, (n, _)) in stuck.iter().filter(|(_, (_, since))| now.duration_since(*since) >= MISSING_AFTER) {
            out.push(format!(
                "Revision {n} of device {} is missing from the storage; that device's later changes arrive when it next syncs and uploads it again.",
                short(d)
            ));
        }
        let mut lost: Vec<(&DeviceId, &Progress)> = m.fetched.iter().filter(|(_, p)| !p.lost.is_empty()).collect();
        lost.sort_by_key(|(d, _)| *d);
        if !lost.is_empty() {
            let total: u64 = lost.iter().map(|(_, p)| p.lost_count()).sum();
            let which: Vec<String> = lost
                .iter()
                .take(3)
                .map(|(d, p)| {
                    let nums: Vec<String> = p.lost_numbers(3).iter().map(u64::to_string).collect();
                    let more = if p.lost_count() > 3 { ", …" } else { "" };
                    format!("device {} revision {}{more}", short(d), nums.join(", "))
                })
                .collect();
            out.push(format!(
                "Skipped {total} revision(s) missing or damaged in the storage ({}); later changes are synced, and the skipped ones are fetched again when their device next syncs.",
                which.join("; ")
            ));
        }
        if m.more {
            out.push("Still downloading other devices' changes.".into());
        }
        if m.no_key {
            out.push("Some changes use the vault's new key, which this device doesn't have yet.".into());
        }
        if let Some(n) = &m.tidy_note {
            out.push(n.clone());
        }
        (!out.is_empty()).then(|| out.join(" "))
    }

    /// Revisions made here and not uploaded yet; `None` before the mirror
    /// was first read (offline from the start).
    pub fn unsent(&self) -> Option<usize> {
        let m = self.lock();
        m.started.then_some(m.outbox.len())
    }

    /// A pass stopped at its byte budget with more to fetch: run another soon.
    pub fn more_to_fetch(&self) -> bool {
        self.lock().more
    }

    /// Changes made here that aren't uploaded yet, and why, if they should be by now.
    pub fn pending(&self) -> Option<String> {
        let m = self.lock();
        let n = m.outbox.len();
        m.upload_error.as_ref().filter(|_| n > 0).map(|e| format!("{n} change(s) saved here, not uploaded yet: {e}"))
    }

    fn progress_path(&self) -> PathBuf {
        self.mirror.join("progress.json")
    }

    fn save_progress(&self, m: &Mirror) -> Result<(), TransportError> {
        let map: std::collections::BTreeMap<&str, &Progress> = m.fetched.iter().map(|(d, p)| (d.0.as_str(), p)).collect();
        let json = serde_json::to_vec(&map).unwrap_or_default();
        write_atomically(&self.progress_path(), &json).map_err(|e| io_err("can't write the sync mirror", &e))
    }

    /// Read the counts and the outbox from the mirror; restore this
    /// device's own revisions from the storage if the mirror lost them, and
    /// put back any of its objects the storage lost.
    fn start(&self, m: &mut Mirror) -> Result<(), TransportError> {
        if m.started {
            return Ok(());
        }
        let saved: HashMap<String, Progress> =
            fs::read(self.progress_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for (name, p) in saved {
            if is_id(&name) && name != self.device.0 {
                m.fetched.insert(DeviceId(name), p);
            }
        }
        let devices = fs::read_dir(self.mirror.join("devices")).map_err(|e| io_err("can't read the sync mirror", &e))?;
        for d in devices.flatten() {
            let Some(name) = d.file_name().to_str().filter(|n| is_id(n)).map(str::to_string) else { continue };
            let id = DeviceId(name);
            if id != self.device && !m.fetched.contains_key(&id) {
                // A mirror from before `progress.json`: one line per object.
                m.fetched.insert(id, Progress { done: read_logs(&d.path()).last().map_or(0, |(n, _)| *n), lost: Vec::new() });
            }
        }
        let mut own = read_logs(&self.device_dir(&self.device));
        if own.iter().any(|(_, r)| r.as_ref().is_some_and(|r| r.seq == 0)) {
            // A mirror from before 0.13: number this device's lines once.
            self.rewrite_own(0)?;
            own = read_logs(&self.device_dir(&self.device));
        }
        m.own = own.last().map_or(0, |(n, _)| *n);
        m.level = read_space(&*self.store, &self.space).map_or(FORMAT, |f| f.min_reader);
        self.folder.set_level(m.level);
        let tip = self.read_tip(&self.device)?.map_or(0, |t| t.n);
        // This device's objects, listed once per session.
        let listed = if tip > 0 {
            match self.store.list(&format!("{}devices/{}/", space_dir(&self.space), self.device)) {
                Ok(l) => Some(numbers(&l)),
                Err(StoreError::Offline(e)) => return Err(TransportError::Offline(e)),
                // Too many to list, or refused: no repair this session.
                Err(_) => None,
            }
        } else {
            None
        };
        if tip > m.own {
            // The mirror lost revisions this device uploaded: fetch them back
            // before the folder transport first reads its own log.
            self.restore(m, tip, listed.as_ref())?;
            own = read_logs(&self.device_dir(&self.device));
            m.own = own.last().map_or(0, |(n, _)| *n).max(m.own);
        }
        m.uploaded = tip;
        m.outbox = own.iter().filter(|(n, _)| *n > tip).filter_map(|(n, r)| r.clone().map(|r| (*n, r))).collect();
        if let Some(l) = &listed {
            self.repair(m, &own, tip, l)?;
        }
        m.started = true;
        Ok(())
    }

    fn read_tip(&self, d: &DeviceId) -> Result<Option<Tip>, TransportError> {
        Ok(self.store.get(&tip_key(&self.space, d), MAX_SMALL)?.and_then(|b| serde_json::from_slice::<Tip>(&b).ok()))
    }

    /// What fetched bytes are: a revision of device `d` (and, with the
    /// vault's keys, one that decrypts), or damaged.
    fn check(&self, bytes: &[u8], d: &DeviceId) -> Got {
        let Some(r) = decode_object(bytes).filter(|r| r.device == *d) else { return Got::Bad };
        match self.keyset().map(|k| Content::open_set(&k, &self.space, &r, true)) {
            None | Some(Ok(_)) => Got::Rev(r),
            Some(Err(CryptoError::UnknownEpoch(_))) => Got::NoKey,
            Some(Err(_)) => Got::Bad,
        }
    }

    /// Objects `nums` of device `d`, in order. An object the storage won't
    /// give (too big) counts as damaged; being offline is an error.
    fn get_many(&self, d: &DeviceId, nums: &[u64], budget: &mut u64) -> Result<Vec<Got>, TransportError> {
        let got = parallel(nums, |n| self.store.get(&object_key(&self.space, d, *n), MAX_OBJECT));
        let mut out = Vec::with_capacity(got.len());
        for r in got {
            out.push(match r {
                Ok(None) => Got::Missing,
                Ok(Some(bytes)) => {
                    *budget = budget.saturating_sub(bytes.len() as u64);
                    self.check(&bytes, d)
                }
                Err(StoreError::Refused(_)) => Got::Bad,
                Err(e) => return Err(e.into()),
            });
        }
        Ok(out)
    }

    /// Fetch this device's own objects `m.own+1 ..= tip` back into its
    /// mirrored log. One the storage no longer has (the listing says so) or
    /// that is damaged gets a [`LostLine`], so the numbering holds. A tip far
    /// beyond the objects there is refused rather than followed.
    fn restore(&self, m: &mut Mirror, tip: u64, listed: Option<&HashMap<u64, Option<u64>>>) -> Result<(), TransportError> {
        if let Some(l) = listed {
            let there = l.keys().filter(|&&n| n > m.own && n <= tip).count() as u64;
            if tip - m.own > there + RESTORE_SLACK {
                return Err(TransportError::Refused(format!(
                    "This device's tip in the storage says it uploaded {tip} revisions, but the storage holds only {there} of the {} missing here. Turn sync off and on again on this device.",
                    tip - m.own
                )));
            }
        }
        let dir = self.device_dir(&self.device);
        // Objects before the first one there were compacted away (0.13):
        // numbering goes on after them.
        if let Some(first) = listed.and_then(|l| l.keys().copied().filter(|&n| n > m.own).min())
            && first > m.own + 1
        {
            append_raw(&dir, &[serde_json::to_vec(&LostLine { lost: first - 1 }).unwrap_or_default()])?;
            m.own = first - 1;
        }
        let mut budget = u64::MAX;
        while m.own < tip {
            let nums: Vec<u64> = (m.own + 1..=tip.min(m.own + BATCH)).collect();
            let got = self.get_many(&self.device, &nums, &mut budget)?;
            let mut lines = Vec::new();
            let mut stopped = false;
            for (n, g) in nums.iter().zip(got) {
                match g {
                    Got::Rev(r) => lines.push(mirror_line(&r, *n)),
                    // Without a listing, a gap may be one still arriving.
                    _ if listed.is_none() => {
                        stopped = true;
                        break;
                    }
                    _ => lines.push(serde_json::to_vec(&LostLine { lost: *n }).unwrap_or_default()),
                }
            }
            append_raw(&dir, &lines)?;
            m.own += lines.len() as u64;
            if stopped {
                return Err(TransportError::Offline("this device's own revisions aren't all readable in the storage yet".into()));
            }
        }
        Ok(())
    }

    /// Put back this device's objects `1..=upto` that the listing shows
    /// missing or of the wrong size, from its mirrored log.
    fn repair(
        &self,
        m: &mut Mirror,
        own: &[(u64, Option<Revision>)],
        upto: u64,
        listed: &HashMap<u64, Option<u64>>,
    ) -> Result<(), TransportError> {
        let mut todo = Vec::new();
        for (n, r) in own.iter().filter(|(n, _)| *n <= upto) {
            let (n, Some(r)) = (*n, r) else { continue };
            let body = encode_object(r);
            match listed.get(&n) {
                None => todo.push((n, r, body, false)),
                Some(Some(size)) if *size != body.len() as u64 => todo.push((n, r, body, true)),
                Some(_) => {}
            }
        }
        let res = parallel(&todo, |(n, r, body, there)| self.place(&object_key(&self.space, &self.device, *n), r, body, *there));
        for r in res {
            match r {
                Ok(true) => {}
                Ok(false) => m.problem = Some(shared_identity()),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Make sure this device's object `key` holds `body` (revision `r`).
    /// Bytes cut short or damaged, or another copy of the same revision, are
    /// written over: it's this device's own object. A different revision
    /// there means another computer writes as this device: `false`.
    fn place(&self, key: &str, r: &Revision, body: &[u8], known_there: bool) -> Result<bool, StoreError> {
        if !known_there && put_new(&*self.store, key, body, &self.conditional)? == Put::Done {
            return Ok(true);
        }
        let existing = match self.store.get(key, MAX_OBJECT) {
            Ok(b) => b,
            // Too big to be one of Quire's: damaged.
            Err(StoreError::Refused(_)) => Some(Vec::new()),
            Err(e) => return Err(e),
        };
        match existing {
            Some(b) if b == body => Ok(true),
            Some(b) if decode_object(&b).is_some_and(|o| o.rev != r.rev) => Ok(false),
            _ => self.store.put(key, body, false).map(|_| true),
        }
    }

    /// Fetch device `d`'s objects after those mirrored, up to `upto`, writing
    /// each batch to the mirror as it arrives. An object that's missing or
    /// damaged is skipped (and recorded) once a later one is there; at a gap
    /// with nothing after it, fetching stops ("waiting"). `true` if it
    /// stopped at this pass's limits instead.
    fn fetch_into(&self, m: &mut Mirror, d: &DeviceId, upto: u64, budget: &mut u64) -> Result<bool, TransportError> {
        let first = m.fetched.get(d).map_or(0, |p| p.done);
        let cap = upto.min(first.saturating_add(MAX_FETCH_PER_PASS));
        let dir = self.device_dir(d);
        let mut listed: Option<HashMap<u64, Option<u64>>> = None;
        loop {
            let done = m.fetched.get(d).map_or(0, |p| p.done);
            if done >= cap {
                return Ok(done < upto);
            }
            if *budget == 0 {
                return Ok(true);
            }
            let mut nums: Vec<u64> = (done + 1..=cap.min(done + BATCH)).collect();
            let mut got = self.get_many(d, &nums, budget)?;
            // Sealed under a key this device doesn't have yet: stop before it.
            if let Some(i) = got.iter().position(|g| matches!(g, Got::NoKey)) {
                m.no_key = true;
                got.truncate(i);
                nums.truncate(i);
                if nums.is_empty() {
                    return Ok(false);
                }
            }
            let cut = if let Some(i) = got.iter().rposition(|g| !matches!(g, Got::Missing)) {
                nums[i]
            } else {
                // A gap a whole batch long: is anything after it? One
                // listing, only in this case.
                if listed.is_none() {
                    listed = Some(numbers(&self.store.list(&format!("{}devices/{d}/", space_dir(&self.space)))?));
                }
                match listed.iter().flat_map(HashMap::keys).copied().filter(|&k| k > done + 1 && k <= cap).min() {
                    Some(k) => k - 1,
                    None => return Ok(false),
                }
            };
            let mut revs = Vec::new();
            let mut lost = Vec::new();
            for (n, g) in nums.iter().zip(got) {
                if *n > cut {
                    break;
                }
                match g {
                    Got::Rev(r) => revs.push((*n, r)),
                    Got::Missing | Got::Bad | Got::NoKey => lost.push(*n),
                }
            }
            append_lines(&dir, &revs)?;
            let p = m.fetched.entry(d.clone()).or_default();
            for n in lost {
                p.lose(n, n);
            }
            p.lose(nums.last().map_or(done, |l| *l) + 1, cut);
            p.done = cut;
            self.save_progress(m)?;
        }
    }

    /// Try device `d`'s skipped revisions again, [`RETRY_LOST`] at a time
    /// while they keep turning up.
    fn retry_lost(&self, m: &mut Mirror, d: &DeviceId, budget: &mut u64) -> Result<(), TransportError> {
        loop {
            let nums = m.fetched.get(d).map(|p| p.lost_numbers(RETRY_LOST)).unwrap_or_default();
            if nums.is_empty() {
                return Ok(());
            }
            let got = self.get_many(d, &nums, budget)?;
            let mut revs = Vec::new();
            let mut found = Vec::new();
            for (n, g) in nums.iter().zip(got) {
                if let Got::Rev(r) = g {
                    revs.push((*n, r));
                    found.push(*n);
                }
            }
            if found.is_empty() {
                return Ok(());
            }
            append_lines(&self.device_dir(d), &revs)?;
            if let Some(p) = m.fetched.get_mut(d) {
                for n in &found {
                    p.found(*n);
                }
            }
            self.save_progress(m)?;
            if found.len() < nums.len() {
                return Ok(());
            }
        }
    }

    /// Mirror what other devices uploaded since last time.
    fn download(&self, m: &mut Mirror) -> Result<(), TransportError> {
        let tips = self.store.list(&format!("{}tips/", space_dir(&self.space)))?;
        let trust_etags = self.store.etag_tracks_content();
        let mut budget = PASS_BYTES;
        let mut devices = 0;
        m.more = false;
        for e in tips.into_iter().filter(|e| !e.dir && is_id(&e.name)) {
            devices += 1;
            if devices > MAX_DEVICES {
                break;
            }
            if trust_etags && !e.etag.is_empty() && m.seen.get(&e.name) == Some(&e.etag) {
                continue;
            }
            let d = DeviceId(e.name.clone());
            let Some(tip) = self.read_tip(&d)? else { continue };
            // 0.13: a snapshot or a key change another device made.
            if !tip.s.is_empty()
                && !m.snaps.contains(&tip.s)
                && crate::compact::parse_name(&tip.s).is_some()
                && self.fetch_snapshot(&tip.s)?
            {
                m.snaps.insert(tip.s.clone());
            }
            if tip.k > self.keyset().map_or(0, |k| k.current()) {
                m.keys_news = true;
            }
            if d == self.device {
                // Ours: unless this process wrote it, someone else writes as us.
                if tip.w != self.nonce && (m.tip_written.is_some() || tip.n != m.uploaded) {
                    m.problem = Some(shared_identity());
                }
                m.seen.insert(e.name, e.etag);
                continue;
            }
            // That device synced again (a new writer): its skipped revisions
            // may be back.
            if m.tried.get(&d) != Some(&tip.w) {
                self.retry_lost(m, &d, &mut budget)?;
                m.tried.insert(d.clone(), tip.w.clone());
            }
            let done = m.fetched.get(&d).map_or(0, |p| p.done);
            if tip.n > done {
                let limited = self.fetch_into(m, &d, tip.n, &mut budget)?;
                let now = m.fetched.get(&d).map_or(0, |p| p.done);
                if now < tip.n {
                    if limited {
                        m.more = true;
                        m.stuck.remove(&d);
                    } else if m.stuck.get(&d).is_none_or(|(n, _)| *n != now + 1) {
                        m.stuck.insert(d, (now + 1, Instant::now()));
                    }
                    continue;
                }
            }
            m.stuck.remove(&d);
            m.seen.insert(e.name, e.etag);
        }
        Ok(())
    }

    /// Upload the outbox, then the tip.
    fn upload(&self, m: &mut Mirror) -> Result<(), TransportError> {
        if let Some(p) = &m.problem {
            return Err(TransportError::Refused(p.clone()));
        }
        let res = parallel(&m.outbox, |(n, r)| {
            // Uploaded before the tip was (a crash, a lost reply): the same
            // bytes are fine, bytes cut short are replaced; another revision
            // there is another computer.
            self.place(&object_key(&self.space, &self.device, *n), r, &encode_object(r), false)
        });
        let mut done = 0;
        let mut err = None;
        for r in res {
            match r {
                Ok(true) => done += 1,
                Ok(false) => {
                    m.problem = Some(shared_identity());
                    break;
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if done > 0 {
            m.uploaded = m.outbox[done - 1].0;
        }
        m.outbox.drain(..done);
        let meta = (self.folder.generation().map(|(g, _)| g.name()).unwrap_or_default(), self.keyset().map_or(0, |k| k.current()));
        let meta_changed = meta != m.tip_meta && (!meta.0.is_empty() || meta.1 > 0);
        if (m.tip_written != Some(m.uploaded) || meta_changed) && (m.uploaded > 0 || m.tip_written.is_some() || meta_changed) {
            let tip = serde_json::to_vec(&Tip { n: m.uploaded, w: self.nonce.clone(), s: meta.0.clone(), k: meta.1 }).unwrap_or_default();
            match self.store.put(&tip_key(&self.space, &self.device), &tip, false) {
                Ok(_) => {
                    m.tip_written = Some(m.uploaded);
                    m.tip_meta = meta;
                }
                Err(e) => err = err.or(Some(e)),
            }
        }
        if let Some(p) = &m.problem {
            return Err(TransportError::Refused(p.clone()));
        }
        m.upload_error = err.as_ref().map(ToString::to_string);
        err.map_or(Ok(()), |e| Err(e.into()))
    }

    /// Upload what this device pushed and say so in its tip. A pass calls
    /// it after the engine is done; the next pull does too.
    pub fn flush(&self) -> Result<(), TransportError> {
        let mut m = self.lock();
        self.start(&mut m)?;
        self.upload(&mut m)
    }

    fn refresh(&self) -> Result<(), TransportError> {
        let mut m = self.lock();
        self.start(&mut m)?;
        self.download(&mut m)?;
        if self.adopt(&mut m)? {
            // Lanes past the snapshot's cut are fetched again, numbered.
            self.download(&mut m)?;
        }
        self.upload(&mut m)?;
        if !m.tidied {
            m.tidied = true;
            m.tidy_note = self.tidy(&m).err();
        }
        Ok(())
    }

    /// Changes whenever another device uploads (one listing).
    pub fn fingerprint(&self) -> Result<u64, TransportError> {
        fingerprint(&*self.store, &self.space, &self.device)
    }

    /// The vault's minimum reader, as read this session.
    pub fn level(&self) -> u32 {
        let mut m = self.lock();
        if !m.started {
            let _ = self.start(&mut m);
        }
        m.level
    }

    /// Upgrade the vault to 0.13 sync (`docs/design/0.13-sync.md` §3).
    pub fn raise_level(&self, level: u32) -> Result<(), TransportError> {
        raise_level(&*self.store, &self.space, level)?;
        self.lock().level = level;
        self.folder.set_level(level);
        Ok(())
    }

    /// A tip named a key epoch newer than this device's: read the key records.
    pub fn take_key_news(&self) -> bool {
        std::mem::take(&mut self.lock().keys_news)
    }

    /// Mirror snapshot `name` from the storage, checked against its
    /// manifest. `false` if it isn't complete or authentic (yet).
    fn fetch_snapshot(&self, name: &str) -> Result<bool, TransportError> {
        let Some(keys) = self.keyset() else { return Ok(false) };
        let dir = self.mirror.join("snapshots").join(name);
        if dir.join("manifest.json").is_file() {
            return Ok(true);
        }
        let base = format!("{}snapshots/{name}/", space_dir(&self.space));
        let Some(bytes) = self.store.get(&format!("{base}manifest"), MAX_OBJECT)? else { return Ok(false) };
        let Ok(man) = serde_json::from_slice::<Manifest>(&bytes) else { return Ok(false) };
        if man.name() != name || !man.verify(&keys, &self.space, "") {
            return Ok(false);
        }
        let tmp = self.mirror.join("snapshots").join(format!(".{name}.tmp"));
        let werr = |e: std::io::Error| io_err("can't write the sync mirror", &e);
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).map_err(werr)?;
        let names: Vec<&crate::compact::Part> = man.parts.iter().filter(|p| crate::compact::parse_part(&p.name)).collect();
        if names.len() != man.parts.len() {
            return Ok(false);
        }
        let got = parallel(&names, |p| self.store.get(&format!("{base}{}", p.name), MAX_OBJECT));
        for (p, g) in names.iter().zip(got) {
            let Some(b) = g? else { return Ok(false) };
            if b.len() as u64 != p.size || crate::compact::sha256_hex(&b) != p.sha256 {
                return Ok(false);
            }
            write_atomically(&tmp.join(&p.name), &b).map_err(werr)?;
        }
        write_atomically(&tmp.join("manifest.json"), &bytes).map_err(werr)?;
        let _ = fs::remove_dir_all(&dir);
        fs::rename(&tmp, &dir).map_err(werr)?;
        Ok(true)
    }

    /// Take the snapshot the mirror now uses: other devices' lanes are
    /// mirrored again from its cut, numbered. `true` if anything changed.
    fn adopt(&self, m: &mut Mirror) -> Result<bool, TransportError> {
        let Some((g, _)) = self.folder.generation() else { return Ok(false) };
        let name = g.name();
        let path = self.mirror.join("adopted");
        if fs::read_to_string(&path).ok().as_deref() == Some(name.as_str()) {
            m.snaps.insert(name);
            return Ok(false);
        }
        for (d, c) in &g.cut {
            if c.n == 0 || !is_id(&d.0) {
                continue;
            }
            if *d == self.device {
                // Its own lines go only once the snapshot has settled
                // (`tidy`): another one may still win, with another cut.
                continue;
            }
            let _ = fs::remove_dir_all(self.device_dir(d));
            let p = m.fetched.entry(d.clone()).or_default();
            p.done = c.n;
            p.lost = p.lost.iter().filter(|(_, b)| *b > c.n).map(|&(a, b)| (a.max(c.n + 1), b)).collect();
        }
        m.stuck.clear();
        m.seen.clear();
        self.save_progress(m)?;
        write_atomically(&path, name.as_bytes()).map_err(|e| io_err("can't write the sync mirror", &e))?;
        m.snaps.insert(name);
        self.folder.reload();
        Ok(true)
    }

    /// Keep only this device's own mirrored lines after `cut`, numbered.
    fn rewrite_own(&self, cut: u64) -> Result<(), TransportError> {
        let dir = self.device_dir(&self.device);
        let lines = read_logs(&dir);
        let max = lines.last().map_or(0, |(n, _)| *n);
        let mut keep: Vec<Vec<u8>> = lines
            .iter()
            .filter(|(n, _)| *n > cut)
            .map(|(n, r)| match r {
                Some(r) => mirror_line(r, *n),
                None => serde_json::to_vec(&LostLine { lost: *n }).unwrap_or_default(),
            })
            .collect();
        if keep.is_empty() && max > 0 {
            // The numbering goes on from here.
            keep.push(serde_json::to_vec(&LostLine { lost: max }).unwrap_or_default());
        }
        let werr = |e: std::io::Error| io_err("can't write the sync mirror", &e);
        let (tmp, old) = (dir.with_extension("new"), dir.with_extension("old"));
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_dir_all(&old);
        fs::create_dir_all(&tmp).map_err(werr)?;
        append_raw(&tmp, &keep)?;
        if dir.exists() {
            fs::rename(&dir, &old).map_err(werr)?;
        }
        fs::rename(&tmp, &dir).map_err(werr)?;
        let _ = fs::remove_dir_all(&old);
        Ok(())
    }

    /// Once a snapshot has settled: delete this device's own objects up to
    /// its cut, and older snapshots. A key that may not delete (the IAM
    /// policy in docs/sync-backends.md before 0.13) leaves them, and says so.
    fn tidy(&self, m: &Mirror) -> Result<(), String> {
        let Some((g, seen)) = self.folder.generation() else { return Ok(()) };
        if unix_now() - seen < self.folder.settle() {
            return Ok(());
        }
        let cut = g.cut.get(&self.device).map_or(0, |c| c.n).min(m.uploaded);
        if cut > 0 {
            self.rewrite_own(cut).map_err(|e| e.to_string())?;
            self.folder.reload();
        }
        let cant = |e: StoreError| format!("Old revisions stay in the storage: this device's key can't delete them ({e}).");
        if cut > 0 {
            let listed = self.store.list(&format!("{}devices/{}/", space_dir(&self.space), self.device)).map_err(|e| e.to_string())?;
            let old: Vec<u64> = numbers(&listed).keys().copied().filter(|&n| n <= cut).collect();
            for r in parallel(&old, |n| self.store.delete(&object_key(&self.space, &self.device, *n))) {
                match r {
                    Ok(()) => {}
                    Err(e @ (StoreError::Auth(_) | StoreError::Refused(_))) => return Err(cant(e)),
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        let snaps = self.store.list(&format!("{}snapshots/", space_dir(&self.space))).map_err(|e| e.to_string())?;
        for e in snaps.iter().filter(|e| e.dir) {
            let older =
                crate::compact::parse_name(&e.name).is_some_and(|(n, _)| n < g.generation || (n == g.generation && e.name != g.name()));
            if !older {
                continue;
            }
            let base = format!("{}snapshots/{}/", space_dir(&self.space), e.name);
            for f in self.store.list(&base).unwrap_or_default().iter().filter(|f| !f.dir) {
                if f.name == "manifest" || crate::compact::parse_part(&f.name) {
                    let _ = self.store.delete(&format!("{base}{}", f.name));
                }
            }
            let _ = fs::remove_dir_all(self.mirror.join("snapshots").join(&e.name));
        }
        Ok(())
    }

    /// Fold old revisions into a snapshot in the storage (see
    /// [`FolderTransport::compact`]).
    pub fn compact(&self, policy: Policy, force: bool) -> Result<Compacted, TransportError> {
        self.refresh()?;
        let mut m = self.lock();
        if m.level < LEVEL_HARDENED {
            return Ok(Compacted::NotUpgraded);
        }
        let mut cut: std::collections::BTreeMap<DeviceId, LanePos> =
            m.fetched.iter().filter(|(_, p)| p.done > 0).map(|(d, p)| (d.clone(), LanePos { n: p.done, ..LanePos::default() })).collect();
        if m.uploaded > 0 {
            cut.insert(self.device.clone(), LanePos { n: m.uploaded, ..LanePos::default() });
        }
        let res = self.folder.compact(policy, Some(cut), force)?;
        let Compacted::Done(man) = &res else { return Ok(res) };
        let name = man.name();
        let local = self.mirror.join("snapshots").join(&name);
        // Another device made one meanwhile: let it settle.
        let there = self.store.list(&format!("{}snapshots/", space_dir(&self.space)))?;
        if there.iter().any(|e| e.dir && crate::compact::parse_name(&e.name).is_some_and(|(g, _)| g >= man.generation)) {
            let _ = fs::remove_dir_all(&local);
            self.folder.reload();
            return Ok(Compacted::Busy("another device is compacting".into()));
        }
        let base = format!("{}snapshots/{name}/", space_dir(&self.space));
        let parts: Vec<&crate::compact::Part> = man.parts.iter().collect();
        for r in parallel(&parts, |p| {
            let b = fs::read(local.join(&p.name)).map_err(|e| StoreError::Refused(e.to_string()))?;
            self.store.put(&format!("{base}{}", p.name), &b, false)
        }) {
            r?;
        }
        let body = fs::read(local.join("manifest.json")).map_err(|e| io_err("can't read the sync mirror", &e))?;
        put_new(&*self.store, &format!("{base}manifest"), &body, &self.conditional)?;
        m.snaps.insert(name);
        self.adopt(&mut m)?;
        self.upload(&mut m)?;
        Ok(res)
    }

    /// The vault's key records (0.13), cached in the mirror.
    pub fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError> {
        let cache = self.mirror.join("keys");
        let listed = self.store.list(&format!("{}keys/", space_dir(&self.space)))?;
        let mut out = Vec::new();
        for e in listed.iter().filter(|e| !e.dir) {
            let Some(stem) = e.name.strip_suffix(".json").filter(|n| crate::compact::parse_name(n).is_some()) else { continue };
            let file = cache.join(format!("{stem}.json"));
            let bytes = if let Ok(b) = fs::read(&file) {
                b
            } else {
                let Some(b) = self.store.get(&format!("{}keys/{stem}.json", space_dir(&self.space)), MAX_SMALL)? else { continue };
                let _ = fs::create_dir_all(&cache);
                let _ = write_atomically(&file, &b);
                b
            };
            if let Ok(r) = serde_json::from_slice::<EpochRecord>(&bytes)
                && r.space == self.space
            {
                out.push(r);
            }
        }
        out.sort_by(|a, b| (a.epoch, &a.maker).cmp(&(b.epoch, &b.maker)));
        Ok(out)
    }

    /// Write a key record (once; never changed).
    pub fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        let key = format!("{}keys/{}.json", space_dir(&self.space), crate::compact::name(r.epoch, &r.maker));
        match put_new(&*self.store, &key, &serde_json::to_vec_pretty(r).unwrap_or_default(), &self.conditional)? {
            Put::Done => {
                let mut m = self.lock();
                m.tip_meta = (String::new(), 0);
                Ok(())
            }
            Put::Exists => Err(TransportError::Refused("that key record is already in the storage".into())),
        }
    }

    /// Every device's record (0.13 devices publish one).
    pub fn device_records(&self) -> Result<Vec<DeviceRecord>, TransportError> {
        let listed = self.store.list(&format!("{}devices/", space_dir(&self.space)))?;
        let ids: Vec<String> = listed.into_iter().filter(|e| e.dir && is_id(&e.name)).map(|e| e.name).take(MAX_DEVICES).collect();
        let got = parallel(&ids, |d| self.store.get(&format!("{}devices/{d}/device.json", space_dir(&self.space)), MAX_SMALL));
        let mut out = Vec::new();
        for (d, g) in ids.iter().zip(got) {
            if let Some(r) = g?.and_then(|b| serde_json::from_slice::<DeviceRecord>(&b).ok()).filter(|r| r.device.0 == *d) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Publish this device's record.
    pub fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        let key = format!("{}devices/{}/device.json", space_dir(&self.space), self.device);
        self.store.put(&key, &serde_json::to_vec_pretty(r).unwrap_or_default(), false)?;
        Ok(())
    }

    /// Devices that have uploaded revisions (they have a tip).
    pub fn writers(&self) -> Result<Vec<DeviceId>, TransportError> {
        let tips = self.store.list(&format!("{}tips/", space_dir(&self.space)))?;
        let mut out: Vec<DeviceId> = tips.into_iter().filter(|e| !e.dir && is_id(&e.name)).map(|e| DeviceId(e.name)).collect();
        out.sort();
        Ok(out)
    }

    /// How far each device's revisions have been fetched here (this
    /// device's: uploaded), as positions.
    pub fn lane_ends(&self) -> Result<std::collections::BTreeMap<DeviceId, LanePos>, TransportError> {
        self.refresh()?;
        let m = self.lock();
        let mut out: std::collections::BTreeMap<DeviceId, LanePos> =
            m.fetched.iter().map(|(d, p)| (d.clone(), LanePos { n: p.done, ..LanePos::default() })).collect();
        out.insert(self.device.clone(), LanePos { n: m.uploaded.max(m.own), ..LanePos::default() });
        Ok(out)
    }

    /// Devices removed from the vault, and the last object still taken from each.
    pub fn set_removed(&self, removed: std::collections::BTreeMap<DeviceId, LanePos>) {
        self.folder.set_removed(removed);
    }

    /// The snapshot in use, and when it was first seen here.
    pub fn generation(&self) -> Option<(Manifest, i64)> {
        self.folder.generation()
    }
}

/// A hash of every other device's tip: changes when one uploads. One
/// listing request on S3; on WebDAV also one small read per device (see
/// [`ObjectStore::etag_tracks_content`]). For the watch thread.
pub fn fingerprint(store: &dyn ObjectStore, space: &SpaceId, device: &DeviceId) -> Result<u64, TransportError> {
    use std::hash::{Hash, Hasher};
    let mut tips = store.list(&format!("{}tips/", space_dir(space)))?;
    tips.retain(|e| !e.dir && is_id(&e.name) && e.name != device.0);
    tips.sort_by(|a, b| a.name.cmp(&b.name));
    tips.truncate(MAX_DEVICES);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let trust = store.etag_tracks_content();
    for e in tips {
        if !trust || e.etag.is_empty() {
            store.get(&format!("{}tips/{}", space_dir(space), e.name), MAX_SMALL)?.hash(&mut h);
        }
        (e.name, e.etag, e.stamp).hash(&mut h);
    }
    Ok(h.finish())
}

impl Transport for RemoteTransport {
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        self.refresh()?;
        self.folder.pull(after)
    }

    /// Appended to the mirror (durable here); uploaded by [`Self::flush`].
    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        let mut m = self.lock();
        self.start(&mut m)?;
        if let Some(p) = &m.problem {
            return Err(TransportError::Refused(p.clone()));
        }
        let out = self.folder.push_numbered(rev, m.own + 1)?;
        if matches!(out, PushOutcome::Accepted { .. }) {
            m.own += 1;
            let n = m.own;
            m.outbox.push((n, Revision { seq: 0, ..rev.clone() }));
        }
        Ok(out)
    }

    fn fork_base(&self, file: &FileId, ours: &RevId, theirs: &Revision) -> Option<Revision> {
        self.folder.fork_base(file, ours, theirs)
    }

    fn fork(&self, file: &FileId, ours: &RevId, theirs: &Revision) -> Fork {
        self.folder.fork(file, ours, theirs)
    }

    fn sealing(&self) -> Sealing {
        if self.level() >= LEVEL_HARDENED { Sealing::Epoch } else { Sealing::Bound }
    }

    /// No Quire before 0.12 reads this storage, so revisions bind their
    /// whole header.
    fn binds_header(&self) -> bool {
        true
    }

    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        self.refresh()?;
        self.folder.history(file)
    }
}

/// An object store in memory, for tests: conditional writes optional,
/// failures on demand.
#[derive(Debug, Default)]
pub struct MemoryObjects {
    objects: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    /// Refuse `If-None-Match` (like B2).
    pub no_conditional: AtomicBool,
    /// Every request fails as offline.
    pub offline: AtomicBool,
    /// Writes fail as offline after this many more succeed (`u64::MAX`: never).
    pub writes_left: std::sync::atomic::AtomicU64,
    version: std::sync::atomic::AtomicU64,
}

impl MemoryObjects {
    pub fn new() -> Self {
        Self { writes_left: std::sync::atomic::AtomicU64::new(u64::MAX), ..Self::default() }
    }

    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect()
    }

    /// Everything stored, for checks of what the storage can see.
    pub fn dump(&self) -> Vec<(String, Vec<u8>)> {
        self.objects.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Change or remove an object behind the client's back.
    pub fn set_raw(&self, key: &str, body: Option<Vec<u8>>) {
        let mut o = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        match body {
            Some(b) => o.insert(key.to_string(), b),
            None => o.remove(key),
        };
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.offline.load(Ordering::Relaxed) { Err(StoreError::Offline("simulated".into())) } else { Ok(()) }
    }
}

impl ObjectStore for MemoryObjects {
    fn get(&self, key: &str, limit: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.check()?;
        let o = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        match o.get(key) {
            Some(b) if b.len() as u64 > limit => Err(StoreError::Refused("too big".into())),
            b => Ok(b.cloned()),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        self.check()?;
        Ok(self.objects.lock().unwrap_or_else(PoisonError::into_inner).contains_key(key))
    }

    fn put(&self, key: &str, body: &[u8], create_only: bool) -> Result<Put, StoreError> {
        self.check()?;
        if create_only && self.no_conditional.load(Ordering::Relaxed) {
            return Err(StoreError::NoConditional);
        }
        // One atomic step: uploads run in parallel, and a separate load and
        // decrement let several through on the last write (and wrapped).
        let took = self.writes_left.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| match left {
            0 => None,
            u64::MAX => Some(u64::MAX),
            n => Some(n - 1),
        });
        if took.is_err() {
            return Err(StoreError::Offline("simulated: connection cut".into()));
        }
        let mut o = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        if create_only && o.contains_key(key) {
            return Ok(Put::Exists);
        }
        o.insert(key.to_string(), body.to_vec());
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(Put::Done)
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        self.check()?;
        let o = self.objects.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<Entry> = Vec::new();
        let mut dirs = HashSet::new();
        for (k, v) in o.range(dir.to_string()..) {
            let Some(rest) = k.strip_prefix(dir) else { break };
            if let Some((d, _)) = rest.split_once('/') {
                if dirs.insert(d.to_string()) {
                    out.push(Entry { name: d.to_string(), dir: true, etag: String::new(), stamp: String::new(), size: None });
                }
            } else {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                v.hash(&mut h);
                out.push(Entry {
                    name: rest.to_string(),
                    dir: false,
                    etag: format!("{:x}", h.finish()),
                    stamp: v.len().to_string(),
                    size: Some(v.len() as u64),
                });
            }
        }
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.check()?;
        self.objects.lock().unwrap_or_else(PoisonError::into_inner).remove(key);
        Ok(())
    }

    fn describe(&self) -> String {
        "memory".into()
    }

    fn etag_tracks_content(&self) -> bool {
        true
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
            sealed: vec![0, 1, 2, 255],
            chain: vec![],
        }
    }

    fn space(store: &MemoryObjects) -> SpaceId {
        let s = SpaceId::random().unwrap();
        create_space(store, &s, &WrappedKey::wrap(&VaultKey::generate().unwrap(), "pass phrase", KdfParams::TEST).unwrap()).unwrap();
        s
    }

    #[test]
    fn objects_round_trip_and_reject_lies() {
        let r = rev(&FileId::random().unwrap(), &[], &DeviceId::random().unwrap());
        let b = encode_object(&r);
        assert_eq!(decode_object(&b), Some(r.clone()));
        assert!(decode_object(&b[1..]).is_none());
        let mut bad = r.clone();
        bad.file = FileId("../../etc".into());
        assert!(decode_object(&encode_object(&bad)).is_none(), "ids must be ids");
        // Hex `sealed` in the head isn't accepted.
        let mut odd = MAGIC.to_vec();
        odd.extend_from_slice(&serde_json::to_vec(&r).unwrap());
        odd.push(b'\n');
        assert!(decode_object(&odd).is_none());
    }

    #[test]
    fn prefixes_and_ids_are_checked() {
        assert_eq!(clean_prefix(" /notes/quire/ ").unwrap(), "notes/quire");
        assert_eq!(clean_prefix("").unwrap(), "");
        for bad in ["a/../b", "..", "a//b", "a\\b", "./a", "a\u{7}b"] {
            assert!(clean_prefix(bad).is_err(), "{bad}");
        }
        assert!(is_id(&DeviceId::random().unwrap().0));
        for bad in ["..", "ABCDEF00112233445566778899aabbcc", "0123456789abcdef0123456789abcde/", ""] {
            assert!(!is_id(bad), "{bad}");
        }
    }

    #[test]
    fn spaces_are_made_once_listed_and_read() {
        let store = MemoryObjects::new();
        assert!(list_spaces(&store).unwrap().is_empty());
        let s = space(&store);
        let key = WrappedKey::wrap(&VaultKey::generate().unwrap(), "pass phrase", KdfParams::TEST).unwrap();
        assert!(create_space(&store, &s, &key).is_err(), "never overwritten");
        store.no_conditional.store(true, Ordering::Relaxed);
        assert!(create_space(&store, &s, &key).is_err(), "nor without conditional writes");
        let listed = list_spaces(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].space, s);
        assert!(wrapped_key(&store, &s).is_ok());
        // A space.json that names another space, or a newer format.
        let other = SpaceId::random().unwrap();
        let raw = store.get(&format!("{ROOT}/{s}/space.json"), MAX_SMALL).unwrap().unwrap();
        store.set_raw(&format!("{ROOT}/{other}/space.json"), Some(raw.clone()));
        assert!(wrapped_key(&store, &other).is_err());
        let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        v["min_reader"] = 9.into();
        store.set_raw(&format!("{ROOT}/{s}/space.json"), Some(serde_json::to_vec(&v).unwrap()));
        assert!(wrapped_key(&store, &s).unwrap_err().to_string().contains("newer Quire"));
    }

    #[test]
    fn probe_tells_conditional_writes_apart() {
        let store = MemoryObjects::new();
        let p = probe(&store).unwrap();
        assert!(p.conditional_writes);
        assert!(store.keys().is_empty(), "the probe cleans up");
        store.no_conditional.store(true, Ordering::Relaxed);
        let p = probe(&store).unwrap();
        assert!(!p.conditional_writes);
        assert!(p.summary().contains("only its own files"));
        store.offline.store(true, Ordering::Relaxed);
        assert!(matches!(probe(&store), Err(StoreError::Offline(_))));
    }

    #[test]
    fn two_devices_see_each_other_and_ties_resolve_alike() {
        let store: Arc<MemoryObjects> = Arc::new(MemoryObjects::new());
        let s = space(&store);
        let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
        let a = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, da.clone(), None).unwrap();
        let b = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, db.clone(), None).unwrap();
        let file = FileId::random().unwrap();
        a.pull(0).unwrap();
        let root = rev(&file, &[], &da);
        assert!(matches!(a.push(&root).unwrap(), PushOutcome::Accepted { .. }));
        assert!(b.pull(0).unwrap().is_empty(), "not uploaded until flushed");
        a.flush().unwrap();
        assert_eq!(b.pull(0).unwrap().len(), 1);
        let (ra, rb) = (rev(&file, &[&root.rev], &da), rev(&file, &[&root.rev], &db));
        a.push(&ra).unwrap();
        b.push(&rb).unwrap();
        a.flush().unwrap();
        b.flush().unwrap();
        let winner = ra.rev.clone().min(rb.rev.clone());
        let ha = a.pull(0).unwrap().into_iter().find(|r| r.file == file).unwrap();
        let hb = b.pull(0).unwrap().into_iter().find(|r| r.file == file).unwrap();
        assert_eq!((&ha.rev, &hb.rev), (&winner, &winner));
        let (loser, lrev) = if winner == ra.rev { (&b, &rb.rev) } else { (&a, &ra.rev) };
        assert_eq!(loser.fork_base(&file, lrev, &ha).map(|r| r.rev), Some(root.rev));
        // Each object is written once, by its device, and named by number.
        let keys = store.keys();
        assert!(keys.iter().any(|k| k.ends_with(&format!("devices/{da}/000000000002.rev"))));
        assert!(keys.iter().any(|k| k.ends_with(&format!("tips/{db}"))));
    }

    #[test]
    fn an_upload_cut_short_resumes_and_a_lost_mirror_is_restored() {
        let store: Arc<MemoryObjects> = Arc::new(MemoryObjects::new());
        let s = space(&store);
        let d = DeviceId::random().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = FileId::random().unwrap();
        let mut revs = vec![rev(&file, &[], &d)];
        {
            let t = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, d.clone(), Some(dir.path())).unwrap();
            t.pull(0).unwrap();
            t.push(&revs[0]).unwrap();
            for _ in 0..5 {
                let r = rev(&file, &[&revs.last().unwrap().rev], &d);
                t.push(&r).unwrap();
                revs.push(r);
            }
            // The connection drops after two uploads.
            store.writes_left.store(2, Ordering::Relaxed);
            assert!(matches!(t.flush(), Err(TransportError::Offline(_))));
            assert!(t.pending().unwrap().contains("not uploaded yet"));
        }
        // A new process picks up where it stopped.
        store.writes_left.store(u64::MAX, Ordering::Relaxed);
        let t = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, d.clone(), Some(dir.path())).unwrap();
        t.pull(0).unwrap();
        assert!(t.pending().is_none());
        drop(t);
        let other = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, DeviceId::random().unwrap(), None).unwrap();
        let got = other.pull(0).unwrap();
        assert_eq!(got.iter().map(|r| r.rev.clone()).collect::<Vec<_>>(), [revs.last().unwrap().rev.clone()]);
        assert_eq!(other.history(&file).unwrap().len(), 6, "every revision arrived, once");
        // The mirror is deleted: this device's revisions come back from the storage.
        fs::remove_dir_all(dir.path().join("mirror")).unwrap();
        let t = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, d.clone(), Some(dir.path())).unwrap();
        assert_eq!(t.pull(0).unwrap()[0].rev, revs.last().unwrap().rev);
        let next = rev(&file, &[&revs.last().unwrap().rev], &d);
        assert!(matches!(t.push(&next).unwrap(), PushOutcome::Accepted { .. }));
        t.flush().unwrap();
        assert!(t.problem().is_none());
        assert!(store.keys().iter().any(|k| k.ends_with(&format!("devices/{d}/000000000007.rev"))));
    }

    #[test]
    fn a_second_computer_writing_as_this_device_is_noticed() {
        for conditional in [true, false] {
            let store: Arc<MemoryObjects> = Arc::new(MemoryObjects::new());
            store.no_conditional.store(!conditional, Ordering::Relaxed);
            let s = space(&store);
            let d = DeviceId::random().unwrap();
            let one = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, d.clone(), None).unwrap();
            let two = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, d.clone(), None).unwrap();
            one.pull(0).unwrap();
            two.pull(0).unwrap();
            one.push(&rev(&FileId::random().unwrap(), &[], &d)).unwrap();
            two.push(&rev(&FileId::random().unwrap(), &[], &d)).unwrap();
            one.flush().unwrap();
            // Two finds its object number taken by different bytes.
            assert!(two.flush().is_err(), "conditional: {conditional}");
            assert!(two.problem().is_some_and(|p| p.contains("Another computer")));
            // One sees a tip it didn't write once two wrote one, or two's objects.
            let _ = one.pull(0);
            assert!(two.push(&rev(&FileId::random().unwrap(), &[], &d)).is_err());
        }
    }

    #[test]
    fn fetched_objects_go_to_the_mirror_a_batch_at_a_time_within_a_budget() {
        let store: Arc<MemoryObjects> = Arc::new(MemoryObjects::new());
        let s = space(&store);
        let db = DeviceId::random().unwrap();
        let b = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, db.clone(), None).unwrap();
        b.pull(0).unwrap();
        for _ in 0..(BATCH * 3) {
            b.push(&rev(&FileId::random().unwrap(), &[], &db)).unwrap();
        }
        b.flush().unwrap();
        let a = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, DeviceId::random().unwrap(), None).unwrap();
        let mut m = a.lock();
        a.start(&mut m).unwrap();
        // A budget smaller than one batch: one batch is fetched, written to
        // the mirror, and the pass stops there with more to come.
        let mut budget = 1;
        assert!(a.fetch_into(&mut m, &db, BATCH * 3, &mut budget).unwrap());
        assert_eq!(m.fetched[&db].done, BATCH);
        assert_eq!(read_logs(&a.device_dir(&db)).len() as u64, BATCH);
        let saved: HashMap<String, Progress> = serde_json::from_slice(&fs::read(a.progress_path()).unwrap()).unwrap();
        assert_eq!(saved[&db.0].done, BATCH);
        drop(m);
        a.pull(0).unwrap();
        assert!(!a.more_to_fetch());
        assert_eq!(a.lock().fetched[&db].done, BATCH * 3);
    }

    #[test]
    fn lost_ranges_split_and_merge() {
        let mut p = Progress::default();
        p.lose(2, 2);
        p.lose(3, 3);
        p.lose(7, 9);
        assert_eq!(p.lost, [(2, 3), (7, 9)]);
        p.found(8);
        assert_eq!(p.lost, [(2, 3), (7, 7), (9, 9)]);
        p.found(2);
        p.found(7);
        assert_eq!((p.lost_count(), p.lost_numbers(5)), (2, vec![3, 9]));
        p.lose(5, 4);
        assert_eq!(p.lost.len(), 2);
    }

    #[test]
    fn hostile_objects_and_names_are_never_used() {
        let store: Arc<MemoryObjects> = Arc::new(MemoryObjects::new());
        let s = space(&store);
        let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
        // Names that aren't ids, in tips/ and devices/.
        store.set_raw(&format!("{ROOT}/{s}/tips/.."), Some(br#"{"n":5}"#.to_vec()));
        store.set_raw(&format!("{ROOT}/{s}/tips/x/y"), Some(br#"{"n":5}"#.to_vec()));
        // A revision in b's folder that says it's from a.
        let liar = rev(&FileId::random().unwrap(), &[], &da);
        store.set_raw(&object_key(&s, &db, 1), Some(encode_object(&liar)));
        store.set_raw(&tip_key(&s, &db), Some(br#"{"n":1,"w":"x"}"#.to_vec()));
        // A tip that promises more than there is.
        let dc = DeviceId::random().unwrap();
        store.set_raw(&tip_key(&s, &dc), Some(br#"{"n":1000000000000,"w":"x"}"#.to_vec()));
        let a = RemoteTransport::new(Arc::<MemoryObjects>::clone(&store), &s, da, None).unwrap();
        assert!(a.pull(0).unwrap().is_empty());
        assert!(a.waiting().is_some());
        let entries: Vec<String> =
            fs::read_dir(a.mirror.join("devices")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(entries.iter().all(|n| is_id(n)), "{entries:?}");
    }
}
