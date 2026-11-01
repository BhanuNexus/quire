// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The sync engine: brings a folder of notes and a space's revision log into
//! agreement, without ever dropping text.
//!
//! One [`Engine::sync`] pass:
//!
//! 1. **Pull** revisions after the cursor and apply each to the folder. If
//!    the note also changed locally since the last sync, the two are merged
//!    three-way, with the last synced text as the ancestor. A clean merge is
//!    written to the note. Overlapping edits keep the local note and write the
//!    other version next to it as a conflict copy.
//! 2. **Push** every local change as a revision whose parent is the last
//!    synced one. If the server says another device got there first, pull
//!    again and retry.
//!
//! Edits beat deletes: a note edited on one device and deleted on another is
//! kept. Renames travel with the note (the path lives inside the encrypted
//! content), and a local rename is recognised by content.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use quire_text::merge;

use crate::crypto::{CryptoError, Keys};
use crate::protocol::{Content, DeviceId, FileId, PushOutcome, RevId, Revision, SpaceId};
use crate::transport::{Transport, TransportError};

/// A folder of notes, as the engine sees it. Paths are vault-relative with `/`.
pub trait LocalStore {
    /// Every note that should sync.
    fn list(&self) -> Vec<String>;
    fn read(&self, path: &str) -> Option<String>;
    fn write(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn remove(&mut self, path: &str) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("couldn't decrypt a revision: {0}")]
    Crypto(#[from] CryptoError),
    #[error("couldn't write {path}: {error}")]
    Local { path: String, error: String },
    #[error("other devices kept changing the same notes; try again")]
    Busy,
}

/// What the engine remembers about one synced note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    /// Where the note is in this folder.
    pub path: String,
    /// Where the server last had it. Differs when a name clash made this
    /// device file it elsewhere; the next push tells the server.
    pub synced_path: String,
    pub head: RevId,
    pub head_seq: u64,
    /// The text as of `head`: the common ancestor for the next merge.
    pub base: String,
    pub deleted: bool,
}

/// Everything the engine persists between runs (small; stored in `.quire/sync/`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// Last log position applied.
    pub cursor: u64,
    pub files: BTreeMap<FileId, Tracked>,
}

/// What one sync pass did, for the status bar and notices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub pulled: usize,
    pub pushed: usize,
    /// Notes where changes from both sides were merged.
    pub merged: Vec<String>,
    /// Conflict copies written (other device's version, next to yours).
    pub conflict_copies: Vec<String>,
    /// Notes kept because they were edited on one side and deleted on the other.
    pub kept_over_delete: Vec<String>,
}

impl Report {
    pub const fn changed_anything(&self) -> bool {
        self.pulled > 0 || self.pushed > 0
    }
}

pub struct Engine {
    pub space: SpaceId,
    pub device: DeviceId,
    /// Shown in conflict copy names, e.g. "Asha's laptop".
    pub device_name: String,
    keys: Keys,
    pub state: SyncState,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("space", &self.space).field("device", &self.device).finish_non_exhaustive()
    }
}

const MAX_ROUNDS: usize = 16;

/// Another device keeps winning the race for the same note: wait a little,
/// with jitter so the devices stop colliding (5 ms, 10 ms … capped at 640 ms).
fn backoff(round: usize) {
    let cap = 5u64 << round.min(7);
    let jitter = crate::crypto::random_bytes::<2>().map_or(0, |b| u64::from(u16::from_le_bytes(b)));
    std::thread::sleep(std::time::Duration::from_millis(cap / 2 + jitter % (cap / 2 + 1)));
}

impl Engine {
    pub fn new(space: SpaceId, device: DeviceId, device_name: impl Into<String>, keys: Keys, state: SyncState) -> Self {
        Self { space, device, device_name: device_name.into(), keys, state }
    }

    /// Pull, merge and push until this device and the server agree.
    /// `today` (e.g. `2026-11-01`) goes into conflict copy names.
    pub fn sync(&mut self, local: &mut dyn LocalStore, server: &dyn Transport, today: &str) -> Result<Report, SyncError> {
        let mut report = Report::default();
        for round in 0..MAX_ROUNDS {
            if round > 0 {
                backoff(round);
            }
            self.pull(local, server, today, &mut report)?;
            if self.push(&*local, server, &mut report)? {
                return Ok(report);
            }
        }
        Err(SyncError::Busy)
    }

    fn pull(&mut self, local: &mut dyn LocalStore, server: &dyn Transport, today: &str, report: &mut Report) -> Result<(), SyncError> {
        let revs = server.pull(self.state.cursor)?;
        for r in revs {
            if self.state.files.get(&r.file).is_none_or(|t| t.head_seq < r.seq) {
                let c = Content::open(&self.keys, &self.space, &r)?;
                // If applying fails (say the file changed underneath us), the
                // cursor stays put and this revision is applied next time.
                self.apply(local, &r, &c, today, report)?;
                report.pulled += 1;
            }
            self.state.cursor = self.state.cursor.max(r.seq);
        }
        Ok(())
    }

    fn apply(&mut self, local: &mut dyn LocalStore, r: &Revision, c: &Content, today: &str, report: &mut Report) -> Result<(), SyncError> {
        let Some(t) = self.state.files.get(&r.file).cloned() else {
            // A note we haven't seen.
            let mut path = c.path.clone();
            if !c.deleted {
                // The same name may have been created here too; that file
                // keeps its name and this note takes the next free one.
                path = self.free_path(local, &c.path, &r.file);
                self.protect_unsynced(local, &path, &c.text)?;
                write(local, &path, &c.text)?;
            }
            self.track_at(r, c, &path);
            return Ok(());
        };

        let here = local.read(&t.path);
        let changed_here = match (&here, t.deleted) {
            (None, deleted) => !deleted,
            (Some(_), true) => true,
            (Some(x), false) => *x != t.base,
        };

        if !changed_here {
            // Nothing to protect: take the remote version.
            let to = if c.deleted { t.path.clone() } else { self.free_path(local, &c.path, &r.file) };
            if t.path != to || c.deleted {
                remove(local, &t.path)?;
            }
            if !c.deleted {
                self.protect_unsynced(local, &to, &c.text)?;
                write(local, &to, &c.text)?;
            }
            self.track_at(r, c, &to);
            return Ok(());
        }

        match (here, c.deleted) {
            // Edited here, deleted there: keep ours (it's pushed as a revival).
            (Some(_), true) => {
                report.kept_over_delete.push(t.path.clone());
                self.track_keep_local(r, &t.path, true);
            }
            // Deleted here, edited there: the edit wins.
            (None, false) => {
                let to = self.free_path(local, &c.path, &r.file);
                self.protect_unsynced(local, &to, &c.text)?;
                write(local, &to, &c.text)?;
                report.kept_over_delete.push(to.clone());
                self.track_at(r, c, &to);
            }
            // Deleted on both sides.
            (None, true) => self.track_at(r, c, &t.path),
            // Edited on both sides.
            (Some(mine), false) => {
                let to = self.free_path(local, &c.path, &r.file);
                if t.path != to {
                    remove(local, &t.path)?;
                }
                if t.path != to {
                    self.protect_unsynced(local, &to, &mine)?;
                }
                let m = merge::merge3(&t.base, &mine, &c.text);
                if m.is_clean() {
                    write(local, &to, &m.text)?;
                    report.merged.push(to.clone());
                } else {
                    // Keep ours in place; theirs goes beside it, and both sync.
                    write(local, &to, &mine)?;
                    let copy = self.conflict_copy_path(local, &to, &c.from, today);
                    write(local, &copy, &c.text)?;
                    report.conflict_copies.push(copy);
                }
                self.track_at(r, c, &to);
            }
        }
        Ok(())
    }

    fn is_tracked_path(&self, path: &str) -> bool {
        self.state.files.values().any(|t| !t.deleted && t.path == path)
    }

    /// `wanted`, unless another synced note already lives there; then a
    /// numbered name next to it.
    /// Where a note may go: `wanted`, unless another synced note is already
    /// there; then a numbered name next to it.
    fn free_path(&self, local: &dyn LocalStore, wanted: &str, file: &FileId) -> String {
        let taken = |p: &str| self.state.files.iter().any(|(id, t)| id != file && !t.deleted && t.path == p);
        if !taken(wanted) {
            return wanted.to_string();
        }
        let (stem, ext) = split_ext(wanted);
        (2..10_000)
            .map(|n| format!("{stem} ({n}){ext}"))
            .find(|p| !taken(p) && local.read(p).is_none())
            .unwrap_or_else(|| wanted.to_string())
    }

    /// Before writing a note to `path`: a file there that hasn't synced yet
    /// (and differs) moves to a free name, so it's never overwritten. It's
    /// pushed later as a note of its own.
    fn protect_unsynced(&self, local: &mut dyn LocalStore, path: &str, text: &str) -> Result<(), SyncError> {
        let tracked = self.state.files.values().any(|t| t.path == path);
        let Some(existing) = local.read(path).filter(|e| e != text && !tracked) else { return Ok(()) };
        let (stem, ext) = split_ext(path);
        let aside = (2..10_000)
            .map(|n| format!("{stem} ({n}){ext}"))
            .find(|p| local.read(p).is_none() && !self.is_tracked_path(p))
            .unwrap_or_else(|| format!("{stem} (unsynced){ext}"));
        write(local, &aside, &existing)
    }

    fn track_at(&mut self, r: &Revision, c: &Content, path: &str) {
        self.state.files.insert(
            r.file.clone(),
            Tracked {
                path: path.to_string(),
                synced_path: c.path.clone(),
                head: r.rev.clone(),
                head_seq: r.seq,
                base: c.text.clone(),
                deleted: c.deleted,
            },
        );
    }

    fn track_keep_local(&mut self, r: &Revision, path: &str, deleted: bool) {
        self.state.files.insert(
            r.file.clone(),
            Tracked {
                path: path.to_string(),
                synced_path: path.to_string(),
                head: r.rev.clone(),
                head_seq: r.seq,
                base: String::new(),
                deleted,
            },
        );
    }

    fn conflict_copy_path(&self, local: &dyn LocalStore, path: &str, from: &str, today: &str) -> String {
        let (stem, ext) = split_ext(path);
        let from = if from.trim().is_empty() { "another device" } else { from.trim() };
        let device = from.replace(['/', '\\', ':'], "-");
        let mut n = 1;
        loop {
            let suffix = if n == 1 { String::new() } else { format!(" {n}") };
            let p = format!("{stem} (conflict from {device} {today}{suffix}){ext}");
            if local.read(&p).is_none() && !self.is_tracked_path(&p) {
                return p;
            }
            n += 1;
        }
    }

    /// Push every local change. Returns false when the server had newer
    /// revisions (pull again first).
    fn push(&mut self, local: &dyn LocalStore, server: &dyn Transport, report: &mut Report) -> Result<bool, SyncError> {
        let present: BTreeSet<String> = local.list().into_iter().collect();
        let live: BTreeSet<String> = self.state.files.values().filter(|t| !t.deleted).map(|t| t.path.clone()).collect();
        // A file at a deleted note's path brings that note back rather than
        // starting a new one, unless another live note has the path now.
        let known: BTreeSet<String> = self.state.files.values().map(|t| t.path.clone()).collect();
        let mut untracked: Vec<String> = present.iter().filter(|p| !known.contains(*p)).cloned().collect();

        let ids: Vec<FileId> = self.state.files.keys().cloned().collect();
        for id in ids {
            let t = self.state.files[&id].clone();
            let here = if present.contains(&t.path) { local.read(&t.path) } else { None };
            let content = match (here, t.deleted) {
                (Some(_), true) if live.contains(&t.path) => continue,
                (Some(x), true) => Content { path: t.path.clone(), deleted: false, text: x, from: String::new() },
                (Some(x), false) if x != t.base || t.path != t.synced_path => {
                    Content { path: t.path.clone(), deleted: false, text: x, from: String::new() }
                }
                (Some(_), false) | (None, true) => continue,
                (None, false) => {
                    // Gone from its path: renamed (same text elsewhere) or deleted.
                    if let Some(i) = untracked.iter().position(|p| local.read(p).as_deref() == Some(t.base.as_str())) {
                        let to = untracked.remove(i);
                        Content { path: to, deleted: false, text: t.base.clone(), from: String::new() }
                    } else {
                        Content { path: t.path.clone(), deleted: true, text: String::new(), from: String::new() }
                    }
                }
            };
            if !self.push_one(server, &id, Some(&t.head), &content, report)? {
                return Ok(false);
            }
        }

        for path in untracked {
            let Some(text) = local.read(&path) else { continue };
            let id = FileId::random()?;
            let content = Content { path, deleted: false, text, from: String::new() };
            if !self.push_one(server, &id, None, &content, report)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn push_one(
        &mut self,
        server: &dyn Transport,
        file: &FileId,
        parent: Option<&RevId>,
        content: &Content,
        report: &mut Report,
    ) -> Result<bool, SyncError> {
        let rev = RevId::random()?;
        let content = &Content { from: self.device_name.clone(), ..content.clone() };
        let sealed = content.seal(&self.keys, &self.space, file, &rev)?;
        let r = Revision {
            file: file.clone(),
            rev,
            parents: parent.into_iter().cloned().collect(),
            seq: 0,
            device: self.device.clone(),
            sealed,
        };
        match server.push(&r)? {
            PushOutcome::Accepted { seq } => {
                self.state.files.insert(
                    file.clone(),
                    Tracked {
                        path: content.path.clone(),
                        synced_path: content.path.clone(),
                        head: r.rev,
                        head_seq: seq,
                        base: content.text.clone(),
                        deleted: content.deleted,
                    },
                );
                report.pushed += 1;
                Ok(true)
            }
            PushOutcome::Conflict { .. } => Ok(false),
        }
    }
}

/// `("Notes/Plan", ".md")`; `("README", "")` when there's no extension.
fn split_ext(path: &str) -> (&str, &str) {
    match path.rfind('.') {
        Some(i) if i > path.rfind('/').map_or(0, |s| s + 1) => (&path[..i], &path[i..]),
        _ => (path, ""),
    }
}

fn write(local: &mut dyn LocalStore, path: &str, text: &str) -> Result<(), SyncError> {
    local.write(path, text).map_err(|error| SyncError::Local { path: path.to_string(), error })
}

fn remove(local: &mut dyn LocalStore, path: &str) -> Result<(), SyncError> {
    if local.read(path).is_none() {
        return Ok(());
    }
    local.remove(path).map_err(|error| SyncError::Local { path: path.to_string(), error })
}

/// A folder in memory, for tests and simulations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStore(pub BTreeMap<String, String>);

impl LocalStore for MemoryStore {
    fn list(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.0.insert(path.to_string(), text.to_string());
        Ok(())
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.0.remove(path);
        Ok(())
    }
}
