// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire's OAuth 2.1 authorization server for MCP over HTTP (design §4.6).
//!
//! 1. A client calls `/mcp` without a token and gets `401` pointing at the
//!    protected-resource metadata (RFC 9728), which names this server as the
//!    issuer (RFC 8414 metadata at `/.well-known/oauth-authorization-server`).
//! 2. `/oauth/authorize` checks the client's metadata document, the redirect
//!    URI, PKCE (S256 only), the resource and the scopes, then files a consent
//!    request and shows "Approve this request in Quire" with the client's
//!    host and a verification code that Quire shows too. It sets a secret,
//!    unguessable per request, in an `HttpOnly` cookie: the browser that
//!    started the request.
//! 3. The person approves in Quire (or with `quire-cli mcp approve <id>`);
//!    the waiting page, in that browser only, redirects with `code`, `state`
//!    and `iss`. Knowing the request id isn't enough to get the code. One
//!    code per approval; codes work once, for 60 s.
//! 4. `/oauth/token` swaps the code (with the PKCE verifier) for a 1-hour
//!    access token and a rotating 30-day refresh token, bound to the MCP
//!    endpoint. `/oauth/revoke` revokes a token's family.
//!
//! Dynamic Client Registration is not offered; clients without a metadata
//! document use personal tokens.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{HttpState, html, no_store, now_ms};
use crate::consent::State as ConsentState;
use crate::tokens::{SCOPE_NAMES, Scopes, hash};

/// How long an authorization code works.
pub const CODE_TTL_MS: i64 = 60_000;

/// An issued, unredeemed code.
#[derive(Debug, Clone)]
pub struct Code {
    client_id: String,
    label: String,
    actor: String,
    redirect_uri: String,
    challenge: String,
    scopes: Scopes,
    resource: String,
    expires: i64,
}

/// Codes by their hash.
#[derive(Debug, Default)]
pub struct Codes(HashMap<String, Code>);

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn page(title: &str, body: &str, refresh: Option<&str>) -> String {
    let meta = refresh.map(|u| format!("<meta http-equiv=\"refresh\" content=\"2;url={}\">", escape(u))).unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">{meta}<title>{t}</title>\
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:34em;margin:4em auto;padding:0 1em;color:#222}}h1{{font-size:1.4em}}code{{background:#eee;padding:.1em .3em}}</style>\
</head><body><h1>{t}</h1>{body}</body></html>",
        t = escape(title)
    )
}

fn error_page(message: &str) -> Response {
    html(StatusCode::BAD_REQUEST, page("Quire can't connect this app", &format!("<p>{}</p>", escape(message)), None))
}

/// Append query parameters to a redirect URI.
fn with_params(uri: &str, params: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes().fold(String::new(), |mut out, b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                let _ = write!(out, "%{b:02X}");
            }
            out
        })
    };
    let q: Vec<String> = params.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={}", enc(v))).collect();
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{sep}{}", q.join("&"))
}

fn json_error(status: StatusCode, error: &str, description: &str) -> Response {
    (status, no_store(), axum::Json(json!({"error": error, "error_description": description}))).into_response()
}

pub(crate) async fn protected_resource(State(st): State<Arc<HttpState>>) -> Response {
    axum::Json(json!({
        "resource": st.urls.resource,
        "authorization_servers": [st.urls.issuer],
        "scopes_supported": SCOPE_NAMES,
        "bearer_methods_supported": ["header"],
        "resource_name": "Quire",
    }))
    .into_response()
}

pub(crate) async fn authorization_server(State(st): State<Arc<HttpState>>) -> Response {
    let i = &st.urls.issuer;
    axum::Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/oauth/authorize"),
        "token_endpoint": format!("{i}/oauth/token"),
        "revocation_endpoint": format!("{i}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": SCOPE_NAMES,
    }))
    .into_response()
}

pub(crate) async fn authorize(State(st): State<Arc<HttpState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
    let client_id = get("client_id").to_string();
    if client_id.is_empty() {
        return error_page("The app didn't say who it is (client_id is missing).");
    }
    let client = {
        let st = Arc::clone(&st);
        let id = client_id.clone();
        match tokio::task::spawn_blocking(move || st.cimd.get(&id)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return error_page(&e),
            Err(_) => return error_page("Internal error"),
        }
    };
    let redirect_uri = get("redirect_uri");
    if !client.allows_redirect(redirect_uri) {
        // Never redirect to an address the client didn't register.
        return error_page("The app asked to return to an address it didn't register.");
    }
    let state = get("state");
    let fail = |error: &str, desc: &str| {
        Redirect::to(&with_params(
            redirect_uri,
            &[("error", error), ("error_description", desc), ("state", state), ("iss", &st.urls.issuer)],
        ))
        .into_response()
    };
    if state.len() > 512 {
        return fail("invalid_request", "state is too long");
    }
    if get("response_type") != "code" {
        return fail("unsupported_response_type", "only response_type=code is supported");
    }
    let challenge = get("code_challenge");
    if get("code_challenge_method") != "S256"
        || challenge.len() != 43
        || !challenge.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return fail("invalid_request", "PKCE with code_challenge_method=S256 is required");
    }
    if get("resource") != st.urls.resource {
        return fail("invalid_target", "resource must be this server's MCP endpoint");
    }
    let scope = if get("scope").trim().is_empty() { "notes:read" } else { get("scope") };
    let Ok(scopes) = Scopes::parse(scope) else { return fail("invalid_scope", "unknown scope") };
    let names = scopes.names();
    let consent = st.consent.clone();
    let (cid, name, host, redirect, resource) =
        (client.client_id.clone(), client.name.clone(), client.host.clone(), redirect_uri.to_string(), st.urls.resource.clone());
    let (challenge, state_s, actor) = (challenge.to_string(), state.to_string(), client.actor());
    // The browser's secret (only its hash is kept) and the code to compare.
    let mut raw = [0u8; 32];
    if getrandom::fill(&mut raw).is_err() {
        return error_page("No randomness available.");
    }
    let secret = URL_SAFE_NO_PAD.encode(raw);
    let (browser, code) = (hash(&secret), crate::consent::verify_code());
    let code2 = code.clone();
    let created = tokio::task::spawn_blocking(move || {
        let id = consent.create(&cid, &name, &host, &redirect, &names, &resource, now_ms())?;
        // What the code needs later; only the browser secret's hash is kept.
        consent.annotate(
            &id,
            &[("code_challenge", &challenge), ("oauth_state", &state_s), ("actor", &actor), ("browser", &browser), ("verify_code", &code2)],
        )?;
        Ok::<_, String>(id)
    })
    .await;
    let Ok(Ok(id)) = created else { return error_page("Quire couldn't file the request.") };
    let mut res = waiting(&st, &id, &client.name, &client.host, &code, scopes);
    let secure = if st.urls.issuer.starts_with("https://") { "; Secure" } else { "" };
    let cookie = format!("{}={secret}; Path=/oauth/authorize; HttpOnly; SameSite=Lax; Max-Age=600{secure}", cookie_name(&id));
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

/// The cookie that holds request `id`'s browser secret.
fn cookie_name(id: &str) -> String {
    format!("quire_oauth_{id}")
}

/// Whether the request comes from the browser that started consent `r`.
fn same_browser(headers: &HeaderMap, id: &str, r: &crate::consent::Request) -> bool {
    let want = r.other.get("browser").and_then(|v| v.as_str()).unwrap_or_default();
    let name = cookie_name(id);
    !want.is_empty()
        && headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().split_once('='))
            .any(|(k, v)| k == name && hash(v) == want)
}

fn waiting(st: &HttpState, id: &str, name: &str, host: &str, code: &str, scopes: Scopes) -> Response {
    let wants = scopes.describe().iter().fold(String::new(), |mut s, d| {
        let _ = write!(s, "<li>{}</li>", escape(d));
        s
    });
    let body = format!(
        "<p>An app at <b style=\"font-size:1.3em\">{}</b> (it calls itself “{}”) asks to use your Quire notes:</p><ul>{wants}</ul>\
<p>Approve or deny this request in the Quire app. Check that Quire shows the same code:</p>\
<p style=\"font-size:1.6em;letter-spacing:.1em\"><b>{}</b></p>\
<p>Without the app open, run:</p><p><code>quire-cli mcp approve {}</code></p>\
<p>This page continues by itself. Keep it open: only this browser window can finish connecting.</p>",
        escape(host),
        escape(name),
        escape(code),
        escape(id)
    );
    html(
        StatusCode::OK,
        page("Approve this request in Quire", &body, Some(&format!("{}/oauth/authorize/wait?request={id}", st.urls.issuer))),
    )
}

pub(crate) async fn wait(State(st): State<Arc<HttpState>>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let id = q.get("request").cloned().unwrap_or_default();
    let consent = st.consent.clone();
    let id2 = id.clone();
    let Ok(Some(r)) = tokio::task::spawn_blocking(move || consent.get(&id2)).await else {
        return error_page("This request doesn't exist. Start connecting again from the app.");
    };
    // Only the browser that started the request may continue it: the code
    // is never handed to someone who only knows the request id.
    if !same_browser(&headers, &id, &r) {
        return html(
            StatusCode::FORBIDDEN,
            page(
                "Quire can't connect this app",
                "<p>Finish connecting in the browser window that started it. If that window is gone, start connecting again from the app.</p>",
                None,
            ),
        );
    }
    let now = now_ms();
    let other = |k: &str| r.other.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let state = other("oauth_state");
    let clear = format!("{}=; Path=/oauth/authorize; HttpOnly; SameSite=Lax; Max-Age=0", cookie_name(&id));
    let finish = |mut res: Response| {
        if let Ok(v) = HeaderValue::from_str(&clear) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
        res
    };
    match r.state {
        ConsentState::Pending if r.expires <= now => error_page("This request expired. Start connecting again from the app."),
        ConsentState::Pending => waiting(&st, &id, &r.client_name, &r.client_host, r.verify_code(), r.scopes()),
        ConsentState::Denied => finish(
            Redirect::to(&with_params(
                &r.redirect_uri,
                &[("error", "access_denied"), ("error_description", "denied in Quire"), ("state", &state), ("iss", &st.urls.issuer)],
            ))
            .into_response(),
        ),
        ConsentState::Used => error_page("This request was already completed."),
        ConsentState::Approved => {
            let consent = st.consent.clone();
            let id2 = id.clone();
            if !tokio::task::spawn_blocking(move || consent.mark_used(&id2)).await.unwrap_or(false) {
                return error_page("This request was already completed.");
            }
            let mut raw = [0u8; 32];
            if getrandom::fill(&mut raw).is_err() {
                return error_page("No randomness available.");
            }
            let code = URL_SAFE_NO_PAD.encode(raw);
            let entry = Code {
                client_id: r.client_id.clone(),
                label: r.client_name.clone(),
                actor: other("actor"),
                redirect_uri: r.redirect_uri.clone(),
                challenge: other("code_challenge"),
                scopes: r.scopes(),
                resource: r.resource.clone(),
                expires: now + CODE_TTL_MS,
            };
            {
                let mut codes = st.codes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                codes.0.retain(|_, c| c.expires > now);
                codes.0.insert(hash(&code), entry);
            }
            finish(
                Redirect::to(&with_params(&r.redirect_uri, &[("code", &code), ("state", &state), ("iss", &st.urls.issuer)]))
                    .into_response(),
            )
        }
    }
}

fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
}

pub(crate) async fn token(State(st): State<Arc<HttpState>>, Form(f): Form<HashMap<String, String>>) -> Response {
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
    let now = now_ms();
    let resource = if get("resource").is_empty() { st.urls.resource.clone() } else { get("resource").to_string() };
    if resource != st.urls.resource {
        return json_error(StatusCode::BAD_REQUEST, "invalid_target", "resource must be this server's MCP endpoint");
    }
    let tokens = st.tokens.clone();
    let issued = match get("grant_type") {
        "authorization_code" => {
            let code = {
                let mut codes = st.codes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                codes.0.remove(&hash(get("code")))
            };
            let Some(c) = code else { return json_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown or used code") };
            if c.expires <= now || c.client_id != get("client_id") || c.redirect_uri != get("redirect_uri") || c.resource != resource {
                return json_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code doesn't match this request");
            }
            if !pkce_ok(get("code_verifier"), &c.challenge) {
                return json_error(StatusCode::BAD_REQUEST, "invalid_grant", "PKCE verification failed");
            }
            tokio::task::spawn_blocking(move || tokens.issue(&c.client_id, &c.label, &c.actor, c.scopes, &c.resource, now)).await
        }
        "refresh_token" => {
            let narrowed = match get("scope") {
                "" => None,
                s => match Scopes::parse(s) {
                    Ok(s) => Some(s),
                    Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_scope", "unknown scope"),
                },
            };
            let (rt, cid) = (get("refresh_token").to_string(), get("client_id").to_string());
            tokio::task::spawn_blocking(move || tokens.refresh(&rt, &cid, &resource, narrowed, now)).await
        }
        _ => return json_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "use authorization_code or refresh_token"),
    };
    match issued {
        Ok(Ok(p)) => (
            StatusCode::OK,
            no_store(),
            axum::Json(json!({
                "access_token": p.access, "token_type": "Bearer", "expires_in": crate::tokens::ACCESS_TTL_MS / 1000,
                "refresh_token": p.refresh, "scope": p.scopes.to_oauth(),
            })),
        )
            .into_response(),
        Ok(Err(e)) => {
            let (error, desc) = e.split_once(": ").unwrap_or((&e, ""));
            let error = if error.starts_with("invalid_") { error } else { "invalid_grant" };
            json_error(StatusCode::BAD_REQUEST, error, desc)
        }
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", ""),
    }
}

pub(crate) async fn revoke(State(st): State<Arc<HttpState>>, Form(f): Form<HashMap<String, String>>) -> Response {
    let token = f.get("token").cloned().unwrap_or_default();
    let tokens = st.tokens.clone();
    let _ = tokio::task::spawn_blocking(move || tokens.revoke_token(&token)).await;
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_is_s256_only() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(pkce_ok(verifier, challenge), "RFC 7636 appendix B");
        assert!(!pkce_ok(challenge, challenge), "plain is refused");
        assert!(!pkce_ok("short", challenge));
    }

    #[test]
    fn pages_escape_and_redirects_encode() {
        assert_eq!(
            with_params("http://127.0.0.1:5/cb", &[("code", "a b"), ("state", ""), ("iss", "http://x")]),
            "http://127.0.0.1:5/cb?code=a%20b&iss=http%3A%2F%2Fx"
        );
        assert_eq!(with_params("https://a/cb?x=1", &[("code", "c")]), "https://a/cb?x=1&code=c");
        assert!(page("<t>", "", None).contains("&lt;t&gt;"));
    }
}
