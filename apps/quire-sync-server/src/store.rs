// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `SQLite` storage: spaces, their wrapped keys, and each space's revision log.
//!
//! Everything stored here is either an opaque id, a number or ciphertext.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use quire_sync::{FileId, PushOutcome, RevId, Revision, SpaceId, WrappedKey};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("no such space")]
    NoSpace,
    #[error("that space already exists")]
    SpaceExists,
    #[error("the revision's parent doesn't exist")]
    BadParent,
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

/// Roughly how much sealed data one page of changes carries.
const PAGE_BYTES: usize = 8 * 1024 * 1024;
/// The most sealed data one history reply carries.
const HISTORY_BYTES: usize = 64 * 1024 * 1024;

pub struct Store {
    db: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS spaces (
    id          TEXT PRIMARY KEY,
    wrapped_key TEXT NOT NULL,
    created     INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE IF NOT EXISTS revisions (
    space   TEXT NOT NULL REFERENCES spaces(id),
    seq     INTEGER NOT NULL,
    file    TEXT NOT NULL,
    rev     TEXT NOT NULL,
    parents TEXT NOT NULL,
    device  TEXT NOT NULL,
    sealed  BLOB NOT NULL,
    created INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (space, seq)
);
CREATE TABLE IF NOT EXISTS heads (
    space TEXT NOT NULL REFERENCES spaces(id),
    file  TEXT NOT NULL,
    seq   INTEGER NOT NULL,
    PRIMARY KEY (space, file)
);
CREATE INDEX IF NOT EXISTS revisions_by_file ON revisions (space, file, seq);
";

fn row_to_rev(r: &rusqlite::Row<'_>) -> rusqlite::Result<(u64, String, String, String, String, Vec<u8>)> {
    Ok((r.get::<_, i64>(0)?.cast_unsigned(), r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
}

fn to_revision(t: (u64, String, String, String, String, Vec<u8>)) -> Result<Revision, StoreError> {
    let (seq, file, rev, parents, device, sealed) = t;
    let parents: Vec<RevId> = serde_json::from_str(&parents).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    Ok(Revision { file: FileId(file), rev: RevId(rev), parents, seq, device: quire_sync::DeviceId(device), sealed })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn create_space(&self, space: &SpaceId, key: &WrappedKey) -> Result<(), StoreError> {
        let json = serde_json::to_string(key).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let res = self.db().execute("INSERT INTO spaces (id, wrapped_key) VALUES (?1, ?2)", params![space.0, json]);
        match res {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => Err(StoreError::SpaceExists),
            Err(e) => Err(e.into()),
        }
    }

    pub fn wrapped_key(&self, space: &SpaceId) -> Result<WrappedKey, StoreError> {
        let json: String = self
            .db()
            .query_row("SELECT wrapped_key FROM spaces WHERE id = ?1", params![space.0], |r| r.get(0))
            .optional()?
            .ok_or(StoreError::NoSpace)?;
        serde_json::from_str(&json).map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    /// Every space: id, creation time (unix seconds) and how many revisions it holds.
    pub fn spaces(&self) -> Result<Vec<(SpaceId, i64, u64)>, StoreError> {
        let db = self.db();
        let mut stmt = db.prepare_cached(
            "SELECT s.id, s.created, (SELECT COUNT(*) FROM revisions r WHERE r.space = s.id) FROM spaces s ORDER BY s.created, s.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((SpaceId(r.get(0)?), r.get(1)?, r.get::<_, i64>(2)?.cast_unsigned())))?;
        rows.collect::<Result<_, _>>().map_err(StoreError::from)
    }

    /// Replace the wrapped key (after a passphrase change).
    pub fn set_wrapped_key(&self, space: &SpaceId, key: &WrappedKey) -> Result<(), StoreError> {
        let json = serde_json::to_string(key).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let n = self.db().execute("UPDATE spaces SET wrapped_key = ?2 WHERE id = ?1", params![space.0, json])?;
        if n == 0 { Err(StoreError::NoSpace) } else { Ok(()) }
    }

    fn require_space(db: &Connection, space: &SpaceId) -> Result<(), StoreError> {
        let found: Option<i64> = db.query_row("SELECT 1 FROM spaces WHERE id = ?1", params![space.0], |r| r.get(0)).optional()?;
        found.map(|_| ()).ok_or(StoreError::NoSpace)
    }

    /// Revisions with `seq > after`, at most `limit`, in order.
    pub fn changes(&self, space: &SpaceId, after: u64, limit: u32) -> Result<Vec<Revision>, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let mut stmt = db.prepare_cached(
            "SELECT seq, file, rev, parents, device, sealed FROM revisions WHERE space = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![space.0, after.cast_signed(), limit], row_to_rev)?;
        // Pages also stop at about 8 MB (attachments make big revisions); the
        // client keeps asking until it reaches `latest`.
        let mut out = Vec::new();
        let mut bytes = 0;
        for r in rows {
            let rev = to_revision(r?)?;
            bytes += rev.sealed.len();
            out.push(rev);
            if bytes > PAGE_BYTES {
                break;
            }
        }
        Ok(out)
    }

    /// A file's revisions, oldest first: the newest 500, and fewer if they'd
    /// pass about 64 MB (big attachments).
    pub fn file_history(&self, space: &SpaceId, file: &FileId) -> Result<Vec<Revision>, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let mut stmt = db.prepare_cached(
            "SELECT seq, file, rev, parents, device, sealed FROM revisions WHERE space = ?1 AND file = ?2 ORDER BY seq DESC LIMIT 500",
        )?;
        let rows = stmt.query_map(params![space.0, file.0], row_to_rev)?;
        let mut out = Vec::new();
        let mut bytes = 0;
        for r in rows {
            let rev = to_revision(r?)?;
            bytes += rev.sealed.len();
            if bytes > HISTORY_BYTES && !out.is_empty() {
                break;
            }
            out.push(rev);
        }
        out.reverse();
        Ok(out)
    }

    pub fn latest(&self, space: &SpaceId) -> Result<u64, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let n: i64 = db.query_row("SELECT COALESCE(MAX(seq), 0) FROM revisions WHERE space = ?1", params![space.0], |r| r.get(0))?;
        Ok(n.cast_unsigned())
    }

    /// Append `rev` if its first parent is still its file's head.
    pub fn push(&self, space: &SpaceId, rev: &Revision) -> Result<PushOutcome, StoreError> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::require_space(&tx, space)?;
        let head: Option<(u64, String, String, String, String, Vec<u8>)> = tx
            .query_row(
                "SELECT r.seq, r.file, r.rev, r.parents, r.device, r.sealed FROM heads h
                 JOIN revisions r ON r.space = h.space AND r.seq = h.seq
                 WHERE h.space = ?1 AND h.file = ?2",
                params![space.0, rev.file.0],
                row_to_rev,
            )
            .optional()?;
        let head_rev = head.as_ref().map(|h| h.2.as_str());
        if head_rev != rev.parents.first().map(|p| p.0.as_str()) {
            return match head {
                Some(h) => Ok(PushOutcome::Conflict { head: to_revision(h)? }),
                None => Err(StoreError::BadParent),
            };
        }
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM revisions WHERE space = ?1", params![space.0], |r| r.get(0))?;
        let parents = serde_json::to_string(&rev.parents).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        tx.execute(
            "INSERT INTO revisions (space, seq, file, rev, parents, device, sealed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![space.0, seq, rev.file.0, rev.rev.0, parents, rev.device.0, rev.sealed],
        )?;
        tx.execute(
            "INSERT INTO heads (space, file, seq) VALUES (?1, ?2, ?3) ON CONFLICT (space, file) DO UPDATE SET seq = excluded.seq",
            params![space.0, rev.file.0, seq],
        )?;
        tx.commit()?;
        Ok(PushOutcome::Accepted { seq: seq.cast_unsigned() })
    }
}
