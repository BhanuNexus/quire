// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Agents → Connections (0.11): every agent that has used the
//! vault, what it did, what it may do, and revoking it.
//!
//! Built on what already exists: the ledger (every device's copy with
//! shared review history), pending suggestions, the agent policy's
//! `[clients]` entries, and the HTTP token store (passed in by the caller:
//! `quire-app` sits below `quire-mcp`, which owns it).

use std::collections::BTreeMap;

use quire_agent::policy::{Mode, OpKind};
use quire_agent::policy_edit::PolicyDoc;
use quire_agent::{Actor, ActorKind, AgentStore, LedgerEntry, LedgerQuery, Op, ProposalFilter, Via};
use quire_vault::NotePath;

/// How long the counts look back (the ledger keeps reads 30 days, the rest longer).
pub const WINDOW_MS: i64 = 90 * 86_400_000;

/// How an agent reaches the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `quire-cli mcp` on this computer (`mcp:<key>`; `mcp:<name>?` unverified).
    Stdio,
    /// MCP over HTTP with OAuth (`oauth:<client>`).
    OAuth,
    /// MCP over HTTP with a personal token (`token:<name>`).
    Token,
    /// An external agent started by Quire (`acp:<name>`).
    Acp,
    /// The in-app agent panel (`panel:<provider>`).
    Panel,
    Other,
}

impl Kind {
    pub fn of(key: &str) -> Self {
        match key.split_once(':').map(|(p, _)| p) {
            Some("mcp") => Self::Stdio,
            Some("oauth") => Self::OAuth,
            Some("token") => Self::Token,
            Some("acp") => Self::Acp,
            Some("panel") => Self::Panel,
            _ => Self::Other,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Stdio => "MCP on this computer",
            Self::OAuth => "MCP over HTTP (signed in)",
            Self::Token => "MCP over HTTP (personal token)",
            Self::Acp => "External agent (ACP)",
            Self::Panel => "Agent panel",
            Self::Other => "Agent",
        }
    }

    /// Whether revoking deletes tokens (HTTP) rather than writing the policy.
    pub const fn uses_tokens(self) -> bool {
        matches!(self, Self::OAuth | Self::Token)
    }
}

/// An HTTP token as the connections list needs it (from `quire-mcp`'s store).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// The actor it acts as: `token:<name>` or `oauth:<client>`.
    pub actor: String,
    /// The token's name, or the OAuth client's name.
    pub label: String,
    pub scopes: Vec<String>,
    pub created: i64,
    pub last_used: Option<i64>,
}

/// One connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The actor id as recorded (`mcp:claude-code`, `mcp:cursor?`, `oauth:…`).
    pub key: String,
    pub label: String,
    pub kind: Kind,
    /// It named itself (no `--client` key): matched by name, can't be widened.
    pub unverified: bool,
    pub last_used: Option<i64>,
    pub reads: usize,
    pub writes: usize,
    /// Suggestions it made (in the window).
    pub suggested: usize,
    /// Its suggestions waiting for review now.
    pub pending: usize,
    /// The person's decisions on its suggestions (in the window).
    pub decided: usize,
    pub revoked: bool,
    /// HTTP tokens it holds.
    pub tokens: usize,
    pub scopes: Vec<String>,
    pub allow_tags: Option<Vec<String>>,
    pub allow_folders: Option<Vec<String>>,
    /// Other devices it was used on (shared review history).
    pub devices: Vec<String>,
}

impl Connection {
    fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            kind: Kind::of(key),
            unverified: key.ends_with('?'),
            last_used: None,
            reads: 0,
            writes: 0,
            suggested: 0,
            pending: 0,
            decided: 0,
            revoked: false,
            tokens: 0,
            scopes: Vec::new(),
            allow_tags: None,
            allow_folders: None,
            devices: Vec::new(),
        }
    }

    /// The key its `[clients]` entry in the agent policy has.
    pub fn policy_key(&self) -> &str {
        self.key.strip_suffix('?').unwrap_or(&self.key)
    }

    /// The actor it is, for asking the policy what it may do.
    pub fn actor(&self) -> Actor {
        let via = match self.kind {
            Kind::OAuth | Kind::Token => Via::McpHttp,
            Kind::Acp => Via::Acp,
            Kind::Panel => Via::Panel,
            Kind::Stdio | Kind::Other => Via::McpStdio,
        };
        if self.unverified {
            Actor::unverified_agent(self.policy_key(), &self.label, via, "")
        } else {
            Actor::agent(self.key.clone(), self.label.clone(), via, "")
        }
    }
}

fn bump(c: &mut Connection, e: &LedgerEntry) {
    c.last_used = c.last_used.max(Some(e.at));
}

/// Every connection: agents in the ledger, HTTP tokens, and clients the
/// policy names. Most recently used first; never-used ones last.
pub fn gather(store: &AgentStore, tokens: &[TokenInfo], now_ms: i64) -> Vec<Connection> {
    let mut map: BTreeMap<String, Connection> = BTreeMap::new();
    let here = store.device().id.clone();
    for e in store.log(&LedgerQuery { since: Some(now_ms - WINDOW_MS), ..LedgerQuery::default() }) {
        if e.actor.kind == ActorKind::Agent {
            let c = map.entry(e.actor.id.clone()).or_insert_with(|| Connection::new(&e.actor.id, &e.actor.label));
            if !e.actor.label.is_empty() {
                c.label.clone_from(&e.actor.label);
            }
            match e.op {
                Op::Read => c.reads += 1,
                Op::Propose => c.suggested += 1,
                Op::Write | Op::Create | Op::Rename | Op::Delete | Op::Revert => c.writes += 1,
                Op::Accept | Op::Reject | Op::Batch => {}
            }
            if !e.actor.device.is_empty() && e.actor.device != here && !c.devices.contains(&e.actor.device) {
                c.devices.push(e.actor.device.clone());
            }
            bump(c, &e);
        }
        // The person's decision on an agent's suggestion.
        if matches!(e.op, Op::Accept | Op::Reject)
            && let Some(from) = &e.from
            && let Some(c) = map.get_mut(from)
        {
            c.decided += 1;
        }
    }
    for p in store.proposals(&ProposalFilter::pending()) {
        let c = map.entry(p.actor.id.clone()).or_insert_with(|| Connection::new(&p.actor.id, &p.actor.label));
        c.pending += 1;
        c.last_used = c.last_used.max(Some(p.updated));
    }
    for t in tokens {
        let c = map.entry(t.actor.clone()).or_insert_with(|| Connection::new(&t.actor, &t.label));
        c.tokens += 1;
        c.last_used = c.last_used.max(t.last_used);
        for s in &t.scopes {
            if !c.scopes.contains(s) {
                c.scopes.push(s.clone());
            }
        }
    }
    let policy = store.policy_state().in_force();
    let active: Vec<quire_agent::Policy> = store.policy_state().active().into_iter().cloned().collect();
    for (key, entry) in &policy.clients {
        let interesting = entry.revoked || entry.limited() || entry.default.is_some() || !entry.zones.is_empty();
        if interesting && !map.contains_key(key) && !map.contains_key(&format!("{key}?")) {
            map.insert(key.clone(), Connection::new(key, key.split_once(':').map_or(key.as_str(), |(_, n)| n)));
        }
    }
    for c in map.values_mut() {
        c.revoked = active.iter().any(|p| p.revoked(&c.key));
        if let Some(entry) = policy.client(&c.key) {
            c.allow_tags.clone_from(&entry.allow_tags);
            c.allow_folders.clone_from(&entry.allow_folders);
        }
    }
    let mut v: Vec<Connection> = map.into_values().collect();
    v.sort_by(|a, b| b.last_used.cmp(&a.last_used).then_with(|| a.key.cmp(&b.key)));
    v
}

/// What a connection may do, per zone: (where, read mode, write mode), as
/// the store decides it (what is enforced, not a restatement).
pub fn permissions(store: &AgentStore, c: &Connection) -> Vec<(String, Mode, Mode)> {
    let actor = c.actor();
    let policy = store.policy_state().in_force();
    let mut places: Vec<(String, NotePath)> = policy
        .zones
        .iter()
        .map(|z| {
            let p = z.path.trim_start_matches("./");
            let probe = if p.ends_with('/') { format!("{p}x.md") } else { p.replace("**", "a").replace(['*', '?'], "x") };
            (z.path.clone(), NotePath::new(probe))
        })
        .collect();
    if let Some(entry) = policy.client(&c.key) {
        for z in entry.zones.keys() {
            if !places.iter().any(|(p, _)| p == z) {
                places.push((z.clone(), NotePath::new(format!("{}x.md", if z.ends_with('/') { z.clone() } else { format!("{z}/") }))));
            }
        }
    }
    places.push(("Everywhere else".into(), NotePath::new("x.md")));
    places
        .into_iter()
        .map(|(label, probe)| {
            let read = store.effective_mode(&actor, &probe, OpKind::Read);
            let write = store.effective_mode(&actor, &probe, OpKind::Edit);
            (label, read, write)
        })
        .collect()
}

/// Which events the activity log shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Reads,
    Writes,
    Suggestions,
    Decisions,
}

impl ActivityKind {
    pub const ALL: [Self; 4] = [Self::Reads, Self::Writes, Self::Suggestions, Self::Decisions];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Reads => "Reads",
            Self::Writes => "Writes",
            Self::Suggestions => "Suggestions",
            Self::Decisions => "Decisions",
        }
    }

    pub const fn of(op: Op) -> Option<Self> {
        match op {
            Op::Read => Some(Self::Reads),
            Op::Write | Op::Create | Op::Rename | Op::Delete | Op::Revert => Some(Self::Writes),
            Op::Propose => Some(Self::Suggestions),
            Op::Accept | Op::Reject => Some(Self::Decisions),
            Op::Batch => None,
        }
    }
}

/// A connection's activity from the ledger, newest first: its own events
/// and the person's decisions on its suggestions, of the kinds in `kinds`,
/// with `text` in the path (case-insensitive) when given.
pub fn activity(store: &AgentStore, key: &str, kinds: &[ActivityKind], text: &str, now_ms: i64, limit: usize) -> Vec<LedgerEntry> {
    let needle = text.trim().to_lowercase();
    let mut v: Vec<LedgerEntry> = store
        .log(&LedgerQuery { since: Some(now_ms - WINDOW_MS), ..LedgerQuery::default() })
        .into_iter()
        .filter(|e| e.actor.id == key || e.from.as_deref() == Some(key))
        .filter(|e| ActivityKind::of(e.op).is_some_and(|k| kinds.contains(&k)))
        .filter(|e| {
            needle.is_empty()
                || [&e.path, &e.to].into_iter().flatten().any(|p| p.as_str().to_lowercase().contains(&needle))
                || e.actor.label.to_lowercase().contains(&needle)
        })
        .collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.at));
    v.truncate(limit);
    v
}

/// One line for an activity entry: "Read wiki/Ropes.md", "You accepted its suggestion for Plan.md".
pub fn describe(e: &LedgerEntry) -> String {
    let path = e.path.as_ref().map_or_else(String::new, |p| p.as_str().to_string());
    match e.op {
        Op::Read => format!("Read {path}"),
        Op::Write => format!("Wrote {path}"),
        Op::Create => format!("Created {path}"),
        Op::Rename => format!("Renamed {path} → {}", e.to.as_ref().map_or("", NotePath::as_str)),
        Op::Delete => format!("Deleted {path}"),
        Op::Revert => format!("Undid changes to {path}"),
        Op::Propose => format!("Suggested a change to {path}"),
        Op::Accept => format!("You accepted its suggestion for {path}"),
        Op::Reject => format!("You rejected its suggestion for {path}"),
        Op::Batch => "Batch".into(),
    }
}

/// Revoke a connection in the agent policy: denied every path, refused at
/// its next start, its reads logged.
///
/// HTTP connections also lose their tokens (the caller, which owns the
/// token store, deletes them); the policy entry denies them too, so a token
/// issued before the revoke reached another device is no use either.
pub fn revoke(store: &AgentStore, c: &Connection) -> Result<(), String> {
    edit_policy(store, |d| d.set_revoked(c.policy_key(), true))
}

/// Give a revoked connection its access back (a widening: other devices
/// approve it before it applies there).
pub fn restore(store: &AgentStore, c: &Connection) -> Result<(), String> {
    edit_policy(store, |d| d.set_revoked(c.policy_key(), false))
}

/// Limit a connection to tags and folders (`None`: no limit of that kind).
pub fn set_allow(store: &AgentStore, c: &Connection, tags: Option<&[String]>, folders: Option<&[String]>) -> Result<(), String> {
    edit_policy(store, |d| d.set_allow(c.policy_key(), tags, folders))
}

fn edit_policy(store: &AgentStore, f: impl FnOnce(&mut PolicyDoc)) -> Result<(), String> {
    let text = store.vault().storage().read(&NotePath::new(quire_agent::policy::POLICY_PATH)).ok();
    let mut doc = PolicyDoc::parse(text.as_deref())?;
    f(&mut doc);
    store.write_policy(&doc.text())
}

/// A comma-separated list as typed ("work, #home, Projects/"), cleaned;
/// `None` for an empty one.
pub fn parse_list(s: &str, folders: bool) -> Option<Vec<String>> {
    let v: Vec<String> = s
        .split(',')
        .map(|x| {
            let x = x.trim();
            if folders { format!("{}/", x.trim_matches('/')) } else { x.trim_start_matches('#').to_string() }
        })
        .filter(|x| !x.is_empty() && x != "/")
        .collect();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use quire_agent::{Change, ChangeSet, DeviceInfo, FixedClock, SubmitOutcome};
    use quire_vault::Vault;

    use super::*;

    fn store() -> AgentStore {
        let v = Vault::in_memory(&[("Plan.md", "# Plan\n#work\n"), ("wiki/Ropes.md", "# Ropes\n"), ("Private.md", "# Diary\n")]);
        AgentStore::open(&v, DeviceInfo { id: "d_here".into(), name: "Here".into() }, FixedClock::new(1_800_000_000_000))
    }

    #[test]
    fn gathers_ledger_suggestions_tokens_and_policy_entries() {
        let s = store();
        s.update_device_state(|d| d.log_reads = true).unwrap();
        let claude = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_here");
        s.record_read(&claude, &NotePath::new("Plan.md"));
        s.record_read(&claude, &NotePath::new("wiki/Ropes.md"));
        let out =
            s.submit(&claude, ChangeSet::one(Change::Edit { path: "Plan.md".into(), new_text: "# Plan\n#work\nmore\n".into() })).unwrap();
        assert!(matches!(out, SubmitOutcome::Proposed { .. }), "a suggestion outside wiki/");
        s.submit(&claude, ChangeSet::one(Change::Edit { path: "wiki/Ropes.md".into(), new_text: "# Ropes\nfast\n".into() })).unwrap();
        let cursor = Actor::unverified_agent("mcp:cursor", "Cursor", Via::McpStdio, "d_here");
        s.record_read(&cursor, &NotePath::new("Plan.md"));
        let tokens = vec![TokenInfo {
            actor: "token:laptop".into(),
            label: "laptop".into(),
            scopes: vec!["notes:read".into()],
            created: 1,
            last_used: None,
        }];
        let list = gather(&s, &tokens, 1_800_000_000_000);
        let c = list.iter().find(|c| c.key == "mcp:claude-code").unwrap();
        assert_eq!((c.reads, c.writes, c.suggested, c.pending), (2, 1, 1, 1), "{c:?}");
        assert_eq!(c.kind, Kind::Stdio);
        let u = list.iter().find(|c| c.key == "mcp:cursor?").unwrap();
        assert!(u.unverified && u.reads == 1);
        let t = list.iter().find(|c| c.key == "token:laptop").unwrap();
        assert_eq!((t.kind, t.tokens, t.last_used), (Kind::Token, 1, None));
        assert_eq!(list.last().unwrap().key, "token:laptop", "never used: last");
        // Permissions are what the store enforces.
        let perms = permissions(&s, c);
        let wiki = perms.iter().find(|(p, _, _)| p == "wiki/").unwrap();
        assert_eq!((wiki.1, wiki.2), (Mode::Direct, Mode::Direct));
        let elsewhere = perms.last().unwrap();
        assert_eq!(elsewhere.2, Mode::Propose);
        // Activity, filtered.
        let reads = activity(&s, "mcp:claude-code", &[ActivityKind::Reads], "", 1_800_000_000_000, 50);
        assert_eq!(reads.len(), 2);
        let wiki_only = activity(&s, "mcp:claude-code", &ActivityKind::ALL, "wiki", 1_800_000_000_000, 50);
        assert!(wiki_only.iter().all(|e| e.path.as_ref().unwrap().as_str().starts_with("wiki/")) && wiki_only.len() == 2);
    }

    #[test]
    fn revoke_denies_everything_restore_gives_it_back() {
        let s = store();
        let claude = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_here");
        let c = Connection::new("mcp:claude-code", "Claude Code");
        revoke(&s, &c).unwrap();
        assert_eq!(s.effective_mode(&claude, &NotePath::new("wiki/Ropes.md"), OpKind::Read), Mode::Deny);
        assert!(s.refused("mcp:claude-code").unwrap().contains("revoked"));
        // Dropping --client doesn't get around it: the name it reports is matched.
        let sneaky = Actor::unverified_agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_here");
        assert_eq!(s.effective_mode(&sneaky, &NotePath::new("Plan.md"), OpKind::Read), Mode::Deny);
        assert!(s.refused("mcp:claude-code?").is_some());
        // Another client is untouched; reads of the revoked one are logged.
        let other = Actor::agent("mcp:other", "Other", Via::McpStdio, "d_here");
        assert_eq!(s.effective_mode(&other, &NotePath::new("Plan.md"), OpKind::Read), Mode::Propose);
        assert!(s.logs_reads_of(&claude) && !s.logs_reads_of(&other));
        let listed = gather(&s, &[], 1_800_000_000_000);
        assert!(listed.iter().any(|x| x.key == "mcp:claude-code" && x.revoked));
        restore(&s, &c).unwrap();
        assert_eq!(s.effective_mode(&claude, &NotePath::new("wiki/Ropes.md"), OpKind::Read), Mode::Direct);
    }

    #[test]
    fn allow_lists_and_ai_off() {
        let s = store();
        let claude = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_here");
        let c = Connection::new("mcp:claude-code", "Claude Code");
        set_allow(&s, &c, parse_list("#work", false).as_deref(), parse_list("wiki", true).as_deref()).unwrap();
        let mode = |p: &str| s.effective_mode(&claude, &NotePath::new(p), OpKind::Read);
        assert_eq!(mode("Plan.md"), Mode::Propose, "tagged #work");
        assert_eq!(mode("wiki/Ropes.md"), Mode::Direct, "in an allowed folder");
        assert_eq!(mode("Private.md"), Mode::Deny, "neither");
        edit_policy(&s, |d| d.set_ai_off(true)).unwrap();
        assert_eq!(mode("wiki/Ropes.md"), Mode::Deny);
        assert!(s.refused("mcp:anyone").unwrap().contains("AI is off"));
        edit_policy(&s, |d| d.set_ai_off(false)).unwrap();
        assert_eq!(mode("wiki/Ropes.md"), Mode::Direct);
        assert_eq!(parse_list(" , ", false), None);
        assert_eq!(parse_list("a/, /b", true).unwrap(), ["a/", "b/"]);
    }
}
