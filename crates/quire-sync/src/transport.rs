// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! How the engine reaches a server, and an in-memory server for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::protocol::{FileId, PushOutcome, RevId, Revision, Sealing};

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
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let head = log.heads.get(&rev.file).copied();
        let head_rev = head.map(|i| &log.revs[i].rev);
        if head_rev != rev.parents.first() {
            return match head {
                Some(i) => Ok(PushOutcome::Conflict { head: log.revs[i].clone() }),
                None => Err(TransportError::Refused("parent doesn't exist".into())),
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

    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        self.check_online()?;
        Ok(self.log.lock().unwrap_or_else(PoisonError::into_inner).revs.iter().filter(|r| r.file == *file).cloned().collect())
    }
}
