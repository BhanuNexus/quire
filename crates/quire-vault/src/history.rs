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
/// A snapshot of a note in a locked folder (0.11): the gzip, encrypted.
const SEALED_EXT: &str = ".md.gz.qenc";
const PROV_EXT: &str = ".prov.json";

/// The time of a snapshot file, and whether it is encrypted.
fn snapshot_time(name: &str) -> Option<(i64, bool)> {
    if let Some(t) = name.strip_suffix(SEALED_EXT) {
        return Some((t.parse().ok()?, true));
    }
    Some((name.strip_suffix(SNAPSHOT_EXT)?.parse().ok()?, false))
}

/// Which note each history folder is for (`{"dir":…,"path":…}` per line,
/// appended when a folder gets its first snapshot): the folder names are
/// hashes, so without it the history of a note that was deleted or renamed
/// can't be found by its old path (locking a folder encrypts that too).
const PATHS_LOG: &str = ".quire/history/paths.jsonl";

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

/// How long the versions around an agent's change stay, whatever the
/// thinning: the snapshot with provenance and the one just before it, which
/// the agent ledger points at and undoing the change needs.
pub(crate) const PINNED_FOR_MS: i64 = 90 * DAY;

/// [`retained`], plus the versions an undo may need: every snapshot with
/// provenance (`true`) younger than [`PINNED_FOR_MS`], and the snapshot just
/// before each of them.
pub(crate) fn retained_pinned(mut snaps: Vec<(i64, bool)>, now: i64) -> Vec<i64> {
    snaps.sort_unstable_by_key(|(t, _)| *t);
    let mut keep = retained(snaps.iter().map(|(t, _)| *t).collect(), now);
    for (i, (t, prov)) in snaps.iter().enumerate() {
        if *prov && now - t < PINNED_FOR_MS {
            keep.push(*t);
            if let Some((before, _)) = i.checked_sub(1).and_then(|j| snaps.get(j)) {
                keep.push(*before);
            }
        }
    }
    keep.sort_unstable_by(|a, b| b.cmp(a));
    keep.dedup();
    keep
}

impl Vault {
    /// Snapshots of a note, newest first.
    pub fn history(&self, note: &NotePath) -> Vec<Snapshot> {
        let files = self.storage().list_under(&folder_for(note)).unwrap_or_default();
        let provs: std::collections::HashMap<i64, NotePath> =
            files.iter().filter_map(|f| Some((f.file_name().strip_suffix(PROV_EXT)?.parse().ok()?, f.clone()))).collect();
        // A conversion interrupted between writing one form and removing the
        // other leaves both: the plain one is shown (same text).
        let mut v: Vec<(Snapshot, bool)> = files
            .into_iter()
            .filter_map(|f| {
                let (at_ms, sealed) = snapshot_time(f.file_name())?;
                Some((Snapshot { prov_file: provs.get(&at_ms).cloned(), file: f, at_ms }, sealed))
            })
            .collect();
        v.sort_by_key(|(s, sealed)| (std::cmp::Reverse(s.at_ms), *sealed));
        v.dedup_by_key(|(s, _)| s.at_ms);
        v.into_iter().map(|(s, _)| s).collect()
    }

    /// Who made a snapshot, as written by [`Vault::snapshot_with`]; `None`
    /// for typing saves, older versions or an unreadable file.
    pub fn read_provenance(&self, s: &Snapshot) -> Option<serde_json::Value> {
        let bytes = self.storage().read_bytes(s.prov_file.as_ref()?).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// A snapshot's text. One of a locked note needs its folder unlocked.
    pub fn read_snapshot(&self, s: &Snapshot) -> Result<String> {
        let bytes = self.snapshot_gz(&s.file)?;
        gunzip(&bytes).map_err(|e| VaultError::Io { path: s.file.to_string(), source: e })
    }

    /// The gzip of a snapshot file, decrypted if it is encrypted.
    fn snapshot_gz(&self, file: &NotePath) -> Result<Vec<u8>> {
        let bytes = self.storage().read_bytes(file)?;
        if !file.as_str().ends_with(SEALED_EXT) {
            return Ok(bytes);
        }
        let h = crate::lock::format::Header::parse(&bytes).map_err(|e| crate::locked::damaged(file, &e))?;
        let key = self.keys().by_id(h.folder).ok_or_else(|| VaultError::Locked("this note's folder".into()))?;
        let kind = crate::lock::format::Kind::Snapshot;
        key.open(kind, h.folder, &bytes)
            .or_else(|e| self.keys().previous_by_id(h.folder).ok_or(e).and_then(|p| p.open(kind, h.folder, &bytes)))
            .map_err(|e| crate::locked::damaged(file, &e))
    }

    /// Encrypt `note`'s snapshots that are still under `old` (a folder's
    /// key before a key change) with `new`. Returns how many changed.
    pub(crate) fn reseal_history(
        &self,
        note: &NotePath,
        id: crate::lock::FolderId,
        old: &crate::lock::FolderKey,
        new: &crate::lock::FolderKey,
    ) -> Result<usize> {
        let kind = crate::lock::format::Kind::Snapshot;
        let mut n = 0;
        for f in self.storage().list_under(&folder_for(note))?.into_iter().filter(|f| f.as_str().ends_with(SEALED_EXT)) {
            let data = match self.storage().read_bytes(&f) {
                Ok(d) => d,
                // Pruned by a save meanwhile.
                Err(VaultError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if crate::lock::format::Header::parse(&data).map(|h| h.folder) != Ok(id) || new.open(kind, id, &data).is_ok() {
                continue;
            }
            let gz = old.open(kind, id, &data).map_err(|e| crate::locked::damaged(&f, &e))?;
            let sealed = new.seal(kind, id, &gz).map_err(|e| crate::locked::damaged(&f, &e))?;
            if self.storage().write_bytes_if(&f, &|cur| cur == Some(data.as_slice()), &sealed)? {
                if new.open(kind, id, &self.storage().read_bytes(&f)?).ok().as_deref() != Some(gz.as_slice()) {
                    return Err(VaultError::Damaged { path: f.to_string(), reason: "the copy didn't read back the same".into() });
                }
                n += 1;
            }
        }
        Ok(n)
    }

    /// Where `note`'s snapshots are written: plain, or encrypted with its
    /// locked folder's key (`Err(Locked)` while that folder is locked).
    fn snapshot_form(&self, note: &NotePath) -> Result<Option<(crate::lock::FolderId, crate::lock::FolderKey)>> {
        match self.locked_folder(note) {
            Some(f) if !f.unlocking => self.keys().get(&f.path).map(Some).ok_or(VaultError::Locked(f.path)),
            _ => Ok(None),
        }
    }

    /// Write one snapshot file for `note` at `at` from its gzip bytes, in
    /// the form `note` needs. Returns the file written.
    fn put_snapshot(&self, note: &NotePath, at: i64, gz: &[u8]) -> Result<NotePath> {
        let folder = folder_for(note);
        if self.storage().list_under(&folder).is_ok_and(|v| v.is_empty()) {
            let line = serde_json::json!({ "dir": folder.rsplit('/').next().unwrap_or_default(), "path": note.as_str() });
            self.storage().append_line(&NotePath::new(PATHS_LOG), &line.to_string())?;
        }
        if let Some((id, key)) = self.snapshot_form(note)? {
            let file = NotePath::new(format!("{folder}/{at}{SEALED_EXT}"));
            let sealed = key.seal(crate::lock::format::Kind::Snapshot, id, gz).map_err(|e| crate::locked::damaged(&file, &e))?;
            self.storage().write_bytes_atomic(&file, &sealed)?;
            return Ok(file);
        }
        let file = NotePath::new(format!("{folder}/{at}{SNAPSHOT_EXT}"));
        self.storage().write_bytes_atomic(&file, gz)?;
        Ok(file)
    }

    /// Bring `from`'s history to `to` (the same note after a move, or the
    /// same path after its folder was locked or unlocked), encrypting or
    /// decrypting each snapshot to match where `to` is. Each file is
    /// written and read back before its old form is removed, so an
    /// interruption leaves a readable copy. Returns how many changed.
    pub fn rehome_history(&self, from: &NotePath, to: &NotePath) -> Result<usize> {
        let (a, b) = (folder_for(from), folder_for(to));
        let want_sealed = self.snapshot_form(to)?.is_some();
        let mut changed = 0;
        for f in self.storage().list_under(&a)? {
            let name = f.file_name().to_string();
            if let Some((at, sealed)) = snapshot_time(&name) {
                if sealed == want_sealed && a == b {
                    continue;
                }
                let gz = match self.snapshot_gz(&f) {
                    Ok(gz) => gz,
                    // Pruned by a save meanwhile.
                    Err(VaultError::NotFound(_)) => continue,
                    Err(e) => return Err(e),
                };
                let written = self.put_snapshot(to, at, &gz)?;
                if self.snapshot_gz(&written)? != gz {
                    return Err(VaultError::Damaged { path: written.to_string(), reason: "the copy didn't read back the same".into() });
                }
                if written != f {
                    self.storage().remove(&f)?;
                }
                changed += 1;
            } else if a != b {
                // A temporary file is another write still in flight (it is
                // renamed into place or removed in a moment): not ours to move.
                if name.starts_with(".tmp") {
                    continue;
                }
                let bytes = match self.storage().read_bytes(&f) {
                    Ok(b) => b,
                    Err(VaultError::NotFound(_)) => continue,
                    Err(e) => return Err(e),
                };
                self.storage().write_bytes_atomic(&NotePath::join(&b, &name), &bytes)?;
                self.storage().remove(&f)?;
            }
        }
        Ok(changed)
    }

    /// Notes under `folder` that have (or had) history here, as the path log
    /// recorded them: notes since deleted or renamed included.
    pub fn history_paths_under(&self, folder: &str) -> Vec<NotePath> {
        let inside = crate::lock::LockedFolder { path: folder.trim_matches('/').to_string(), unlocking: false };
        let text = self.storage().read(&NotePath::new(PATHS_LOG)).unwrap_or_default();
        let mut out: Vec<NotePath> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["path"].as_str().map(NotePath::new))
            .filter(|p| inside.contains(p.as_str()) && !self.storage().list_under(&folder_for(p)).unwrap_or_default().is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
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
        // A locked note's provenance isn't written: it holds a hash of the text.
        let prov = if self.locked_folder(note).is_some() { None } else { prov };
        if let Some(p) = prov {
            // Provenance first: a snapshot is never visible without the record of who made it.
            let bytes = serde_json::to_vec(p).map_err(|e| VaultError::Io { path: file.to_string(), source: std::io::Error::other(e) })?;
            self.storage().write_bytes_atomic(&NotePath::new(format!("{folder}/{at}{PROV_EXT}")), &bytes)?;
        }
        self.put_snapshot(note, at, &gzip(content))?;
        self.prune_history(note, now_ms)?;
        Ok(true)
    }

    /// Thin out old snapshots, removing provenance with its snapshot, and
    /// delete provenance files whose snapshot is gone (Quire 0.8 prunes
    /// snapshots without knowing about them).
    fn prune_history(&self, note: &NotePath, now_ms: i64) -> Result<()> {
        let hist = self.history(note);
        let keep: std::collections::HashSet<i64> =
            retained_pinned(hist.iter().map(|s| (s.at_ms, s.prov_file.is_some())).collect(), now_ms).into_iter().collect();
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
        let files = self.storage().list_under(&a)?;
        if !files.is_empty() && self.storage().list_under(&b).is_ok_and(|v| v.is_empty()) {
            let line = serde_json::json!({ "dir": b.rsplit('/').next().unwrap_or_default(), "path": to.as_str() });
            self.storage().append_line(&NotePath::new(PATHS_LOG), &line.to_string())?;
        }
        for f in files {
            // A temporary file is a write still in flight; one gone meanwhile was pruned.
            if f.file_name().starts_with(".tmp") {
                continue;
            }
            let bytes = match self.storage().read_bytes(&f) {
                Ok(b) => b,
                Err(VaultError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
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
        // Two versions in the same old week, past the time agent versions
        // are pinned: retention keeps only the newer.
        let old = (now - 100 * DAY) / (7 * DAY) * (7 * DAY) + DAY;
        v.snapshot_with(&a, "old 1", old, true, Some(&prov)).unwrap();
        v.snapshot_with(&a, "old 2", old + HOUR, true, Some(&prov)).unwrap();
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
    fn review_thinning_keeps_the_versions_an_undo_needs() {
        let v = Vault::in_memory(&[("A.md", "x")]);
        let a = NotePath::new("A.md");
        let day = 1_800_000_000_000 / DAY * DAY;
        let prov = serde_json::json!({"format": 1, "actor": {"kind": "agent", "id": "mcp:claude-code"}});
        // 14:04:59 the version before the agent's write, 14:05 the agent's, 14:30 the person's.
        let (before, agent, human) = (day + 14 * HOUR + 4 * MIN + 59_000, day + 14 * HOUR + 5 * MIN, day + 14 * HOUR + 30 * MIN);
        v.snapshot(&a, "before", before, true).unwrap();
        v.snapshot_with(&a, "agent", agent, true, Some(&prov)).unwrap();
        v.snapshot(&a, "human", human, true).unwrap();
        // The next day, and a month later, thinning runs again.
        for later in [day + DAY + 10 * HOUR, day + 40 * DAY] {
            v.snapshot(&a, &format!("later {later}"), later, true).unwrap();
            let kept: Vec<i64> = v.history(&a).iter().map(|s| s.at_ms).collect();
            assert!(kept.contains(&before) && kept.contains(&agent), "{kept:?}");
        }
        // Past the pinning time, thinning applies as before.
        v.snapshot(&a, "much later", day + 200 * DAY, true).unwrap();
        let kept: Vec<i64> = v.history(&a).iter().map(|s| s.at_ms).collect();
        assert!(!kept.contains(&before), "{kept:?}");
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
