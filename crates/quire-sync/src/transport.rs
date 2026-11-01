// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! How the engine reaches a server, and an in-memory server for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::protocol::{FileId, PushOutcome, Revision};

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
}
