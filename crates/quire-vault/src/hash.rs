// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Content hashes: the version id of a note's text.
//!
//! `sha256:` followed by the first 32 hex characters (128 bits) of the
//! SHA-256 of the UTF-8 bytes, after stripping a byte-order mark. Used for
//! versions, anchors, proposals and caches (docs/formats/README.md).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// A note version: `sha256:` + 32 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(String);

const PREFIX: &str = "sha256:";
const HEX_LEN: usize = 32;

impl ContentHash {
    /// Hash of a note's text (a leading BOM is ignored).
    pub fn of(text: &str) -> Self {
        Self::of_bytes(text.as_bytes())
    }

    /// Hash of raw bytes; a leading UTF-8 BOM is ignored, as when reading notes.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        let digest = Sha256::digest(bytes);
        let mut s = String::with_capacity(PREFIX.len() + HEX_LEN);
        s.push_str(PREFIX);
        for b in digest.iter().take(HEX_LEN / 2) {
            s.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
            s.push(char::from(b"0123456789abcdef"[usize::from(b & 0xf)]));
        }
        Self(s)
    }

    /// Accepts only the canonical form.
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.strip_prefix(PREFIX)?;
        (hex.len() == HEX_LEN && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hex part (for file names).
    pub fn hex(&self) -> &str {
        self.0.strip_prefix(PREFIX).unwrap_or(&self.0)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("not a content hash: {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_form() {
        let h = ContentHash::of("hello\n");
        assert_eq!(h.as_str(), "sha256:5891b5b522d5df086d0ff0b110fbd9d2");
        assert_eq!(ContentHash::of("\u{feff}hello\n"), h, "a BOM doesn't change the version");
        assert_eq!(ContentHash::parse(h.as_str()), Some(h.clone()));
        assert!(ContentHash::parse("sha256:XYZ").is_none());
        assert!(ContentHash::parse("5891b5b522d5df086d0ff0b110fbd9d2").is_none());
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), h);
        assert_eq!(h.hex().len(), 32);
    }
}
