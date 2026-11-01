// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Version history: compressed snapshots of every saved version of a note.
//!
//! Layout: `.quire/history/<hash of path>/<unix-ms>.md.gz`. The newest
//! snapshot is the latest saved content. While typing, saves within two
//! minutes replace the newest snapshot instead of adding one, so history
//! keeps one version per pause rather than one per autosave.

use std::io::{Read, Write};

use crate::{NotePath, Result, Vault, VaultError};

const THROTTLE_MS: i64 = 2 * 60 * 1000;
const MAX_SNAPSHOTS: usize = 200;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Storage path of the snapshot file.
    pub file: NotePath,
    /// Unix time in milliseconds.
    pub at_ms: i64,
}

fn folder_for(note: &NotePath) -> String {
    // FNV-1a: stable across Rust versions (unlike DefaultHasher).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in note.as_str().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!(".quire/history/{h:016x}")
}

fn gzip(s: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(s.as_bytes());
    e.finish().unwrap_or_default()
}

fn gunzip(b: &[u8]) -> std::io::Result<String> {
    let mut out = String::new();
    flate2::read::GzDecoder::new(b).read_to_string(&mut out)?;
    Ok(out)
}

/// Which snapshots to keep: everything from the last hour, then the newest per
/// hour (≤1 day), per day (≤30 days) and per week, capped at 200.
pub fn retained(mut times: Vec<i64>, now: i64) -> Vec<i64> {
    times.sort_unstable_by(|a, b| b.cmp(a));
    let mut keep = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for t in times {
        let age = now - t;
        let bucket = if age < HOUR {
            (0, t)
        } else if age < DAY {
            (1, t / HOUR)
        } else if age < 30 * DAY {
            (2, t / DAY)
        } else {
            (3, t / (7 * DAY))
        };
        if seen.insert(bucket) {
            keep.push(t);
        }
    }
    keep.truncate(MAX_SNAPSHOTS);
    keep
}

impl Vault {
    /// Snapshots of a note, newest first.
    pub fn history(&self, note: &NotePath) -> Vec<Snapshot> {
        let mut v: Vec<Snapshot> = self
            .storage()
            .list_under(&folder_for(note))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| {
                let at_ms = f.file_name().strip_suffix(".md.gz")?.parse().ok()?;
                Some(Snapshot { file: f, at_ms })
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
        v
    }

    pub fn read_snapshot(&self, s: &Snapshot) -> Result<String> {
        let bytes = self.storage().read_bytes(&s.file)?;
        gunzip(&bytes).map_err(|e| VaultError::Io { path: s.file.to_string(), source: e })
    }

    /// Record `content` as the latest version. Returns true if a snapshot was
    /// written. `force` skips throttling (external edits, restores, AI writes).
    pub fn snapshot(&self, note: &NotePath, content: &str, now_ms: i64, force: bool) -> Result<bool> {
        let hist = self.history(note);
        if let Some(newest) = hist.first() {
            if self.read_snapshot(newest).is_ok_and(|c| c == content) {
                return Ok(false);
            }
            // Within a typing burst: the newest snapshot is superseded.
            if !force && now_ms - newest.at_ms < THROTTLE_MS && hist.len() > 1 {
                self.storage().remove(&newest.file)?;
            }
        }
        let at = hist.first().map_or(now_ms, |n| now_ms.max(n.at_ms + 1));
        let file = NotePath::new(format!("{}/{at}.md.gz", folder_for(note)));
        self.storage().write_bytes_atomic(&file, &gzip(content))?;
        self.prune_history(note, now_ms)?;
        Ok(true)
    }

    fn prune_history(&self, note: &NotePath, now_ms: i64) -> Result<()> {
        let hist = self.history(note);
        let keep: std::collections::HashSet<i64> = retained(hist.iter().map(|s| s.at_ms).collect(), now_ms).into_iter().collect();
        for s in hist.into_iter().filter(|s| !keep.contains(&s.at_ms)) {
            self.storage().remove(&s.file)?;
        }
        Ok(())
    }

    /// Move a note's history when it is renamed.
    pub fn move_history(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let (a, b) = (folder_for(from), folder_for(to));
        if a == b {
            return Ok(());
        }
        for f in self.storage().list_under(&a)? {
            let bytes = self.storage().read_bytes(&f)?;
            self.storage().write_bytes_atomic(&NotePath::join(&b, f.file_name()), &bytes)?;
            self.storage().remove(&f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    #[test]
    fn snapshots_throttle_dedupe_and_restore() {
        let v = Vault::in_memory(&[("A.md", "v1")]);
        let a = NotePath::new("A.md");
        let t0 = 1_800_000_000_000;
        assert!(v.snapshot(&a, "v1", t0, false).unwrap());
        assert!(!v.snapshot(&a, "v1", t0 + MIN, false).unwrap(), "identical content is skipped");
        assert!(v.snapshot(&a, "v2", t0 + MIN, false).unwrap());
        // A third save inside the burst replaces "v2" instead of adding.
        assert!(v.snapshot(&a, "v3", t0 + 2 * MIN, false).unwrap());
        let h = v.history(&a);
        assert_eq!(h.len(), 2);
        assert_eq!(v.read_snapshot(&h[0]).unwrap(), "v3");
        assert_eq!(v.read_snapshot(&h[1]).unwrap(), "v1");
        // After a pause, a new version is added.
        assert!(v.snapshot(&a, "v4", t0 + 10 * MIN, false).unwrap());
        assert_eq!(v.history(&a).len(), 3);
        // Forced snapshots (external edits) never replace.
        assert!(v.snapshot(&a, "v5", t0 + 10 * MIN + 1, true).unwrap());
        assert_eq!(v.history(&a).len(), 4);
        assert!(v.notes().unwrap().iter().all(|p| !p.as_str().starts_with(".quire")));
    }

    #[test]
    fn retention_thins_old_versions() {
        let now = 1_800_000_000_000;
        // One snapshot every 10 minutes for 60 days.
        let times: Vec<i64> = (0..60 * 24 * 6).map(|i| now - i * 10 * MIN).collect();
        let kept = retained(times, now);
        assert!(kept.len() <= MAX_SNAPSHOTS);
        assert!(kept.iter().filter(|t| now - **t < HOUR).count() >= 6, "recent ones all kept");
        assert!(kept.iter().any(|t| now - t > 40 * DAY), "weekly ones survive");
    }

    #[test]
    fn history_follows_renames() {
        let v = Vault::in_memory(&[("A.md", "x")]);
        let (a, b) = (NotePath::new("A.md"), NotePath::new("B.md"));
        v.snapshot(&a, "x", 1, true).unwrap();
        v.move_history(&a, &b).unwrap();
        assert!(v.history(&a).is_empty());
        assert_eq!(v.history(&b).len(), 1);
    }
}
