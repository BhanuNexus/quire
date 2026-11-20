// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! How the engine reaches a server, and an in-memory server for tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::protocol::{FileId, PushOutcome, RevId, Revision, Sealing};

/// What a transport says when a revision builds on one it doesn't have
/// (lost for good, or folded away): the engine sends the note again as a
/// new first revision.
pub const LOST_PARENT: &str = "parent doesn't exist";

/// The most revisions the engine hands [`Transport::push_batch`] at once
/// (`docs/design/0.14-sync-speed.md` §3.4).
pub const BATCH_REVISIONS: usize = 1000;
/// The most request body one batch takes: the revisions' JSON as sent
/// (sealed bytes hex-encoded). A revision whose JSON alone is bigger (a
/// large attachment) goes through [`Transport::push`].
pub const BATCH_BYTES: usize = 4 * 1024 * 1024;

/// The size of `rev` in a request body: its JSON.
pub fn request_len(rev: &Revision) -> usize {
    // The JSON of the header fields, plus the sealed bytes in hex.
    let ids = rev.file.0.len() + rev.rev.0.len() + rev.device.0.len();
    let links: usize = rev.parents.iter().chain(&rev.chain).map(|r| r.0.len() + 3).sum();
    120 + ids + links + rev.sealed.len() * 2
}

/// How the transport took one revision of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Stored as the file's new head, at this position.
    Accepted { seq: u64 },
    /// Another device got there first: pull, then send it again.
    Conflict,
    /// The revision it builds on is gone for good (see [`LOST_PARENT`]).
    LostParent,
}

/// What [`Transport::push_batch`] did: an outcome for each revision of the
/// prefix it handled, and the error that stopped it, if one did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchReply {
    pub outcomes: Vec<BatchOutcome>,
    pub error: Option<TransportError>,
}

impl BatchReply {
    /// [`Transport::push`] of each revision in turn, until one fails.
    pub fn one_by_one(t: &(impl Transport + ?Sized), revs: &[Revision]) -> Self {
        let mut out = Self::default();
        for r in revs {
            match t.push(r) {
                Ok(PushOutcome::Accepted { seq }) => out.outcomes.push(BatchOutcome::Accepted { seq }),
                Ok(PushOutcome::Conflict { .. }) => out.outcomes.push(BatchOutcome::Conflict),
                Err(TransportError::Refused(why)) if why.contains(LOST_PARENT) => out.outcomes.push(BatchOutcome::LostParent),
                Err(e) => {
                    out.error = Some(e);
                    break;
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("can't reach the sync server: {0}")]
    Offline(String),
    #[error("the sync server refused: {0}")]
    Refused(String),
}

/// A sync server, seen from one device.
pub trait Transport {
    /// Revisions with `seq > after`, in order.
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError>;
    /// Store `rev` if its first parent is still its file's head.
    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError>;

    /// Store each of `revs` in order, as [`Self::push`] would (at most
    /// [`BATCH_REVISIONS`], about [`BATCH_BYTES`]): one request to a server
    /// that takes batches, one log write in a sync folder. The default
    /// pushes them one at a time.
    fn push_batch(&self, revs: &[Revision]) -> BatchReply {
        BatchReply::one_by_one(self, revs)
    }

    /// Whether the positions of this device's accepted pushes can be
    /// skipped by its cursor: true only where one server hands out every
    /// position in commit order, so a position given to this device's
    /// revision can't hold another device's
    /// (`docs/design/0.14-sync-speed.md` §3.4). A synced folder numbers what
    /// it reads per process: false.
    fn own_seqs_skippable(&self) -> bool {
        false
    }

    /// For a pulled `theirs` that doesn't build on `ours` (this device's
    /// last synced revision of `file`): the revision both share, if some
    /// revision only `ours` has was written on this device. Those changes
    /// never reached `theirs`, so this device merges them again from there.
    ///
    /// A server puts every note's revisions in one line, so this never
    /// happens there. It does in a synced folder, when two devices build on
    /// the same revision before seeing each other's.
    fn fork_base(&self, file: &FileId, ours: &RevId, theirs: &Revision) -> Option<Revision> {
        let _ = (file, ours, theirs);
        None
    }

    /// [`Self::fork_base`], also telling apart a fork whose shared revision
    /// is gone: `ours` was written here, `theirs` doesn't build on it, and
    /// the revisions between them were folded into a snapshot (0.13
    /// compaction). The engine can't merge three-way then, so it keeps
    /// both versions.
    fn fork(&self, file: &FileId, ours: &RevId, theirs: &Revision) -> Fork {
        self.fork_base(file, ours, theirs).map_or(Fork::Builds, Fork::Base)
    }

    /// How revisions sent here are sealed (0.13: [`Sealing::Epoch`] in a
    /// vault whose minimum reader is 0.13).
    fn sealing(&self) -> Sealing {
        if self.binds_header() { Sealing::Bound } else { Sealing::Plain }
    }

    /// Whether revisions sent here are sealed with their whole header bound
    /// ([`crate::Content::seal_bound`]): only where no Quire older than 0.12
    /// reads them (S3 and WebDAV). A server and a sync folder keep the old
    /// sealing, which 0.8–0.11 devices read.
    fn binds_header(&self) -> bool {
        false
    }

    /// Every revision of `file` this server holds, in log order.
    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        let _ = file;
        Err(TransportError::Refused("this sync server doesn't keep history; update it".into()))
    }
}

/// See [`Transport::fork`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fork {
    /// `theirs` builds on `ours` (or this device wrote nothing it lacks).
    Builds,
    /// Merge this device's changes again from this shared revision.
    Base(Revision),
    /// Changes written here aren't in `theirs`, and the revision both
    /// share was compacted away: keep both versions.
    Unknown,
}

#[derive(Debug, Default)]
struct Log {
    revs: Vec<Revision>,
    heads: HashMap<FileId, usize>,
}

/// A whole sync server in memory: an append-only log with a
/// compare-and-swap per file. Clones share the same log.
#[derive(Debug, Clone, Default)]
pub struct MemoryServer {
    log: Arc<Mutex<Log>>,
    offline: Arc<Mutex<bool>>,
    /// Requests that sent revisions: single pushes and batches.
    requests: Arc<AtomicUsize>,
    /// Take batches (a server before 0.14 doesn't).
    no_batches: Arc<Mutex<bool>>,
}

impl MemoryServer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate the network going away (pushes and pulls fail).
    pub fn set_offline(&self, off: bool) {
        *self.offline.lock().unwrap_or_else(PoisonError::into_inner) = off;
    }

    pub fn len(&self) -> usize {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).revs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Requests that sent revisions so far (a batch is one).
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// Act as a server before 0.14: every revision is its own request.
    pub fn set_batches(&self, on: bool) {
        *self.no_batches.lock().unwrap_or_else(PoisonError::into_inner) = !on;
    }

    /// Everything the server holds, for checks that it learns nothing.
    pub fn dump(&self) -> Vec<Revision> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).revs.clone()
    }

    fn check_online(&self) -> Result<(), TransportError> {
        if *self.offline.lock().unwrap_or_else(PoisonError::into_inner) { Err(TransportError::Offline("simulated".into())) } else { Ok(()) }
    }
}

impl Transport for MemoryServer {
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        self.check_online()?;
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let start = usize::try_from(after).unwrap_or(usize::MAX).min(log.revs.len());
        Ok(log.revs[start..].to_vec())
    }

    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        self.check_online()?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        Self::push_to(&mut log, rev)
    }

    /// One request, all or nothing, as the server's batch route: a revision
    /// already stored (a batch sent again after its reply was lost) is
    /// accepted at the position it has.
    fn push_batch(&self, revs: &[Revision]) -> BatchReply {
        if *self.no_batches.lock().unwrap_or_else(PoisonError::into_inner) {
            return BatchReply::one_by_one(self, revs);
        }
        if let Err(e) = self.check_online() {
            return BatchReply { outcomes: Vec::new(), error: Some(e) };
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let mut outcomes = Vec::with_capacity(revs.len());
        for r in revs {
            if let Some(i) = log.revs.iter().position(|x| x.rev == r.rev && x.file == r.file) {
                outcomes.push(BatchOutcome::Accepted { seq: log.revs[i].seq });
                continue;
            }
            outcomes.push(match Self::push_to(&mut log, r) {
                Ok(PushOutcome::Accepted { seq }) => BatchOutcome::Accepted { seq },
                Ok(PushOutcome::Conflict { .. }) => BatchOutcome::Conflict,
                Err(_) => BatchOutcome::LostParent,
            });
        }
        BatchReply { outcomes, error: None }
    }

    fn own_seqs_skippable(&self) -> bool {
        true
    }

    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        self.check_online()?;
        Ok(self.log.lock().unwrap_or_else(PoisonError::into_inner).revs.iter().filter(|r| r.file == *file).cloned().collect())
    }
}

impl MemoryServer {
    fn push_to(log: &mut Log, rev: &Revision) -> Result<PushOutcome, TransportError> {
        let head = log.heads.get(&rev.file).copied();
        let head_rev = head.map(|i| &log.revs[i].rev);
        if head_rev != rev.parents.first() {
            return match head {
                Some(i) => Ok(PushOutcome::Conflict { head: log.revs[i].clone() }),
                None => Err(TransportError::Refused(LOST_PARENT.into())),
            };
        }
        let seq = log.revs.len() as u64 + 1;
        let mut stored = rev.clone();
        stored.seq = seq;
        log.revs.push(stored);
        let at = log.revs.len() - 1;
        log.heads.insert(rev.file.clone(), at);
        Ok(PushOutcome::Accepted { seq })
    }
}
