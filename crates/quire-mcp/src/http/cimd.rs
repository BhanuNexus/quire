// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! OAuth Client ID Metadata Documents (design §4.6): a client's `client_id`
//! is an `https://` URL whose JSON says its name and redirect URIs.
//!
//! The document is fetched with a 5 s timeout, at most 64 KB and no
//! redirects; hosts that resolve to private, loopback or link-local
//! addresses are refused. It must name the same `client_id`, list
//! `redirect_uris`, and use `token_endpoint_auth_method: "none"`. Documents
//! are cached as `Cache-Control` allows, at most a day. `logo_uri` is never
//! loaded (it would tell the client when its consent screen is shown).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use quire_import::web::{self, FetchOptions, HttpUrl};
use serde_json::Value;

const MAX_DOC: u64 = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CACHE: Duration = Duration::from_hours(24);

/// How documents may be fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// Tests only: accept `http://` documents on loopback addresses.
    pub allow_http_loopback: bool,
}

/// A validated client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(clippy::struct_field_names, reason = "`client_id` is OAuth's name for it")]
pub struct Client {
    pub client_id: String,
    /// As the client names itself (shown with the host, never trusted alone).
    pub name: String,
    /// The `client_id`'s host, with its port when it isn't the default:
    /// exactly whose document this is, shown prominently on the consent screen.
    pub host: String,
    /// `host/path`, for the actor id `oauth:<host/path>`.
    pub host_path: String,
    pub redirect_uris: Vec<String>,
}

impl Client {
    /// The actor id agents using this client act as.
    pub fn actor(&self) -> String {
        format!("oauth:{}", self.host_path)
    }

    /// Whether `uri` is one of the client's redirect URIs: exactly, or a
    /// loopback `http` URI that differs only in its port (RFC 8252 §7.3).
    pub fn allows_redirect(&self, uri: &str) -> bool {
        if self.redirect_uris.iter().any(|r| r == uri) {
            return true;
        }
        let Ok(asked) = HttpUrl::parse(uri) else { return false };
        let loopback = |u: &HttpUrl| !u.https && matches!(u.host.as_str(), "127.0.0.1" | "::1" | "localhost");
        loopback(&asked)
            && self
                .redirect_uris
                .iter()
                .filter_map(|r| HttpUrl::parse(r).ok())
                .any(|r| loopback(&r) && r.host == asked.host && r.rest == asked.rest)
    }
}

/// Fetched documents by `client_id`.
#[derive(Debug)]
pub struct Cache {
    policy: Policy,
    entries: Mutex<HashMap<String, (Client, Instant)>>,
}

fn max_age(cache_control: &str) -> Duration {
    let cc = cache_control.to_lowercase();
    if cc.contains("no-store") || cc.contains("no-cache") {
        return Duration::ZERO;
    }
    cc.split(',')
        .find_map(|d| d.trim().strip_prefix("max-age=").and_then(|n| n.trim().parse::<u64>().ok()))
        .map_or(Duration::from_mins(5), Duration::from_secs)
        .min(MAX_CACHE)
}

impl Cache {
    pub fn new(policy: Policy) -> Self {
        Self { policy, entries: Mutex::new(HashMap::new()) }
    }

    /// The client behind `client_id`, fetched and checked. Blocking.
    pub fn get(&self, client_id: &str) -> Result<Client, String> {
        if let Some((c, until)) = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(client_id)
            && Instant::now() < *until
        {
            return Ok(c.clone());
        }
        let url = HttpUrl::parse(client_id).map_err(|_| "client_id must be an https URL".to_string())?;
        let loopback_http = self.policy.allow_http_loopback && !url.https && matches!(url.host.as_str(), "127.0.0.1" | "localhost" | "::1");
        if !url.https && !loopback_http {
            return Err("client_id must be an https URL".into());
        }
        let path = url.rest.split(['?', '#']).next().unwrap_or("/");
        if path == "/" || url.rest.contains('#') {
            return Err("client_id must name a document (a URL with a path and no fragment)".into());
        }
        // The consent screen shows the host as it is: no look-alike letters.
        if !url.host.is_ascii() || url.host.ends_with('.') {
            return Err("client_id's host must be written in ASCII (punycode for international names), without a trailing dot".into());
        }
        let opts = FetchOptions { allow_private: loopback_http, max_redirects: 0, timeout: TIMEOUT, ..FetchOptions::default() };
        let got = web::fetch(client_id, &opts, MAX_DOC).map_err(|e| format!("couldn't read the client's metadata at {client_id}: {e}"))?;
        let doc: Value = serde_json::from_slice(&got.bytes).map_err(|_| "the client's metadata isn't JSON".to_string())?;
        if doc.get("client_id").and_then(Value::as_str) != Some(client_id) {
            return Err("the client's metadata names a different client_id".into());
        }
        if doc.get("token_endpoint_auth_method").and_then(Value::as_str) != Some("none") {
            return Err("the client must use token_endpoint_auth_method \"none\"".into());
        }
        let redirect_uris: Vec<String> = doc
            .get("redirect_uris")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if redirect_uris.is_empty() {
            return Err("the client's metadata lists no redirect_uris".into());
        }
        let default_port = if url.https { 443 } else { 80 };
        let host = if url.port == default_port { url.host.clone() } else { format!("{}:{}", url.host, url.port) };
        let name = doc
            .get("client_name")
            .and_then(Value::as_str)
            .map(|n| n.chars().filter(|c| !c.is_control()).take(80).collect::<String>())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| host.clone());
        let client = Client { client_id: client_id.to_string(), name, host, host_path: format!("{}{path}", url.host), redirect_uris };
        let ttl = max_age(&got.cache_control);
        if !ttl.is_zero() {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(client_id.to_string(), (client.clone(), Instant::now() + ttl));
        }
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_match_exactly_or_by_loopback_port() {
        let c = Client {
            client_id: "https://a.example/c.json".into(),
            name: "A".into(),
            host: "a.example".into(),
            host_path: "a.example/c.json".into(),
            redirect_uris: vec!["http://127.0.0.1:3000/callback".into(), "https://a.example/cb".into()],
        };
        assert!(c.allows_redirect("https://a.example/cb"));
        assert!(c.allows_redirect("http://127.0.0.1:49152/callback"), "any loopback port");
        assert!(!c.allows_redirect("http://127.0.0.1:49152/other"));
        assert!(!c.allows_redirect("http://localhost:3000/callback"), "same host only");
        assert!(!c.allows_redirect("https://a.example/cb2"));
        assert!(!c.allows_redirect("https://evil.example/cb"));
        assert_eq!(c.actor(), "oauth:a.example/c.json");
    }

    #[test]
    fn client_ids_must_be_https_documents_on_public_hosts() {
        let cache = Cache::new(Policy::default());
        assert!(cache.get("http://a.example/c.json").unwrap_err().contains("https"));
        assert!(cache.get("https://a.example/").unwrap_err().contains("path"));
        assert!(cache.get("https://a.example/c.json#x").unwrap_err().contains("path"));
        assert!(cache.get("https://\u{441}laude.ai/c.json").unwrap_err().contains("ASCII"), "look-alike letters");
        assert!(cache.get("https://claude.ai./c.json").unwrap_err().contains("ASCII"));
        let local = cache.get("https://127.0.0.1/c.json").unwrap_err();
        assert!(local.contains("private or local"), "{local}");
        assert_eq!(max_age("public, max-age=600"), Duration::from_mins(10));
        assert_eq!(max_age("max-age=999999"), MAX_CACHE);
        assert_eq!(max_age("no-store"), Duration::ZERO);
    }
}
