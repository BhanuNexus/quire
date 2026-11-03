// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The thinking timeline (design §C.7): when claims about a topic appeared,
//! were taken back, or were contradicted.
//!
//! Events come from two places. The claim job appends to
//! `.quire/cache/claims/seen.jsonl` (device-local) whenever a passage's
//! claims change. And a note's history can be replayed: each saved version's
//! claims (from a model, or the offline sentence heuristic) compared with
//! the previous version's. See docs/formats/claims.md.

use std::collections::{BTreeMap, HashMap, HashSet};

use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::claims::Claim;

pub const SEEN_FILE: &str = ".quire/cache/claims/seen.jsonl";
/// History versions replayed per note.
pub const MAX_VERSIONS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    /// ● The claim appeared.
    Added,
    /// ○ It was taken out (or its note was superseded).
    Removed,
    /// ⚡ Another note contradicts it.
    Contradicted,
}

/// One line of `seen.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub v: u32,
    pub claim: String,
    pub path: NotePath,
    pub event: Event,
    pub at: i64,
    /// The claim's text, so taken-back claims can still be shown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// Append events (one line each).
pub fn append_seen(vault: &Vault, events: &[Seen]) -> Result<(), String> {
    for e in events {
        let line = serde_json::to_string(e).map_err(|e| e.to_string())?;
        vault.storage().append_line(&NotePath::new(SEEN_FILE), &line).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Every event recorded on this device (lines that don't read are skipped).
pub fn read_seen(vault: &Vault) -> Vec<Seen> {
    vault
        .storage()
        .read(&NotePath::new(SEEN_FILE))
        .map(|t| t.lines().filter_map(|l| serde_json::from_str::<Seen>(l).ok()).filter(|s| s.v <= 1).collect())
        .unwrap_or_default()
}

/// What changed between two claim sets of a passage or note.
pub fn changes<'a>(old: &'a [Claim], new: &'a [Claim]) -> (Vec<&'a Claim>, Vec<&'a Claim>) {
    let o: HashSet<&str> = old.iter().map(|c| c.id.as_str()).collect();
    let n: HashSet<&str> = new.iter().map(|c| c.id.as_str()).collect();
    (new.iter().filter(|c| !o.contains(c.id.as_str())).collect(), old.iter().filter(|c| !n.contains(c.id.as_str())).collect())
}

/// A point on the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub at: i64,
    pub event: Event,
    pub claim: String,
    pub text: String,
    pub path: NotePath,
}

/// Events from a note's versions, oldest first: `versions` are (time, claims).
/// The first version's claims count as added at its time.
pub fn replay(path: &NotePath, versions: &[(i64, Vec<Claim>)]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut prev: &[Claim] = &[];
    for (at, claims) in versions {
        let (added, removed) = changes(prev, claims);
        out.extend(added.into_iter().map(|c| Item {
            at: *at,
            event: Event::Added,
            claim: c.id.clone(),
            text: c.text.clone(),
            path: path.clone(),
        }));
        out.extend(removed.into_iter().map(|c| Item {
            at: *at,
            event: Event::Removed,
            claim: c.id.clone(),
            text: c.text.clone(),
            path: path.clone(),
        }));
        prev = claims;
    }
    out
}

/// Timeline items from `seen.jsonl` lines for the notes in `scope`.
pub fn from_seen<S: std::hash::BuildHasher>(seen: &[Seen], scope: &HashSet<NotePath, S>) -> Vec<Item> {
    seen.iter()
        .filter(|s| scope.contains(&s.path))
        .map(|s| Item { at: s.at, event: s.event, claim: s.claim.clone(), text: s.text.clone(), path: s.path.clone() })
        .collect()
}

/// Items in time order, without repeats.
///
/// A claim's "added" and "taken out" alternate in each note: adding a claim the note already has (the
/// same claim seen again in a later version, or reported by both the
/// replay and the claims job) is not shown again, nor is taking out one it
/// no longer has. The same contradiction within a minute shows once.
pub fn merge(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by(|a, b| a.at.cmp(&b.at).then(a.event.cmp(&b.event)).then_with(|| a.claim.cmp(&b.claim)));
    let mut last: HashMap<(String, Event, NotePath), i64> = HashMap::new();
    let mut present: HashMap<(String, NotePath), bool> = HashMap::new();
    items.retain(|i| {
        let state = (i.claim.clone(), i.path.clone());
        match i.event {
            Event::Added => present.insert(state, true) != Some(true),
            Event::Removed => present.insert(state, false) != Some(false),
            Event::Contradicted => {
                let key = (i.claim.clone(), i.event, i.path.clone());
                let keep = last.get(&key).is_none_or(|t| i.at - t > 60_000);
                if keep {
                    last.insert(key, i.at);
                }
                keep
            }
        }
    });
    items
}

/// Items grouped by day (`YYYY-MM-DD`, local time), oldest day first.
pub fn by_day(items: &[Item]) -> BTreeMap<String, Vec<&Item>> {
    let mut out: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
    for i in items {
        let day = chrono::DateTime::from_timestamp_millis(i.at)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        out.entry(day).or_default().push(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::heuristic_claims;

    #[test]
    fn review_a_claim_shows_once_however_many_versions_have_it() {
        let p = NotePath::new("Plan.md");
        let item = |at: i64, event: Event| Item { at, event, claim: "c_1".into(), text: "The launch is in March.".into(), path: p.clone() };
        let items = vec![
            item(1, Event::Added),
            item(200_000, Event::Added),
            item(400_000, Event::Added),
            item(500_000, Event::Removed),
            item(560_000, Event::Removed),
            item(600_000, Event::Added),
            item(700_000, Event::Contradicted),
            item(710_000, Event::Contradicted),
        ];
        let ev: Vec<(i64, Event)> = merge(items).iter().map(|i| (i.at, i.event)).collect();
        assert_eq!(
            ev,
            [(1, Event::Added), (500_000, Event::Removed), (600_000, Event::Added), (700_000, Event::Contradicted)],
            "once per change, not once per version"
        );
    }

    #[test]
    fn replays_versions_into_events() {
        let p = NotePath::new("Plan.md");
        let v1 = heuristic_claims("The launch is planned for March 2027 at the latest.\n");
        let v2 =
            heuristic_claims("The launch is planned for March 2027 at the latest.\nThe budget was approved by the board in October.\n");
        let v3 = heuristic_claims("The budget was approved by the board in October.\nThe launch moved to June 2027 after the review.\n");
        let items = replay(&p, &[(1, v1), (2, v2), (3, v3)]);
        let ev: Vec<(i64, Event)> = items.iter().map(|i| (i.at, i.event)).collect();
        assert_eq!(ev, [(1, Event::Added), (2, Event::Added), (3, Event::Added), (3, Event::Removed)]);
        assert!(items[3].text.contains("March 2027"));
        let merged = merge([items.clone(), items].concat());
        assert_eq!(merged.len(), 4, "repeats within a minute collapse");
        assert_eq!(by_day(&merged).len(), 1);
    }

    #[test]
    fn seen_lines_round_trip() {
        let vault = Vault::in_memory(&[]);
        let s = Seen {
            v: 1,
            claim: "c_1".into(),
            path: NotePath::new("A.md"),
            event: Event::Added,
            at: 5,
            text: "x".into(),
            other: serde_json::Map::new(),
        };
        append_seen(&vault, &[s.clone(), Seen { path: NotePath::new("B.md"), ..s.clone() }]).unwrap();
        vault.storage().append_line(&NotePath::new(SEEN_FILE), "{\"v\": 9, \"claim\": \"future\"}").unwrap();
        let all = read_seen(&vault);
        assert_eq!(all.len(), 2);
        let scope = HashSet::from([NotePath::new("A.md")]);
        assert_eq!(from_seen(&all, &scope).len(), 1);
        let line = serde_json::to_value(&s).unwrap();
        assert_eq!(line["event"], "added");
    }
}
