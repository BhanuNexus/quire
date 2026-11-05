// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! MCP over Streamable HTTP, end to end on loopback (design §4.6, §4.7):
//! the header validation matrix, Origin and Host checks, authentication,
//! and a client that completes OAuth with a Client ID Metadata Document,
//! then calls `search`.
#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::Duration;

use base64::Engine as _;
use quire_app::vault::Vault;
use quire_mcp::consent::ConsentStore;
use quire_mcp::http::{HttpConfig, HttpServer, cimd};
use quire_mcp::tokens::{Scopes, TokenStore};
use quire_mcp::{Server, ServerConfig};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

struct Env {
    http: HttpServer,
    base: String,
    tokens: TokenStore,
    consent: ConsentStore,
    _dir: tempfile::TempDir,
}

fn start(public_url: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::in_memory(&[
        ("Rope Notes.md", "# Rope Notes\n## Undo\nRopes make undo cheap because subtrees are shared.\n"),
        ("Ideas.md", "# Ideas\nWrite about [[Rope Notes]].\n"),
    ]);
    let device = Some(quire_agent::DeviceInfo { id: "d_http".into(), name: "test".into() });
    let srv = Server::with_config(vault, None, ServerConfig { device, ..ServerConfig::default() });
    let tokens = TokenStore::in_dir(&dir.path().join("mcp"));
    let consent = ConsentStore::in_dir(&dir.path().join("mcp/consent"));
    let cfg = HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        public_url: public_url.into(),
        tokens: tokens.clone(),
        consent: consent.clone(),
        cimd: cimd::Policy { allow_http_loopback: true },
    };
    // Every test here runs with other routes on the same port, as
    // `quire-cli serve` mounts its web app: MCP must behave the same.
    let http = HttpServer::start_with(srv, cfg, |_| quire_mcp::http::Mount {
        router: axum::Router::new().fallback(|| async { "web app" }),
        consent_page: Some("/consent".into()),
    })
    .unwrap();
    let base = format!("http://127.0.0.1:{}", http.addr().port());
    Env { http, base, tokens, consent, _dir: dir }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, n: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str())
    }
}

fn post(url: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut req = agent().post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req.send(body).unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    Resp { status, headers, body: res.body_mut().read_to_string().unwrap_or_default() }
}

fn get(url: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = agent().get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut res = req.call().unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    Resp { status, headers, body: res.body_mut().read_to_string().unwrap_or_default() }
}

const ACCEPT: (&str, &str) = ("Accept", "application/json, text/event-stream");

fn modern(method: &str, params: &Value) -> String {
    let mut p = params.clone();
    p["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "http-test", "version": "1"}});
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": p}).to_string()
}

fn mcp(env: &Env, token: &str, method: &str, name: Option<&str>, params: &Value) -> Resp {
    let auth = format!("Bearer {token}");
    let mut h = vec![ACCEPT, ("Authorization", auth.as_str()), ("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", method)];
    if let Some(n) = name {
        h.push(("Mcp-Name", n));
    }
    post(&format!("{}/mcp", env.base), &h, &modern(method, params))
}

#[test]
fn requests_are_authenticated_and_validated() {
    let env = start("");
    let url = format!("{}/mcp", env.base);
    let token = env.tokens.create_personal("t", Scopes::parse("notes:read").unwrap(), None, 0).unwrap();
    let auth = format!("Bearer {token}");
    // No token: 401 pointing at the protected-resource metadata.
    let r = post(&url, &[ACCEPT], &modern("tools/list", &json!({})));
    assert_eq!(r.status, 401);
    let www = r.header("www-authenticate").unwrap();
    assert!(
        www.contains(&format!("resource_metadata=\"{}/.well-known/oauth-protected-resource\"", env.base))
            && www.contains("scope=\"notes:read\""),
        "{www}"
    );
    assert_eq!(post(&url, &[ACCEPT, ("Authorization", "Bearer qmcp_wrong")], "{}").status, 401);
    // The happy path.
    let r = mcp(&env, &token, "tools/list", None, &json!({}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["result"]["resultType"], "complete");
    assert!(
        r.json()["result"]["tools"].as_array().unwrap().iter().all(|t| t["annotations"]["readOnlyHint"] == true),
        "read scope: read tools"
    );
    // Header matrix.
    let base_h = |extra: &[(&'static str, &'static str)]| {
        let mut h = vec![ACCEPT, ("Authorization", auth.as_str())];
        h.extend_from_slice(extra);
        h
    };
    let call = modern("tools/call", &json!({"name": "search", "arguments": {"query": "undo"}}));
    for (headers, want) in [
        (base_h(&[("Mcp-Method", "tools/call"), ("Mcp-Name", "search")]), "missing version header"),
        (base_h(&[("MCP-Protocol-Version", "2025-06-18"), ("Mcp-Method", "tools/call"), ("Mcp-Name", "search")]), "version mismatch"),
        (base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Name", "search")]), "missing method"),
        (base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/list"), ("Mcp-Name", "search")]), "method mismatch"),
        (base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call")]), "missing name"),
        (base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", "view")]), "name mismatch"),
    ] {
        let r = post(&url, &headers, &call);
        assert_eq!((r.status, r.json()["error"]["code"].clone()), (400, json!(-32020)), "{want}: {}", r.body);
    }
    // Base64 sentinel names are decoded before comparing.
    let enc = format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode("search"));
    let r = post(
        &url,
        &base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", &*Box::leak(enc.into_boxed_str()))]),
        &call,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.json()["result"]["structuredContent"]["hits"].as_array().unwrap().iter().any(|h| h["path"] == "Rope Notes.md"));
    // Accept must list both types.
    let r = post(
        &url,
        &[("Accept", "application/json"), ("Authorization", &auth), ("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/list")],
        &modern("tools/list", &json!({})),
    );
    assert_eq!(r.status, 400);
    // Unknown method: 404 with a JSON-RPC error; unsupported version: 400.
    let r = mcp(&env, &token, "nope/nope", None, &json!({}));
    assert_eq!((r.status, r.json()["error"]["code"].clone()), (404, json!(-32601)));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2030-01-01"}}}).to_string();
    let r = post(&url, &base_h(&[("MCP-Protocol-Version", "2030-01-01"), ("Mcp-Method", "tools/list")]), &body);
    assert_eq!((r.status, r.json()["error"]["code"].clone()), (400, json!(-32022)));
    // Insufficient scope for a write tool: 403 naming the scope.
    let r = mcp(&env, &token, "tools/call", Some("create"), &json!({"name": "create", "arguments": {"path": "wiki/x.md", "content": "x"}}));
    assert_eq!(r.status, 403);
    assert!(r.header("www-authenticate").unwrap().contains("error=\"insufficient_scope\", scope=\"notes:propose\""));
    // GET and DELETE: 405. Notifications: 202.
    assert_eq!(get(&url, &[("Authorization", &auth)]).status, 405);
    let mut del = agent().delete(&url).header("Authorization", &auth).call().unwrap();
    assert_eq!(del.status().as_u16(), 405);
    let _ = del.body_mut().read_to_string();
    let r = post(
        &url,
        &base_h(&[("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "notifications/initialized")]),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert_eq!(r.status, 202);
    // A legacy client: initialize, then plain requests with its version header (no session ids).
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "old", "version": "1"}}});
    let r = post(&url, &base_h(&[]), &init.to_string());
    assert_eq!(r.json()["result"]["protocolVersion"], "2025-06-18");
    assert!(r.header("mcp-session-id").is_none());
    let r = post(
        &url,
        &base_h(&[("MCP-Protocol-Version", "2025-06-18")]),
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
    );
    assert!(r.json()["result"].get("resultType").is_none() && r.json()["result"]["tools"][0]["title"] == "Search notes");
    let r = post(&url, &base_h(&[]), &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}).to_string());
    assert_eq!(r.json()["result"], json!({}), "no header: 2025-03-26");
}

#[test]
fn origin_and_host_are_checked() {
    let env = start("");
    let token = env.tokens.create_personal("t", Scopes::parse("notes:read").unwrap(), None, 0).unwrap();
    let auth = format!("Bearer {token}");
    let url = format!("{}/mcp", env.base);
    let h = |extra: (&'static str, &'static str)| {
        vec![ACCEPT, ("Authorization", auth.as_str()), ("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/list"), extra]
    };
    let body = modern("tools/list", &json!({}));
    assert_eq!(post(&url, &h(("Origin", "https://evil.example")), &body).status, 403, "a web page can't call it");
    assert_eq!(post(&url, &h(("Origin", "null")), &body).status, 403);
    // DNS rebinding: the attacker's name resolves to 127.0.0.1, but Host gives it away.
    assert_eq!(post(&url, &h(("Host", "evil.example:7311")), &body).status, 403);
    assert_eq!(post(&url, &h(("Host", &*Box::leak(format!("localhost:{}", env.http.addr().port()).into_boxed_str()))), &body).status, 200);
    // Behind a proxy: the public URL's origin and host are accepted.
    let env = start("https://notes.example.com");
    let token = env.tokens.create_personal("t", Scopes::parse("notes:read").unwrap(), None, 0).unwrap();
    let auth = format!("Bearer {token}");
    let url = format!("{}/mcp", env.base);
    let hh = |o: &'static str, host: &'static str| {
        vec![
            ACCEPT,
            ("Authorization", auth.as_str()),
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/list"),
            ("Origin", o),
            ("Host", host),
        ]
    };
    assert_eq!(post(&url, &hh("https://notes.example.com", "notes.example.com"), &body).status, 200);
    assert_eq!(post(&url, &hh("https://other.example.com", "notes.example.com"), &body).status, 403);
    let prm = get(&format!("{}/.well-known/oauth-protected-resource", env.base), &[("Host", "notes.example.com")]).json();
    assert_eq!(prm["resource"], "https://notes.example.com/mcp");
    assert_eq!(prm["authorization_servers"][0], "https://notes.example.com");
}

/// Serve one JSON document (a client's metadata) on loopback.
fn serve_doc(make: impl Fn(&str) -> String + Send + 'static) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let me = base.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let body = make(&me);
            let mut s = s;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

/// Every byte percent-encoded.
fn pct(v: &str) -> String {
    v.bytes().fold(String::new(), |mut s, b| {
        let _ = write!(s, "%{b:02X}");
        s
    })
}

fn query(url: &str) -> Vec<(String, String)> {
    url.split_once('?')
        .map_or("", |(_, q)| q)
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), decode(v)))
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn form(url: &str, fields: &[(&str, &str)]) -> Resp {
    let body: String = fields.iter().map(|(k, v)| format!("{k}={}", pct(v))).collect::<Vec<_>>().join("&");
    post(url, &[("Content-Type", "application/x-www-form-urlencoded")], &body)
}

/// The `name=value` of the browser cookie an authorize page sets (`HttpOnly`).
fn browser_cookie(r: &Resp) -> String {
    let set = r.header("set-cookie").unwrap_or_else(|| panic!("no cookie: {:?}", r.headers));
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax") && set.contains("Path=/oauth/authorize"), "{set}");
    set.split(';').next().unwrap().to_string()
}

#[test]
fn oauth_with_a_client_metadata_document_then_search() {
    let env = start("");
    let doc_base = serve_doc(|me| {
        json!({"client_id": format!("{me}/client.json"), "client_name": "Test Agent", "redirect_uris": ["http://127.0.0.1:9/callback"], "token_endpoint_auth_method": "none", "logo_uri": "http://127.0.0.1:1/tracker.png"}).to_string()
    });
    let client_id = format!("{doc_base}/client.json");
    // 1. Unauthenticated call → 401 → protected-resource metadata → AS metadata.
    let r = post(&format!("{}/mcp", env.base), &[ACCEPT], &modern("tools/list", &json!({})));
    let www = r.header("www-authenticate").unwrap().to_string();
    let prm_url = www.split("resource_metadata=\"").nth(1).unwrap().split('"').next().unwrap();
    let prm = get(prm_url, &[]).json();
    let resource = prm["resource"].as_str().unwrap().to_string();
    assert_eq!(resource, format!("{}/mcp", env.base));
    let asm = get(&format!("{}/.well-known/oauth-authorization-server", prm["authorization_servers"][0].as_str().unwrap()), &[]).json();
    assert_eq!(asm["client_id_metadata_document_supported"], true);
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(asm["authorization_response_iss_parameter_supported"], true);
    // 2. Authorize with PKCE.
    let verifier = "a".repeat(20) + "-b.c_d~e" + &"f".repeat(30);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect = "http://127.0.0.1:54321/callback";
    let authorize = |extra: &[(&str, &str)]| {
        let mut q = vec![
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", "xyz"),
            ("scope", "notes:read notes:propose"),
            ("resource", resource.as_str()),
        ];
        for (k, v) in extra {
            q.retain(|(qk, _)| qk != k);
            q.push((k, v));
        }
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", pct(v))).collect();
        get(&format!("{}/oauth/authorize?{}", asm["issuer"].as_str().unwrap(), qs.join("&")), &[])
    };
    // Bad requests first: an unregistered redirect is never followed; others redirect with an error.
    let r = authorize(&[("redirect_uri", "https://evil.example/cb")]);
    assert_eq!(r.status, 400);
    assert!(r.header("location").is_none());
    let r = authorize(&[("code_challenge_method", "plain")]);
    assert_eq!(r.status, 303);
    assert!(r.header("location").unwrap().contains("error=invalid_request"));
    let r = authorize(&[("resource", "http://127.0.0.1:1/mcp")]);
    assert!(r.header("location").unwrap().contains("error=invalid_target"));
    let r = authorize(&[("scope", "notes:admin")]);
    assert!(r.header("location").unwrap().contains("error=invalid_scope"));
    // The real request waits for the person.
    let r = authorize(&[]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Approve this request in Quire") && r.body.contains("Test Agent") && r.body.contains("127.0.0.1"));
    assert!(!r.body.contains("tracker.png"), "the logo is never loaded");
    assert!(r.body.contains("/consent?request="), "a mounted web app's approval page is linked");
    assert_eq!(get(&format!("{}/elsewhere", env.base), &[]).body, "web app", "other paths reach the mounted routes");
    let pending = env.consent.pending(quire_app::history_ops::now_ms());
    assert_eq!(pending.len(), 1);
    let req = &pending[0];
    let port = doc_base.rsplit(':').next().unwrap();
    assert_eq!((req.client_name.as_str(), req.client_host.as_str()), ("Test Agent", format!("127.0.0.1:{port}").as_str()), "host and port");
    assert!(r.body.contains(req.verify_code()) && !req.verify_code().is_empty(), "the page shows the code Quire shows");
    // The browser that started the request holds its secret in an HttpOnly cookie.
    let cookie = browser_cookie(&r);
    let browser = [("Cookie", cookie.as_str())];
    let wait_url = format!("{}/oauth/authorize/wait?request={}", env.base, req.id);
    assert_eq!(get(&wait_url, &browser).status, 200, "still waiting");
    // 3. The person approves (Quire's dialog or `quire-cli mcp approve`).
    env.consent.decide(&req.id, true, quire_app::history_ops::now_ms()).unwrap();
    // Someone who knows only the request id gets nothing.
    for stolen in [vec![], vec![("Cookie", format!("{}=forged", cookie.split('=').next().unwrap()))]] {
        let h: Vec<(&str, &str)> = stolen.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = get(&wait_url, &h);
        assert_eq!(r.status, 403, "{}", r.body);
        assert!(r.header("location").is_none() && !r.body.contains("code="));
    }
    let r = get(&wait_url, &browser);
    assert_eq!(r.status, 303);
    let loc = r.header("location").unwrap().to_string();
    assert!(loc.starts_with(redirect));
    let q = query(&loc);
    let param = |k: &str| q.iter().find(|(qk, _)| qk == k).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(param("state"), "xyz");
    assert_eq!(param("iss"), env.base);
    let code = param("code");
    assert!(!code.is_empty());
    assert_ne!(get(&wait_url, &browser).status, 303, "one code per approval");
    // 4. Exchange the code: wrong verifier fails and burns it; a fresh code works.
    let token_url = format!("{}/oauth/token", env.base);
    let bad = form(
        &token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            ("code_verifier", &"z".repeat(50)),
            ("resource", &resource),
        ],
    );
    assert_eq!((bad.status, bad.json()["error"].clone()), (400, json!("invalid_grant")));
    let again = form(
        &token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", &resource),
        ],
    );
    assert_eq!(again.json()["error"], "invalid_grant", "codes work once");
    let cookie = browser_cookie(&authorize(&[]));
    let req = env.consent.pending(quire_app::history_ops::now_ms()).remove(0);
    env.consent.decide(&req.id, true, quire_app::history_ops::now_ms()).unwrap();
    let loc = get(&format!("{}/oauth/authorize/wait?request={}", env.base, req.id), &[("Cookie", &cookie)])
        .header("location")
        .unwrap()
        .to_string();
    let code = query(&loc).into_iter().find(|(k, _)| k == "code").unwrap().1;
    let tok = form(
        &token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
            ("resource", &resource),
        ],
    );
    assert_eq!(tok.status, 200, "{}", tok.body);
    assert_eq!(tok.header("cache-control"), Some("no-store"));
    let t = tok.json();
    assert_eq!(
        (t["token_type"].clone(), t["expires_in"].clone(), t["scope"].clone()),
        (json!("Bearer"), json!(3600), json!("notes:read notes:propose"))
    );
    let access = t["access_token"].as_str().unwrap().to_string();
    let refresh = t["refresh_token"].as_str().unwrap().to_string();
    assert!(!std::fs::read_to_string(env.tokens.path()).unwrap().contains(&access), "stored hashed");
    // 5. Call search with the token.
    let r = mcp(&env, &access, "tools/call", Some("search"), &json!({"name": "search", "arguments": {"query": "undo"}}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.json()["result"]["structuredContent"]["hits"].as_array().unwrap().iter().any(|h| h["path"] == "Rope Notes.md"));
    // propose scope: writes become proposals even in direct zones.
    let r =
        mcp(&env, &access, "tools/call", Some("create"), &json!({"name": "create", "arguments": {"path": "wiki/New.md", "content": "x"}}));
    assert_eq!(r.json()["result"]["structuredContent"]["outcome"], "proposed", "{}", r.body);
    // 6. Refresh rotates; revoking ends the family.
    let r2 = form(&token_url, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &client_id)]);
    assert_eq!(r2.status, 200, "{}", r2.body);
    let access2 = r2.json()["access_token"].as_str().unwrap().to_string();
    assert_eq!(mcp(&env, &access2, "tools/list", None, &json!({})).status, 200);
    assert_eq!(form(&token_url, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &client_id)]).status, 400);
    assert_eq!(mcp(&env, &access2, "tools/list", None, &json!({})).status, 401, "a replayed refresh token revokes the family");
    // A denied request redirects with access_denied.
    let cookie = browser_cookie(&authorize(&[]));
    let req = env.consent.pending(quire_app::history_ops::now_ms()).remove(0);
    env.consent.decide(&req.id, false, quire_app::history_ops::now_ms()).unwrap();
    let loc = get(&format!("{}/oauth/authorize/wait?request={}", env.base, req.id), &[("Cookie", &cookie)])
        .header("location")
        .unwrap()
        .to_string();
    assert!(loc.contains("error=access_denied") && loc.contains("state=xyz"));
}

#[test]
fn bad_client_metadata_is_refused() {
    let env = start("");
    for (doc, why) in [
        (
            json!({"client_id": "https://other/client.json", "redirect_uris": ["http://127.0.0.1:9/cb"], "token_endpoint_auth_method": "none"}),
            "different client_id",
        ),
        (
            json!({"client_id": "SELF", "redirect_uris": ["http://127.0.0.1:9/cb"], "token_endpoint_auth_method": "client_secret_basic"}),
            "token_endpoint_auth_method",
        ),
        (json!({"client_id": "SELF", "redirect_uris": [], "token_endpoint_auth_method": "none"}), "redirect_uris"),
    ] {
        let doc_base = serve_doc(move |me| doc.to_string().replace("SELF", &format!("{me}/client.json")));
        let cid = format!("{doc_base}/client.json");
        let r = get(
            &format!(
                "{}/oauth/authorize?client_id={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb",
                env.base,
                cid.replace(':', "%3A").replace('/', "%2F")
            ),
            &[],
        );
        assert_eq!(r.status, 400, "{why}");
        assert!(r.body.contains(why), "{why}: {}", r.body);
    }
}

#[test]
fn subscriptions_stream_events() {
    let env = start("");
    let token = env.tokens.create_personal("t", Scopes::parse("notes:read").unwrap(), None, 0).unwrap();
    let addr = env.http.addr();
    let body = modern("subscriptions/listen", &json!({"notifications": {"toolsListChanged": true, "promptsListChanged": true}}));
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        addr.port(),
        body.len()
    )
    .unwrap();
    let mut got = String::new();
    let mut buf = [0u8; 4096];
    while !got.contains("acknowledged") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "stream closed: {got}");
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    let lower = got.to_lowercase();
    assert!(lower.contains("content-type: text/event-stream"), "{got}");
    assert!(lower.contains("x-accel-buffering: no"));
    assert!(got.contains("\"toolsListChanged\":true") && !got.contains("promptsListChanged"), "{got}");
    assert!(got.contains("\"io.modelcontextprotocol/subscriptionId\":1"));
}
