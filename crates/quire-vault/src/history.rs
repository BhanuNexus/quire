// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Version history: compressed snapshots of every saved version of a note.
//!
//! Layout: `.quire/history/<hash of path>/<unix-ms>.md.gz`. The newest
//! snapshot is the latest saved content. While typing, saves within two
//! minutes replace the newest snapshot instead of adding one, so history
//! keeps one version per pause rather than one per autosave.
//!
//! A snapshot written by someone other than the person typing (an agent, an
//! accepted suggestion, a background job) has a provenance file next to it,
//! `<unix-ms>.prov.json` (docs/formats/provenance.md). A snapshot with
//! provenance is never replaced by a later typing save, so an agent's
//! version always survives in history.

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
    /// Storage path of its provenance file, if it has one (absent means the
    /// person typing, or a version written before Quire 0.9).
    pub prov_file: Option<NotePath>,
}

const SNAPSHOT_EXT: &str = ".md.gz";
const PROV_EXT: &str = ".prov.json";

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
        let files = self.storage().list_under(&folder_for(note)).unwrap_or_default();
        let provs: std::collections::HashMap<i64, NotePath> =
            files.iter().filter_map(|f| Some((f.file_name().strip_suffix(PROV_EXT)?.parse().ok()?, f.clone()))).collect();
        let mut v: Vec<Snapshot> = files
            .into_iter()
            .filter_map(|f| {
                let at_ms = f.file_name().strip_suffix(SNAPSHOT_EXT)?.parse().ok()?;
                Some(Snapshot { prov_file: provs.get(&at_ms).cloned(), file: f, at_ms })
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
        v
    }

    /// Who made a snapshot, as written by [`Vault::snapshot_with`]; `None`
    /// for typing saves, older versions or an unreadable file.
    pub fn read_provenance(&self, s: &Snapshot) -> Option<serde_json::Value> {
        let bytes = self.storage().read_bytes(s.prov_file.as_ref()?).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn read_snapshot(&self, s: &Snapshot) -> Result<String> {
        let bytes = self.storage().read_bytes(&s.file)?;
        gunzip(&bytes).map_err(|e| VaultError::Io { path: s.file.to_string(), source: e })
    }

    /// Record `content` as the latest version. Returns true if a snapshot was
    /// written. `force` skips throttling (external edits, restores, AI writes).
    pub fn snapshot(&self, note: &NotePath, content: &str, now_ms: i64, force: bool) -> Result<bool> {
        self.snapshot_with(note, content, now_ms, force, None)
    }

    /// [`Vault::snapshot`] with provenance: `prov` (a JSON object, see
    /// docs/formats/provenance.md) is written next to the snapshot.
    ///
    /// A typing save (`force == false`, no `prov`) replaces the newest
    /// snapshot only if that one was also a typing save less than two minutes
    /// ago. A snapshot with provenance is never replaced, so a human save right
    /// after an agent's change can't erase the agent's version.
    pub fn snapshot_with(
        &self,
        note: &NotePath,
        content: &str,
        now_ms: i64,
        force: bool,
        prov: Option<&serde_json::Value>,
    ) -> Result<bool> {
        let hist = self.history(note);
        if let Some(newest) = hist.first() {
            if self.read_snapshot(newest).is_ok_and(|c| c == content) {
                return Ok(false);
            }
            // Within one person's typing burst: the newest snapshot is superseded.
            let same_burst = !force && prov.is_none() && newest.prov_file.is_none() && now_ms - newest.at_ms < THROTTLE_MS;
            if same_burst && hist.len() > 1 {
                self.storage().remove(&newest.file)?;
            }
        }
        let at = hist.first().map_or(now_ms, |n| now_ms.max(n.at_ms + 1));
        let folder = folder_for(note);
        let file = NotePath::new(format!("{folder}/{at}{SNAPSHOT_EXT}"));
        if let Some(p) = prov {
            // Provenance first: a snapshot is never visible without the record of who made it.
            let bytes = serde_json::to_vec(p).map_err(|e| VaultError::Io { path: file.to_string(), source: std::io::Error::other(e) })?;
            self.storage().write_bytes_atomic(&NotePath::new(format!("{folder}/{at}{PROV_EXT}")), &bytes)?;
        }
        self.storage().write_bytes_atomic(&file, &gzip(content))?;
        self.prune_history(note, now_ms)?;
        Ok(true)
    }

    /// Thin out old snapshots, removing provenance with its snapshot, and
    /// delete provenance files whose snapshot is gone (Quire 0.8 prunes
    /// snapshots without knowing about them).
    fn prune_history(&self, note: &NotePath, now_ms: i64) -> Result<()> {
        let hist = self.history(note);
        let keep: std::collections::HashSet<i64> = retained(hist.iter().map(|s| s.at_ms).collect(), now_ms).into_iter().collect();
        for s in hist.iter().filter(|s| !keep.contains(&s.at_ms)) {
            if let Some(p) = &s.prov_file {
                self.storage().remove(p)?;
            }
            self.storage().remove(&s.file)?;
        }
        let live: std::collections::HashSet<i64> = hist.iter().map(|s| s.at_ms).filter(|t| keep.contains(t)).collect();
        for f in self.storage().list_under(&folder_for(note))? {
            let orphan = f.file_name().strip_suffix(PROV_EXT).and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| !live.contains(&t));
            if orphan {
                self.storage().remove(&f)?;
            }
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
    fn an_agent_version_survives_a_human_save_right_after_it() {
        let v = Vault::in_memory(&[("A.md", "v1")]);
        let a = NotePath::new("A.md");
        let t0 = 1_800_000_000_000;
        let prov = serde_json::json!({"format": 1, "actor": {"kind": "agent", "id": "mcp:claude-code"}});
        assert!(v.snapshot(&a, "v1", t0, false).unwrap());
        assert!(v.snapshot(&a, "human edit", t0 + MIN, false).unwrap());
        assert!(v.snapshot_with(&a, "agent edit", t0 + MIN + 10, true, Some(&prov)).unwrap());
        // The person keeps typing within the two-minute window.
        assert!(v.snapshot(&a, "human again", t0 + MIN + 20, false).unwrap());
        assert!(v.snapshot(&a, "human again, more", t0 + MIN + 30, false).unwrap());
        let h = v.history(&a);
        let texts: Vec<String> = h.iter().map(|s| v.read_snapshot(s).unwrap()).collect();
        assert_eq!(texts, ["human again, more", "agent edit", "human edit", "v1"]);
        assert_eq!(v.read_provenance(&h[1]), Some(prov.clone()));
        assert!(h[0].prov_file.is_none() && h[2].prov_file.is_none());
        // Even a non-forced write with provenance never replaces a typing save.
        assert!(v.snapshot_with(&a, "job edit", t0 + MIN + 40, false, Some(&prov)).unwrap());
        assert_eq!(v.history(&a).len(), 5);
    }

    #[test]
    fn provenance_is_pruned_with_its_snapshot_and_orphans_are_removed() {
        let v = Vault::in_memory(&[("A.md", "x")]);
        let a = NotePath::new("A.md");
        let now = 1_800_000_000_000;
        let prov = serde_json::json!({"format": 1});
        // Two versions in the same old day: retention keeps only the newer.
        v.snapshot_with(&a, "old 1", now - 10 * DAY, true, Some(&prov)).unwrap();
        v.snapshot_with(&a, "old 2", now - 10 * DAY + HOUR, true, Some(&prov)).unwrap();
        // A 0.8 build left a provenance file without its snapshot.
        let folder = folder_for(&a);
        v.storage().write_bytes_atomic(&NotePath::new(format!("{folder}/123.prov.json")), b"{}").unwrap();
        v.snapshot(&a, "new", now, true).unwrap();
        let files: Vec<String> = v.storage().list_under(&folder).unwrap().iter().map(|f| f.file_name().to_string()).collect();
        let provs = files.iter().filter(|f| f.ends_with(PROV_EXT)).count();
        assert_eq!(v.history(&a).len(), 2, "{files:?}");
        assert_eq!(provs, 1, "only the kept agent snapshot has provenance: {files:?}");
    }

    #[test]
    fn history_written_by_0_8_reads_the_same() {
        let v = Vault::in_memory(&[("A.md", "x")]);
        let a = NotePath::new("A.md");
        let folder = folder_for(&a);
        for (t, body) in [(1_000, "one"), (2_000, "two")] {
            v.storage().write_bytes_atomic(&NotePath::new(format!("{folder}/{t}.md.gz")), &gzip(body)).unwrap();
        }
        let h = v.history(&a);
        assert_eq!(h.iter().map(|s| s.at_ms).collect::<Vec<_>>(), [2_000, 1_000]);
        assert!(h.iter().all(|s| s.prov_file.is_none()));
        assert_eq!(v.read_snapshot(&h[1]).unwrap(), "one");
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
