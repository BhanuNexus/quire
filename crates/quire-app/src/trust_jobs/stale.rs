// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The staleness job (design §C.6).
//!
//! Once a day, and after changes at most hourly, it scores every note and
//! keeps the Inbox's "Getting stale" in step. It reaches the status bar
//! only as part of the weekly digest.
//!
//! A finding the person dismissed stays away until the note changes: the
//! job remembers which version of each note it last raised
//! (`.quire/cache/stale.json`, device-local).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use quire_agent::inbox::{InboxItem, StaleReason};
use quire_intel::stale::{self, LinkChange, Reason};
use quire_markdown::Value;
use quire_vault::{ContentHash, NotePath, Vault};
use serde::{Deserialize, Serialize};

use super::TrustShared;
use crate::jobs::{Budget, Job, JobCx, Needs, Step};

/// After a change, wait at least this long since the last run.
pub const AFTER_CHANGE: Duration = Duration::from_hours(1);
/// Run at least this often.
pub const DAILY: Duration = Duration::from_hours(24);
/// The first run waits this long after the vault opens.
const FIRST_RUN: Duration = Duration::from_mins(2);
/// Notes scored per step.
const CHUNK: usize = 150;
pub const SEEN_FILE: &str = ".quire/cache/stale.json";

/// `[staleness]` in `.quire/shared/vault.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StalePrefs {
    /// Flag notes scoring at least this (0–1).
    pub flag_at: f32,
    /// Folders never flagged (the daily-notes and templates folders always are left out).
    pub exclude: Vec<String>,
    /// How long notes in a folder stay fresh: `{ "Projects/" = "90d" }`.
    pub folders: BTreeMap<String, String>,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for StalePrefs {
    fn default() -> Self {
        Self { flag_at: stale::FLAG_AT, exclude: vec!["raw/".into()], folders: BTreeMap::new(), other: toml::Table::new() }
    }
}

impl StalePrefs {
    pub fn load(vault: &Vault) -> Self {
        quire_vault::VaultPrefs::load(vault).section::<Self>("staleness")
    }

    fn excluded(&self, path: &NotePath, vault: &Vault) -> bool {
        let p = path.as_str().to_lowercase();
        let under = |f: &str| {
            let f = f.trim_matches('/').to_lowercase();
            !f.is_empty() && p.starts_with(&format!("{f}/"))
        };
        under(&vault.config.daily_folder)
            || under(&vault.config.templates_folder)
            || self.exclude.iter().any(|f| under(f))
            || p.starts_with(".quire/")
    }

    /// The fresh period of a folder (the longest matching prefix wins).
    fn folder_days(&self, path: &NotePath) -> Option<u32> {
        let p = path.as_str().to_lowercase();
        self.folders
            .iter()
            .filter(|(f, _)| p.starts_with(&format!("{}/", f.trim_matches('/').to_lowercase())))
            .max_by_key(|(f, _)| f.len())
            .and_then(|(_, d)| stale::parse_days(d))
    }
}

/// Which version of each note was last raised as stale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub format: u32,
    pub notes: BTreeMap<String, String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Seen {
    pub fn load(vault: &Vault) -> Self {
        vault.storage().read_bytes(&NotePath::new(SEEN_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, vault: &Vault) {
        if let Ok(b) = serde_json::to_vec(&Self { format: 1, ..self.clone() }) {
            let _ = vault.storage().write_bytes_atomic(&NotePath::new(SEEN_FILE), &b);
        }
    }
}

/// What the scorer needs from one note.
struct NoteFacts {
    path: NotePath,
    hash: ContentHash,
    modified: i64,
    plain: String,
    props: BTreeMap<String, Value>,
    /// Resolved link targets with their last change.
    links: Vec<(NotePath, String, i64)>,
}

fn prop_str(props: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    props.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).and_then(|(_, v)| match v {
        Value::Str(s) => Some(s.clone()),
        Value::List(l) => l.first().cloned(),
        Value::Map(_) => None,
    })
}

fn date_of_ms(ms: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).date_naive())
}

/// When a person last edited `path`, from its history (agents' versions
/// skipped); the file time when history says nothing.
fn last_human_edit(vault: &Vault, path: &NotePath, modified: i64) -> i64 {
    for s in vault.history(path).iter().take(30) {
        let agent = vault
            .read_provenance(s)
            .and_then(|p| p.get("actor").and_then(|a| a.get("kind")).and_then(serde_json::Value::as_str).map(|k| k == "agent"))
            .unwrap_or(false);
        if !agent {
            return s.at_ms.min(modified.max(s.at_ms));
        }
    }
    modified
}

/// Lines of `target` changed since `since_ms`: against the newest version
/// saved at or before then. `None` when history doesn't reach back.
fn changed_since(vault: &Vault, target: &NotePath, current: &str, since_ms: i64) -> Option<(u32, u32)> {
    let snap = vault.history(target).into_iter().find(|s| s.at_ms <= since_ms)?;
    let old = vault.read_snapshot(&snap).ok()?;
    let (added, removed) = crate::history_ops::diff_stats(&old, current);
    Some((u32::try_from(added + removed).unwrap_or(u32::MAX), u32::try_from(current.lines().count().max(1)).unwrap_or(u32::MAX)))
}

/// Score one note; `None` when it isn't stale.
fn assess(
    vault: &Vault,
    prefs: &StalePrefs,
    n: &NoteFacts,
    today: NaiveDate,
    now_ms: i64,
    source: &dyn Fn(&NotePath) -> Option<String>,
) -> Option<InboxItem> {
    let p = &n.props;
    let reviewed = prop_str(p, "reviewed").and_then(|d| stale::parse_date(&d));
    let half = prop_str(p, "review_every").and_then(|d| stale::parse_days(&d)).or_else(|| prefs.folder_days(&n.path));
    let valid_until = prop_str(p, "valid_until").and_then(|d| stale::parse_date(&d));
    let superseded_by = prop_str(p, "superseded_by").filter(|s| !s.trim().is_empty());
    let mut input = stale::Input {
        today,
        last_edit: date_of_ms(n.modified),
        reviewed,
        half_days: half,
        valid_until,
        superseded_by,
        text: &n.plain,
        links: Vec::new(),
    };
    let quick = stale::score(&input);
    // Links can add at most 0.35: skip the history reads when that can't flag it.
    if !quick.hard && quick.score + 0.35 < prefs.flag_at {
        return None;
    }
    let edited = last_human_edit(vault, &n.path, n.modified);
    input.last_edit = date_of_ms(edited);
    for (target, name, modified) in &n.links {
        if *modified <= edited {
            continue;
        }
        let Some(text) = source(target) else { continue };
        if let Some((changed_lines, lines)) = changed_since(vault, target, &text, edited) {
            input.links.push(LinkChange { target: name.clone(), changed_lines, lines });
        }
    }
    let s = stale::score(&input);
    let _ = now_ms;
    if !s.flagged(prefs.flag_at) {
        return None;
    }
    let reasons = s
        .reasons
        .iter()
        .map(|r| match r {
            Reason::Age { days } => StaleReason::Age { days: *days },
            Reason::LinkedChanged { target } => StaleReason::LinkedChanged { path: NotePath::new(target.clone()) },
            Reason::TimeBound { phrase, count } => StaleReason::TimeBound { phrase: phrase.clone(), count: *count },
            Reason::ValidUntil { date } => StaleReason::ValidUntil { date: date.clone() },
            Reason::Superseded { by } => StaleReason::Superseded { by: by.clone() },
        })
        .collect();
    // Two decimals: the stored item changes when the score really does.
    let score = (s.score * 100.0).round() / 100.0;
    Some(InboxItem::Stale { path: n.path.clone(), score, reasons })
}

enum Phase {
    Waiting,
    Scoring { paths: Vec<NotePath>, found: Vec<(InboxItem, ContentHash)>, prefs: StalePrefs, started: Instant },
}

/// What the last run did.
#[derive(Debug, Clone, Default)]
pub struct StaleRun {
    pub at: Option<Instant>,
    pub notes: usize,
    pub flagged: usize,
    pub took: Duration,
}

pub struct StaleJob {
    shared: Arc<TrustShared>,
    phase: Phase,
    opened: Instant,
    last_run: Option<Instant>,
    seen_changes: u64,
    changed: bool,
}

impl StaleJob {
    pub fn new(shared: Arc<TrustShared>) -> Self {
        Self { shared, phase: Phase::Waiting, opened: Instant::now(), last_run: None, seen_changes: 0, changed: false }
    }

    fn due(&mut self) -> Option<Duration> {
        let n = self.shared.changes.load(Ordering::Acquire);
        if n != self.seen_changes {
            self.seen_changes = n;
            self.changed = true;
        }
        if self.shared.stale_now.swap(false, Ordering::AcqRel) {
            return None;
        }
        let wait = match self.last_run {
            None => FIRST_RUN.saturating_sub(self.opened.elapsed()),
            Some(t) if self.changed => AFTER_CHANGE.saturating_sub(t.elapsed()),
            Some(t) => DAILY.saturating_sub(t.elapsed()),
        };
        (!wait.is_zero()).then_some(wait)
    }
}

impl Job for StaleJob {
    fn id(&self) -> &'static str {
        "staleness"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 2_000, max_calls_per_day: 0, needs: Needs::Any }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Step::Done;
        }
        if !self.shared.settings().staleness {
            return Step::Idle(Duration::from_secs(5));
        }
        match &mut self.phase {
            Phase::Waiting => {
                if let Some(wait) = self.due() {
                    // Look again every second: "Check now" and new changes are atomics.
                    return Step::Idle(wait.min(Duration::from_secs(1)));
                }
                self.changed = false;
                let paths = cx.index.read().map(|ix| ix.paths()).unwrap_or_default();
                self.phase = Phase::Scoring { paths, found: Vec::new(), prefs: StalePrefs::load(&cx.vault), started: Instant::now() };
                Step::Continue
            }
            Phase::Scoring { paths, found, prefs, started } => {
                if !paths.is_empty() {
                    let take: Vec<NotePath> = paths.drain(..paths.len().min(CHUNK)).filter(|p| !prefs.excluded(p, &cx.vault)).collect();
                    // Copy under the lock (briefly); score outside it.
                    let facts: Vec<NoteFacts> = cx
                        .index
                        .read()
                        .map(|ix| {
                            take.iter()
                                .filter_map(|p| {
                                    let r = ix.get(p)?;
                                    let src = ix.source(p)?;
                                    let links = r
                                        .links
                                        .iter()
                                        .filter(|l| l.is_internal() && !l.target.is_empty())
                                        .filter_map(|l| {
                                            let t = ix.resolve(&l.target, Some(p))?;
                                            (t != *p).then(|| (t.clone(), t.stem().to_string(), ix.modified(&t).unwrap_or(0)))
                                        })
                                        .collect();
                                    Some(NoteFacts {
                                        path: p.clone(),
                                        hash: ContentHash::of(src),
                                        modified: ix.modified(p).unwrap_or(0),
                                        plain: r.plain.clone(),
                                        props: r.properties.clone(),
                                        links,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let today = chrono::Local::now().date_naive();
                    let now = chrono::Utc::now().timestamp_millis();
                    let source = |t: &NotePath| cx.index.read().ok().and_then(|ix| ix.source(t).map(str::to_string));
                    for f in &facts {
                        if let Some(item) = assess(&cx.vault, prefs, f, today, now, &source) {
                            found.push((item, f.hash.clone()));
                        }
                    }
                    return Step::Continue;
                }
                let found = std::mem::take(found);
                let took = started.elapsed();
                let notes = cx.index.read().map_or(0, |ix| ix.len());
                self.phase = Phase::Waiting;
                self.last_run = Some(Instant::now());
                sync_inbox(&cx.vault, &cx.agent, &found);
                if let Ok(mut r) = self.shared.stale.lock() {
                    *r = StaleRun { at: self.last_run, notes, flagged: found.len(), took };
                }
                self.shared.bump();
                Step::Idle(Duration::from_secs(1))
            }
        }
    }
}

/// Raise new findings, refresh standing ones, resolve the rest. A note
/// raised before at this same version and no longer in the Inbox was
/// dismissed: it stays away until it changes.
fn sync_inbox(vault: &Vault, agent: &quire_agent::AgentStore, found: &[(InboxItem, ContentHash)]) {
    let inbox = agent.inbox();
    let mut seen = Seen::load(vault);
    let stored: HashMap<String, quire_agent::inbox::StoredItem> =
        inbox.stored().into_iter().filter(|s| matches!(s.item, InboxItem::Stale { .. })).map(|s| (s.fingerprint.clone(), s)).collect();
    let mut live = HashSet::new();
    for (item, hash) in found {
        let InboxItem::Stale { path, .. } = item else { continue };
        let fp = item.fingerprint();
        live.insert(fp.clone());
        let key = path.as_str().to_string();
        let raised_here = seen.notes.get(&key) == Some(&hash.hex().to_string());
        if stored.contains_key(&fp) || !raised_here {
            inbox.upsert(item.clone());
            seen.notes.insert(key, hash.hex().to_string());
        }
    }
    for (fp, s) in &stored {
        if !live.contains(fp) {
            let _ = inbox.resolve(fp);
            if let InboxItem::Stale { path, .. } = &s.item {
                seen.notes.remove(path.as_str());
            }
        }
    }
    let flagged: HashSet<String> =
        found.iter().filter_map(|(i, _)| if let InboxItem::Stale { path, .. } = i { Some(path.to_string()) } else { None }).collect();
    seen.notes.retain(|p, _| flagged.contains(p));
    seen.save(vault);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn flags_old_and_time_bound_notes_and_respects_dismissal() {
        let v = Vault::in_memory(&[
            (
                "Old.md",
                "---\nreview_every: 30d\n---\n# Old\nWe currently use Postgres. Currently it is fine. As of now, it currently works.\n",
            ),
            ("Fresh.md", "# Fresh\nNothing time-bound here at all.\n"),
            ("Gone.md", "---\nvalid_until: 2020-01-01\n---\n# Gone\nA note.\n"),
            ("Daily/2020-01-01.md", "---\nvalid_until: 2020-01-01\n---\ncurrently currently\n"),
            ("raw/page.md", "---\nvalid_until: 2020-01-01\n---\ncurrently\n"),
        ]);
        let mut ix = quire_index::Index::new();
        for p in v.notes().unwrap() {
            ix.upsert(p.clone(), &v.read(&p).unwrap());
            ix.set_modified(&p, chrono::Utc::now().timestamp_millis() - 400 * 86_400_000);
        }
        ix.set_modified(&NotePath::new("Fresh.md"), chrono::Utc::now().timestamp_millis());
        let agent = quire_agent::AgentStore::open(
            &v,
            quire_agent::DeviceInfo { id: "d".into(), name: "D".into() },
            Arc::new(quire_agent::SystemClock),
        );
        let cx = JobCx {
            vault: v,
            index: Arc::new(RwLock::new(ix)),
            agent: agent.clone(),
            chat: None,
            user_active: Arc::new(AtomicBool::new(false)),
            cancel: quire_intel::chat::CancelFlag::new(),
        };
        let shared = Arc::new(TrustShared::default());
        let mut job = StaleJob::new(Arc::clone(&shared));
        assert!(matches!(job.step(&cx), Step::Idle(_)), "waits after opening");
        shared.stale_now.store(true, Ordering::Release);
        while matches!(job.step(&cx), Step::Continue) {}
        let mut paths: Vec<String> = agent
            .inbox()
            .stored()
            .iter()
            .filter_map(|s| match &s.item {
                InboxItem::Stale { path, .. } => Some(path.to_string()),
                _ => None,
            })
            .collect();
        paths.sort();
        assert_eq!(paths, ["Gone.md", "Old.md"]);
        let old = agent
            .inbox()
            .stored()
            .into_iter()
            .find(|s| matches!(&s.item, InboxItem::Stale { path, .. } if path.as_str() == "Old.md"))
            .unwrap();
        let InboxItem::Stale { score, reasons, .. } = &old.item else { unreachable!() };
        assert!(*score >= 0.6, "{score}");
        assert!(reasons.iter().any(|r| matches!(r, StaleReason::TimeBound { phrase, .. } if phrase == "currently")));
        // Dismissed: not raised again while the note is the same.
        agent.inbox().dismiss(&old.fingerprint).unwrap();
        shared.stale_now.store(true, Ordering::Release);
        while matches!(job.step(&cx), Step::Continue) {}
        assert_eq!(agent.inbox().stored().len(), 1);
        // Reviewed today: fixed, so resolved.
        if let Ok(mut ix) = cx.index.write() {
            ix.upsert("Gone.md".into(), "# Gone\nA note.\n");
            ix.set_modified(&NotePath::new("Gone.md"), chrono::Utc::now().timestamp_millis());
        }
        shared.stale_now.store(true, Ordering::Release);
        while matches!(job.step(&cx), Step::Continue) {}
        assert!(agent.inbox().stored().is_empty());
        assert_eq!(shared.stale.lock().unwrap().flagged, 1, "Old is still stale, just dismissed");
    }

    #[test]
    fn prefs_read_folders() {
        let p: StalePrefs =
            toml::from_str("flag_at = 0.7\nexclude = [\"Archive\"]\n[folders]\n\"Projects/\" = \"90d\"\n\"Projects/Big/\" = \"1y\"\n")
                .unwrap();
        assert_eq!(p.folder_days(&NotePath::new("Projects/Big/x.md")), Some(365));
        assert_eq!(p.folder_days(&NotePath::new("projects/y.md")), Some(90));
        assert_eq!(p.folder_days(&NotePath::new("Other/y.md")), None);
        let v = Vault::in_memory(&[]);
        assert!(p.excluded(&NotePath::new("Archive/a.md"), &v));
        assert!(p.excluded(&NotePath::new("Templates/t.md"), &v));
        assert!(!p.excluded(&NotePath::new("Notes/a.md"), &v));
    }
}
