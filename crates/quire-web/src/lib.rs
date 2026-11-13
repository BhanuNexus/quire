// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The web app of `quire-cli serve` (design 0.10-serve §2): read and lightly
//! edit a vault in a browser, on the same port as MCP over HTTP.
//!
//! Server-rendered pages with a little plain JavaScript, everything embedded
//! in the binary. The note view is the publish pipeline's
//! ([`quire_app::publish::render_note`]); search is the vault index (BM25,
//! accents and case ignored).
//!
//! * [`auth`]: the passphrase, sessions, CSRF, the limit on wrong guesses.
//! * [`paths`]: what may be shown (hidden folders, symlinks, `..`).
//! * `pages`: the pages; `edit`: saving with conflict checks.
//!
//! Every response says not to be cached (notes are private), not to be
//! framed, and carries a Content-Security-Policy that allows only this
//! server's own scripts, styles and images: HTML inside a note (which can
//! come from captured web pages or agents) can't run scripts or load
//! anything from elsewhere.

pub mod auth;
mod edit;
mod html;
mod pages;
pub mod paths;
mod render;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use quire_agent::{AgentStore, DeviceInfo, SystemClock};
use quire_mcp::consent::ConsentStore;
use quire_mcp::http::{Mount, Urls};

use crate::auth::{LoginStore, SessionLimits};

/// The largest form accepted (a note being saved).
pub const MAX_FORM: usize = 8 * 1024 * 1024;

/// Milliseconds since the Unix epoch, injectable for tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// How the web app runs.
#[derive(Clone)]
pub struct WebConfig {
    /// The MCP server sharing the port: its vault and index serve the web too.
    pub server: quire_mcp::Server,
    /// No editing and no new notes.
    pub read_only: bool,
    /// Folders hidden from the web (`--hide`), on top of `[web] hide`.
    pub hide: Vec<String>,
    pub login: LoginStore,
    /// Where MCP connection requests wait (shared with `quire-cli mcp approve`).
    pub consent: ConsentStore,
    pub limits: SessionLimits,
    /// Recorded with web edits (`You · via web · <device>`).
    pub device: DeviceInfo,
    pub clock: Clock,
}

impl std::fmt::Debug for WebConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebConfig").field("read_only", &self.read_only).field("hide", &self.hide).finish_non_exhaustive()
    }
}

impl WebConfig {
    /// The defaults for a server: the config folder's login and consent
    /// stores, this device, the system clock.
    pub fn new(server: quire_mcp::Server) -> Result<Self, String> {
        Ok(Self {
            server,
            read_only: false,
            hide: Vec::new(),
            login: LoginStore::default_store().ok_or("no config folder on this system")?,
            consent: ConsentStore::default_store().ok_or("no config folder on this system")?,
            limits: SessionLimits::default(),
            device: quire_app::device::this_device(),
            clock: Arc::new(quire_app::history_ops::now_ms),
        })
    }
}

pub(crate) struct Web {
    pub(crate) cfg: WebConfig,
    pub(crate) guard: paths::Guard,
    pub(crate) urls: Urls,
    /// Origins a form may come from: the public URL's, and this server's own addresses.
    origins: Vec<String>,
    /// Cookies get `Secure` and the `__Host-` prefix (the public URL is https).
    pub(crate) secure: bool,
    pub(crate) sessions: Mutex<auth::Sessions>,
    pub(crate) limiter: Mutex<auth::Limiter>,
    /// One web save at a time, so the version check and the write go together.
    pub(crate) writing: Mutex<()>,
    /// The files the web may list, and when they were read.
    pub(crate) files: Mutex<Option<(i64, Arc<Vec<quire_app::vault::NotePath>>)>>,
    /// Whether the reader may see a hierarchy edge, and when it was worked out.
    pub(crate) hierarchy: Mutex<Option<(i64, bool)>>,
}

/// A request, as the pages see it.
pub(crate) struct Req {
    pub(crate) method: Method,
    /// Percent-decoded path.
    pub(crate) path: String,
    pub(crate) query: HashMap<String, String>,
    pub(crate) form: HashMap<String, String>,
    pub(crate) headers: HeaderMap,
}

impl Req {
    pub(crate) fn q(&self, k: &str) -> &str {
        self.query.get(k).map_or("", String::as_str)
    }

    pub(crate) fn f(&self, k: &str) -> &str {
        self.form.get(k).map_or("", String::as_str)
    }

    pub(crate) fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
    }
}

/// A response before it becomes an HTTP one.
pub(crate) struct Res {
    pub(crate) status: StatusCode,
    pub(crate) headers: Vec<(header::HeaderName, String)>,
    pub(crate) body: Vec<u8>,
}

impl Res {
    pub(crate) fn html(status: StatusCode, page: String) -> Self {
        Self { status, headers: vec![(header::CONTENT_TYPE, "text/html; charset=utf-8".into())], body: page.into_bytes() }
    }

    pub(crate) fn json(status: StatusCode, v: &serde_json::Value) -> Self {
        Self { status, headers: vec![(header::CONTENT_TYPE, "application/json".into())], body: v.to_string().into_bytes() }
    }

    /// `303 See Other`, so a reload never posts again.
    pub(crate) fn redirect(to: &str) -> Self {
        Self { status: StatusCode::SEE_OTHER, headers: vec![(header::LOCATION, to.to_string())], body: Vec::new() }
    }

    pub(crate) fn with(mut self, name: header::HeaderName, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// The policy for pages: only this server's scripts, styles and images.
const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self'; connect-src 'self'; \
form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'self'; frame-src 'self'";

/// Routes for [`quire_mcp::http::HttpServer::start_with`].
pub fn mount(cfg: WebConfig, urls: &Urls) -> Mount {
    let vault = cfg.server.vault().clone();
    let store = AgentStore::open(&vault, cfg.device.clone(), Arc::new(SystemClock));
    let guard = paths::Guard::new(vault, store, &cfg.hide);
    let mut origins: Vec<String> = urls.hosts.iter().map(|h| format!("http://{h}")).collect();
    origins.extend(urls.origin.clone());
    let web = Arc::new(Web {
        secure: urls.issuer.starts_with("https://"),
        guard,
        urls: urls.clone(),
        origins,
        sessions: Mutex::default(),
        limiter: Mutex::default(),
        writing: Mutex::new(()),
        files: Mutex::new(None),
        hierarchy: Mutex::new(None),
        cfg,
    });
    let router = axum::Router::new().fallback(dispatch).with_state(web);
    Mount { router, consent_page: Some("/consent".into()) }
}

impl Web {
    /// Whether a request that changes something comes from one of this
    /// server's own pages (an `Origin` other than ours is refused; one is
    /// required, as every current browser sends it on a form post).
    pub(crate) fn same_origin(&self, h: &HeaderMap) -> bool {
        h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).is_some_and(|o| self.origins.iter().any(|a| a.eq_ignore_ascii_case(o)))
    }

    pub(crate) fn now(&self) -> i64 {
        (self.cfg.clock)()
    }

    pub(crate) fn session_cookie(&self) -> &'static str {
        if self.secure { "__Host-quire_session" } else { "quire_session" }
    }

    pub(crate) fn cookie(&self, name: &str, value: &str, max_age_s: i64) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}{secure}")
    }
}

async fn dispatch(State(web): State<Arc<Web>>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let (method, uri, headers) = (parts.method, parts.uri, parts.headers);
    // The DNS-rebinding defence MCP has: `Host` must be this server's.
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default().to_lowercase();
    if !web.urls.hosts.contains(&host) {
        return finish(Res::html(StatusCode::FORBIDDEN, html::error_page("This address isn't one Quire answers on.")));
    }
    let form = if method == Method::POST {
        let Ok(body) = axum::body::to_bytes(body, MAX_FORM).await else {
            return finish(Res::html(StatusCode::PAYLOAD_TOO_LARGE, html::error_page("That's too much to send at once.")));
        };
        parse_query(&String::from_utf8_lossy(&body))
    } else {
        HashMap::new()
    };
    let req = Req {
        method,
        path: percent_decode(uri.path()).unwrap_or_default(),
        query: uri.query().map(parse_query).unwrap_or_default(),
        form,
        headers,
    };
    let w = Arc::clone(&web);
    let res = tokio::task::spawn_blocking(move || pages::handle(&w, &req))
        .await
        .unwrap_or_else(|_| Res::html(StatusCode::INTERNAL_SERVER_ERROR, html::error_page("Something went wrong.")));
    finish(res)
}

fn finish(res: Res) -> Response {
    let mut r = Response::new(Body::from(res.body));
    *r.status_mut() = res.status;
    let h = r.headers_mut();
    let mut has_csp = false;
    let mut has_cache = false;
    for (k, v) in res.headers {
        has_csp |= k == header::CONTENT_SECURITY_POLICY;
        has_cache |= k == header::CACHE_CONTROL;
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.append(k, v);
        }
    }
    if !has_csp {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_CSP));
    }
    if !has_cache {
        // Private notes stay out of browser and proxy caches.
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    r
}

/// `%XX` decoded as UTF-8; `None` for bad escapes or bytes.
pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `a=1&b=x+y` (form or query string).
pub(crate) fn parse_query(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((percent_decode(&k.replace('+', " "))?, percent_decode(&v.replace('+', " "))?))
        })
        .collect()
}

/// A vault path in a URL: each segment percent-encoded.
pub(crate) fn url_path(p: &str) -> String {
    p.split('/').map(quire_app::markdown::render::encode_url).collect::<Vec<_>>().join("/")
}

/// For URL query values.
pub(crate) fn encode_query(s: &str) -> String {
    s.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_decode_and_encode() {
        assert_eq!(percent_decode("/n/Caf%C3%A9%20notes.md").unwrap(), "/n/Café notes.md");
        assert!(percent_decode("%zz").is_none() && percent_decode("%ff").is_none());
        let q = parse_query("q=a+b%26c&x=&y");
        assert_eq!((q["q"].as_str(), q["x"].as_str(), q["y"].as_str()), ("a b&c", "", ""));
        assert_eq!(url_path("A b/C#d.md"), "A%20b/C%23d.md");
        assert_eq!(encode_query("a b/c&"), "a%20b/c%26");
    }
}
