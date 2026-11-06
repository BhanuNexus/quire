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
//!
//! **Selective sync.** A store can hold only part of a vault
//! ([`LocalStore::in_scope`]). Notes outside it are tracked but never
//! written, read or pushed: their absence here is never sent as a delete.
//! A note moved out of the selection on another device leaves this folder
//! (to its trash); one moved in arrives. When the selection grows, the notes
//! that were outside it are fetched again from the log: only each note's
//! current revision, so a note never passes through its old names or texts.
//!
//! **Identity.** A note the engine hasn't seen arrives as its latest
//! revision in the batch, so a fresh state over a folder that already has
//! the notes (a new state file, another version's state) adopts them instead
//! of copying them aside. Two notes created at the same path with the same
//! text on two devices (an agent's suggestion accepted on both) are one
//! note: the smaller id keeps it, the other is sent as deleted.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use quire_text::merge;

use crate::crypto::{CryptoError, Keys};
use crate::protocol::{Content, DeviceId, FileId, PushOutcome, RevId, Revision, SpaceId};
use crate::transport::{Transport, TransportError};

/// A folder of notes, as the engine sees it. Paths are vault-relative with `/`.
///
/// Notes are text. Other files (images, PDFs: see [`is_attachment`]) travel
/// as base64 text, so `read` and `write` use that for them.
pub trait LocalStore {
    /// Every file that should sync.
    fn list(&self) -> Vec<String>;
    fn read(&self, path: &str) -> Option<String>;
    fn write(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn remove(&mut self, path: &str) -> Result<(), String>;
    /// What [`fingerprint`] gives for the file, so an unchanged attachment
    /// needn't be read in full on every pass. Stores can cache it.
    fn digest(&self, path: &str) -> Option<String> {
        self.read(path).map(|t| fingerprint(path, &t))
    }
    /// Who wrote the current version of `path`, if not the person (sent
    /// with it, protocol 2).
    fn actor(&self, path: &str) -> Option<crate::protocol::ActorRef> {
        let _ = path;
        None
    }
    /// A revision from another device is about to be applied (its text may be
    /// written as is, or merged); stores can keep its `actor`.
    fn incoming(&mut self, content: &Content) {
        let _ = content;
    }
    /// Whether `path` belongs to what this store holds. A store that holds
    /// part of a vault (selective sync) says no for the rest: the engine
    /// then never writes, removes or pushes those notes, so their absence
    /// here never reads as a delete. `list`, `read` and `digest` must leave
    /// them out too.
    fn in_scope(&self, path: &str) -> bool {
        let _ = path;
        true
    }
    /// A file outside the selection whose contents [`fingerprint`] to
    /// `base`, as `(path, text)`: where a note that left the selection
    /// here went (moved into an unselected folder), so the move is sent as
    /// a rename rather than a delete.
    ///
    /// Never a path in `skip` (notes the engine already tracks, which are
    /// other notes, not this one). Stores should only offer a file that
    /// appeared since their last pass: one that was already there is
    /// another note with the same text, and the note was deleted.
    fn moved_out(&self, base: &str, skip: &BTreeSet<String>) -> Option<(String, String)> {
        let _ = (base, skip);
        None
    }
}

/// Files that aren't notes: they're never merged line by line, and the
/// engine remembers them by hash rather than keeping their contents.
pub fn is_attachment(path: &str) -> bool {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default();
    !["md", "markdown", "txt"].iter().any(|t| ext.eq_ignore_ascii_case(t))
}

/// How the engine remembers a file's synced contents: notes in full (they're
/// the ancestor for the next merge), attachments as a hash.
pub fn fingerprint(path: &str, text: &str) -> String {
    use sha2::Digest;
    if is_attachment(path) { format!("sha256:{}", crate::crypto::hex(&sha2::Sha256::digest(text.as_bytes()))) } else { text.to_string() }
}

/// Whether `text` is what was last synced (`base`, a note's text or an
/// attachment's [`fingerprint`]).
fn same_as_base(path: &str, text: &str, base: &str) -> bool {
    text == base || (is_attachment(path) && fingerprint(path, text) == base)
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
    /// The text as of `head`: the common ancestor for the next merge. For an
    /// attachment, its [`fingerprint`].
    pub base: String,
    pub deleted: bool,
    /// The note is outside this device's selection (selective sync): it is
    /// tracked, never written or pushed here. When the selection grows to
    /// include it, it is fetched again from the log.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    /// The same note as the live one at `path`, created there with the
    /// same text on another device (an agent's suggestion accepted on
    /// both): kept as deleted here, and the delete is sent so every device
    /// files the two as one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absorbed: bool,
    /// For a note that left this device's selection: where its copy stays
    /// on this disk and the text last synced then. When the selection grows
    /// again, changes made to that copy meanwhile are merged with the note
    /// as it is now, wherever it moved (never copied aside, never lost).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<KeptCopy>,
}

/// See [`Tracked::kept`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptCopy {
    pub path: String,
    pub base: String,
}

impl Tracked {
    /// Tracked here but not on this device's disk yet: outside the
    /// selection, or inside a selection that just grew, waiting to be
    /// fetched.
    pub const fn held(&self) -> bool {
        !self.outside
    }
}

/// Everything the engine persists between runs (small; stored in `.quire/sync/`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// Last log position applied.
    pub cursor: u64,
    pub files: BTreeMap<FileId, Tracked>,
    /// Paths where this device may still have an old copy of a note that
    /// moved on or was deleted while it was outside the selection (the
    /// copy stays: a file outside the selection is never touched). When the
    /// selection grows to include one, a file there that is exactly a
    /// version of that note goes to the trash here (the version is in the
    /// note's history), so it isn't sent back as a note of its own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub left: BTreeMap<String, LeftCopy>,
}

/// A copy of a note that may be left on this disk (see [`SyncState::left`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftCopy {
    pub file: FileId,
    /// The text it was last synced with, when known: changes made to the
    /// copy since are merged when the note is written there again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
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

/// One version of a note from the sync log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub rev: RevId,
    /// Position in a server's log (0 in a synced folder).
    pub seq: u64,
    /// When it was written (Unix ms; 0 for versions from before Quire 0.7).
    pub at: i64,
    /// The device that wrote it.
    pub device: String,
    pub path: String,
    pub deleted: bool,
    pub text: String,
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
        self.rescope(&*local);
        let res = self.clear_left(local, server).and_then(|()| self.rounds(local, server, today));
        // Whatever happened, notes now tracked outside the selection are
        // marked, so a later, wider selection fetches them instead of
        // reading their absence here as a delete.
        for t in self.state.files.values_mut() {
            if !local.in_scope(&t.path) {
                t.outside = true;
            }
        }
        if res.is_ok() {
            // Copies inside the selection that no note took back are notes
            // of their own now (sent by this pass).
            self.state.left.retain(|p, _| !local.in_scope(p));
        }
        res
    }

    fn rounds(&mut self, local: &mut dyn LocalStore, server: &dyn Transport, today: &str) -> Result<Report, SyncError> {
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

    /// Bring the tracked notes in line with the store's selection: notes now
    /// outside it are marked so. Notes that were outside and are now inside
    /// stay marked (and tracked, with their current revision) until that
    /// revision is fetched: the log is read again from the start and only
    /// that revision, or a newer one, is applied, so they arrive as they are
    /// now, never by way of their old names or texts. A file already here
    /// with other text is kept beside them, never overwritten.
    fn rescope(&mut self, local: &dyn LocalStore) {
        let mut widened = false;
        for (id, t) in &mut self.state.files {
            let rejoin = t.outside && t.kept.as_ref().is_some_and(|k| local.in_scope(&k.path) && local.read(&k.path).is_some());
            if let Some(k) = t.kept.clone().filter(|_| rejoin) {
                // The copy this device kept is in the selection again: it is
                // brought up to date from the note's current revision (merged,
                // or following it when it moved), as if edited here since it
                // was last synced. Never sent before that.
                t.path = k.path;
                t.base = k.base;
                t.deleted = false;
                t.outside = false;
                widened = true;
            } else if !t.outside && t.kept.is_some() {
                // Still to be brought up to date (a pass that didn't finish).
                widened = true;
            } else if !local.in_scope(&t.path) {
                if !t.outside && !t.deleted && !is_attachment(&t.path) {
                    // Leaving this device's selection: the copy here derives from this.
                    t.kept = Some(KeptCopy { path: t.path.clone(), base: t.base.clone() });
                }
                t.outside = true;
            } else if t.outside {
                if t.deleted {
                    // Nothing to fetch; a copy from before may still be here.
                    t.outside = false;
                    let base = t.kept.take().filter(|k| k.path == t.path).map(|k| k.base);
                    self.state.left.insert(t.path.clone(), LeftCopy { file: id.clone(), base });
                } else {
                    widened = true;
                }
            }
        }
        if widened {
            self.state.cursor = 0;
        }
    }

    /// Old copies of notes that moved on or were deleted while outside the
    /// selection, now inside it: to the trash here when they are exactly a
    /// version of that note (see [`SyncState::left`]). Anything else stays,
    /// and is sent as a note of its own.
    fn clear_left(&mut self, local: &mut dyn LocalStore, server: &dyn Transport) -> Result<(), SyncError> {
        let due: Vec<(String, FileId)> =
            self.state.left.iter().filter(|(p, _)| local.in_scope(p)).map(|(p, l)| (p.clone(), l.file.clone())).collect();
        for (path, file) in due {
            // A note lives there now (or this note will be fetched there): not a leftover.
            let lives = self.state.files.iter().any(|(id, t)| !t.deleted && t.path == path && (t.held() || *id == file));
            if lives || local.read(&path).is_none() {
                self.state.left.remove(&path);
            } else if self.is_version_of(local, server, &path, &file) {
                remove(local, &path)?;
                self.state.left.remove(&path);
            }
            // Changed since: the note may come back to this path in this
            // pass and take the changes in (see `Self::merge_left`).
        }
        Ok(())
    }

    /// The text to write for note `file` arriving at `path` here (`c`): a
    /// file there that is an older version of this very note is brought up
    /// to date, not copied aside; one this device changed while the note
    /// was out of reach takes the note's changes in. Any other file there
    /// is moved to a free name first.
    fn arriving(
        &mut self,
        local: &mut dyn LocalStore,
        server: &dyn Transport,
        path: &str,
        file: &FileId,
        c: &Content,
        report: &mut Report,
    ) -> Result<String, SyncError> {
        if let Some(m) = self.merge_left(&*local, path, file, &c.text) {
            report.merged.push(path.to_string());
            return Ok(m);
        }
        if local.read(path).is_some_and(|h| h != c.text) && !self.is_version_of(&*local, server, path, file) {
            self.protect_unsynced(local, path, &c.text)?;
        }
        Ok(c.text.clone())
    }

    /// Before writing note `file` at `path`, where this device has a copy
    /// of it that was changed while the note was out of reach (see
    /// [`SyncState::left`]): the copy's changes merged into `incoming`,
    /// when they merge cleanly.
    fn merge_left(&mut self, local: &dyn LocalStore, path: &str, file: &FileId, incoming: &str) -> Option<String> {
        let left = self.state.left.get(path).filter(|l| l.file == *file && !is_attachment(path))?;
        let base = left.base.clone()?;
        let mine = local.read(path)?;
        let m = merge::merge3(&base, &mine, incoming);
        m.is_clean().then(|| {
            self.state.left.remove(path);
            m.text
        })
    }

    /// Whether the file at `path` here is exactly one of the versions of
    /// `file` the server holds (or the one last synced). Empty files never
    /// count: too many notes are empty.
    fn is_version_of(&self, local: &dyn LocalStore, server: &dyn Transport, path: &str, file: &FileId) -> bool {
        let Some(here) = local.read(path).filter(|h| !h.trim().is_empty()) else { return false };
        let print = fingerprint(path, &here);
        if self.state.files.get(file).is_some_and(|t| !t.deleted && t.base == print) {
            return true;
        }
        server.history(file).is_ok_and(|revs| {
            revs.iter().any(|r| Content::open(&self.keys, &self.space, r).is_ok_and(|c| !c.deleted && fingerprint(path, &c.text) == print))
        })
    }

    /// A note inside the selection that is tracked but not here yet (see
    /// [`Self::rescope`]).
    fn waiting(local: &dyn LocalStore, t: &Tracked) -> bool {
        t.outside && local.in_scope(&t.path)
    }

    /// A note whose kept copy is in the selection again, still to be
    /// brought up to date (see [`Self::rescope`]).
    const fn rejoining(t: &Tracked) -> bool {
        !t.outside && t.kept.is_some()
    }

    fn pull(&mut self, local: &mut dyn LocalStore, server: &dyn Transport, today: &str, report: &mut Report) -> Result<(), SyncError> {
        let revs = server.pull(self.state.cursor)?;
        // A note this device doesn't hold yet arrives as its latest revision
        // in the batch: replaying its history would file it under old names
        // (moving whatever note lives there now) and compare the file here
        // with old texts (copying an up-to-date file aside).
        let mut latest: std::collections::HashMap<&FileId, usize> = std::collections::HashMap::new();
        for (i, r) in revs.iter().enumerate() {
            if self.state.files.get(&r.file).is_none_or(|t| Self::waiting(&*local, t) || Self::rejoining(t)) {
                latest.insert(&r.file, i);
            }
        }
        let latest: std::collections::HashMap<FileId, usize> = latest.into_iter().map(|(f, i)| (f.clone(), i)).collect();
        // Their earlier revisions name paths they have left. A file at one
        // of those that is exactly that old version is an old copy (a
        // device starting afresh over a folder that was synced before):
        // it goes to the trash here rather than being sent as a new note.
        let mut opened: std::collections::HashMap<usize, Content> = std::collections::HashMap::new();
        let several: BTreeSet<&FileId> =
            revs.iter().enumerate().filter(|(i, r)| latest.get(&r.file).is_some_and(|l| l != i)).map(|(_, r)| &r.file).collect();
        for (i, r) in revs.iter().enumerate() {
            if several.contains(&r.file) {
                opened.insert(i, Content::open(&self.keys, &self.space, r)?);
            }
        }
        let mut old: Vec<(String, String)> = Vec::new();
        for (i, c) in &opened {
            let last = &opened[&latest[&revs[*i].file]];
            if latest[&revs[*i].file] != *i && !c.deleted && (last.deleted || last.path != c.path) && local.in_scope(&c.path) {
                old.push((c.path.clone(), fingerprint(&c.path, &c.text)));
            }
        }
        old.sort();
        old.dedup();
        for (path, print) in old {
            let lives = self.state.files.values().any(|t| !t.deleted && t.held() && t.path == path);
            if !lives && local.read(&path).is_some_and(|h| !h.trim().is_empty() && fingerprint(&path, &h) == print) {
                remove(local, &path)?;
            }
        }
        for (i, r) in revs.into_iter().enumerate() {
            let wanted = match self.state.files.get(&r.file) {
                None => latest.get(&r.file) == Some(&i),
                // Waiting to be fetched or brought up to date: its current
                // revision or a newer one.
                Some(t) if Self::waiting(&*local, t) || Self::rejoining(t) => {
                    latest.get(&r.file) == Some(&i) && (t.head == r.rev || t.head_seq < r.seq)
                }
                Some(t) => t.head_seq < r.seq && t.head != r.rev,
            };
            if wanted {
                let c = match opened.remove(&i) {
                    Some(c) => c,
                    None => Content::open(&self.keys, &self.space, &r)?,
                };
                // If applying fails (say the file changed underneath us), the
                // cursor stays put and this revision is applied next time.
                self.apply(local, server, &r, &c, today, report)?;
                report.pulled += 1;
            }
            self.state.cursor = self.state.cursor.max(r.seq);
        }
        Ok(())
    }

    fn apply(
        &mut self,
        local: &mut dyn LocalStore,
        server: &dyn Transport,
        r: &Revision,
        c: &Content,
        today: &str,
        report: &mut Report,
    ) -> Result<(), SyncError> {
        local.incoming(c);
        // Inside a selection that grew: fetched like a note never seen.
        let waiting = self.state.files.get(&r.file).filter(|t| Self::waiting(&*local, t)).map(|t| t.kept.clone());
        let tracked = if waiting.is_some() { None } else { self.state.files.get(&r.file).cloned() };
        let Some(t) = tracked else {
            // A note we haven't seen.
            let mut path = c.path.clone();
            if !c.deleted && local.in_scope(&c.path) {
                if self.same_note_as_holder(local, r, c) {
                    return Ok(());
                }
                // The same name may have been created here too; that file
                // keeps its name and this note takes the next free one.
                path = self.claim_path(local, &c.path, &r.file)?;
                // A file there that is an older version of this very note (a
                // copy from before the selection grew, or before this state
                // started) is brought up to date, not copied aside; one this
                // device changed meanwhile takes the note's changes in.
                let text = self.arriving(local, server, &path, &r.file, c, report)?;
                write(local, &path, &text)?;
            }
            self.track_at(r, c, &path);
            if let Some(kept) = waiting.flatten().filter(|_| !local.in_scope(&path))
                && let Some(t) = self.state.files.get_mut(&r.file)
            {
                // Still out of reach here: the copy kept from before stays known.
                t.kept = Some(kept);
            }
            return Ok(());
        };

        // Normally the last synced text is the ancestor. If this device's
        // last revision lost a tie in a synced folder, its changes aren't in
        // `r`: merge from the revision both share instead, so they're kept.
        // A kept copy coming back is compared with the text it was kept with.
        let shared = if r.parents.first() == Some(&t.head) || Self::rejoining(&t) { None } else { server.fork_base(&r.file, &t.head, r) };
        let lost_tie = shared.is_some();
        let (base, base_deleted) = match shared {
            Some(shared) => {
                let s = Content::open(&self.keys, &self.space, &shared)?;
                (s.text, s.deleted)
            }
            None => (t.base.clone(), t.deleted),
        };
        if !local.in_scope(&t.path) {
            // Outside the selection here: nothing local to protect. It is
            // written only if it moved into the selection. A copy from
            // before may stay at its old path (see `SyncState::left`).
            if c.path != t.path {
                let base = t.kept.clone().filter(|k| k.path == t.path).map(|k| k.base);
                self.state.left.insert(t.path.clone(), LeftCopy { file: r.file.clone(), base });
            }
            let kept = t.kept.clone();
            if !c.deleted && local.in_scope(&c.path) {
                let to = self.claim_path(local, &c.path, &r.file)?;
                if let Some(k) = kept {
                    // The copy kept from before: taken in below if it is where
                    // the note lands, otherwise left where it is (see `left`).
                    self.state.left.insert(k.path, LeftCopy { file: r.file.clone(), base: Some(k.base) });
                }
                let text = self.arriving(local, server, &to, &r.file, c, report)?;
                write(local, &to, &text)?;
                self.track_at(r, c, &to);
            } else {
                self.track_at(r, c, &c.path);
                if let Some(t) = self.state.files.get_mut(&r.file) {
                    t.outside = true;
                    t.kept = kept;
                }
            }
            return Ok(());
        }
        // A deleted note's old path may hold another note now (one it was
        // filed as the same as, or a new one): that file isn't this note's.
        let here = if t.deleted && self.held_by_other(&t.path, &r.file) { None } else { local.read(&t.path) };
        let changed_here = match (&here, base_deleted) {
            (None, deleted) => !deleted,
            (Some(_), true) => true,
            (Some(x), false) => !same_as_base(&t.path, x, &base),
        };
        if !c.deleted && !local.in_scope(&c.path) {
            // Moved out of the selection on another device. Unchanged here:
            // it leaves this folder (to the trash). Changed here: what was
            // typed here follows the note when it merges cleanly (sent as
            // its next version); otherwise this device's text stays where it
            // is and is sent as a note of its own, so nothing is lost.
            if let Some(mine) = here.as_ref().filter(|_| changed_here) {
                let merged = (!is_attachment(&t.path) && !base_deleted)
                    .then(|| merge::merge3(&base, mine, &c.text))
                    .filter(merge::MergeResult::is_clean);
                if let Some(m) = merged {
                    let content = Content {
                        actor: local.actor(&t.path),
                        path: c.path.clone(),
                        deleted: false,
                        text: m.text,
                        from: String::new(),
                        at: 0,
                    };
                    if self.push_one(server, &r.file, Some(&r.rev), &content, report)? {
                        remove(local, &t.path)?;
                        report.merged.push(c.path.clone());
                    }
                    // Otherwise another device changed it again first: this
                    // device's text merges with that version when it arrives.
                    return Ok(());
                }
                report.conflict_copies.push(t.path.clone());
            } else {
                remove(local, &t.path)?;
            }
            self.track_at(r, c, &c.path);
            return Ok(());
        }

        if !changed_here {
            // Nothing to protect: take the remote version.
            let to = if c.deleted { t.path.clone() } else { self.claim_path(local, &c.path, &r.file)? };
            if (t.path != to || c.deleted) && here.is_some() {
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
                let to = self.claim_path(local, &c.path, &r.file)?;
                self.protect_unsynced(local, &to, &c.text)?;
                write(local, &to, &c.text)?;
                report.kept_over_delete.push(to.clone());
                self.track_at(r, c, &to);
            }
            // Deleted on both sides.
            (None, true) => self.track_at(r, c, &t.path),
            // Edited on both sides.
            (Some(mine), false) => {
                let to = self.claim_path(local, &c.path, &r.file)?;
                if t.path != to {
                    remove(local, &t.path)?;
                }
                if t.path != to {
                    self.protect_unsynced(local, &to, &mine)?;
                }
                // Attachments can't be merged: both versions are kept.
                let merged = (!is_attachment(&to)).then(|| merge::merge3(&base, &mine, &c.text)).filter(merge::MergeResult::is_clean);
                if let Some(m) = merged {
                    write(local, &to, &m.text)?;
                    report.merged.push(to.clone());
                } else if lost_tie {
                    // This device's version lost a tie (a synced folder): the
                    // winner stays, as on every other device, and this device's
                    // text goes beside it. Each losing device keeps only its own
                    // text, so the same version is never copied twice.
                    write(local, &to, &c.text)?;
                    let copy = self.conflict_copy_path(local, &to, &self.device_name, today);
                    write(local, &copy, &mine)?;
                    report.conflict_copies.push(copy);
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

    /// Every version of the note at `path` that the server (or sync folder)
    /// holds, from every device, oldest first. Includes versions that lost
    /// a tie in a synced folder, so nothing written anywhere is out of reach.
    pub fn history(&self, server: &dyn Transport, path: &str) -> Result<Vec<Version>, SyncError> {
        let Some((file, _)) = self.state.files.iter().find(|(_, t)| t.path == path || t.synced_path == path) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Version> = server
            .history(file)?
            .into_iter()
            .map(|r| {
                let c = Content::open(&self.keys, &self.space, &r)?;
                Ok(Version { rev: r.rev, seq: r.seq, at: c.at, device: c.from, path: c.path, deleted: c.deleted, text: c.text })
            })
            .collect::<Result<_, CryptoError>>()?;
        // Servers number versions; a synced folder can't, so it goes by when they were written.
        out.sort_by(|a, b| (a.seq, a.at).cmp(&(b.seq, b.at)).then_with(|| a.rev.cmp(&b.rev)));
        Ok(out)
    }

    fn is_tracked_path(&self, path: &str) -> bool {
        self.state.files.values().any(|t| !t.deleted && t.path == path)
    }

    /// Whether a live note held here, other than `file`, is at `path`.
    fn held_by_other(&self, path: &str, file: &FileId) -> bool {
        self.state.files.iter().any(|(id, t)| id != file && !t.deleted && t.held() && t.path == path)
    }

    /// A new note `r` whose path a live note here already holds with exactly
    /// its text: the same note, made on two devices (an agent's suggestion
    /// accepted on both before they synced). The one with the smaller id
    /// keeps it; the other is tracked as deleted and its delete is sent, so
    /// every device ends with one note, whichever it saw first. Returns
    /// whether `r` was taken in that way (nothing to write).
    fn same_note_as_holder(&mut self, local: &dyn LocalStore, r: &Revision, c: &Content) -> bool {
        let Some(holder) =
            self.state.files.iter().find(|(id, t)| **id != r.file && !t.deleted && t.held() && t.path == c.path).map(|(id, _)| id.clone())
        else {
            return false;
        };
        if !local.read(&c.path).is_some_and(|x| same_as_base(&c.path, &x, &fingerprint(&c.path, &c.text))) {
            return false;
        }
        self.track_at(r, c, &c.path);
        let loser = if r.file < holder { holder } else { r.file.clone() };
        if let Some(t) = self.state.files.get_mut(&loser) {
            t.deleted = true;
            t.absorbed = true;
        }
        true
    }

    /// Where an incoming note goes when it wants `wanted`. If another synced
    /// note is there, the one with the smaller id keeps the name and the
    /// other gets a numbered one, so every device files them the same way
    /// whichever it saw first. Moving a note here is pushed like any rename.
    fn claim_path(&mut self, local: &mut dyn LocalStore, wanted: &str, file: &FileId) -> Result<String, SyncError> {
        let holder =
            self.state.files.iter().find(|(id, t)| *id != file && !t.deleted && t.held() && t.path == wanted).map(|(id, _)| id.clone());
        let Some(holder) = holder.filter(|h| file < h) else { return Ok(self.free_path(local, wanted, file)) };
        let (stem, ext) = split_ext(wanted);
        let aside = (2..10_000)
            .map(|n| format!("{stem} ({n}){ext}"))
            .find(|p| local.read(p).is_none() && !self.is_tracked_path(p))
            .unwrap_or_else(|| format!("{stem} (moved){ext}"));
        if let Some(text) = local.read(wanted) {
            write(local, &aside, &text)?;
            remove(local, wanted)?;
        }
        if let Some(t) = self.state.files.get_mut(&holder) {
            t.path = aside;
        }
        Ok(wanted.to_string())
    }

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
        // A note tracked but not held here (waiting to be fetched) doesn't
        // make the file at its path its own.
        let tracked = self.state.files.values().any(|t| t.path == path && t.held());
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
                base: fingerprint(&c.path, &c.text),
                deleted: c.deleted,
                outside: false,
                absorbed: false,
                kept: None,
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
                outside: false,
                absorbed: false,
                kept: None,
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
        let mut known: BTreeSet<String> = self.state.files.values().map(|t| t.path.clone()).collect();
        let mut untracked: Vec<String> = present.iter().filter(|p| !known.contains(*p)).cloned().collect();

        let ids: Vec<FileId> = self.state.files.keys().cloned().collect();
        for id in ids {
            let t = self.state.files[&id].clone();
            if t.outside || !local.in_scope(&t.path) || Self::rejoining(&t) {
                // Not held here (selective sync): never pushed, so never
                // deleted. A kept copy waits until it's brought up to date.
                continue;
            }
            if t.absorbed {
                // Filed as the same note as the one at its path: tell the others.
                let content = Content { path: t.path.clone(), deleted: true, text: String::new(), from: String::new(), at: 0, actor: None };
                if !self.push_one(server, &id, Some(&t.head), &content, report)? {
                    return Ok(false);
                }
                continue;
            }
            // Compare fingerprints first, so unchanged attachments aren't read.
            let here = if present.contains(&t.path) { local.digest(&t.path) } else { None };
            let full = |p: &str| local.read(p);
            let content = match (here, t.deleted) {
                (Some(_), true) if live.contains(&t.path) => continue,
                (Some(_), true) => {
                    let Some(x) = full(&t.path) else { continue };
                    Content { actor: local.actor(&t.path), path: t.path.clone(), deleted: false, text: x, from: String::new(), at: 0 }
                }
                (Some(x), false) if x != t.base || t.path != t.synced_path => {
                    let Some(x) = (if x == t.base && !is_attachment(&t.path) { Some(x) } else { full(&t.path) }) else { continue };
                    Content { actor: local.actor(&t.path), path: t.path.clone(), deleted: false, text: x, from: String::new(), at: 0 }
                }
                (Some(_), false) | (None, true) => continue,
                (None, false) => {
                    // Gone from its path: renamed (same contents elsewhere) or deleted.
                    if let Some(i) = untracked.iter().position(|p| local.digest(p).as_deref() == Some(t.base.as_str())) {
                        let to = untracked.remove(i);
                        let Some(text) = full(&to) else { continue };
                        Content { actor: local.actor(&to), path: to, deleted: false, text, from: String::new(), at: 0 }
                    } else if let Some((to, text)) = (!t.base.trim().is_empty())
                        .then(|| local.moved_out(&t.base, &known))
                        .flatten()
                        .filter(|(to, text)| !known.contains(to) && !text.trim().is_empty())
                    {
                        // Each file is where one note went, at most.
                        known.insert(to.clone());
                        // Moved into a folder this device doesn't sync: a rename, not a delete.
                        Content { actor: None, path: to, deleted: false, text, from: String::new(), at: 0 }
                    } else {
                        Content { path: t.path.clone(), deleted: true, text: String::new(), from: String::new(), at: 0, actor: None }
                    }
                }
            };
            if !self.push_one(server, &id, Some(&t.head), &content, report)? {
                return Ok(false);
            }
            if !content.deleted
                && !local.in_scope(&content.path)
                && !is_attachment(&content.path)
                && let Some(t) = self.state.files.get_mut(&id)
            {
                // Moved here into a folder this device doesn't sync: the file
                // stays, so a wider selection later merges what's typed in it.
                t.kept = Some(KeptCopy { path: content.path.clone(), base: content.text.clone() });
            }
        }

        for path in untracked {
            let Some(text) = local.read(&path) else { continue };
            let id = FileId::random()?;
            let content = Content { actor: local.actor(&path), path, deleted: false, text, from: String::new(), at: 0 };
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
        let at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
        let content = &Content { from: self.device_name.clone(), at, ..content.clone() };
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
                        base: fingerprint(&content.path, &content.text),
                        deleted: content.deleted,
                        outside: false,
                        absorbed: false,
                        kept: None,
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
