// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compaction: fold old revisions into a sealed snapshot (0.13,
//! `docs/design/0.13-sync.md` §1).
//!
//! In a synced folder, S3 or WebDAV every device's lane only grows. A
//! **snapshot** of generation *g* covers every lane up to a cut and holds
//! the revisions still needed: each note's current revision, recent
//! history, and anything a device may still build on (a revision that
//! lost a tie and wasn't merged again, and the revision it forked from).
//! Every other revision before the cut is an ancestor of a note's current
//! revision and is dropped. Kept revisions keep their ids, parents and
//! sealed bytes; nothing is sealed again.
//!
//! Readers take the snapshot plus every lane line after its cut. Lines up
//! to the cut stay readable until each device deletes its own, a week
//! after the snapshot appeared ([`SETTLE`]), so a second snapshot made at
//! the same moment can still win. Two snapshots of one generation: the one
//! made by the device with the smaller id wins, on every device.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::crypto::hex;
use crate::keyring::KeySet;
use crate::protocol::{DeviceId, FileId, LanePos, RevId, SpaceId};

/// How long a snapshot waits before lanes up to its cut are deleted.
///
/// Also before the next one may be made. Far longer than any sync delay, so a
/// snapshot made elsewhere at the same time is seen before anything goes.
pub const SETTLE: i64 = 7 * 24 * 3600;
/// A part file of a snapshot holds about this many bytes.
pub const PART_BYTES: usize = 1 << 20;
/// The format of snapshot manifests.
pub const FORMAT: u32 = 1;

/// What history to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Keep every revision written in the last this many days, and every
    /// revision replaced in that time. Also the grace for devices that are
    /// offline: one offline longer that edited a note others changed gets a
    /// conflict copy instead of a merge.
    pub keep_days: u32,
    /// Keep at least this many versions of each note.
    pub min_versions: usize,
}

impl Policy {
    pub const DEFAULT: Self = Self { keep_days: 90, min_versions: 10 };
}

impl Default for Policy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One part file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub lines: u64,
}

/// What compaction folded and kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub kept: u64,
    pub dropped: u64,
    pub kept_bytes: u64,
    pub dropped_bytes: u64,
}

/// A snapshot's `manifest.json`, written last: a snapshot without one (or
/// whose parts don't match it) isn't used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub space: SpaceId,
    /// `""` for notes, `"shared"` for vault settings.
    #[serde(default)]
    pub lane: String,
    pub generation: u32,
    pub maker: DeviceId,
    /// Unix seconds.
    pub made: i64,
    /// The key epoch that authenticates it.
    pub epoch: u32,
    /// Every lane is covered up to here.
    pub cut: BTreeMap<DeviceId, LanePos>,
    pub parts: Vec<Part>,
    /// Where each note's current revision is found from: its oldest kept
    /// revision on the way to it.
    pub starts: BTreeMap<FileId, RevId>,
    /// `(a, b)`: kept revision `b` is an ancestor of kept revision `a`
    /// through revisions that were dropped.
    pub links: Vec<(RevId, RevId)>,
    pub stats: Stats,
    #[serde(default)]
    pub mac: String,
}

const MANIFEST_MAC: &str = "quire-sync/snapshot/v1";

impl Manifest {
    /// `<generation>-<maker>`: the folder (or key prefix) of its files.
    pub fn name(&self) -> String {
        name(self.generation, &self.maker)
    }

    fn body(&self) -> Vec<u8> {
        serde_json::to_vec(&Self { mac: String::new(), ..self.clone() }).unwrap_or_default()
    }

    /// Authenticate it with the vault key of `epoch`.
    pub fn seal(&mut self, keys: &KeySet) {
        self.epoch = keys.current();
        self.mac = hex(&keys.current_keys().mac(MANIFEST_MAC, &self.body()));
    }

    /// Whether it is authentic, for `space` and `lane`.
    pub fn verify(&self, keys: &KeySet, space: &SpaceId, lane: &str) -> bool {
        self.format == FORMAT
            && self.space == *space
            && self.lane == lane
            && keys
                .get(self.epoch)
                .is_some_and(|k| crate::crypto::unhex(&self.mac).is_some_and(|t| k.verify_mac(MANIFEST_MAC, &self.body(), &t)))
    }
}

/// SHA-256 of `b`, hex.
pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(b))
}

/// `000004-<device>`.
pub fn name(generation: u32, maker: &DeviceId) -> String {
    format!("{generation:06}-{maker}")
}

/// `(generation, maker)` from a snapshot's folder name.
pub fn parse_name(s: &str) -> Option<(u32, DeviceId)> {
    let (g, d) = (s.get(..6)?, s.get(6..)?.strip_prefix('-')?);
    if !g.bytes().all(|b| b.is_ascii_digit()) || !crate::folder::is_device_name(d) {
        return None;
    }
    Some((g.parse().ok()?, DeviceId(d.to_string())))
}

/// Whether `s` is a part's name (`part-000001.jsonl`).
pub fn parse_part(s: &str) -> bool {
    s.len() == 17 && s.starts_with("part-") && s.get(11..) == Some(".jsonl") && s[5..11].bytes().all(|b| b.is_ascii_digit())
}

/// The snapshot that counts among complete ones: the newest generation,
/// and in it the one whose maker has the smallest id.
pub fn winner<'a>(complete: impl IntoIterator<Item = &'a Manifest>) -> Option<&'a Manifest> {
    complete.into_iter().min_by(|a, b| (std::cmp::Reverse(a.generation), &a.maker).cmp(&(std::cmp::Reverse(b.generation), &b.maker)))
}

/// A revision, as compaction sees it.
#[derive(Debug, Clone)]
pub struct Node {
    pub file: FileId,
    pub parents: Vec<RevId>,
    /// When it was written (Unix ms; 0 if unknown or unreadable).
    pub at: i64,
    pub bytes: u64,
    /// It couldn't be decrypted here (a newer key): never dropped.
    pub opaque: bool,
}

/// What to keep.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub keep: HashSet<RevId>,
    pub starts: BTreeMap<FileId, RevId>,
    pub links: Vec<(RevId, RevId)>,
    pub stats: Stats,
}

/// Every revision `r` builds on, through parents and earlier links.
fn ancestry<S: std::hash::BuildHasher, T: std::hash::BuildHasher>(
    nodes: &HashMap<RevId, Node, S>,
    links: &HashMap<RevId, Vec<RevId>, T>,
    r: &RevId,
) -> HashSet<RevId> {
    let mut seen = HashSet::new();
    let mut todo = vec![r.clone()];
    while let Some(x) = todo.pop() {
        if !seen.insert(x.clone()) {
            continue;
        }
        if let Some(n) = nodes.get(&x) {
            todo.extend(n.parents.iter().cloned());
        }
        if let Some(l) = links.get(&x) {
            todo.extend(l.iter().cloned());
        }
    }
    seen
}

/// Work out what to keep. `heads` is each note's current revision;
/// `links` the ancestor links of the snapshot in use; `now` Unix ms.
pub fn plan<S: std::hash::BuildHasher, T: std::hash::BuildHasher>(
    nodes: &HashMap<RevId, Node, S>,
    heads: &BTreeMap<FileId, RevId>,
    links: &HashMap<RevId, Vec<RevId>, T>,
    policy: Policy,
    now: i64,
) -> Plan {
    let recent = now - i64::from(policy.keep_days) * 24 * 3600 * 1000;
    let mut keep: HashSet<RevId> = HashSet::new();
    // Recent, unreadable here, or of a note with no current revision: kept.
    for (id, n) in nodes {
        if n.at >= recent || n.opaque || !heads.contains_key(&n.file) {
            keep.insert(id.clone());
        }
    }
    let mut by_file: HashMap<&FileId, Vec<&RevId>> = HashMap::new();
    for (id, n) in nodes {
        by_file.entry(&n.file).or_default().push(id);
    }
    let mut starts = BTreeMap::new();
    for (file, head) in heads {
        if !nodes.contains_key(head) {
            continue;
        }
        let anc = ancestry(nodes, links, head);
        // The way to the head: recent replacements and the last few versions.
        let mut cur = head.clone();
        let mut i = 0;
        keep.insert(cur.clone());
        while let Some(next) = nodes.get(&cur).and_then(|n| n.parents.first()).filter(|p| nodes.contains_key(*p)).cloned() {
            // When `next` was replaced (an unreadable replacement counts as now).
            let replaced_at = nodes.get(&cur).map_or(0, |n| if n.opaque { now } else { n.at });
            i += 1;
            if i < policy.min_versions || replaced_at >= recent {
                keep.insert(next.clone());
            }
            if i > nodes.len() {
                break;
            }
            cur = next;
        }
        // Anything not folded into the head (a tie lost and not merged
        // again), and the way back to where it forked, so its device can
        // still merge three-way.
        for id in by_file.get(file).into_iter().flatten() {
            if anc.contains(*id) {
                continue;
            }
            keep.insert((*id).clone());
            let mut c = (*id).clone();
            for _ in 0..nodes.len() {
                let Some(p) = nodes.get(&c).and_then(|n| n.parents.first()).cloned() else { break };
                keep.insert(p.clone());
                if anc.contains(&p) || !nodes.contains_key(&p) {
                    break;
                }
                c = p;
            }
        }
        // Where the head is found from: the oldest kept revision on its way.
        let mut start = head.clone();
        while let Some(p) = nodes.get(&start).and_then(|n| n.parents.first()).filter(|p| keep.contains(*p) && nodes.contains_key(*p)) {
            start = p.clone();
        }
        starts.insert(file.clone(), start);
    }
    keep.retain(|k| nodes.contains_key(k));
    // Ancestor links across what's dropped.
    let mut out_links = Vec::new();
    for k in &keep {
        let Some(n) = nodes.get(k) else { continue };
        let mut todo: VecDeque<RevId> = n.parents.iter().filter(|p| !keep.contains(*p)).cloned().collect();
        todo.extend(links.get(k).into_iter().flatten().cloned());
        let mut seen = HashSet::new();
        while let Some(x) = todo.pop_front() {
            if !seen.insert(x.clone()) {
                continue;
            }
            if keep.contains(&x) {
                out_links.push((k.clone(), x));
                continue;
            }
            if let Some(m) = nodes.get(&x) {
                todo.extend(m.parents.iter().cloned());
            }
            todo.extend(links.get(&x).into_iter().flatten().cloned());
        }
    }
    out_links.sort();
    out_links.dedup();
    let mut stats = Stats::default();
    for (id, n) in nodes {
        if keep.contains(id) {
            stats.kept += 1;
            stats.kept_bytes += n.bytes;
        } else {
            stats.dropped += 1;
            stats.dropped_bytes += n.bytes;
        }
    }
    Plan { keep, starts, links: out_links, stats }
}

/// Whether compacting now is worth it: at least 1 000 revisions and half
/// the bytes would go.
pub const fn worth_it(s: &Stats) -> bool {
    s.dropped >= 1000 && s.dropped_bytes >= s.kept_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> RevId {
        RevId(format!("{n:032x}"))
    }

    fn node(file: &FileId, parents: &[u32], at: i64) -> Node {
        Node { file: file.clone(), parents: parents.iter().map(|p| id(*p)).collect(), at, bytes: 300, opaque: false }
    }

    #[test]
    fn old_history_goes_ties_and_their_fork_points_stay() {
        let f = FileId("f".repeat(32));
        let day = 24 * 3600 * 1000;
        let now = 1000 * day;
        // 1 → 2 → … → 30 (head), all old; 31 lost a tie at 5 and was never merged.
        let mut nodes = HashMap::new();
        nodes.insert(id(1), node(&f, &[], day));
        for n in 2..=30 {
            nodes.insert(id(n), node(&f, &[n - 1], i64::from(n) * day));
        }
        nodes.insert(id(31), node(&f, &[5], 6 * day));
        let heads = BTreeMap::from([(f.clone(), id(30))]);
        let p = plan(&nodes, &heads, &HashMap::new(), Policy { keep_days: 90, min_versions: 3 }, now);
        let kept: HashSet<u32> = (1..=31).filter(|n| p.keep.contains(&id(*n))).collect();
        assert_eq!(kept, HashSet::from([28, 29, 30, 31, 5]));
        assert_eq!(p.starts[&f], id(28));
        // 5 is reached from 28 only through dropped revisions: a link says so.
        assert!(p.links.contains(&(id(28), id(5))), "{:?}", p.links);
        assert_eq!(p.stats.dropped, 26);
    }

    #[test]
    fn recent_history_and_unreadable_revisions_stay() {
        let f = FileId("a".repeat(32));
        let day = 24 * 3600 * 1000;
        let now = 1000 * day;
        let mut nodes = HashMap::new();
        nodes.insert(id(1), node(&f, &[], 1));
        nodes.insert(id(2), node(&f, &[1], now - 10 * day));
        nodes.insert(id(3), Node { opaque: true, ..node(&f, &[2], 0) });
        let heads = BTreeMap::from([(f.clone(), id(3))]);
        let p = plan(&nodes, &heads, &HashMap::new(), Policy { keep_days: 90, min_versions: 1 }, now);
        // 1 was replaced 10 days ago: kept.
        assert_eq!(p.keep.len(), 3);
        assert_eq!(p.starts[&f], id(1));
    }

    #[test]
    fn concurrent_snapshots_pick_the_same_winner() {
        let space = SpaceId("1".repeat(32));
        let m = |g: u32, d: &str| Manifest {
            format: FORMAT,
            space: space.clone(),
            lane: String::new(),
            generation: g,
            maker: DeviceId(d.repeat(32)),
            made: 0,
            epoch: 0,
            cut: BTreeMap::new(),
            parts: vec![],
            starts: BTreeMap::new(),
            links: vec![],
            stats: Stats::default(),
            mac: String::new(),
        };
        let all = [m(2, "b"), m(2, "a"), m(1, "0")];
        assert_eq!(winner(&all).unwrap().maker.0, "a".repeat(32));
        let rev: Vec<Manifest> = all.iter().rev().cloned().collect();
        assert_eq!(winner(&rev).unwrap().maker.0, "a".repeat(32));
        assert_eq!(parse_name(&all[0].name()), Some((2, DeviceId("b".repeat(32)))));
        assert_eq!(parse_name("2-x"), None);
    }
}
