// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ids that sort by time: a prefix, 13 base32 characters of Unix ms and 8
//! random base32 characters (`p_01JB9Q7H2K3M4X7Q2W9Z`). Crockford's
//! alphabet, so ids read unambiguously. See docs/formats/README.md.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 13 characters: 65 bits, most significant first, so ids sort by time.
fn time_part(ms: i64) -> String {
    let v = u64::try_from(ms.max(0)).unwrap_or(0);
    (0..13).rev().map(|i| char::from(ALPHABET[usize::try_from((u128::from(v) >> (i * 5)) & 31).unwrap_or(0)])).collect()
}

/// `n` random characters. Falls back to a time-and-counter mix when the
/// OS has no randomness to give (ids stay unique within a process).
fn random_part(n: usize) -> String {
    let mut bytes = vec![0_u8; n];
    if getrandom::fill(&mut bytes).is_err() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let mut x = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0) ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        for b in &mut bytes {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x.to_le_bytes()[0];
        }
    }
    bytes.iter().map(|b| char::from(ALPHABET[usize::from(b & 31)])).collect()
}

/// A new id: `prefix` + time + randomness.
pub fn new_id(prefix: &str, now_ms: i64) -> String {
    format!("{prefix}{}{}", time_part(now_ms), random_part(8))
}

/// `prefix` + `n` random characters (device ids).
pub fn random_id(prefix: &str, n: usize) -> String {
    format!("{prefix}{}", random_part(n))
}

fn valid(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn new(now_ms: i64) -> Self {
                Self(new_id($prefix, now_ms))
            }

            /// Accepts any id with the right prefix and safe characters (it
            /// becomes a file name).
            pub fn parse(s: &str) -> Option<Self> {
                valid(s, $prefix).then(|| Self(s.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(s: String) -> Result<Self, String> {
                Self::parse(&s).ok_or_else(|| format!("not a {} id: {s}", stringify!($name)))
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

id_type!(
    /// A proposal (`p_…`).
    ProposalId,
    "p_"
);
id_type!(
    /// An agent run: the changes one task made, revertable together (`r_…`).
    RunId,
    "r_"
);
id_type!(
    /// One batch of writes applied together, undone together (`b_…`).
    BatchId,
    "b_"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_time_and_round_trip() {
        let a = ProposalId::new(1_793_000_000_000);
        let b = ProposalId::new(1_793_000_000_001);
        assert!(a < b);
        assert_eq!(a.as_str().len(), 2 + 13 + 8);
        assert!(a.as_str().starts_with("p_"));
        assert_ne!(ProposalId::new(5).as_str(), ProposalId::new(5).as_str(), "random part differs");
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<ProposalId>(&json).unwrap(), a);
        assert!(serde_json::from_str::<ProposalId>("\"r_X\"").is_err());
        assert!(ProposalId::parse("p_../../etc").is_none());
        assert_eq!(time_part(0), "0000000000000");
    }
}
