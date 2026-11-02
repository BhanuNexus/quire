// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The served-version cache (design §B.2).
//!
//! Every text Quire hands an agent (`view`, `read_version`, resources) is kept by its hash in
//! `.quire/cache/served/<hex>.md.gz`, so an edit made against an older
//! version can be rebased or merged three-way. Shared by every process on
//! the vault; at most 2,000 entries or 64 MB, each kept 7 days. History
//! snapshots are the fallback.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use quire_app::vault::{ContentHash, NotePath, Vault};

pub const SERVED_DIR: &str = ".quire/cache/served";
const MAX_ENTRIES: usize = 2_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const TTL: Duration = Duration::from_hours(7 * 24);
/// Eviction runs every this many stores.
const EVICT_EVERY: usize = 64;
/// How many snapshots to check when the cache misses.
const HISTORY_LOOKBACK: usize = 60;

pub struct ServedCache {
    vault: Vault,
    stores: AtomicUsize,
}

fn entry(hash: &ContentHash) -> NotePath {
    NotePath::new(format!("{SERVED_DIR}/{}.md.gz", hash.hex()))
}

fn gzip(s: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = e.write_all(s.as_bytes());
    e.finish().unwrap_or_default()
}

fn gunzip(b: &[u8]) -> Option<String> {
    let mut out = String::new();
    flate2::read::GzDecoder::new(b).read_to_string(&mut out).ok()?;
    Some(out)
}

impl ServedCache {
    pub const fn new(vault: Vault) -> Self {
        Self { vault, stores: AtomicUsize::new(0) }
    }

    /// Remember that `text` was served. Returns its version.
    pub fn store(&self, text: &str) -> ContentHash {
        let hash = ContentHash::of(text);
        let path = entry(&hash);
        let fresh = self
            .vault
            .storage()
            .modified(&path)
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age < Duration::from_hours(24));
        if !fresh {
            // A failure only costs a later merge its base; the read still works.
            let _ = self.vault.storage().write_bytes_atomic(&path, &gzip(text));
            if self.stores.fetch_add(1, Ordering::Relaxed).is_multiple_of(EVICT_EVERY) {
                self.evict();
            }
        }
        hash
    }

    /// The text of `version` of `path`: from the cache, else from history.
    pub fn get(&self, path: &NotePath, version: &ContentHash) -> Option<String> {
        if let Some(t) = self.vault.storage().read_bytes(&entry(version)).ok().and_then(|b| gunzip(&b))
            && ContentHash::of(&t) == *version
        {
            return Some(t);
        }
        if let Ok(t) = self.vault.read(path)
            && ContentHash::of(&t) == *version
        {
            return Some(t);
        }
        for snap in self.vault.history(path).into_iter().take(HISTORY_LOOKBACK) {
            // Provenance names the hash, so most snapshots needn't be unpacked.
            let named = self.vault.read_provenance(&snap).and_then(|p| p.get("hash").and_then(|h| h.as_str()).map(str::to_string));
            if named.as_deref().is_some_and(|h| h != version.as_str()) {
                continue;
            }
            if let Ok(t) = self.vault.read_snapshot(&snap)
                && ContentHash::of(&t) == *version
            {
                return Some(t);
            }
        }
        None
    }

    /// Drop expired entries, then the oldest beyond the count and size limits.
    pub fn evict(&self) {
        let storage = self.vault.storage();
        let Ok(files) = storage.list_under(SERVED_DIR) else { return };
        let now = SystemTime::now();
        let mut live: Vec<(SystemTime, u64, NotePath)> = Vec::new();
        for f in files {
            let Ok(m) = storage.modified(&f) else { continue };
            if now.duration_since(m).is_ok_and(|age| age > TTL) {
                let _ = storage.remove(&f);
                continue;
            }
            let size = storage.read_bytes(&f).map_or(0, |b| b.len() as u64);
            live.push((m, size, f));
        }
        live.sort_by_key(|(m, _, _)| std::cmp::Reverse(*m));
        let mut total = 0;
        for (i, (_, size, f)) in live.iter().enumerate() {
            total += size;
            if i >= MAX_ENTRIES || total > MAX_BYTES {
                let _ = storage.remove(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_back_what_was_stored_and_falls_back_to_history() {
        let v = Vault::in_memory(&[("A.md", "one\n")]);
        let c = ServedCache::new(v.clone());
        let h1 = c.store("one\n");
        v.write(&NotePath::new("A.md"), "two\n").unwrap();
        assert_eq!(c.get(&NotePath::new("A.md"), &h1).as_deref(), Some("one\n"));
        // Not served, but in history.
        let _ = v.snapshot(&NotePath::new("A.md"), "zero\n", 1, true);
        assert_eq!(c.get(&NotePath::new("A.md"), &ContentHash::of("zero\n")).as_deref(), Some("zero\n"));
        assert_eq!(c.get(&NotePath::new("A.md"), &ContentHash::of("never\n")), None);
        // The current text needs no cache.
        assert_eq!(c.get(&NotePath::new("A.md"), &ContentHash::of("two\n")).as_deref(), Some("two\n"));
    }
}
