// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Consent requests from the OAuth authorization server (design §4.6).
//!
//! When an MCP client asks for access over HTTP, the server writes a request
//! to `<config>/mcp/consent/<id>.json` and the browser waits. The person
//! approves or denies it in Quire (a dialog) or with `quire-cli mcp approve
//! <id>` / `deny <id>`; the server sees the decision and finishes the
//! sign-in. Files keep the app and a separate `serve-http` process in step.
//! Requests expire after 10 minutes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONSENT_FORMAT: u32 = 1;
pub const CONSENT_TTL_MS: i64 = 10 * 60 * 1000;
/// At most this many requests wait at once.
pub const MAX_PENDING: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Approved,
    Denied,
    /// The code was handed to the client.
    Used,
}

/// One request for access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub format: u32,
    pub id: String,
    pub created: i64,
    pub expires: i64,
    pub client_id: String,
    /// As the client names itself (shown, never trusted).
    pub client_name: String,
    /// The `client_id` URL's host, shown prominently.
    pub client_host: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: State,
    #[serde(default)]
    pub decided_at: Option<i64>,
    /// The MCP endpoint the request is for.
    #[serde(default)]
    pub resource: String,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Request {
    pub fn scopes(&self) -> crate::tokens::Scopes {
        crate::tokens::Scopes::parse(&self.scopes.join(",")).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct ConsentStore {
    dir: PathBuf,
}

fn random_id() -> String {
    use base64::Engine as _;
    let mut b = [0u8; 12];
    let _ = getrandom::fill(&mut b);
    // Short enough to type in `quire-cli mcp approve`, too long to guess.
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b).replace(['-', '_'], "x")
}

impl ConsentStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    /// `<config>/mcp/consent/`.
    pub fn default_store() -> Option<Self> {
        Some(Self::in_dir(&quire_app::settings::config_dir()?.join("mcp").join("consent")))
    }

    fn file(&self, id: &str) -> Option<PathBuf> {
        id.chars().all(|c| c.is_ascii_alphanumeric()).then(|| self.dir.join(format!("{id}.json")))
    }

    fn save(&self, r: &Request) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.file(&r.id).ok_or("bad id")?;
        quire_app::atomic::write(&path, &serde_json::to_vec_pretty(r).map_err(|e| e.to_string())?, true).map_err(|e| e.to_string())
    }

    /// File a new request; returns its id.
    pub fn create(
        &self,
        client_id: &str,
        client_name: &str,
        client_host: &str,
        redirect_uri: &str,
        scopes: &[&str],
        resource: &str,
        now: i64,
    ) -> Result<String, String> {
        // A web page could file requests in a loop; a handful is plenty.
        if self.pending(now).len() >= MAX_PENDING {
            return Err("too many connection requests are waiting; deny some in Quire first".into());
        }
        let id = random_id();
        let r = Request {
            format: CONSENT_FORMAT,
            id: id.clone(),
            created: now,
            expires: now + CONSENT_TTL_MS,
            client_id: client_id.into(),
            client_name: client_name.chars().take(80).collect(),
            client_host: client_host.into(),
            redirect_uri: redirect_uri.into(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            state: State::Pending,
            decided_at: None,
            resource: resource.into(),
            other: serde_json::Map::new(),
        };
        self.save(&r)?;
        self.prune(now);
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Request> {
        let b = std::fs::read(self.file(id)?).ok()?;
        let r: Request = serde_json::from_slice(&b).ok()?;
        (r.format <= CONSENT_FORMAT).then_some(r)
    }

    /// Requests waiting for a decision, oldest first.
    pub fn pending(&self, now: i64) -> Vec<Request> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut v: Vec<Request> = dir
            .flatten()
            .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).and_then(|id| self.get(id)))
            .filter(|r| r.state == State::Pending && r.expires > now)
            .collect();
        v.sort_by_key(|r| r.created);
        v
    }

    /// Approve or deny a pending request.
    pub fn decide(&self, id: &str, approve: bool, now: i64) -> Result<Request, String> {
        let mut r = self.get(id).ok_or_else(|| format!("no request {id}"))?;
        if r.expires <= now {
            return Err(format!("request {id} expired; ask the app to connect again"));
        }
        if r.state != State::Pending {
            return Err(format!("request {id} was already decided"));
        }
        r.state = if approve { State::Approved } else { State::Denied };
        r.decided_at = Some(now);
        self.save(&r)?;
        Ok(r)
    }

    /// Keep extra (non-secret) fields with a request.
    pub fn annotate(&self, id: &str, pairs: &[(&str, &str)]) -> Result<(), String> {
        let mut r = self.get(id).ok_or_else(|| format!("no request {id}"))?;
        for (k, v) in pairs {
            r.other.insert((*k).to_string(), (*v).into());
        }
        self.save(&r)
    }

    /// Mark an approved request as used (its code was issued). Only once.
    pub fn mark_used(&self, id: &str) -> bool {
        let Some(mut r) = self.get(id) else { return false };
        if r.state != State::Approved {
            return false;
        }
        r.state = State::Used;
        self.save(&r).is_ok()
    }

    /// Delete requests older than a day.
    pub fn prune(&self, now: i64) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else { return };
        for e in dir.flatten() {
            let stale =
                e.path().file_stem().and_then(|s| s.to_str()).and_then(|id| self.get(id)).is_none_or(|r| r.expires + 86_400_000 < now);
            if stale && e.path().extension().is_some_and(|x| x == "json") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_decided_once() {
        let dir = tempfile::tempdir().unwrap();
        let st = ConsentStore::in_dir(dir.path());
        let id = st.create("https://a.example/c.json", "App", "a.example", "http://127.0.0.1:5/cb", &["notes:read"], "r", 0).unwrap();
        assert_eq!(st.pending(1).len(), 1);
        assert!(st.decide("nope", true, 1).is_err());
        assert!(st.decide("../../etc", true, 1).is_err());
        assert!(!st.mark_used(&id), "not approved yet");
        assert_eq!(st.decide(&id, true, 1).unwrap().state, State::Approved);
        assert!(st.decide(&id, false, 2).is_err(), "decided once");
        assert!(st.pending(2).is_empty());
        assert!(st.mark_used(&id));
        assert!(!st.mark_used(&id), "one code per approval");
        let late = st.create("c", "n", "h", "r", &["notes:read"], "r", 0).unwrap();
        assert!(st.decide(&late, true, CONSENT_TTL_MS).is_err(), "expired");
        // `late` is still waiting at time 5.
        for _ in 1..MAX_PENDING {
            st.create("c", "n", "h", "r", &["notes:read"], "r", 5).unwrap();
        }
        assert!(st.create("c", "n", "h", "r", &["notes:read"], "r", 5).is_err(), "no flood");
    }
}
