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
//! ```
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::crypto::WrappedKey;
use crate::protocol::{DeviceId, FileId, PushOutcome, RevId, Revision, SpaceId};
use crate::transport::{Transport, TransportError};

/// The folder Quire keeps inside the one the user picks.
pub const FOLDER_NAME: &str = "Quire Sync";
/// A device starts a new log file past this size, so tools that upload whole
/// files on every change don't re-send a growing log.
const SEGMENT_LIMIT: u64 = 1 << 20;
const FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpaceFile {
    format: u32,
    space: SpaceId,
    /// Unix seconds.
    created: i64,
    wrapped_key: WrappedKey,
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

fn is_segment_name(name: &str) -> bool {
    name.len() == 12 && name.is_char_boundary(6) && &name[6..] == ".jsonl" && name[..6].bytes().all(|b| b.is_ascii_digit())
}

/// Write a small file so readers see the old version or the new one, never half.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Make a new vault in `root` (see [`sync_root`]).
pub fn create_space(root: &Path, space: &SpaceId, key: &WrappedKey) -> Result<(), TransportError> {
    let dir = root.join(&space.0);
    fs::create_dir_all(dir.join("devices")).map_err(|e| io_err("can't create the sync folder", &e))?;
    let file = SpaceFile { format: FORMAT, space: space.clone(), created: unix_now(), wrapped_key: key.clone() };
    let json = serde_json::to_vec_pretty(&file).unwrap_or_default();
    write_atomically(&dir.join("space.json"), &json).map_err(|e| io_err("can't write to the sync folder", &e))
}

fn read_space(dir: &Path) -> Result<SpaceFile, TransportError> {
    let bytes = fs::read(dir.join("space.json")).map_err(|e| io_err("can't read the vault in the sync folder", &e))?;
    let f: SpaceFile = serde_json::from_slice(&bytes)
        .map_err(|_| TransportError::Refused("the vault's space.json is damaged or still downloading".into()))?;
    if f.format > FORMAT {
        return Err(TransportError::Refused("this vault was made by a newer Quire; update Quire on this device".into()));
    }
    Ok(f)
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

/// What this device knows about the folder. Only ids and positions are kept
/// in memory; sealed bytes are read back from the files when needed.
#[derive(Debug, Default)]
struct Index {
    revs: HashMap<RevId, Meta>,
    /// Revisions by their first parent.
    children: HashMap<RevId, Vec<RevId>>,
    /// Each note's first revision(s).
    roots: HashMap<FileId, Vec<RevId>>,
    /// How far each log file has been read (always just after a newline).
    read_to: HashMap<PathBuf, u64>,
    /// The revision handed to the engine for each note, and when.
    delivered: HashMap<FileId, (RevId, u64)>,
    seq: u64,
    /// Revisions this process wrote, to notice another computer writing as us.
    mine: HashSet<RevId>,
    scanned_once: bool,
    problem: Option<String>,
    waiting_for_download: usize,
}

/// A space in a synced folder, seen from one device.
#[derive(Debug)]
pub struct FolderTransport {
    dir: PathBuf,
    device: DeviceId,
    /// Remembers the last order number handed out, across restarts.
    seq_file: Option<PathBuf>,
    index: Mutex<Index>,
}

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
        Self { dir: root.join(&space.0), device, seq_file, index: Mutex::new(Index { seq, ..Index::default() }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn own_dir(&self) -> PathBuf {
        self.dir.join("devices").join(&self.device.0)
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
        let n = self.lock().waiting_for_download;
        (n > 0).then(|| {
            format!(
                "Waiting for {n} file(s) in the sync folder to download. In iCloud Drive, choose “Keep Downloaded” for the Quire Sync folder."
            )
        })
    }

    /// Changes whenever any device's log changes; cheap enough to poll.
    pub fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if let Ok(devs) = fs::read_dir(self.dir.join("devices")) {
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
        h.finish()
    }

    /// Read whatever is new in every device's logs.
    fn scan(&self, ix: &mut Index) -> Result<(), TransportError> {
        self.check_folder()?;
        ix.waiting_for_download = 0;
        let Ok(devs) = fs::read_dir(self.dir.join("devices")) else { return Ok(()) };
        let mut logs = Vec::new();
        for dev in devs.flatten() {
            let own = dev.file_name().to_str() == Some(self.device.0.as_str());
            let Ok(files) = fs::read_dir(dev.path()) else { continue };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.ends_with(".icloud") {
                    // iCloud evicted it to save space; it downloads on demand.
                    ix.waiting_for_download += 1;
                } else if Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl")) {
                    if own && !is_segment_name(&name) {
                        ix.problem = Some(Self::shared_identity());
                    }
                    logs.push((own, f.path()));
                }
            }
        }
        // A stable order, so a revision's parent tends to be read first.
        logs.sort();
        for (own, path) in logs {
            Self::read_log(ix, own, &path);
        }
        ix.scanned_once = true;
        Ok(())
    }

    fn shared_identity() -> String {
        "Another computer is syncing as this one (a copy of this vault's .quire folder?). Turn sync off and on again on one of them.".into()
    }

    fn read_log(ix: &mut Index, own: bool, path: &Path) {
        let Ok(mut f) = File::open(path) else { return };
        let len = f.metadata().map_or(0, |m| m.len());
        let mut from = ix.read_to.get(path).copied().unwrap_or(0);
        if len < from {
            // Replaced by a shorter file; read it again (known lines are skipped).
            from = 0;
        }
        if len == from || f.seek(SeekFrom::Start(from)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.take(len - from).read_to_end(&mut buf).is_err() {
            return;
        }
        let shared = Arc::new(path.to_path_buf());
        let mut start = 0;
        while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + nl];
            if let Ok(r) = serde_json::from_slice::<Revision>(line) {
                let loc = Loc { path: Arc::clone(&shared), offset: from + start as u64, len: line.len() };
                if own && ix.scanned_once && !ix.mine.contains(&r.rev) && !ix.revs.contains_key(&r.rev) {
                    ix.problem = Some(Self::shared_identity());
                }
                Self::insert(ix, &r, loc);
            }
            // A line that doesn't parse is damaged; it's skipped for good.
            start += nl + 1;
        }
        ix.read_to.insert(path.to_path_buf(), from + start as u64);
    }

    fn insert(ix: &mut Index, r: &Revision, loc: Loc) {
        if ix.revs.contains_key(&r.rev) {
            return;
        }
        match r.parents.first() {
            Some(p) => ix.children.entry(p.clone()).or_default().push(r.rev.clone()),
            None => ix.roots.entry(r.file.clone()).or_default().push(r.rev.clone()),
        }
        ix.revs.insert(r.rev.clone(), Meta { file: r.file.clone(), parents: r.parents.clone(), device: r.device.clone(), loc });
    }

    /// A note's current revision: from its first revision, keep following
    /// the child with the smallest id.
    fn head(ix: &Index, file: &FileId) -> Option<RevId> {
        let mut cur = ix.roots.get(file)?.iter().min()?;
        for _ in 0..=ix.revs.len() {
            match ix.children.get(cur).and_then(|c| c.iter().min()) {
                Some(next) => cur = next,
                None => break,
            }
        }
        Some(cur.clone())
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

    /// Every revision `rev` builds on, and itself.
    fn ancestry(ix: &Index, rev: &RevId) -> HashSet<RevId> {
        let mut seen = HashSet::new();
        let mut todo = vec![rev.clone()];
        while let Some(r) = todo.pop() {
            if let Some(m) = ix.revs.get(&r) {
                todo.extend(m.parents.iter().filter(|p| !seen.contains(*p)).cloned());
            }
            seen.insert(r);
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

    fn append(&self, ix: &mut Index, r: &Revision) -> Result<(), TransportError> {
        let path = self.current_segment()?;
        let werr = |e: std::io::Error| io_err("can't write to the sync folder", &e);
        let mut f = OpenOptions::new().create(true).read(true).append(true).open(&path).map_err(werr)?;
        let mut offset = f.metadata().map_err(werr)?.len();
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
        let json = serde_json::to_vec(&Revision { seq: 0, ..r.clone() }).map_err(|e| TransportError::Refused(e.to_string()))?;
        line.extend_from_slice(&json);
        line.push(b'\n');
        f.write_all(&line).map_err(werr)?;
        f.sync_data().map_err(werr)?;
        ix.mine.insert(r.rev.clone());
        Self::insert(ix, r, Loc { path: Arc::new(path), offset, len: json.len() });
        Ok(())
    }
}

impl Transport for FolderTransport {
    /// The current revision of every note that changed since it was last
    /// handed out, each with a new order number past `after`.
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        ix.seq = ix.seq.max(after);
        let mut files: Vec<FileId> = ix.roots.keys().cloned().collect();
        files.sort();
        for f in files {
            let Some(h) = Self::head(&ix, &f) else { continue };
            if ix.delivered.get(&f).is_none_or(|(d, _)| *d != h) {
                let seq = self.next_seq(&mut ix);
                ix.delivered.insert(f, (h, seq));
            }
        }
        let mut due: Vec<(u64, RevId)> = ix.delivered.values().filter(|(_, s)| *s > after).map(|(r, s)| (*s, r.clone())).collect();
        due.sort();
        due.into_iter()
            .map(|(seq, rev)| {
                let mut r = Self::read_rev(&ix, &rev)?;
                r.seq = seq;
                Ok(r)
            })
            .collect()
    }

    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
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
            (None, Some(_)) => return Err(TransportError::Refused("parent doesn't exist".into())),
        }
        self.append(&mut ix, rev)?;
        let seq = self.next_seq(&mut ix);
        ix.delivered.insert(rev.file.clone(), (rev.rev.clone(), seq));
        Ok(PushOutcome::Accepted { seq })
    }

    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        let mut ix = self.lock();
        self.scan(&mut ix)?;
        let mine: Vec<RevId> = ix.revs.iter().filter(|(_, m)| m.file == *file).map(|(r, _)| r.clone()).collect();
        mine.iter().map(|r| Self::read_rev(&ix, r)).collect()
    }

    fn fork_base(&self, _file: &FileId, ours: &RevId, theirs: &Revision) -> Option<Revision> {
        let ix = self.lock();
        let theirs_has = Self::ancestry(&ix, &theirs.rev);
        if theirs_has.contains(ours) {
            return None;
        }
        // Walk back from ours to the nearest revision theirs also has, noting
        // whether anything on the way was written here.
        let (mut written_here, mut base) = (false, None);
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
            if let Some(m) = ix.revs.get(&r) {
                written_here |= m.device == self.device;
                todo.extend(m.parents.iter().cloned());
            }
        }
        if written_here { base.and_then(|b| Self::read_rev(&ix, &b).ok()) } else { None }
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
        }
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
