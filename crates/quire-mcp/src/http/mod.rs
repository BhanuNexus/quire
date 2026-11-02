// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! MCP over Streamable HTTP, with Quire's own OAuth authorization server (design §4.6).
//!
//! Off unless the person turns it on (Settings → Connections, or
//! `quire-cli mcp serve-http`); it listens on 127.0.0.1:7311 by default.
//!
//! * `POST /mcp` takes one JSON-RPC message. Modern (2026-07-28) requests
//!   carry `MCP-Protocol-Version`, `Mcp-Method` and, for `tools/call`,
//!   `resources/read` and `prompts/get`, `Mcp-Name`, which must match the
//!   body (else `400`, `-32020`). Older clients use `initialize` and
//!   `MCP-Protocol-Version`; they get JSON answers and no server streams.
//!   `subscriptions/listen` answers with an event stream.
//! * Every request needs a bearer token, even on loopback: a personal token,
//!   or one from the OAuth flow (see [`oauth`]).
//! * DNS-rebinding defence: a present `Origin` must be the public URL's
//!   origin, and `Host` must be the address Quire listens on or the public
//!   URL's host.
//! * `GET` and `DELETE` on `/mcp` are `405`. Quire terminates no TLS: put a
//!   reverse proxy or tunnel in front for remote use and set the public URL.

pub mod cimd;
pub mod oauth;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use quire_app::intel::chat::CancelFlag;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::consent::ConsentStore;
use crate::protocol::{self, MODERN, code};
use crate::server::{Conn, Reply, Server};
use crate::tokens::TokenStore;

/// The largest JSON-RPC body accepted.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// Keep-alive comments on event streams.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How to serve.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: IpAddr,
    pub port: u16,
    /// The URL clients use when Quire is behind a proxy or tunnel
    /// (`https://notes.example.com` or `…/mcp`). Empty: the bind address.
    pub public_url: String,
    pub tokens: TokenStore,
    pub consent: ConsentStore,
    /// How client metadata documents are fetched (tests relax it).
    pub cimd: cimd::Policy,
}

impl HttpConfig {
    /// Loopback on `port`, with the default token and consent stores.
    pub fn local(port: u16) -> Option<Self> {
        Some(Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            public_url: String::new(),
            tokens: TokenStore::default_store()?,
            consent: ConsentStore::default_store()?,
            cimd: cimd::Policy::default(),
        })
    }
}

/// Where the server answers, worked out once.
#[derive(Debug, Clone)]
pub struct Urls {
    /// The canonical MCP endpoint (the OAuth resource): `…/mcp`.
    pub resource: String,
    /// The issuer: the resource's origin.
    pub issuer: String,
    /// Hosts accepted in `Host` (lowercase, with port when not default).
    pub hosts: Vec<String>,
    /// The one origin accepted in `Origin` (the public URL's), if any.
    pub origin: Option<String>,
}

impl Urls {
    pub fn new(bind: IpAddr, port: u16, public_url: &str) -> Result<Self, String> {
        let host_of = |ip: IpAddr| if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() };
        let mut hosts = vec![format!("{}:{port}", host_of(bind))];
        if bind.is_loopback() {
            hosts.extend([format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]);
        }
        let public = public_url.trim().trim_end_matches('/');
        if public.is_empty() {
            let shown = if bind.is_unspecified() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { bind };
            let issuer = format!("http://{}:{port}", host_of(shown));
            return Ok(Self { resource: format!("{issuer}/mcp"), issuer, hosts, origin: None });
        }
        let u = quire_import::web::HttpUrl::parse(public).map_err(|_| format!("the public URL {public} isn't an http(s) URL"))?;
        let origin = u.origin();
        let default_port = if u.https { 443 } else { 80 };
        hosts.push(if u.port == default_port { u.host.clone() } else { format!("{}:{}", u.host, u.port) });
        hosts.push(format!("{}:{}", u.host, u.port));
        let path = u.rest.split(['?', '#']).next().unwrap_or("/").trim_end_matches('/');
        let resource = if path.is_empty() { format!("{origin}/mcp") } else { format!("{origin}{path}") };
        Ok(Self { issuer: origin.clone(), resource, hosts, origin: Some(origin) })
    }

    /// The path of the MCP endpoint (`/mcp`).
    pub fn mcp_path(&self) -> String {
        self.resource.strip_prefix(&self.issuer).filter(|p| !p.is_empty()).unwrap_or("/mcp").to_string()
    }
}

pub(crate) struct HttpState {
    pub(crate) server: Server,
    pub(crate) urls: Urls,
    pub(crate) tokens: TokenStore,
    pub(crate) consent: ConsentStore,
    pub(crate) cimd: cimd::Cache,
    pub(crate) codes: Mutex<oauth::Codes>,
}

/// A running HTTP server. Dropping it stops it.
pub struct HttpServer {
    addr: SocketAddr,
    urls: Urls,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for HttpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpServer").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl HttpServer {
    /// Bind and serve on a background thread with its own small runtime.
    pub fn start(server: Server, cfg: HttpConfig) -> Result<Self, String> {
        let listener =
            std::net::TcpListener::bind((cfg.bind, cfg.port)).map_err(|e| format!("can't listen on {}:{}: {e}", cfg.bind, cfg.port))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let urls = Urls::new(cfg.bind, addr.port(), &cfg.public_url)?;
        server.start_pump();
        let state = Arc::new(HttpState {
            server,
            urls: urls.clone(),
            tokens: cfg.tokens,
            consent: cfg.consent,
            cimd: cimd::Cache::new(cfg.cimd),
            codes: Mutex::default(),
        });
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mcp-http")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let thread = std::thread::Builder::new()
            .name("mcp-http".into())
            .spawn(move || {
                rt.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
                    let app = router(state);
                    let _ = axum::serve(listener, app.into_make_service())
                        .with_graceful_shutdown(async {
                            let _ = stop_rx.await;
                        })
                        .await;
                });
                rt.shutdown_timeout(Duration::from_secs(2));
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { addr, urls, stop: Some(stop_tx), thread: Some(thread) })
    }

    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub const fn urls(&self) -> &Urls {
        &self.urls
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn router(state: Arc<HttpState>) -> Router {
    let mcp_path = state.urls.mcp_path();
    Router::new()
        .route(&mcp_path, post(mcp).get(not_allowed).delete(not_allowed))
        .route("/.well-known/oauth-protected-resource", get(oauth::protected_resource))
        .route(&format!("/.well-known/oauth-protected-resource{mcp_path}"), get(oauth::protected_resource))
        .route("/.well-known/oauth-authorization-server", get(oauth::authorization_server))
        .route("/oauth/authorize", get(oauth::authorize))
        .route("/oauth/authorize/wait", get(oauth::wait))
        .route("/oauth/token", post(oauth::token))
        .route("/oauth/revoke", post(oauth::revoke))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state)
}

/// A JSON-RPC error response with an HTTP status.
fn rpc_error(status: StatusCode, id: Value, code: i64, message: impl Into<String>) -> Response {
    let mut body = json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message.into() } });
    if !id.is_null() {
        body["id"] = id;
    }
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Origin and Host checks for every request (DNS rebinding).
async fn guard(State(st): State<Arc<HttpState>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let h = req.headers();
    if let Some(origin) = h.get(header::ORIGIN) {
        let ok = origin.to_str().ok().is_some_and(|o| st.urls.origin.as_deref().is_some_and(|allowed| o.eq_ignore_ascii_case(allowed)));
        if !ok {
            return rpc_error(StatusCode::FORBIDDEN, Value::Null, code::INVALID_REQUEST, "Origin not allowed");
        }
    }
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default().to_lowercase();
    if !st.urls.hosts.contains(&host) {
        return rpc_error(StatusCode::FORBIDDEN, Value::Null, code::INVALID_REQUEST, "Host not allowed");
    }
    let mut res = next.run(req).await;
    res.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    res
}

async fn not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")], "").into_response()
}

fn unauthorized(st: &HttpState) -> Response {
    let www = format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"notes:read\"", st.urls.issuer);
    let mut r = rpc_error(StatusCode::UNAUTHORIZED, Value::Null, code::INVALID_REQUEST, "Unauthorized: a bearer token is required");
    if let Ok(v) = HeaderValue::from_str(&www) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

fn insufficient_scope(st: &HttpState, id: Value, scope: &str) -> Response {
    let www = format!(
        "Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
        st.urls.issuer
    );
    let mut r = rpc_error(StatusCode::FORBIDDEN, id, code::INVALID_REQUEST, format!("This needs the {scope} permission"));
    if let Ok(v) = HeaderValue::from_str(&www) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

/// A header value, with the `=?base64?…?=` sentinel decoded.
fn header_text(h: &HeaderMap, name: &str) -> Option<String> {
    let raw = h.get(name)?.to_str().ok()?;
    if let Some(b) = raw.strip_prefix("=?base64?").and_then(|r| r.strip_suffix("?=")) {
        return String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b).ok()?).ok();
    }
    Some(raw.to_string())
}

fn now_ms() -> i64 {
    quire_app::history_ops::now_ms()
}

/// Sets the request's cancel flag if the client goes away first.
struct CancelOnDrop(CancelFlag, bool);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.1 {
            self.0.cancel();
        }
    }
}

/// An event stream fed by a channel; removes its subscription when dropped
/// (the client closed it).
struct Events {
    rx: mpsc::UnboundedReceiver<Value>,
    server: Server,
    handle: u64,
}

impl futures_core::Stream for Events {
    type Item = Result<Event, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|v| v.map(|v| Ok(Event::default().event("message").data(v.to_string()))))
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.server.remove_subscription(self.handle);
    }
}

async fn mcp(State(st): State<Arc<HttpState>>, headers: HeaderMap, body: Bytes) -> Response {
    // Authentication first: nothing about the vault without a token.
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
    let Some(token) = bearer else { return unauthorized(&st) };
    let grant = {
        let (tokens, token, resource) = (st.tokens.clone(), token.to_string(), st.urls.resource.clone());
        tokio::task::spawn_blocking(move || tokens.authenticate(&token, &resource, now_ms())).await.ok().flatten()
    };
    let Some(grant) = grant else { return unauthorized(&st) };
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or_default().to_lowercase();
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            code::HEADER_MISMATCH,
            "Accept must list application/json and text/event-stream",
        );
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error(StatusCode::BAD_REQUEST, Value::Null, code::PARSE_ERROR, "Parse error");
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_string) else {
        return rpc_error(StatusCode::BAD_REQUEST, id, code::INVALID_REQUEST, "Expected a JSON-RPC request or notification");
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    if msg.get("id").is_none() {
        // Notifications (notifications/initialized from older clients): accepted, nothing to do.
        return StatusCode::ACCEPTED.into_response();
    }
    let header_version = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(v) = protocol::requested_version(&params) {
        // Modern: the headers mirror the body.
        let mismatch =
            |what: &str| rpc_error(StatusCode::BAD_REQUEST, id.clone(), code::HEADER_MISMATCH, format!("Header mismatch: {what}"));
        match header_version.as_deref() {
            None => return mismatch("MCP-Protocol-Version is required"),
            Some(h) if h != v => return mismatch(&format!("MCP-Protocol-Version {h} doesn't match the body's {v}")),
            Some(_) => {}
        }
        if let Err(e) = protocol::era_for(v) {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                json!({"jsonrpc": "2.0", "id": id, "error": e}).to_string(),
            )
                .into_response();
        }
        if header_text(&headers, "mcp-method").as_deref() != Some(method.as_str()) {
            return mismatch("Mcp-Method doesn't match the body's method");
        }
        let named = match method.as_str() {
            "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
            "resources/read" => params.get("uri").and_then(Value::as_str),
            _ => None,
        };
        if let Some(n) = named
            && header_text(&headers, "mcp-name").as_deref() != Some(n)
        {
            return mismatch("Mcp-Name doesn't match the body");
        }
    } else if header_version.as_deref() == Some(MODERN) && method != "initialize" {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            id,
            code::INVALID_PARAMS,
            protocol::missing_version()["message"].as_str().unwrap_or_default(),
        );
    }
    if method == "tools/call"
        && let Some(t) = params.get("name").and_then(Value::as_str).and_then(|n| st.server.tool_access(n))
        && !grant.scopes.grant().allows(t)
    {
        let need = if t == crate::tools::Access::Ask { "notes:ask" } else { "notes:propose" };
        return insufficient_scope(&st, id, need);
    }
    let conn = Conn::http(st.server.new_conn(), grant, header_version);
    let server = st.server.clone();
    let cancel = CancelFlag::new();
    let mut guard = CancelOnDrop(cancel.clone(), false);
    let reply = tokio::task::spawn_blocking(move || server.handle(&msg, &conn, &cancel).with_conn(conn.id)).await;
    guard.1 = true;
    let Ok((reply, conn_id)) = reply else {
        return rpc_error(StatusCode::INTERNAL_SERVER_ERROR, id, code::INTERNAL, "Internal error");
    };
    match reply {
        Reply::Response(v) | Reply::Initialized { response: v, .. } | Reply::Subscribe { response: v, .. } => {
            let status = match v.pointer("/error/code").and_then(Value::as_i64) {
                Some(code::METHOD_NOT_FOUND) => StatusCode::NOT_FOUND,
                Some(code::UNSUPPORTED_VERSION | code::HEADER_MISMATCH) => StatusCode::BAD_REQUEST,
                Some(code::INVALID_PARAMS) if v.pointer("/error/data/supported").is_some() => StatusCode::BAD_REQUEST,
                _ => StatusCode::OK,
            };
            (status, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
        }
        Reply::None => StatusCode::ACCEPTED.into_response(),
        Reply::Listen { id, filter } => {
            let (tx, rx) = mpsc::unbounded_channel::<Value>();
            let sink: crate::subs::Sink = Arc::new(move |v| {
                let _ = tx.send(v);
            });
            let handle = st.server.listen(conn_id, id, &filter, sink);
            let events = Events { rx, server: st.server.clone(), handle };
            let mut r = Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("")).into_response();
            r.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            r
        }
    }
}

impl Reply {
    fn with_conn(self, conn: u64) -> (Self, u64) {
        (self, conn)
    }
}

/// Headers every JSON answer of the authorization server carries.
pub(crate) fn no_store() -> [(header::HeaderName, HeaderValue); 2] {
    [(header::CACHE_CONTROL, HeaderValue::from_static("no-store")), (header::PRAGMA, HeaderValue::from_static("no-cache"))]
}

/// For tests: the methods `/mcp` refuses.
pub const REFUSED_METHODS: [Method; 2] = [Method::GET, Method::DELETE];

/// A body from a string, for HTML pages.
pub(crate) fn html(status: StatusCode, page: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], Body::from(page)).into_response()
}

#[cfg(test)]
mod tests;
