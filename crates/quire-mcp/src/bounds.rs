// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Token bounds and cursors (`docs/design/0.9.md` §B.1).
//!
//! Results are bounded by an estimate that over-counts for prose:
//! `ceil(utf8 bytes / 3)` (prose is closer to 4 characters a token). A
//! result that doesn't fit is cut and carries `next_cursor`: base64 of
//! `{"o": offset, "v": version}`. A cursor for a version that has changed
//! since is refused with `cursor_stale` and the new version.
//!
//! Lists with a total order page by key (`docs/design/0.14-agent-tools.md` §3.3):
//! the cursor also carries the last row's key (`"k"`), and the next page starts
//! after it, so a row added or removed between two pages makes
//! none repeat or go missing. `v` is then a fingerprint of the arguments.
//! A cursor without `k` (made before keys) still reads as an offset.

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
    /// For a list paged by key: the last row's key; the next page starts after it.
    pub key: Option<String>,
}

impl Cursor {
    /// An offset cursor.
    pub fn at(offset: usize, version: impl Into<String>) -> Self {
        Self { offset, version: version.into(), key: None }
    }

    pub fn encode(&self) -> String {
        let v = match &self.key {
            None => json!({ "o": self.offset, "v": self.version }),
            Some(k) => json!({ "o": self.offset, "v": self.version, "k": k }),
        };
        URL_SAFE_NO_PAD.encode(v.to_string())
    }

    pub fn decode(s: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(s.trim()).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        let key = match v.get("k") {
            None => None,
            Some(k) => Some(k.as_str()?.to_string()),
        };
        Some(Self { offset: usize::try_from(v.get("o")?.as_u64()?).ok()?, version: v.get("v")?.as_str()?.to_string(), key })
    }
}

/// The fingerprint of a list call: its tool and its arguments without
/// `cursor`, so a cursor made for other arguments is stale.
pub fn fingerprint(tool: &str, args: &Value) -> String {
    let mut key = args.clone();
    if let Some(o) = key.as_object_mut() {
        o.remove("cursor");
    }
    format!("{tool}:{}", quire_app::vault::ContentHash::of(&key.to_string()).hex())
}

/// One page of a list sorted by a key ([`keyset`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The rows shown: `rows[range]`.
    pub range: std::ops::Range<usize>,
    /// Each shown row as `render` made it.
    pub items: Vec<Value>,
    /// Rows in the whole list (after filtering).
    pub total: usize,
    /// More rows follow the page.
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

impl Page {
    /// How many rows follow the page.
    pub const fn more(&self) -> usize {
        self.total - self.range.end
    }
}

/// The page of `rows` that `args.cursor` asks for.
///
/// The rows after the cursor's key, at most `limit` of them and as many as
/// fit in `budget` tokens (at least one), each rendered by `render`. `rows`
/// must be sorted by `key`, ascending, with no two equal keys. `None`: the
/// cursor is stale.
pub fn keyset<T>(
    rows: &[T],
    key: impl Fn(&T) -> String,
    render: impl Fn(&T) -> Value,
    args: &Value,
    version: &str,
    limit: usize,
    budget: usize,
) -> Option<Page> {
    let start = match args.get("cursor").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        None => 0,
        Some(c) => match Cursor::decode(c) {
            Some(Cursor { version: v, key: Some(k), .. }) if v == version => rows.partition_point(|r| key(r) <= k),
            // A cursor from before keys: an offset into the same list.
            Some(Cursor { version: v, key: None, offset }) if v == version => offset.min(rows.len()),
            _ => return None,
        },
    };
    let candidates: Vec<Value> = rows[start..].iter().take(limit).map(&render).collect();
    let texts: Vec<String> = candidates.iter().map(Value::to_string).collect();
    let n = fit(texts.iter().map(String::as_str), budget);
    let end = start + n;
    let truncated = end < rows.len();
    let next_cursor =
        truncated.then(|| Cursor { offset: end, version: version.to_string(), key: rows.get(end.wrapping_sub(1)).map(&key) }.encode());
    Some(Page { range: start..end, items: candidates.into_iter().take(n).collect(), total: rows.len(), truncated, next_cursor })
}

/// The order-keeping key of a number for [`keyset`]: fixed width, so text
/// order is number order.
pub fn num_key(n: u64) -> String {
    format!("{n:020}")
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
        let c = Cursor::at(40, "sha256:abc");
        let enc = c.encode();
        assert_eq!(Cursor::decode(&enc), Some(c));
        assert_eq!(start(&json!({"cursor": enc}), "sha256:abc"), Start::At(40));
        assert_eq!(start(&json!({"cursor": enc}), "sha256:def"), Start::Stale);
        assert_eq!(start(&json!({"cursor": "%%%"}), "x"), Start::Stale);
        assert_eq!(start(&json!({}), "x"), Start::Beginning);
    }

    #[test]
    fn keyed_cursors_round_trip_and_old_ones_still_read() {
        let c = Cursor { offset: 2, version: "l:ab".into(), key: Some("b\u{0}x".into()) };
        assert_eq!(Cursor::decode(&c.encode()), Some(c));
        let old = URL_SAFE_NO_PAD.encode(r#"{"o":3,"v":"l:ab"}"#);
        assert_eq!(Cursor::decode(&old), Some(Cursor::at(3, "l:ab")));
    }

    /// A row added or removed between two pages makes none repeat or go missing.
    #[test]
    fn keyset_pages_survive_inserts_and_deletes() {
        let page = |rows: &[&str], cursor: Option<&str>, limit: usize| {
            let args = cursor.map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            keyset(rows, |r| (*r).to_string(), |r| json!(r), &args, "v", limit, 10_000).unwrap()
        };
        let rows = ["a", "b", "c", "d", "e"];
        let first = page(&rows, None, 2);
        assert_eq!((first.items.clone(), first.total, first.truncated), (vec![json!("a"), json!("b")], 5, true));
        // "b" is deleted and "bb" added before the second page.
        let changed = ["a", "bb", "c", "d", "e"];
        let second = page(&changed, first.next_cursor.as_deref(), 2);
        assert_eq!(second.items, vec![json!("bb"), json!("c")]);
        let third = page(&changed, second.next_cursor.as_deref(), 2);
        assert_eq!((third.items.clone(), third.truncated, third.next_cursor), (vec![json!("d"), json!("e")], false, None));
        assert!(keyset(&rows, |r| (*r).to_string(), |r| json!(r), &json!({"cursor": first.next_cursor}), "w", 2, 100).is_none(), "stale");
        // A huge row still makes a page of one.
        let x = "x".repeat(900);
        let big = keyset(&[x.as_str(), "y"], |r| (*r).to_string(), |r| json!(r), &json!({}), "v", 10, 100).unwrap();
        assert_eq!((big.items.len(), big.truncated), (1, true));
        assert!(page(&[], None, 3).items.is_empty());
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
