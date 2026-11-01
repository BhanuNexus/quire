// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What travels between devices and the server.
//!
//! The server sees [`Revision`]s: ids, parents, an order number and sealed
//! bytes. What a revision says (path and text) is a [`Content`], encrypted
//! and padded so its length gives little away.

use serde::{Deserialize, Serialize};

use crate::crypto::{CryptoError, Keys, hex_bytes, random_bytes};

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

impl Content {
    /// Encrypt into the bytes a [`Revision`] carries.
    pub fn seal(&self, keys: &Keys, space: &SpaceId, file: &FileId, rev: &RevId) -> Result<Vec<u8>, CryptoError> {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let len = u32::try_from(json.len()).map_err(|_| CryptoError::Truncated)?;
        let mut plain = Vec::with_capacity(padded_len(json.len() + 4));
        plain.extend_from_slice(&len.to_le_bytes());
        plain.extend_from_slice(&json);
        plain.resize(padded_len(json.len() + 4), 0);
        keys.seal(&plain, &aad(space, file, rev))
    }

    pub fn open(keys: &Keys, space: &SpaceId, r: &Revision) -> Result<Self, CryptoError> {
        let plain = keys.open(&r.sealed, &aad(space, &r.file, &r.rev))?;
        let len_bytes: [u8; 4] = plain.get(..4).and_then(|b| b.try_into().ok()).ok_or(CryptoError::Truncated)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let json = plain.get(4..4 + len).ok_or(CryptoError::Truncated)?;
        serde_json::from_slice(json).map_err(|_| CryptoError::Tampered)
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
        let c =
            Content { path: "Projects/Plan.md".into(), deleted: false, text: "# Plan\nShip it.\n".into(), from: "laptop".into(), at: 1 };
        let sealed = c.seal(&keys, &space, &file, &rev).unwrap();
        // Short notes all look the same size.
        let other = Content { text: "x".into(), ..c.clone() }.seal(&keys, &space, &file, &rev).unwrap();
        assert_eq!(sealed.len(), other.len());
        let r = Revision { file, rev, parents: vec![], seq: 0, device: DeviceId::random().unwrap(), sealed };
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
    fn padding_buckets() {
        assert_eq!(padded_len(1), 256);
        assert_eq!(padded_len(257), 512);
        assert_eq!(padded_len(40_000), 65_536);
        assert_eq!(padded_len(70_000), 131_072);
    }
}
