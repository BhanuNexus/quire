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
//! Per-client overrides are parsed but not applied yet, and a policy file
//! can only narrow the defaults until device approval lands (both WS-A).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    #[serde(flatten)]
    pub other: toml::Table,
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
            other: toml::Table::new(),
        }
    }
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

    /// The zone that governs `path`: the longest literal prefix wins, and
    /// at equal length a literal beats a glob.
    pub fn zone_for(&self, path: &str) -> Option<&Zone> {
        self.zones.iter().filter(|z| zone_matches(&z.path, path)).max_by_key(|z| specificity(&z.path))
    }

    /// The mode the zones give `path` (no hard rules or grants).
    pub fn zone_mode(&self, path: &str) -> Mode {
        self.zone_for(path).map_or(self.default, |z| z.mode)
    }
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
    let (p, path) = (pattern.trim_start_matches("./").to_lowercase(), path.to_lowercase());
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
