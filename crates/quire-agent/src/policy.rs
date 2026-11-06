// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Zones and modes: what an agent may do where (design §A.2,
//! docs/formats/policy.md).
//!
//! `.quire/shared/agent-policy.toml` maps folders to roles and modes. The
//! most specific zone wins, then hard rules cap it (`.quire/**` is denied;
//! agent instructions, skills and templates are suggestions only), then the
//! client's grant (stdio without `--allow-write` reads only).
//!
//! A client's entry in `[clients]` can cap its mode (`default`) and give it
//! its own zones; a client that only reported its name (no Quire-assigned
//! key) can be narrowed by its entry but never widened. A policy file changed
//! outside Quire on this device only narrows what was approved before
//! (`AgentStore` and [`crate::approval`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// Where the vault's agent policy lives.
pub const POLICY_PATH: &str = ".quire/shared/agent-policy.toml";
pub const POLICY_FORMAT: u32 = 1;

/// What an agent may do in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Not even listed or read.
    Deny,
    Read,
    /// Writes become proposals for the person to review.
    Propose,
    /// New files are written directly; changes to existing ones are refused.
    CreateOnly,
    /// Writes apply directly (and can be undone).
    Direct,
}

/// What a zone is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZoneRole {
    /// Agent-owned (`wiki/`).
    Agent,
    /// Sources kept as captured (`raw/`); untrusted.
    Raw,
    /// The person's notes.
    Human,
    /// Hidden from agents.
    Private,
}

/// The kind of operation a mode is checked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    List,
    Create,
    Edit,
    Rename,
    Delete,
}

impl OpKind {
    pub const fn is_write(self) -> bool {
        !matches!(self, Self::Read | Self::List)
    }
}

/// What happens to an operation, from most to least restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Route {
    Deny,
    /// A change to an existing file in a create-only zone.
    Immutable,
    Propose,
    Apply,
}

impl Mode {
    /// Every mode, from most to least restrictive.
    pub const ALL: [Self; 5] = [Self::Deny, Self::Read, Self::Propose, Self::CreateOnly, Self::Direct];

    /// The word used in the policy file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Read => "read",
            Self::Propose => "propose",
            Self::CreateOnly => "create-only",
            Self::Direct => "direct",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Plain words for the UI: "suggest changes".
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Deny => "not see it",
            Self::Read => "only read",
            Self::Propose => "suggest changes",
            Self::CreateOnly => "add new notes",
            Self::Direct => "write directly",
        }
    }

    pub const fn route(self, op: OpKind) -> Route {
        match (self, op) {
            (Self::Deny, _) => Route::Deny,
            (_, OpKind::Read | OpKind::List) | (Self::Direct, _) | (Self::CreateOnly, OpKind::Create) => Route::Apply,
            (Self::Read, _) => Route::Deny,
            (Self::Propose, _) => Route::Propose,
            (Self::CreateOnly, _) => Route::Immutable,
        }
    }
}

impl ZoneRole {
    pub const ALL: [Self; 4] = [Self::Agent, Self::Raw, Self::Human, Self::Private];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Raw => "raw",
            Self::Human => "human",
            Self::Private => "private",
        }
    }

    /// The UI's name for the role (Settings → Agents).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent-owned",
            Self::Raw => "Sources (raw)",
            Self::Human => "Your notes (suggestions only)",
            Self::Private => "Private (hidden from agents)",
        }
    }

    /// The mode a zone of this role gets unless the file says otherwise.
    pub const fn default_mode(self) -> Mode {
        match self {
            Self::Agent => Mode::Direct,
            Self::Raw => Mode::CreateOnly,
            Self::Human => Mode::Propose,
            Self::Private => Mode::Deny,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    /// A folder prefix (`wiki/`), a file, or a glob with `*` and `**`.
    pub path: String,
    pub role: ZoneRole,
    pub mode: Mode,
    #[serde(flatten)]
    pub other: toml::Table,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub writes_per_minute: u32,
    pub max_write_bytes: u64,
    pub max_files_per_change: u32,
    pub max_pending_per_client: u32,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            writes_per_minute: 30,
            max_write_bytes: 262_144,
            max_files_per_change: 50,
            max_pending_per_client: 200,
            other: toml::Table::new(),
        }
    }
}

/// Per-client settings, keyed by the Quire-assigned client key.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientPolicy {
    pub default: Option<Mode>,
    pub zones: BTreeMap<String, Mode>,
    pub approve_in_client: bool,
    /// Revoked in Settings → Agents → Connections (0.11): denied every path,
    /// and a stdio server for this key refuses to start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub revoked: bool,
    /// Record this client's reads in the ledger even when the device
    /// doesn't log reads (set on revoke, so a comeback is visible).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub log_reads: bool,
    /// Only notes carrying one of these tags (0.11, Bear-style). Absent: no
    /// tag limit. With `allow_folders`, a note in either passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_tags: Option<Vec<String>>,
    /// Only notes under one of these folders (0.11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_folders: Option<Vec<String>>,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl ClientPolicy {
    /// Whether this client is limited to chosen tags or folders.
    pub const fn limited(&self) -> bool {
        self.allow_tags.is_some() || self.allow_folders.is_some()
    }

    /// Whether a note at `path` with `tags` is inside this client's allow
    /// lists (always, when it has none). `#work/q4` counts as `work`.
    pub fn allows(&self, path: &str, tags: &[String]) -> bool {
        if !self.limited() {
            return true;
        }
        let in_folder = self.allow_folders.as_ref().is_some_and(|fs| {
            fs.iter().any(|f| {
                let f = f.trim().trim_start_matches("./").trim_end_matches('/');
                !f.is_empty() && zone_matches(&format!("{f}/"), path)
            })
        });
        let tagged = self.allow_tags.as_ref().is_some_and(|want| {
            want.iter().any(|w| {
                let w = w.trim().trim_start_matches('#');
                !w.is_empty()
                    && tags.iter().any(|t| {
                        let t = t.trim_start_matches('#');
                        t.eq_ignore_ascii_case(w)
                            || (t.len() > w.len()
                                && t.is_char_boundary(w.len())
                                && t[..w.len()].eq_ignore_ascii_case(w)
                                && t.as_bytes()[w.len()] == b'/')
                    })
            })
        });
        in_folder || tagged
    }
}

/// The vault's AI switch (0.11): `ai = "off"` blocks every agent and every
/// AI provider for this vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiSwitch {
    #[default]
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub format: u32,
    pub default: Mode,
    #[serde(rename = "zone")]
    pub zones: Vec<Zone>,
    pub limits: Limits,
    pub clients: BTreeMap<String, ClientPolicy>,
    /// Write contracts (design §A.7), kept as written until WS-A types them.
    #[serde(rename = "contract")]
    pub contracts: Vec<toml::Table>,
    /// "AI off" for this vault (0.11).
    #[serde(skip_serializing_if = "is_on")]
    pub ai: AiSwitch,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for Policy {
    /// The policy of a vault without a policy file: agents write directly in
    /// `wiki/`, may add files to `raw/`, and suggest changes everywhere else.
    fn default() -> Self {
        let zone = |path: &str, role, mode| Zone { path: path.into(), role, mode, other: toml::Table::new() };
        Self {
            format: POLICY_FORMAT,
            default: Mode::Propose,
            zones: vec![zone("wiki/", ZoneRole::Agent, Mode::Direct), zone("raw/", ZoneRole::Raw, Mode::CreateOnly)],
            limits: Limits::default(),
            clients: BTreeMap::new(),
            contracts: Vec::new(),
            ai: AiSwitch::On,
            other: toml::Table::new(),
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
fn is_on(a: &AiSwitch) -> bool {
    *a == AiSwitch::On
}

/// A hard rule that applies whatever the policy says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardRule {
    /// `.quire/**`: never readable or writable by agents.
    AppData,
    /// Agent instructions, skills and templates: suggestions only.
    Instructions,
}

impl HardRule {
    /// The most an agent may do under this rule.
    pub const fn cap(self) -> Mode {
        match self {
            Self::AppData => Mode::Deny,
            Self::Instructions => Mode::Propose,
        }
    }
}

/// The hard rule for a (canonical) vault path, if any. `templates` is the
/// vault's templates folder.
pub fn hard_rule(path: &str, templates: &str) -> Option<HardRule> {
    let lower = path.to_lowercase();
    if lower == ".quire" || lower.starts_with(".quire/") {
        return Some(HardRule::AppData);
    }
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let tpl = format!("{}/", templates.trim_matches('/').to_lowercase());
    if matches!(lower.as_str(), "agents.md" | "claude.md")
        || name == "skill.md"
        || lower.starts_with(&tpl)
        || lower.starts_with("templates/")
    {
        return Some(HardRule::Instructions);
    }
    None
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Made by a newer Quire: read what we can, never rewrite.
    pub const fn is_newer(&self) -> bool {
        self.format > POLICY_FORMAT
    }

    /// Whether AI is off for this vault.
    pub fn ai_off(&self) -> bool {
        self.ai == AiSwitch::Off
    }

    /// The `[clients]` entry that governs `key`: its own, or for an
    /// unverified client (`mcp:name?`) the entry of the name it reported.
    pub fn client(&self, key: &str) -> Option<&ClientPolicy> {
        self.clients.get(key).or_else(|| self.clients.get(key.strip_suffix('?')?))
    }

    /// Whether `key` was revoked (unverified keys by the name they report).
    pub fn revoked(&self, key: &str) -> bool {
        self.client(key).is_some_and(|c| c.revoked)
    }

    /// The zone that governs `path`: the longest literal prefix wins, and
    /// at equal length a literal beats a glob.
    pub fn zone_for(&self, path: &str) -> Option<&Zone> {
        self.zones.iter().filter(|z| zone_matches(&z.path, path)).max_by_key(|z| specificity(&z.path))
    }

    /// The mode the zones give `path` (no hard rules or grants).
    pub fn zone_mode(&self, path: &str) -> Mode {
        self.zone_for(path).map_or(self.default, |z| z.mode)
    }

    /// The zone role of `path` (`None`: the vault default, the person's notes).
    pub fn role_of(&self, path: &str) -> Option<ZoneRole> {
        self.zone_for(path).map(|z| z.role)
    }

    /// The modes that limit `client` on `path`, with where each comes from
    /// (a zone's path, `default`, or `<key> (client)`). The most restrictive
    /// one for the operation applies.
    ///
    /// A client entry's own zone wins over the vault's zone when it is at
    /// least as specific; otherwise the vault's zone applies, capped by the
    /// entry's `default`. For an `unverified` client (it only reported its
    /// name) the entry can narrow but never widen.
    pub fn modes_for(&self, client: Option<&str>, unverified: bool, path: &str) -> Vec<(Mode, String)> {
        let vault_zone = self.zone_for(path);
        let vault = vault_zone.map_or_else(|| (self.default, "default".to_string()), |z| (z.mode, z.path.clone()));
        let Some((key, c)) = client.and_then(|k| self.clients.get_key_value(k)) else { return vec![vault] };
        let client_zone = c.zones.iter().filter(|(p, _)| zone_matches(p, path)).max_by_key(|(p, _)| specificity(p));
        let label = |what: &str| format!("{what} ({key})");
        let mut out = Vec::new();
        match client_zone {
            Some((p, m)) if !unverified && vault_zone.is_none_or(|z| specificity(p) >= specificity(&z.path)) => {
                out.push((*m, label(p)));
            }
            other => {
                out.push(vault);
                if let Some(d) = c.default {
                    out.push((d, label("default")));
                }
                if let Some((p, m)) = other.filter(|_| unverified) {
                    out.push((*m, label(p)));
                }
            }
        }
        out
    }

    /// One sentence for Settings → Agents: "Agents can write directly in
    /// wiki/ and add new notes in raw/; elsewhere they can suggest changes."
    pub fn sentence(&self) -> String {
        let mut by_mode: Vec<(Mode, Vec<String>)> = Vec::new();
        for z in &self.zones {
            if z.mode == self.default {
                continue;
            }
            match by_mode.iter_mut().find(|(m, _)| *m == z.mode) {
                Some((_, v)) => v.push(z.path.clone()),
                None => by_mode.push((z.mode, vec![z.path.clone()])),
            }
        }
        by_mode.sort_by_key(|(m, _)| std::cmp::Reverse(rank(*m)));
        let list = |v: &[String]| match v {
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => String::new(),
        };
        let parts: Vec<String> = by_mode
            .iter()
            .map(|(m, paths)| match m {
                Mode::Deny => format!("can't see {}", list(paths)),
                Mode::Read => format!("can only read {}", list(paths)),
                Mode::Propose => format!("can only suggest changes in {}", list(paths)),
                Mode::CreateOnly => format!("can add new notes to {} but not change them", list(paths)),
                Mode::Direct => format!("can write directly in {}", list(paths)),
            })
            .collect();
        let elsewhere = match self.default {
            Mode::Deny => "they can't see anything else",
            Mode::Read => "everything else they can only read",
            Mode::Propose => "everywhere else they can only suggest changes",
            Mode::CreateOnly => "everywhere else they can add notes but not change them",
            Mode::Direct => "everywhere else they write directly",
        };
        if parts.is_empty() {
            return match self.default {
                Mode::Deny => "Agents can't see any notes.",
                Mode::Read => "Agents can only read notes.",
                Mode::Propose => "Agents can only suggest changes, everywhere.",
                Mode::CreateOnly => "Agents can add new notes but not change any.",
                Mode::Direct => "Agents can write directly everywhere.",
            }
            .to_string();
        }
        format!("Agents {}; {elsewhere}.", parts.join("; "))
    }
}

/// Restrictiveness order of modes, least first.
pub const fn rank(m: Mode) -> u8 {
    match m {
        Mode::Deny => 0,
        Mode::Read => 1,
        Mode::Propose => 2,
        Mode::CreateOnly => 3,
        Mode::Direct => 4,
    }
}

/// The most restrictive of `modes` for `op`; at equal outcome the narrower
/// mode is reported (a read-only grant reads as `read`).
pub fn strictest(modes: impl IntoIterator<Item = (Mode, String)>, op: OpKind) -> (Mode, String) {
    modes.into_iter().min_by_key(|(m, _)| (m.route(op), rank(*m))).unwrap_or((Mode::Deny, String::new()))
}

/// A way `new` gives agents more than `old`, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widening {
    pub what: String,
}

/// A path inside what `pattern` covers, for comparing policies.
fn probe_path(pattern: &str) -> String {
    let p = pattern.trim_start_matches("./");
    let p = p.replace("**", "a").replace(['*', '?'], "x");
    let is_note = std::path::Path::new(&p).extension().is_some_and(|e| e.eq_ignore_ascii_case("md"));
    if p.ends_with('/') {
        format!("{p}x.md")
    } else if is_note {
        p
    } else {
        format!("{p}/x.md")
    }
}

/// Every way `new` lets an agent (any client) do more than `old`: modes per
/// zone and operation, limits, and approval in the agent's app.
pub fn widenings(old: &Policy, new: &Policy) -> Vec<Widening> {
    let mut paths: Vec<String> = vec!["Note.md".into()];
    for z in old.zones.iter().chain(&new.zones) {
        paths.push(probe_path(&z.path));
    }
    let mut clients: Vec<Option<&str>> = vec![None];
    for (k, c) in old.clients.iter().chain(&new.clients) {
        clients.push(Some(k.as_str()));
        paths.extend(c.zones.keys().map(|p| probe_path(p)));
    }
    paths.sort();
    paths.dedup();
    clients.dedup();
    let mut out: Vec<Widening> = Vec::new();
    let ops = [OpKind::Read, OpKind::Create, OpKind::Edit, OpKind::Delete];
    for client in &clients {
        for path in &paths {
            let o = ops.iter().map(|op| strictest(old.modes_for(*client, false, path), *op)).collect::<Vec<_>>();
            let n = ops.iter().map(|op| strictest(new.modes_for(*client, false, path), *op)).collect::<Vec<_>>();
            let wider = ops.iter().zip(o.iter().zip(&n)).any(|(op, (a, b))| b.0.route(*op) > a.0.route(*op));
            if wider {
                let who = client.map_or_else(|| "Agents".to_string(), |k| format!("Agent {k}"));
                let place =
                    if path == "Note.md" { "notes outside any zone".to_string() } else { path.trim_end_matches("x.md").to_string() };
                let (was, now) = (o.iter().map(|x| x.0).min_by_key(|m| rank(*m)), n.iter().map(|x| x.0).min_by_key(|m| rank(*m)));
                let (was, now) = (was.map_or(Mode::Deny, |m| m), now.map_or(Mode::Deny, |m| m));
                let what = format!("{who} could {} in {place} (before: {})", now.describe(), was.describe());
                if !out.iter().any(|w| w.what == what) {
                    out.push(Widening { what });
                }
            }
        }
    }
    // Roles decide trust labels: a zone that marks sources untrusted (raw)
    // or hides notes (private) losing that role gives agents more (P7, P8).
    for path in &paths {
        let (was, now) = (old.role_of(path), new.role_of(path));
        if let Some(r @ (ZoneRole::Raw | ZoneRole::Private)) = was
            && now != was
        {
            let place = if path == "Note.md" { "notes outside any zone".to_string() } else { path.trim_end_matches("x.md").to_string() };
            let label = if r == ZoneRole::Raw { "untrusted sources" } else { "private" };
            let now = now.map_or("none", ZoneRole::as_str);
            let what = format!("Notes in {place} would no longer be treated as {label} (role {} → {now})", r.as_str());
            if !out.iter().any(|w| w.what == what) {
                out.push(Widening { what });
            }
        }
    }
    let (ol, nl) = (&old.limits, &new.limits);
    for (name, a, b) in [
        ("writes per minute", u64::from(ol.writes_per_minute), u64::from(nl.writes_per_minute)),
        ("bytes per write", ol.max_write_bytes, nl.max_write_bytes),
        ("files per change", u64::from(ol.max_files_per_change), u64::from(nl.max_files_per_change)),
        ("waiting suggestions per agent", u64::from(ol.max_pending_per_client), u64::from(nl.max_pending_per_client)),
    ] {
        if b > a {
            out.push(Widening { what: format!("The limit on {name} went up from {a} to {b}") });
        }
    }
    for (k, c) in &new.clients {
        if c.approve_in_client && !old.clients.get(k).is_some_and(|o| o.approve_in_client) {
            out.push(Widening { what: format!("Agent {k} could apply changes after approval in its own app") });
        }
    }
    // 0.11: AI off, revoked clients, allow lists.
    if old.ai_off() && !new.ai_off() {
        out.push(Widening { what: "AI would be turned back on for this vault".into() });
    }
    for (k, o) in &old.clients {
        let n = new.clients.get(k);
        if o.revoked && !n.is_some_and(|n| n.revoked) {
            out.push(Widening { what: format!("Agent {k} would get its access back (it was revoked)") });
        }
        if o.limited() {
            let empty = ClientPolicy::default();
            let n = n.unwrap_or(&empty);
            let lists = |c: &ClientPolicy| {
                let norm = |v: &Option<Vec<String>>| {
                    v.as_ref().map(|v| {
                        v.iter().map(|s| s.trim().trim_matches(['#', '/']).to_lowercase()).collect::<std::collections::BTreeSet<_>>()
                    })
                };
                (norm(&c.allow_tags), norm(&c.allow_folders))
            };
            let ((old_tags, old_folders), (new_tags, new_folders)) = (lists(o), lists(n));
            // Wider: a limit dropped, or a list gained an entry.
            let gained = |a: &Option<std::collections::BTreeSet<String>>, b: &Option<std::collections::BTreeSet<String>>| match (a, b) {
                (Some(a), Some(b)) => !b.is_subset(a),
                (None, Some(b)) => !b.is_empty(),
                _ => false,
            };
            if !n.limited() || gained(&old_tags, &new_tags) || gained(&old_folders, &new_folders) {
                out.push(Widening { what: format!("Agent {k} could see notes outside the tags and folders it was limited to") });
            }
        }
    }
    out
}

/// The stricter of two policies' limits.
pub fn stricter_limits(a: &Limits, b: &Limits) -> Limits {
    Limits {
        writes_per_minute: a.writes_per_minute.min(b.writes_per_minute),
        max_write_bytes: a.max_write_bytes.min(b.max_write_bytes),
        max_files_per_change: a.max_files_per_change.min(b.max_files_per_change),
        max_pending_per_client: a.max_pending_per_client.min(b.max_pending_per_client),
        other: a.other.clone(),
    }
}

/// Whether `path` is inside the zone (or contract folder) `pattern`.
pub fn path_in_zone(pattern: &str, path: &str) -> bool {
    zone_matches(pattern, path)
}

const fn is_glob(c: char) -> bool {
    matches!(c, '*' | '?' | '[')
}

/// (literal prefix length, is a literal).
fn specificity(pattern: &str) -> (usize, bool) {
    let literal = pattern.find(is_glob).unwrap_or(pattern.len());
    (literal, literal == pattern.len())
}

/// Case-insensitive: a zone written `Wiki/` also covers `wiki/` on
/// case-insensitive file systems, the safe direction for deny zones.
fn zone_matches(pattern: &str, path: &str) -> bool {
    // NFC as well: a zone written in one Unicode form covers paths in the other.
    let fold = |s: &str| s.nfc().collect::<String>().to_lowercase();
    let (p, path) = (fold(pattern.trim_start_matches("./")), fold(path));
    if !p.contains(is_glob) {
        return if p.ends_with('/') { path.starts_with(&p) } else { path == p || path.starts_with(&format!("{p}/")) };
    }
    // A glob ending in `/` covers everything below what it matches.
    let p = if p.ends_with('/') { format!("{p}**") } else { p };
    glob_match(&p.split('/').collect::<Vec<_>>(), &path.split('/').collect::<Vec<_>>())
}

/// Segment-wise glob: `**` matches any number of segments, `*` and `?`
/// match within a segment.
fn glob_match(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| glob_match(rest, &segs[i..])),
        Some((p, rest)) => segs.split_first().is_some_and(|(s, srest)| seg_match(p.as_bytes(), s.as_bytes()) && glob_match(rest, srest)),
    }
}

fn seg_match(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| seg_match(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && seg_match(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && seg_match(rest, &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_the_design() {
        let p = Policy::default();
        assert_eq!(p.zone_mode("wiki/Ropes.md"), Mode::Direct);
        assert_eq!(p.zone_mode("raw/paper.md"), Mode::CreateOnly);
        assert_eq!(p.zone_mode("Rope Notes.md"), Mode::Propose);
        assert_eq!(p.zone_mode("wikipedia.md"), Mode::Propose, "a prefix is a folder, not a string prefix");
        assert_eq!(Mode::CreateOnly.route(OpKind::Edit), Route::Immutable);
        assert_eq!(Mode::CreateOnly.route(OpKind::Create), Route::Apply);
        assert_eq!(Mode::Read.route(OpKind::Edit), Route::Deny);
        assert_eq!(Mode::Deny.route(OpKind::Read), Route::Deny);
    }

    #[test]
    fn parses_the_documented_file() {
        let text = r#"
format = 1
default = "propose"
[[zone]]
path = "wiki/"
role = "agent"
mode = "direct"
[[zone]]
path = "Private/"
role = "private"
mode = "deny"
[[zone]]
path = "**/drafts/"
role = "human"
mode = "propose"
[[zone]]
path = "wiki/people/"
role = "human"
mode = "read"
[limits]
writes_per_minute = 10
[clients."mcp:claude-code"]
default = "propose"
zones = { "wiki/" = "direct" }
approve_in_client = false
[[contract]]
name = "concept pages"
required = ["type"]
"#;
        let p = Policy::parse(text).unwrap();
        assert_eq!(p.limits.writes_per_minute, 10);
        assert_eq!(p.limits.max_write_bytes, 262_144, "missing limits keep their defaults");
        assert_eq!(p.zone_mode("private/diary.md"), Mode::Deny);
        assert_eq!(p.zone_mode("wiki/people/Ada.md"), Mode::Read, "longest literal prefix");
        assert_eq!(p.zone_mode("notes/a/drafts/x.md"), Mode::Propose, "glob");
        assert_eq!(p.zone_mode("wiki/a/drafts/x.md"), Mode::Direct, "a literal prefix beats a glob with a shorter one");
        assert_eq!(p.zone_mode("wiki/x.md"), Mode::Direct);
        assert_eq!(p.clients["mcp:claude-code"].zones["wiki/"], Mode::Direct);
        assert_eq!(p.contracts.len(), 1);
        assert!(Policy::parse("default = \"sometimes\"").is_err());
        assert!(Policy::parse("format = 2").unwrap().is_newer());
    }

    #[test]
    fn client_entries_cap_and_add_zones() {
        let p = Policy::parse(
            r#"
default = "propose"
[[zone]]
path = "wiki/"
role = "agent"
mode = "direct"
[[zone]]
path = "Private/"
role = "private"
mode = "deny"
[clients."mcp:reader"]
default = "read"
[clients."mcp:claude-code"]
default = "propose"
zones = { "wiki/" = "direct", "notes/" = "direct" }
"#,
        )
        .unwrap();
        let m = |c: Option<&str>, unverified: bool, path: &str, op: OpKind| strictest(p.modes_for(c, unverified, path), op).0;
        assert_eq!(m(Some("mcp:reader"), false, "wiki/a.md", OpKind::Edit), Mode::Read, "a client default caps zones");
        assert_eq!(m(Some("mcp:reader"), false, "Private/a.md", OpKind::Read), Mode::Deny);
        assert_eq!(m(Some("mcp:claude-code"), false, "notes/a.md", OpKind::Edit), Mode::Direct, "its own zone");
        assert_eq!(m(Some("mcp:claude-code"), false, "Other.md", OpKind::Edit), Mode::Propose);
        assert_eq!(m(Some("mcp:claude-code"), true, "notes/a.md", OpKind::Edit), Mode::Propose, "unverified: no widening");
        assert_eq!(m(Some("mcp:reader"), true, "wiki/a.md", OpKind::Edit), Mode::Read, "unverified: narrowing still applies");
        assert_eq!(m(None, false, "wiki/a.md", OpKind::Edit), Mode::Direct);
    }

    #[test]
    fn widening_is_found_and_narrowing_is_not() {
        let d = Policy::default();
        let mut narrower = Policy::default();
        narrower.zones[0].mode = Mode::Read;
        narrower.limits.writes_per_minute = 5;
        assert!(widenings(&d, &narrower).is_empty());
        let wider = Policy { default: Mode::Direct, ..Policy::default() };
        let w = widenings(&d, &wider);
        assert!(w.iter().any(|w| w.what.contains("notes outside any zone") && w.what.contains("write directly")), "{w:?}");
        let mut client = Policy::default();
        client.clients.insert("mcp:x".into(), ClientPolicy { zones: [("raw/".into(), Mode::Direct)].into(), ..Default::default() });
        assert!(widenings(&d, &client).iter().any(|w| w.what.starts_with("Agent mcp:x could write directly in raw/")));
        let mut limits = Policy::default();
        limits.limits.max_write_bytes = 10_000_000;
        assert_eq!(widenings(&d, &limits).len(), 1);
    }

    #[test]
    fn relabelling_raw_or_private_is_a_widening() {
        let d = Policy::default();
        let mut relabel = Policy::default();
        relabel.zones[1].role = ZoneRole::Human;
        let w = widenings(&d, &relabel);
        assert!(w.iter().any(|w| w.what.contains("raw/") && w.what.contains("untrusted")), "{w:?}");
        let mut gone = Policy::default();
        gone.zones.pop();
        gone.zones.push(Zone { path: "raw/".into(), role: ZoneRole::Agent, mode: Mode::Read, other: toml::Table::new() });
        assert!(widenings(&d, &gone).iter().any(|w| w.what.contains("role raw → agent")));
        let mut private = Policy::default();
        private.zones.push(Zone { path: "Private/".into(), role: ZoneRole::Private, mode: Mode::Deny, other: toml::Table::new() });
        let mut unhidden = private.clone();
        unhidden.zones[2].role = ZoneRole::Human;
        assert!(widenings(&private, &unhidden).iter().any(|w| w.what.contains("Private/") && w.what.contains("private")));
        assert!(widenings(&relabel, &d).is_empty(), "making a zone raw narrows");
    }

    #[test]
    fn zones_match_in_either_unicode_form() {
        let mut p = Policy::default();
        p.zones.push(Zone { path: "Priv\u{e9}/".into(), role: ZoneRole::Private, mode: Mode::Deny, other: toml::Table::new() });
        assert_eq!(p.zone_mode("Prive\u{301}/diary.md"), Mode::Deny, "NFD path, NFC zone");
        p.zones[2].path = "Prive\u{301}/".into();
        assert_eq!(p.zone_mode("Priv\u{e9}/diary.md"), Mode::Deny, "NFC path, NFD zone");
        assert_eq!(p.zone_mode("PRIV\u{c9}/diary.md"), Mode::Deny, "any case");
    }

    #[test]
    fn sentences() {
        assert_eq!(
            Policy::default().sentence(),
            "Agents can write directly in wiki/; can add new notes to raw/ but not change them; everywhere else they can only suggest changes."
        );
        let mut p = Policy::default();
        p.zones.push(Zone { path: "Private/".into(), role: ZoneRole::Private, mode: Mode::Deny, other: toml::Table::new() });
        assert!(p.sentence().contains("can't see Private/"));
        let bare = Policy { zones: Vec::new(), ..Policy::default() };
        assert_eq!(bare.sentence(), "Agents can only suggest changes, everywhere.");
    }

    #[test]
    fn hard_rules() {
        assert_eq!(hard_rule(".quire/agent/x.json", "Templates"), Some(HardRule::AppData));
        assert_eq!(hard_rule(".Quire/shared/agent-policy.toml", "Templates"), Some(HardRule::AppData));
        assert_eq!(hard_rule("AGENTS.md", "Templates"), Some(HardRule::Instructions));
        assert_eq!(hard_rule("claude.md", "Templates"), Some(HardRule::Instructions));
        assert_eq!(hard_rule("skills/quire/SKILL.md", "Templates"), Some(HardRule::Instructions));
        assert_eq!(hard_rule("Templates/Daily.md", "Templates"), Some(HardRule::Instructions));
        assert_eq!(hard_rule("My templates/Daily.md", "My templates"), Some(HardRule::Instructions));
        assert_eq!(hard_rule("notes/AGENTS.md", "Templates"), None, "only the root file is the vault's instructions");
        assert_eq!(hard_rule("wiki/x.md", "Templates"), None);
    }
}
