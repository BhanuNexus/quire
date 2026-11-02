// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Access tokens for MCP over HTTP (design §4.6): personal tokens
//! (`quire-cli mcp token create`, Settings → Agents) and the OAuth access
//! and refresh tokens Quire's authorization server issues.
//!
//! Tokens are 256-bit random strings shown once. Only their SHA-256 is kept,
//! in `<config>/mcp/tokens.json` (mode 0600), with who they belong to, their
//! scopes and expiry. Refresh tokens rotate: each works once, and using one
//! twice revokes its whole family (someone copied it).

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use quire_agent::Mode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tools::Grant;

pub const TOKENS_FORMAT: u32 = 1;
pub const ACCESS_TTL_MS: i64 = 3_600_000;
pub const REFRESH_TTL_MS: i64 = 30 * 86_400_000;
/// `last_used` is written at most this often per token.
const TOUCH_EVERY_MS: i64 = 60_000;
const PREFIX: &str = "qmcp_";

/// What a token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scopes {
    pub read: bool,
    pub propose: bool,
    pub write: bool,
    pub ask: bool,
}

pub const SCOPE_NAMES: [&str; 4] = ["notes:read", "notes:propose", "notes:write", "notes:ask"];

impl Scopes {
    /// From `notes:read,notes:propose` or `notes:read notes:propose`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for part in s.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "notes:read" => out.read = true,
                "notes:propose" => out.propose = true,
                "notes:write" => out.write = true,
                "notes:ask" => out.ask = true,
                other => return Err(format!("unknown scope {other} (use {})", SCOPE_NAMES.join(", "))),
            }
        }
        // Anything more than reading includes reading.
        if out.propose || out.write || out.ask {
            out.read = true;
        }
        if !out.read {
            return Err("no scopes given".into());
        }
        Ok(out)
    }

    pub fn names(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (on, n) in
            [(self.read, SCOPE_NAMES[0]), (self.propose, SCOPE_NAMES[1]), (self.write, SCOPE_NAMES[2]), (self.ask, SCOPE_NAMES[3])]
        {
            if on {
                v.push(n);
            }
        }
        v
    }

    /// Space-separated, as OAuth writes scopes.
    pub fn to_oauth(self) -> String {
        self.names().join(" ")
    }

    /// Whether every scope of `other` is in `self`.
    pub const fn covers(self, other: Self) -> bool {
        (self.read || !other.read) && (self.propose || !other.propose) && (self.write || !other.write) && (self.ask || !other.ask)
    }

    /// The tools these scopes offer.
    pub const fn grant(self) -> Grant {
        let w = self.propose || self.write;
        Grant { write: w, ask: self.ask, network: w }
    }

    /// The most a token with these scopes may do anywhere (design §4.6):
    /// read; propose (so `direct` zones become proposals); or what the zones say.
    pub const fn cap(self) -> Option<Mode> {
        if self.write {
            None
        } else if self.propose {
            Some(Mode::Propose)
        } else {
            Some(Mode::Read)
        }
    }

    /// Plain words for the consent screen.
    pub fn describe(self) -> Vec<&'static str> {
        let mut v = vec!["Read and search your notes (not private folders)"];
        if self.write {
            v.push("Write directly in agent folders (wiki/) and suggest changes elsewhere");
        } else if self.propose {
            v.push("Suggest changes, which you review in Quire");
        }
        if self.ask {
            v.push("Answer questions with your AI provider (may cost you)");
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Personal,
    Access,
    Refresh,
}

/// One stored token (its hash, never the token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Hex SHA-256 of the token.
    pub hash: String,
    pub kind: Kind,
    /// Personal tokens: the name given. OAuth: the client's name.
    pub name: String,
    /// The OAuth `client_id` (a URL), or empty for personal tokens.
    #[serde(default)]
    pub client: String,
    /// The actor id agents act as: `token:<name>` or `oauth:<host/path>`.
    pub actor: String,
    pub scopes: Vec<String>,
    /// The MCP endpoint the token is for (OAuth audience); empty for personal tokens.
    #[serde(default)]
    pub resource: String,
    pub created: i64,
    #[serde(default)]
    pub last_used: Option<i64>,
    #[serde(default)]
    pub expires: Option<i64>,
    /// Tokens issued together and rotated together.
    #[serde(default)]
    pub family: String,
    /// A refresh token that was already exchanged.
    #[serde(default)]
    pub used: bool,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Record {
    pub fn scopes(&self) -> Scopes {
        Scopes::parse(&self.scopes.join(",")).unwrap_or_default()
    }

    fn live(&self, now: i64) -> bool {
        self.expires.is_none_or(|e| now < e)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    format: u32,
    #[serde(default)]
    tokens: Vec<Record>,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// Who a valid token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGrant {
    pub actor_id: String,
    pub label: String,
    pub scopes: Scopes,
    pub kind: Kind,
    pub client: String,
}

/// A new token pair from the authorization server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub access: String,
    pub refresh: String,
    pub scopes: Scopes,
}

pub fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn new_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|_| "no randomness available".to_string())?;
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(b)))
}

fn new_family() -> String {
    let mut b = [0u8; 9];
    let _ = getrandom::fill(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// The token file, read and written under a lock so the app and the CLI can share it.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// `<dir>/tokens.json`.
    pub fn in_dir(dir: &Path) -> Self {
        Self { path: dir.join("tokens.json") }
    }

    /// `<config>/mcp/tokens.json`.
    pub fn default_store() -> Option<Self> {
        Some(Self::in_dir(&quire_app::settings::config_dir()?.join("mcp")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<File, String> {
        match std::fs::read(&self.path) {
            Ok(b) => {
                let f: File = serde_json::from_slice(&b).map_err(|e| format!("{} is damaged: {e}", self.path.display()))?;
                if f.format > TOKENS_FORMAT {
                    return Err(format!("{} was made by a newer Quire", self.path.display()));
                }
                Ok(f)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File { format: TOKENS_FORMAT, ..File::default() }),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Read, change and write the file under an exclusive lock.
    fn update<T>(&self, f: impl FnOnce(&mut Vec<Record>) -> Result<T, String>) -> Result<T, String> {
        let dir = self.path.parent().ok_or("bad token path")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("tokens.lock")).map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let mut file = self.read()?;
        let out = f(&mut file.tokens)?;
        file.format = TOKENS_FORMAT;
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        quire_app::atomic::write(&self.path, &bytes, true).map_err(|e| e.to_string())?;
        drop(lock);
        Ok(out)
    }

    /// Every token record (expired refresh tokens and used ones are pruned on write).
    pub fn list(&self) -> Vec<Record> {
        self.read().map(|f| f.tokens).unwrap_or_default()
    }

    /// A personal token. The token is returned once and never stored.
    pub fn create_personal(&self, name: &str, scopes: Scopes, expires: Option<i64>, now: i64) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err("a token name is 1–64 letters, digits, -, _ or .".into());
        }
        let token = new_token()?;
        self.update(|list| {
            if list.iter().any(|r| r.kind == Kind::Personal && r.name == name) {
                return Err(format!("a token named {name} exists; revoke it first"));
            }
            list.push(Record {
                hash: hash(&token),
                kind: Kind::Personal,
                name: name.to_string(),
                client: String::new(),
                actor: format!("token:{name}"),
                scopes: scopes.names().iter().map(|s| (*s).to_string()).collect(),
                resource: String::new(),
                created: now,
                last_used: None,
                expires,
                family: new_family(),
                used: false,
                other: serde_json::Map::new(),
            });
            Ok(())
        })?;
        Ok(token)
    }

    /// Issue an access and a refresh token for an OAuth client.
    pub fn issue(&self, client_id: &str, label: &str, actor: &str, scopes: Scopes, resource: &str, now: i64) -> Result<Pair, String> {
        self.issue_in(client_id, label, actor, scopes, resource, now, &new_family())
    }

    fn issue_in(
        &self,
        client_id: &str,
        label: &str,
        actor: &str,
        scopes: Scopes,
        resource: &str,
        now: i64,
        family: &str,
    ) -> Result<Pair, String> {
        let (access, refresh) = (new_token()?, new_token()?);
        let rec = |token: &str, kind: Kind, ttl: i64| Record {
            hash: hash(token),
            kind,
            name: label.to_string(),
            client: client_id.to_string(),
            actor: actor.to_string(),
            scopes: scopes.names().iter().map(|s| (*s).to_string()).collect(),
            resource: resource.to_string(),
            created: now,
            last_used: None,
            expires: Some(now + ttl),
            family: family.to_string(),
            used: false,
            other: serde_json::Map::new(),
        };
        let (a, r) = (rec(&access, Kind::Access, ACCESS_TTL_MS), rec(&refresh, Kind::Refresh, REFRESH_TTL_MS));
        self.update(|list| {
            list.retain(|x| x.live(now));
            list.push(a);
            list.push(r);
            Ok(())
        })?;
        Ok(Pair { access, refresh, scopes })
    }

    /// Exchange a refresh token (once). `scopes`, if given, may only narrow.
    pub fn refresh(&self, token: &str, client_id: &str, resource: &str, scopes: Option<Scopes>, now: i64) -> Result<Pair, String> {
        let h = hash(token);
        // The inner result: the replay revocation must be saved even though the refresh fails.
        let found = self.update(|list| {
            let Some(i) = list.iter().position(|r| r.hash == h && r.kind == Kind::Refresh) else {
                return Ok(Err("invalid_grant".to_string()));
            };
            if list[i].used {
                // Replayed: someone else has a copy. End the whole family.
                let fam = list[i].family.clone();
                list.retain(|r| r.family != fam);
                return Ok(Err("invalid_grant: refresh token was already used; sign in again".into()));
            }
            if !list[i].live(now) || list[i].client != client_id || list[i].resource != resource {
                return Ok(Err("invalid_grant".into()));
            }
            if scopes.is_some_and(|s| !list[i].scopes().covers(s)) {
                return Ok(Err("invalid_scope: a refresh can only narrow the scopes".into()));
            }
            list[i].used = true;
            Ok(Ok(list[i].clone()))
        })?;
        let rec = found?;
        let scopes = scopes.unwrap_or_else(|| rec.scopes());
        // The old access tokens of the family stay valid until they expire (≤ 1 h).
        self.issue_in(&rec.client, &rec.name, &rec.actor, scopes, &rec.resource, now, &rec.family)
    }

    /// Check a bearer token for `resource` (the MCP endpoint's canonical URI).
    pub fn authenticate(&self, token: &str, resource: &str, now: i64) -> Option<TokenGrant> {
        if !token.starts_with(PREFIX) {
            return None;
        }
        let h = hash(token);
        let rec = self.list().into_iter().find(|r| r.hash == h)?;
        if rec.kind == Kind::Refresh || !rec.live(now) || (rec.kind == Kind::Access && rec.resource != resource) {
            return None;
        }
        if rec.last_used.is_none_or(|t| now - t > TOUCH_EVERY_MS) {
            let _ = self.update(|list| {
                if let Some(r) = list.iter_mut().find(|r| r.hash == h) {
                    r.last_used = Some(now);
                }
                Ok(())
            });
        }
        Some(TokenGrant { actor_id: rec.actor.clone(), label: rec.name.clone(), scopes: rec.scopes(), kind: rec.kind, client: rec.client })
    }

    /// Revoke a personal token by name, or an OAuth client's tokens by
    /// `client_id`. Returns how many records went.
    pub fn revoke(&self, name_or_client: &str) -> Result<usize, String> {
        self.update(|list| {
            let before = list.len();
            list.retain(|r| {
                !((r.kind == Kind::Personal && r.name == name_or_client) || (r.kind != Kind::Personal && r.client == name_or_client))
            });
            Ok(before - list.len())
        })
    }

    /// RFC 7009: revoke the family of a presented token. Unknown tokens are fine.
    pub fn revoke_token(&self, token: &str) -> Result<(), String> {
        let h = hash(token);
        self.update(|list| {
            if let Some(fam) = list.iter().find(|r| r.hash == h).map(|r| r.family.clone()) {
                list.retain(|r| r.family != fam);
            }
            Ok(())
        })
    }

    /// Clients and personal tokens, one row each, for Settings and `token list`.
    pub fn summary(&self, now: i64) -> Vec<Record> {
        let mut out: Vec<Record> = Vec::new();
        for r in self.list().into_iter().filter(|r| r.live(now) && !r.used) {
            let key = |x: &Record| if x.kind == Kind::Personal { x.name.clone() } else { x.client.clone() };
            if let Some(o) = out.iter_mut().find(|o| key(o) == key(&r) && (o.kind == Kind::Personal) == (r.kind == Kind::Personal)) {
                o.last_used = o.last_used.max(r.last_used);
                o.expires = o.expires.max(r.expires);
                continue;
            }
            out.push(r);
        }
        out
    }
}

/// `30d`, `12h`, `90m` or `never` as milliseconds from now.
pub fn parse_expiry(s: &str, now: i64) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| format!("expiry like 30d, 12h or never, not {s}"))?;
    let ms = match unit {
        "d" => 86_400_000,
        "h" => 3_600_000,
        "m" => 60_000,
        _ => return Err(format!("expiry like 30d, 12h or never, not {s}")),
    };
    if n <= 0 {
        return Err("expiry must be positive".into());
    }
    Ok(Some(now + n * ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse_and_cap() {
        let s = Scopes::parse("notes:propose").unwrap();
        assert!(s.read && s.propose && !s.write);
        assert_eq!(s.cap(), Some(Mode::Propose));
        assert_eq!(Scopes::parse("notes:read").unwrap().cap(), Some(Mode::Read));
        assert_eq!(Scopes::parse("notes:read notes:write").unwrap().cap(), None);
        assert!(Scopes::parse("notes:admin").is_err());
        assert!(Scopes::parse("").is_err());
        assert!(Scopes::parse("notes:read,notes:propose").unwrap().covers(Scopes::parse("notes:read").unwrap()));
        assert!(!Scopes::parse("notes:read").unwrap().covers(Scopes::parse("notes:propose").unwrap()));
        assert!(!Scopes::parse("notes:read").unwrap().grant().write);
    }

    #[test]
    fn personal_tokens_are_hashed_and_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let st = TokenStore::in_dir(dir.path());
        let tok = st.create_personal("laptop", Scopes::parse("notes:read").unwrap(), None, 1).unwrap();
        assert!(st.create_personal("laptop", Scopes::default(), None, 1).is_err(), "names are unique");
        assert!(st.create_personal("bad name", Scopes::default(), None, 1).is_err());
        let text = std::fs::read_to_string(st.path()).unwrap();
        assert!(!text.contains(&tok), "only the hash is stored");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(st.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let g = st.authenticate(&tok, "anything", 2).unwrap();
        assert_eq!((g.actor_id.as_str(), g.scopes.propose), ("token:laptop", false));
        assert!(st.authenticate("qmcp_nope", "x", 2).is_none());
        assert!(st.authenticate(&tok.replace("qmcp_", "x"), "x", 2).is_none());
        assert_eq!(st.revoke("laptop").unwrap(), 1);
        assert!(st.authenticate(&tok, "x", 3).is_none());
        let exp = st.create_personal("short", Scopes::parse("notes:read").unwrap(), Some(10), 1).unwrap();
        assert!(st.authenticate(&exp, "x", 9).is_some() && st.authenticate(&exp, "x", 10).is_none(), "expires");
    }

    #[test]
    fn oauth_tokens_are_bound_rotate_and_detect_replay() {
        let dir = tempfile::tempdir().unwrap();
        let st = TokenStore::in_dir(dir.path());
        let res = "http://127.0.0.1:7311/mcp";
        let cid = "https://app.example/client.json";
        let p = st.issue(cid, "App", "oauth:app.example/client.json", Scopes::parse("notes:propose").unwrap(), res, 0).unwrap();
        assert!(st.authenticate(&p.access, res, 1).is_some());
        assert!(st.authenticate(&p.access, "http://other/mcp", 1).is_none(), "audience-bound");
        assert!(st.authenticate(&p.refresh, res, 1).is_none(), "refresh tokens don't authorize calls");
        assert!(st.authenticate(&p.access, res, ACCESS_TTL_MS).is_none(), "an hour");
        assert!(st.refresh(&p.refresh, "https://evil/c.json", res, None, 1).is_err(), "client-bound");
        assert!(st.refresh(&p.refresh, cid, res, Some(Scopes::parse("notes:write").unwrap()), 1).is_err(), "can't widen");
        let p2 = st.refresh(&p.refresh, cid, res, None, 2).unwrap();
        assert!(st.authenticate(&p2.access, res, 3).is_some());
        // Replaying the first refresh token ends the family.
        assert!(st.refresh(&p.refresh, cid, res, None, 4).is_err());
        assert!(st.authenticate(&p2.access, res, 5).is_none());
        assert!(st.refresh(&p2.refresh, cid, res, None, 5).is_err());
        let p3 = st.issue(cid, "App", "oauth:x", Scopes::parse("notes:read").unwrap(), res, 0).unwrap();
        st.revoke_token(&p3.refresh).unwrap();
        assert!(st.authenticate(&p3.access, res, 1).is_none(), "revoking one revokes the family");
        assert_eq!(parse_expiry("30d", 0).unwrap(), Some(30 * 86_400_000));
        assert_eq!(parse_expiry("never", 0).unwrap(), None);
        assert!(parse_expiry("3y", 0).is_err());
    }
}
