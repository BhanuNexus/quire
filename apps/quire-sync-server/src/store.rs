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
    /// 0.13: a key record for an epoch that isn't the next one.
    #[error("another device changed the vault's key at the same moment; try again")]
    KeyConflict,
    /// 0.13: a revision sealed under a key epoch older than the space's.
    #[error("this revision is sealed with the vault's old key; take the new key first")]
    OldKey,
    /// 0.13: the device was removed from the space.
    #[error("this device was removed from the sync vault")]
    Removed,
    /// 0.13: the device already has a token of its own.
    #[error("this device already has a device token; to sync it again under a new token, turn sync off and on on it (a new device)")]
    Enrolled,
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
-- 0.9: the oldest sync protocol a space accepts (design §B.5).
CREATE TABLE IF NOT EXISTS space_settings (
    space        TEXT PRIMARY KEY REFERENCES spaces(id),
    min_protocol INTEGER NOT NULL DEFAULT 1
);
-- 0.13: a token per device, its record, and when it was removed.
CREATE TABLE IF NOT EXISTS devices (
    space      TEXT NOT NULL REFERENCES spaces(id),
    device     TEXT NOT NULL,
    token_hash BLOB NOT NULL UNIQUE,
    created    INTEGER NOT NULL DEFAULT (unixepoch()),
    removed    INTEGER,
    record     TEXT,
    enrolled   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (space, device)
);
-- 0.13: the vault's key changes, one per epoch (compare-and-swap).
CREATE TABLE IF NOT EXISTS key_records (
    space  TEXT NOT NULL REFERENCES spaces(id),
    epoch  INTEGER NOT NULL,
    record TEXT NOT NULL,
    PRIMARY KEY (space, epoch)
);
";

/// A device of a space, as the server knows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceRow {
    pub device: String,
    /// It has a device token (0.13).
    pub enrolled: bool,
    pub removed: bool,
    /// Its newest revision in the log.
    pub last_seq: u64,
    /// What it published about itself (sealed name, public key, version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<serde_json::Value>,
}

/// Who a device token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOwner {
    pub space: SpaceId,
    pub device: String,
    pub removed: bool,
}

/// A new random device token (256 bits, hex).
fn new_token() -> Result<String, StoreError> {
    let a = quire_sync::DeviceId::random().map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let b = quire_sync::DeviceId::random().map_err(|e| StoreError::Corrupt(e.to_string()))?;
    Ok(format!("qd_{}{}", a.0, b.0))
}

/// How tokens are kept: their SHA-256.
pub fn token_hash(token: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).to_vec()
}

type Row = (u64, String, String, String, String, Vec<u8>, String);

fn row_to_rev(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((r.get::<_, i64>(0)?.cast_unsigned(), r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
}

fn to_revision(t: Row) -> Result<Revision, StoreError> {
    let (seq, file, rev, parents, device, sealed, chain) = t;
    let parents: Vec<RevId> = serde_json::from_str(&parents).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let chain: Vec<RevId> = serde_json::from_str(&chain).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    Ok(Revision { file: FileId(file), rev: RevId(rev), parents, seq, device: quire_sync::DeviceId(device), sealed, chain })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Connection::open(path)?;
        Self::migrate(&db)?;
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let db = Connection::open_in_memory()?;
        Self::migrate(&db)?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn migrate(db: &Connection) -> Result<(), StoreError> {
        db.execute_batch(SCHEMA)?;
        // 0.13: revisions name their first-parent ancestors (bound into v3 sealing).
        let has_chain: bool =
            db.query_row("SELECT COUNT(*) FROM pragma_table_info('revisions') WHERE name = 'chain'", [], |r| r.get::<_, i64>(0))? > 0;
        if !has_chain {
            db.execute_batch("ALTER TABLE revisions ADD COLUMN chain TEXT NOT NULL DEFAULT '[]'")?;
        }
        // 0.13: whether a row's token is a device's own (rows made by a
        // removal or a device record hold a placeholder).
        let has_enrolled: bool =
            db.query_row("SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'enrolled'", [], |r| r.get::<_, i64>(0))? > 0;
        if !has_enrolled {
            db.execute_batch(
                "ALTER TABLE devices ADD COLUMN enrolled INTEGER NOT NULL DEFAULT 0;
                 UPDATE devices SET enrolled = 1 WHERE record IS NULL AND removed IS NULL;",
            )?;
        }
        Ok(())
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
            "SELECT seq, file, rev, parents, device, sealed, chain FROM revisions WHERE space = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
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
            "SELECT seq, file, rev, parents, device, sealed, chain FROM revisions WHERE space = ?1 AND file = ?2 ORDER BY seq DESC LIMIT 500",
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

    /// The oldest protocol `space` accepts (1 unless raised).
    pub fn min_protocol(&self, space: &SpaceId) -> Result<u32, StoreError> {
        let db = self.db();
        let v: Option<i64> =
            db.query_row("SELECT min_protocol FROM space_settings WHERE space = ?1", params![space.0], |r| r.get(0)).optional()?;
        Ok(v.map_or(1, |n| u32::try_from(n).unwrap_or(1)))
    }

    /// Raise the oldest protocol `space` accepts; never lowers it. Returns
    /// the value in force.
    pub fn raise_min_protocol(&self, space: &SpaceId, min: u32) -> Result<u32, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        db.execute(
            "INSERT INTO space_settings (space, min_protocol) VALUES (?1, ?2)
             ON CONFLICT(space) DO UPDATE SET min_protocol = max(min_protocol, excluded.min_protocol)",
            params![space.0, i64::from(min)],
        )?;
        let v: i64 = db.query_row("SELECT min_protocol FROM space_settings WHERE space = ?1", params![space.0], |r| r.get(0))?;
        Ok(u32::try_from(v).unwrap_or(1))
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
        // 0.13: once the vault's key changed, only revisions sealed under
        // the newest key are taken (a lagging device takes the key and retries).
        let epoch: i64 =
            tx.query_row("SELECT COALESCE(MAX(epoch), 0) FROM key_records WHERE space = ?1", params![space.0], |r| r.get(0))?;
        if i64::from(quire_sync::crypto::sealed_epoch(&rev.sealed).unwrap_or(0)) < epoch {
            return Err(StoreError::OldKey);
        }
        let head: Option<Row> = tx
            .query_row(
                "SELECT r.seq, r.file, r.rev, r.parents, r.device, r.sealed, r.chain FROM heads h
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
        let chain = serde_json::to_string(&rev.chain).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        tx.execute(
            "INSERT INTO revisions (space, seq, file, rev, parents, device, sealed, chain) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![space.0, seq, rev.file.0, rev.rev.0, parents, rev.device.0, rev.sealed, chain],
        )?;
        tx.execute(
            "INSERT INTO heads (space, file, seq) VALUES (?1, ?2, ?3) ON CONFLICT (space, file) DO UPDATE SET seq = excluded.seq",
            params![space.0, rev.file.0, seq],
        )?;
        tx.commit()?;
        Ok(PushOutcome::Accepted { seq: seq.cast_unsigned() })
    }

    /// Give `device` a token for `space` (0.13), once: a device that has
    /// one gets no other (whoever holds the server's token could otherwise
    /// take over its identity), and a removed device gets none.
    pub fn enroll(&self, space: &SpaceId, device: &str) -> Result<String, StoreError> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::require_space(&tx, space)?;
        let row: Option<(Option<i64>, i64)> = tx
            .query_row("SELECT removed, enrolled FROM devices WHERE space = ?1 AND device = ?2", params![space.0, device], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match row {
            Some((Some(_), _)) => return Err(StoreError::Removed),
            Some((None, e)) if e != 0 => return Err(StoreError::Enrolled),
            _ => {}
        }
        let token = new_token()?;
        tx.execute(
            "INSERT INTO devices (space, device, token_hash, enrolled) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT (space, device) DO UPDATE SET token_hash = excluded.token_hash, enrolled = 1",
            params![space.0, device, token_hash(&token)],
        )?;
        tx.commit()?;
        Ok(token)
    }

    /// Whose device token this is.
    pub fn token_owner(&self, token: &str) -> Result<Option<TokenOwner>, StoreError> {
        let db = self.db();
        Ok(db
            .query_row("SELECT space, device, removed FROM devices WHERE token_hash = ?1", params![token_hash(token)], |r| {
                Ok(TokenOwner { space: SpaceId(r.get(0)?), device: r.get(1)?, removed: r.get::<_, Option<i64>>(2)?.is_some() })
            })
            .optional()?)
    }

    /// Stop taking `device`'s token, for good. A device that never had one
    /// (it wrote with the server's token) is recorded as removed too.
    pub fn remove_device(&self, space: &SpaceId, device: &str) -> Result<(), StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let placeholder = token_hash(&new_token()?);
        db.execute(
            "INSERT INTO devices (space, device, token_hash, removed) VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT (space, device) DO UPDATE SET removed = COALESCE(removed, unixepoch())",
            params![space.0, device, placeholder],
        )?;
        Ok(())
    }

    /// Store what a device says about itself.
    pub fn set_device_record(&self, space: &SpaceId, device: &str, record: &str) -> Result<(), StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let placeholder = token_hash(&new_token()?);
        db.execute(
            "INSERT INTO devices (space, device, token_hash, record) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (space, device) DO UPDATE SET record = excluded.record",
            params![space.0, device, placeholder, record],
        )?;
        Ok(())
    }

    /// Every device of a space: enrolled ones and those that wrote revisions.
    pub fn devices(&self, space: &SpaceId) -> Result<Vec<DeviceRow>, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let mut out: std::collections::BTreeMap<String, DeviceRow> = std::collections::BTreeMap::new();
        let mut stmt = db.prepare_cached("SELECT device, MAX(seq) FROM revisions WHERE space = ?1 GROUP BY device")?;
        for row in stmt.query_map(params![space.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.cast_unsigned())))? {
            let (device, last_seq) = row?;
            out.insert(device.clone(), DeviceRow { device, enrolled: false, removed: false, last_seq, record: None });
        }
        let mut stmt = db.prepare_cached("SELECT device, removed, record, enrolled FROM devices WHERE space = ?1")?;
        for row in stmt.query_map(params![space.0], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.is_some(), r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)? != 0))
        })? {
            let (device, removed, record, enrolled) = row?;
            let e = out.entry(device.clone()).or_insert(DeviceRow { device, enrolled: false, removed: false, last_seq: 0, record: None });
            e.enrolled = enrolled;
            e.removed = removed;
            e.record = record.and_then(|r| serde_json::from_str(&r).ok());
        }
        Ok(out.into_values().collect())
    }

    /// The vault's key records, oldest first.
    pub fn key_records(&self, space: &SpaceId) -> Result<Vec<serde_json::Value>, StoreError> {
        let db = self.db();
        Self::require_space(&db, space)?;
        let mut stmt = db.prepare_cached("SELECT record FROM key_records WHERE space = ?1 ORDER BY epoch")?;
        let rows = stmt.query_map(params![space.0], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?).map_err(|e| StoreError::Corrupt(e.to_string()))?);
        }
        Ok(out)
    }

    /// Add the key record of `epoch`, only if it is the next one.
    pub fn put_key_record(&self, space: &SpaceId, epoch: u32, record: &str) -> Result<(), StoreError> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::require_space(&tx, space)?;
        let max: i64 = tx.query_row("SELECT COALESCE(MAX(epoch), 0) FROM key_records WHERE space = ?1", params![space.0], |r| r.get(0))?;
        if i64::from(epoch) != max + 1 {
            return Err(StoreError::KeyConflict);
        }
        tx.execute("INSERT INTO key_records (space, epoch, record) VALUES (?1, ?2, ?3)", params![space.0, i64::from(epoch), record])?;
        tx.commit()?;
        Ok(())
    }

    /// Fold old revisions away (0.13): of each file, keep its newest
    /// `keep_versions` revisions (at least two: a deleted note's head is
    /// its tombstone, and the one before holds its last text) and every revision
    /// replaced within the last `keep_days` days. Returns how many went.
    /// A device whose cursor is older gets each file's newest revision
    /// and merges from what it last synced, as when it pulls a long log.
    pub fn compact(&self, keep_days: u32, keep_versions: u32) -> Result<usize, StoreError> {
        let db = self.db();
        let cutoff = i64::from(keep_days) * 86_400;
        let n = db.execute(
            "DELETE FROM revisions WHERE rowid IN (
               SELECT id FROM (
                 SELECT rowid AS id,
                        ROW_NUMBER() OVER (PARTITION BY space, file ORDER BY seq DESC) AS rk,
                        LEAD(created) OVER (PARTITION BY space, file ORDER BY seq) AS replaced
                 FROM revisions)
               WHERE rk > ?1 AND replaced IS NOT NULL AND replaced < unixepoch() - ?2)",
            params![i64::from(keep_versions.max(2)), cutoff],
        )?;
        Ok(n)
    }
}
