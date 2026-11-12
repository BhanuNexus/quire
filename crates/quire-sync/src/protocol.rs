// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What travels between devices and the server.
//!
//! The server sees [`Revision`]s: ids, parents, an order number and sealed
//! bytes. What a revision says (path and text) is a [`Content`], encrypted
//! and padded so its length gives little away.
//!
//! **Protocol versions** (design 0.9 §B.5). Version 1 is Quire 0.8. Version
//! 2 (0.9) adds `GET /v1/info`, the `Quire-Sync-Protocol` request header, a
//! per-space minimum (`PUT /v1/spaces/{id}/min_protocol`) and an optional
//! `actor` in [`Content`] (who made the change, for attribution on other
//! devices). Version 1 peers ignore `actor`, so mixed fleets keep syncing.

use serde::{Deserialize, Serialize};

use crate::crypto::{CryptoError, Keys, hex_bytes, random_bytes};
use crate::keyring::KeySet;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// A new random id (128 bits, hex).
            pub fn random() -> Result<Self, CryptoError> {
                Ok(Self(crate::crypto::hex(&random_bytes::<16>()?)))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// A vault on the server.
    SpaceId
);
id_type!(
    /// A note, stable across renames.
    FileId
);
id_type!(
    /// One version of a note.
    RevId
);
id_type!(
    /// A device that syncs a space.
    DeviceId
);

/// The newest protocol this build speaks.
///
/// Version 3 (0.13) adds device tokens, key epochs and authenticated
/// revision headers on the server (`docs/design/0.13-sync.md`); a space at
/// minimum 3 refuses older clients.
pub const PROTOCOL: u32 = 3;
/// The protocol of a server space whose revisions bind their whole header,
/// can change keys and accepts device tokens only (0.13).
pub const HARDENED_PROTOCOL: u32 = 3;
/// The oldest protocol this build still speaks.
pub const MIN_PROTOCOL: u32 = 1;
/// The request header that names the client's protocol (absent: version 1).
pub const PROTOCOL_HEADER: &str = "Quire-Sync-Protocol";

/// What `GET /v1/info` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol: ProtocolRange,
    /// The server's version (`0.9.0`).
    #[serde(default)]
    pub server: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

/// Who made a change (design 0.9 §A.1): enough to show "Claude Code" on
/// another device. Absent for changes typed by the person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// `mcp:claude-code`, `import:url`…
    pub id: String,
    pub label: String,
    /// `agent`, `job`, `import`, `human`.
    pub kind: String,
}

/// One version of one file, as the server stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub file: FileId,
    pub rev: RevId,
    /// The revision this one replaces (empty for a new file).
    pub parents: Vec<RevId>,
    /// Order in the space's log, assigned by the server (0 until accepted).
    pub seq: u64,
    pub device: DeviceId,
    #[serde(with = "hex_bytes")]
    pub sealed: Vec<u8>,
    /// 0.13: the next first-parent ancestors after `parents[0]` (its parent
    /// first), at most [`CHAIN`]. A reader that never got one of them (lost
    /// for good) still knows where it sat, so the note's current revision
    /// is the same on every device (`docs/design/0.13-sync.md` §4). Bound
    /// into the sealing, so whoever stores it can't change it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<RevId>,
}

/// How many ancestors a revision names in [`Revision::chain`].
pub const CHAIN: usize = 3;

/// How revisions sent through a transport are sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sealing {
    /// `v1`: space, file and revision bound (read by every Quire since 0.5).
    Plain,
    /// `v2`: the whole header bound (S3 and WebDAV in 0.12).
    Bound,
    /// `v3` (0.13): the whole header, its [`Revision::chain`] and the key
    /// epoch bound, under the vault's current key epoch ([`crate::crypto::SEALED_V2`]).
    /// Only in a vault whose minimum reader is 0.13.
    Epoch,
}

/// Where a line sits in one device's lane.
///
/// A segment of a folder log and the line in it, or (S3, WebDAV, a server)
/// its number. Used for the cut of a snapshot and the last revision taken
/// from a removed device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LanePos {
    /// Folder log segment (`000003.jsonl` is 3); 0 when numbered.
    #[serde(default, skip_serializing_if = "is_zero32")]
    pub seg: u32,
    /// Lines in that segment, counting every whole line.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line: u64,
    /// The object number (S3, WebDAV) or a server's log position.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub n: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
const fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
const fn is_zero32(n: &u32) -> bool {
    *n == 0
}

impl LanePos {
    /// Whether the line at `(seg, line)` with number `n` (0 when it has
    /// none) is at or before this position.
    pub const fn covers(&self, seg: u32, line: u64, n: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.seg == 0 {
            // A numbered position (S3, WebDAV, a server). A line without a
            // number was mirrored before 0.13: older than any position.
            return n == 0 || n <= self.n;
        }
        if n > 0 {
            return false;
        }
        seg < self.seg || (seg == self.seg && line <= self.line)
    }

    /// Nothing covered.
    pub const fn is_empty(&self) -> bool {
        self.seg == 0 && self.line == 0 && self.n == 0
    }
}

/// What a revision contains once decrypted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    /// Vault-relative path with `/` separators.
    pub path: String,
    pub deleted: bool,
    pub text: String,
    /// Name of the device that wrote it ("Asha's laptop"), for conflict copies.
    #[serde(default)]
    pub from: String,
    /// When it was written, Unix milliseconds (0 if unknown). Encrypted with
    /// the rest, so only devices with the key see it.
    #[serde(default)]
    pub at: i64,
    /// Who wrote this version, when it wasn't the person (protocol 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorRef>,
}

/// Padded lengths: 256 bytes, then powers of two up to 64 KiB, then 64 KiB steps.
fn padded_len(n: usize) -> usize {
    if n <= 256 {
        256
    } else if n <= 64 * 1024 {
        n.next_power_of_two()
    } else {
        n.div_ceil(64 * 1024) * 64 * 1024
    }
}

fn aad(space: &SpaceId, file: &FileId, rev: &RevId) -> Vec<u8> {
    format!("quire-sync/rev/v1/{space}/{file}/{rev}").into_bytes()
}

/// 0.13: the whole header, the chain and the key epoch.
fn aad_v3(space: &SpaceId, r: &Revision, epoch: u32) -> Vec<u8> {
    let join = |v: &[RevId]| v.iter().map(|p| p.0.as_str()).collect::<Vec<_>>().join(",");
    format!("quire-sync/rev/v3/{space}/{}/{}/{}/{}/{}/{epoch}", r.file, r.rev, r.device, join(&r.parents), join(&r.chain)).into_bytes()
}

/// The associated data of a revision whose whole header is bound: its
/// `device` and `parents` too, so whoever stores it can't change which
/// revision it builds on or who wrote it (S3 and WebDAV, design 0.12).
fn aad_bound(space: &SpaceId, r: &Revision) -> Vec<u8> {
    let parents: Vec<&str> = r.parents.iter().map(|p| p.0.as_str()).collect();
    format!("quire-sync/rev/v2/{space}/{}/{}/{}/{}", r.file, r.rev, r.device, parents.join(",")).into_bytes()
}

impl Content {
    /// Encrypt into the bytes a [`Revision`] carries.
    pub fn seal(&self, keys: &Keys, space: &SpaceId, file: &FileId, rev: &RevId) -> Result<Vec<u8>, CryptoError> {
        let plain = self.padded()?;
        keys.seal(&plain, &aad(space, file, rev))
    }

    /// Encrypt for `r` (its `sealed` is ignored), binding its whole header:
    /// `parents` and `device` as well as `file` and `rev`. Only Quire 0.12
    /// and later read these, so only transports no older Quire reads use
    /// them ([`crate::Transport::binds_header`]).
    pub fn seal_bound(&self, keys: &Keys, space: &SpaceId, r: &Revision) -> Result<Vec<u8>, CryptoError> {
        let plain = self.padded()?;
        keys.seal(&plain, &aad_bound(space, r))
    }

    /// Encrypt for `r` under the current key epoch of `keys`, binding its
    /// whole header, its chain and the epoch (`v3`, 0.13).
    pub fn seal_epoch(&self, keys: &KeySet, space: &SpaceId, r: &Revision) -> Result<Vec<u8>, CryptoError> {
        let plain = self.padded()?;
        let epoch = keys.current();
        keys.current_keys().seal_epoch(epoch, &plain, &aad_v3(space, r, epoch))
    }

    /// Seal for `r` the way `how` says.
    pub fn seal_as(&self, how: Sealing, keys: &KeySet, space: &SpaceId, r: &Revision) -> Result<Vec<u8>, CryptoError> {
        let base = keys.get(0).unwrap_or_else(|| keys.current_keys());
        match how {
            Sealing::Plain => self.seal(base, space, &r.file, &r.rev),
            Sealing::Bound => self.seal_bound(base, space, r),
            Sealing::Epoch => self.seal_epoch(keys, space, r),
        }
    }

    /// Decrypt a revision sealed any way, with the key of the epoch it
    /// names. A `v3` revision whose header, chain or epoch was changed
    /// never opens.
    pub fn open_set(keys: &KeySet, space: &SpaceId, r: &Revision, bound_first: bool) -> Result<Self, CryptoError> {
        match crate::crypto::sealed_epoch(&r.sealed) {
            Some(e) if r.sealed.first() == Some(&crate::crypto::SEALED_V2) => {
                let k = keys.get(e).ok_or(CryptoError::UnknownEpoch(e))?;
                let plain = k.open_epoch(&r.sealed, &aad_v3(space, r, e))?;
                Self::unpad(&plain)
            }
            _ => {
                let k = keys.get(0).ok_or(CryptoError::UnknownEpoch(0))?;
                Self::open_hinted(k, space, r, bound_first)
            }
        }
    }

    fn unpad(plain: &[u8]) -> Result<Self, CryptoError> {
        let len_bytes: [u8; 4] = plain.get(..4).and_then(|b| b.try_into().ok()).ok_or(CryptoError::Truncated)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let json = plain.get(4..4 + len).ok_or(CryptoError::Truncated)?;
        serde_json::from_slice(json).map_err(|_| CryptoError::Tampered)
    }

    fn padded(&self) -> Result<Vec<u8>, CryptoError> {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let len = u32::try_from(json.len()).map_err(|_| CryptoError::Truncated)?;
        let mut plain = Vec::with_capacity(padded_len(json.len() + 4));
        plain.extend_from_slice(&len.to_le_bytes());
        plain.extend_from_slice(&json);
        plain.resize(padded_len(json.len() + 4), 0);
        Ok(plain)
    }

    /// Decrypt a revision sealed either way ([`Self::seal`] or
    /// [`Self::seal_bound`]). A revision sealed with its header bound fails
    /// both ways if anything in that header was changed: it never opens
    /// under the shorter associated data.
    pub fn open(keys: &Keys, space: &SpaceId, r: &Revision) -> Result<Self, CryptoError> {
        Self::open_hinted(keys, space, r, false)
    }

    /// [`Self::open`], trying the bound form first when `bound_first`
    /// (it's what that transport writes), to decrypt once in the usual case.
    pub fn open_hinted(keys: &Keys, space: &SpaceId, r: &Revision, bound_first: bool) -> Result<Self, CryptoError> {
        let (short, bound) = (aad(space, &r.file, &r.rev), aad_bound(space, r));
        let (first, second) = if bound_first { (&bound, &short) } else { (&short, &bound) };
        let plain = keys.open(&r.sealed, first).or_else(|_| keys.open(&r.sealed, second))?;
        Self::unpad(&plain)
    }
}

/// The server's answer to a push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushOutcome {
    /// Stored as the file's new head, at this position in the log.
    Accepted { seq: u64 },
    /// Someone else got there first: this is the file's head now.
    Conflict { head: Revision },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::VaultKey;

    #[test]
    fn content_round_trips_padded_and_bound_to_its_place() {
        let keys = VaultKey::generate().unwrap().keys();
        let space = SpaceId::random().unwrap();
        let (file, rev) = (FileId::random().unwrap(), RevId::random().unwrap());
        let c = Content {
            path: "Projects/Plan.md".into(),
            deleted: false,
            text: "# Plan\nShip it.\n".into(),
            from: "laptop".into(),
            at: 1,
            actor: None,
        };
        let sealed = c.seal(&keys, &space, &file, &rev).unwrap();
        // Short notes all look the same size.
        let other = Content { text: "x".into(), ..c.clone() }.seal(&keys, &space, &file, &rev).unwrap();
        assert_eq!(sealed.len(), other.len());
        let r = Revision { file, rev, parents: vec![], seq: 0, device: DeviceId::random().unwrap(), sealed, chain: vec![] };
        assert_eq!(Content::open(&keys, &space, &r).unwrap(), c);
        // The path isn't readable from the stored bytes.
        assert!(!String::from_utf8_lossy(&r.sealed).contains("Plan"));
        // Swapping the ciphertext onto another revision fails.
        let moved = Revision { rev: RevId::random().unwrap(), ..r.clone() };
        assert!(Content::open(&keys, &space, &moved).is_err());
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Revision>(&json).unwrap(), r);
    }

    #[test]
    fn a_bound_header_cant_be_changed_and_old_revisions_still_open() {
        let keys = VaultKey::generate().unwrap().keys();
        let space = SpaceId::random().unwrap();
        let c = Content { path: "A.md".into(), deleted: false, text: "old text".into(), from: "laptop".into(), at: 1, actor: None };
        let parent = RevId::random().unwrap();
        let mut r = Revision {
            file: FileId::random().unwrap(),
            rev: RevId::random().unwrap(),
            parents: vec![parent],
            seq: 0,
            device: DeviceId::random().unwrap(),
            sealed: Vec::new(),
            chain: vec![],
        };
        r.sealed = c.seal_bound(&keys, &space, &r).unwrap();
        for hint in [true, false] {
            assert_eq!(Content::open_hinted(&keys, &space, &r, hint).unwrap(), c);
        }
        // The storage points it at another parent (a rollback), drops the
        // parent, or says another device wrote it: it never opens.
        let newest = Revision { parents: vec![RevId::random().unwrap()], ..r.clone() };
        let orphan = Revision { parents: vec![], ..r.clone() };
        let other = Revision { device: DeviceId::random().unwrap(), ..r.clone() };
        for bad in [newest, orphan, other] {
            assert!(Content::open(&keys, &space, &bad).is_err());
            assert!(Content::open_hinted(&keys, &space, &bad, true).is_err());
        }
        // Revisions sealed the old way still open, either way round.
        let old = Revision { sealed: c.seal(&keys, &space, &r.file, &r.rev).unwrap(), ..r };
        assert_eq!(Content::open_hinted(&keys, &space, &old, true).unwrap(), c);
        assert_eq!(Content::open(&keys, &space, &old).unwrap(), c);
    }

    /// Quire 0.8's `Content`, field for field.
    #[derive(Debug, Deserialize)]
    struct Content08 {
        path: String,
        deleted: bool,
        text: String,
        #[serde(default)]
        from: String,
        #[serde(default)]
        at: i64,
    }

    #[test]
    fn actors_travel_and_older_devices_ignore_them() {
        let actor = ActorRef { id: "mcp:claude-code".into(), label: "Claude Code".into(), kind: "agent".into() };
        let c = Content { path: "wiki/A.md".into(), deleted: false, text: "a".into(), from: "laptop".into(), at: 5, actor: Some(actor) };
        // What a 0.8 device decodes from a 0.9 revision's JSON.
        let json = serde_json::to_string(&c).unwrap();
        let old: Content08 = serde_json::from_str(&json).unwrap();
        assert_eq!((old.path.as_str(), old.deleted, old.text.as_str(), old.from.as_str(), old.at), ("wiki/A.md", false, "a", "laptop", 5));
        // And a 0.8 revision on a 0.9 device.
        let from_08: Content = serde_json::from_str(r#"{"path":"A.md","deleted":false,"text":"x","from":"old","at":1}"#).unwrap();
        assert_eq!(from_08.actor, None);
        assert!(!serde_json::to_string(&from_08).unwrap().contains("actor"), "nothing extra for the person's own edits");
    }

    #[test]
    fn padding_buckets() {
        assert_eq!(padded_len(1), 256);
        assert_eq!(padded_len(257), 512);
        assert_eq!(padded_len(40_000), 65_536);
        assert_eq!(padded_len(70_000), 131_072);
    }
}
