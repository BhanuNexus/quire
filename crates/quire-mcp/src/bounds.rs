// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Token bounds and cursors (design §B.1).
//!
//! Results are bounded by an estimate that over-counts for prose:
//! `ceil(utf8 bytes / 3)` (prose is closer to 4 characters a token). A
//! result that doesn't fit is cut and carries `next_cursor`: base64 of
//! `{"o": offset, "v": version}`. A cursor for a version that has changed
//! since is refused with `cursor_stale` and the new version.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

/// Default bound for one result.
pub const DEFAULT_MAX_TOKENS: usize = 4_000;
/// No result is ever larger than this.
pub const HARD_MAX_TOKENS: usize = 20_000;

/// The estimated token count of `s`.
pub const fn est_tokens(s: &str) -> usize {
    s.len().div_ceil(3)
}

/// The caller's `max_tokens`, clamped to `[100, HARD_MAX_TOKENS]`.
pub fn max_tokens(args: &Value, default: usize) -> usize {
    args.get("max_tokens")
        .and_then(Value::as_u64)
        .map_or(default, |n| usize::try_from(n).unwrap_or(HARD_MAX_TOKENS))
        .clamp(100, HARD_MAX_TOKENS)
}

/// A position in a versioned result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Where to continue (a line, a hit, a row).
    pub offset: usize,
    /// What the offset refers to: a note version or a query fingerprint.
    pub version: String,
}

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(json!({ "o": self.offset, "v": self.version }).to_string())
    }

    pub fn decode(s: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(s.trim()).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        Some(Self { offset: usize::try_from(v.get("o")?.as_u64()?).ok()?, version: v.get("v")?.as_str()?.to_string() })
    }
}

/// What a cursor argument says about where to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// No cursor.
    Beginning,
    At(usize),
    /// Unreadable, or made for another version.
    Stale,
}

/// Where `args.cursor` says to start, given the current `version`.
pub fn start(args: &Value, version: &str) -> Start {
    match args.get("cursor").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        None => Start::Beginning,
        Some(c) => match Cursor::decode(c) {
            Some(c) if c.version == version => Start::At(c.offset),
            _ => Start::Stale,
        },
    }
}

/// Take items while their estimated tokens fit in `budget`; always at least one.
/// Returns how many fit.
pub fn fit<'a>(items: impl IntoIterator<Item = &'a str>, budget: usize) -> usize {
    let mut used = 0;
    let mut n = 0;
    for s in items {
        let t = est_tokens(s) + 1;
        if n > 0 && used + t > budget {
            break;
        }
        used += t;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_round_up() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abcd"), 2);
        assert_eq!(est_tokens("é"), 1);
    }

    #[test]
    fn cursors_round_trip_and_go_stale() {
        let c = Cursor { offset: 40, version: "sha256:abc".into() };
        let enc = c.encode();
        assert_eq!(Cursor::decode(&enc), Some(c));
        assert_eq!(start(&json!({"cursor": enc}), "sha256:abc"), Start::At(40));
        assert_eq!(start(&json!({"cursor": enc}), "sha256:def"), Start::Stale);
        assert_eq!(start(&json!({"cursor": "%%%"}), "x"), Start::Stale);
        assert_eq!(start(&json!({}), "x"), Start::Beginning);
    }

    #[test]
    fn fitting_keeps_at_least_one() {
        let big = "x".repeat(900);
        assert_eq!(fit([big.as_str(), "y"], 10), 1);
        assert_eq!(fit(["aaa", "bbb", "ccc"], 4), 2);
        assert_eq!(max_tokens(&json!({"max_tokens": 1_000_000}), 4000), HARD_MAX_TOKENS);
        assert_eq!(max_tokens(&json!({}), 2000), 2000);
    }
}
