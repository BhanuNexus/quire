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
//! is in.

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

use serde::{Deserialize, Serialize};

use crate::crypto::WrappedKey;
use crate::folder::FolderTransport;
use crate::protocol::{DeviceId, FileId, PushOutcome, RevId, Revision, SpaceId};
use crate::transport::{Transport, TransportError};

pub use s3::{S3Config, S3Credentials, S3Store};
pub use webdav::{WebDavConfig, WebDavCredentials, WebDavStore};
/// Secrets in memory are wiped when dropped.
pub use zeroize::Zeroizing;

/// The folder Quire keeps in the bucket or WebDAV folder.
pub const ROOT: &str = "quire-sync";
/// The format of remote spaces; Quire 0.12 is the first to read it.
pub const FORMAT: u32 = 3;
/// A revision object, at most: a 10 MB attachment is about 14 MB sealed.
pub const MAX_OBJECT: u64 = 64 * 1024 * 1024;
/// `space.json` and tips are small.
const MAX_SMALL: u64 = 256 * 1024;
/// Downloads and uploads at once.
const PARALLEL: usize = 8;
/// Revisions fetched in one pass, at most (the rest come next pass).
const MAX_FETCH_PER_PASS: u64 = 50_000;
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

/// Make a new vault in the storage.
pub fn create_space(store: &dyn ObjectStore, space: &SpaceId, key: &WrappedKey) -> Result<(), TransportError> {
    let file = SpaceFile {
        format: FORMAT,
        space: space.clone(),
        created: unix_now(),
        wrapped_key: key.clone(),
        min_reader: FORMAT,
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
    if f.min_reader > FORMAT {
        return Err(TransportError::Refused("this vault was made by a newer Quire; update Quire on this device".into()));
    }
    Ok(f)
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
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tip {
    n: u64,
    #[serde(default)]
    w: String,
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
    let ids_ok = is_id(&r.file.0) && is_id(&r.rev.0) && is_id(&r.device.0) && r.parents.iter().all(|p| is_id(&p.0)) && r.parents.len() <= 2;
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

#[derive(Debug, Default)]
struct Mirror {
    started: bool,
    /// Revisions mirrored from each other device.
    fetched: HashMap<DeviceId, u64>,
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
    problem: Option<String>,
    /// Devices whose newest revisions haven't arrived in the storage yet.
    waiting: usize,
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
    _temp: Option<TempDir>,
}

fn io_err(what: &str, e: &std::io::Error) -> TransportError {
    TransportError::Refused(format!("{what}: {e}"))
}

fn shared_identity() -> String {
    "Another computer is syncing as this one (a copy of this vault's .quire folder?). Turn sync off and on again on one of them.".into()
}

/// Revision lines in a device's mirrored logs, in order. Torn or damaged
/// lines are skipped, as the folder transport skips them.
fn read_logs(dir: &Path) -> Vec<Revision> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let Ok(bytes) = fs::read(&f) else { continue };
        for line in bytes.split(|&b| b == b'\n') {
            if let Ok(r) = serde_json::from_slice::<Revision>(line) {
                out.push(r);
            }
        }
    }
    out
}

/// Append whole lines to a device's mirrored log, starting a fresh line
/// after a torn one, and a new segment past 1 MiB.
fn append_lines(dir: &Path, revs: &[Revision]) -> Result<(), TransportError> {
    if revs.is_empty() {
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
    for r in revs {
        buf.extend_from_slice(&serde_json::to_vec(&Revision { seq: 0, ..r.clone() }).unwrap_or_default());
        buf.push(b'\n');
    }
    f.write_all(&buf).map_err(werr)?;
    f.sync_data().map_err(werr)
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
            _temp: temp,
        })
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

    pub fn store(&self) -> &dyn ObjectStore {
        &*self.store
    }

    /// Another computer is writing as this device; sync stops.
    pub fn problem(&self) -> Option<String> {
        self.lock().problem.clone().or_else(|| self.folder.problem())
    }

    /// Other devices' revisions that their tips announce but that aren't
    /// readable in the storage yet.
    pub fn waiting(&self) -> Option<String> {
        let n = self.lock().waiting;
        (n > 0).then(|| format!("Waiting for {n} device(s)' latest changes to finish uploading to the storage."))
    }

    /// Changes made here that aren't uploaded yet, and why, if they should be by now.
    pub fn pending(&self) -> Option<String> {
        let m = self.lock();
        let n = m.outbox.len();
        m.upload_error.as_ref().filter(|_| n > 0).map(|e| format!("{n} change(s) saved here, not uploaded yet: {e}"))
    }

    /// Read the counts and the outbox from the mirror; restore this
    /// device's own revisions from the storage if the mirror lost them.
    fn start(&self, m: &mut Mirror) -> Result<(), TransportError> {
        if m.started {
            return Ok(());
        }
        let devices = fs::read_dir(self.mirror.join("devices")).map_err(|e| io_err("can't read the sync mirror", &e))?;
        for d in devices.flatten() {
            let Some(name) = d.file_name().to_str().filter(|n| is_id(n)).map(str::to_string) else { continue };
            let id = DeviceId(name);
            if id != self.device {
                m.fetched.insert(id.clone(), read_logs(&d.path()).len() as u64);
            }
        }
        let own = read_logs(&self.device_dir(&self.device));
        m.own = own.len() as u64;
        let tip = self.read_tip(&self.device)?.map_or(0, |t| t.n);
        // The mirror lost revisions this device uploaded: fetch them back
        // before the folder transport first reads its own log.
        while tip > m.own {
            let got = self.fetch_range(&self.device, m.own, tip)?;
            if got.is_empty() {
                return Err(TransportError::Offline("this device's own revisions aren't all readable in the storage yet".into()));
            }
            append_lines(&self.device_dir(&self.device), &got)?;
            m.own += got.len() as u64;
        }
        m.uploaded = tip;
        m.outbox = own.into_iter().zip(1u64..).filter(|(_, n)| *n > tip).map(|(r, n)| (n, r)).collect();
        m.started = true;
        Ok(())
    }

    fn read_tip(&self, d: &DeviceId) -> Result<Option<Tip>, TransportError> {
        Ok(self.store.get(&tip_key(&self.space, d), MAX_SMALL)?.and_then(|b| serde_json::from_slice::<Tip>(&b).ok()))
    }

    /// Revisions `after+1 ..= upto` of device `d`, in order, up to the
    /// first that isn't there (yet) or isn't valid.
    fn fetch_range(&self, d: &DeviceId, after: u64, upto: u64) -> Result<Vec<Revision>, TransportError> {
        let upto = upto.min(after.saturating_add(MAX_FETCH_PER_PASS));
        let mut out = Vec::new();
        let mut next = after + 1;
        while next <= upto {
            let batch: Vec<u64> = (next..=upto.min(next + 63)).collect();
            let got = parallel(&batch, |n| self.store.get(&object_key(&self.space, d, *n), MAX_OBJECT));
            for r in got {
                match r? {
                    Some(bytes) => match decode_object(&bytes) {
                        Some(rev) if rev.device == *d => out.push(rev),
                        // Damaged, or claims another device: never used.
                        _ => return Ok(out),
                    },
                    None => return Ok(out),
                }
            }
            next += batch.len() as u64;
        }
        Ok(out)
    }

    /// Mirror what other devices uploaded since last time.
    fn download(&self, m: &mut Mirror) -> Result<(), TransportError> {
        let tips = self.store.list(&format!("{}tips/", space_dir(&self.space)))?;
        let trust_etags = self.store.etag_tracks_content();
        let mut waiting = 0;
        let mut devices = 0;
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
            if d == self.device {
                // Ours: unless this process wrote it, someone else writes as us.
                if tip.w != self.nonce && (m.tip_written.is_some() || tip.n != m.uploaded) {
                    m.problem = Some(shared_identity());
                }
                m.seen.insert(e.name, e.etag);
                continue;
            }
            let have = m.fetched.get(&d).copied().unwrap_or(0);
            if tip.n > have {
                let got = self.fetch_range(&d, have, tip.n)?;
                append_lines(&self.device_dir(&d), &got)?;
                let now = have + got.len() as u64;
                m.fetched.insert(d, now);
                if now < tip.n {
                    waiting += 1;
                    continue;
                }
            }
            m.seen.insert(e.name, e.etag);
        }
        m.waiting = waiting;
        Ok(())
    }

    /// Upload the outbox, then the tip.
    fn upload(&self, m: &mut Mirror) -> Result<(), TransportError> {
        if let Some(p) = &m.problem {
            return Err(TransportError::Refused(p.clone()));
        }
        let res = parallel(&m.outbox, |(n, r)| {
            let key = object_key(&self.space, &self.device, *n);
            let body = encode_object(r);
            match put_new(&*self.store, &key, &body, &self.conditional)? {
                Put::Done => Ok(true),
                // Uploaded before the tip was (a crash, a lost reply): the
                // same bytes are fine; anything else is another computer.
                Put::Exists => Ok(self.store.get(&key, MAX_OBJECT)?.is_some_and(|b| b == body)),
            }
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
        if m.tip_written != Some(m.uploaded) && (m.uploaded > 0 || m.tip_written.is_some()) {
            let tip = serde_json::to_vec(&Tip { n: m.uploaded, w: self.nonce.clone() }).unwrap_or_default();
            match self.store.put(&tip_key(&self.space, &self.device), &tip, false) {
                Ok(_) => m.tip_written = Some(m.uploaded),
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
        self.upload(&mut m)
    }

    /// Changes whenever another device uploads (one listing).
    pub fn fingerprint(&self) -> Result<u64, TransportError> {
        fingerprint(&*self.store, &self.space, &self.device)
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
        let out = self.folder.push(rev)?;
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
        let left = self.writes_left.load(Ordering::Relaxed);
        if left == 0 {
            return Err(StoreError::Offline("simulated: connection cut".into()));
        }
        if left != u64::MAX {
            self.writes_left.fetch_sub(1, Ordering::Relaxed);
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
                    out.push(Entry { name: d.to_string(), dir: true, etag: String::new(), stamp: String::new() });
                }
            } else {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                v.hash(&mut h);
                out.push(Entry { name: rest.to_string(), dir: false, etag: format!("{:x}", h.finish()), stamp: v.len().to_string() });
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
