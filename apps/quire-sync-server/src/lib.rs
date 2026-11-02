// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Quire sync server: an HTTP API over an encrypted revision log.
//!
//! The server never sees note text or names; it orders revisions, rejects
//! pushes made against a stale head, and wakes waiting devices.
//!
//! | Method and path | What it does |
//! |---|---|
//! | `GET /health` | `ok` (no token needed) |
//! | `GET /v1/info` | `{"protocol":{"min":1,"max":2},"server":"0.9.0"}` (no token needed) |
//! | `GET /v1/spaces` | List spaces (id, created, revision count), so a new device can pick one |
//! | `POST /v1/spaces` | Create a space with its wrapped key |
//! | `GET /v1/spaces/{space}/key` | The wrapped key, so a new device can join |
//! | `PUT /v1/spaces/{space}/key` | Replace it (passphrase change) |
//! | `GET /v1/spaces/{space}/changes?after=N&limit=M` | Revisions after `N` |
//! | `POST /v1/spaces/{space}/revisions` | Push one revision: 200 accepted, 409 with the current head |
//! | `GET /v1/spaces/{space}/wait?after=N&timeout=S` | Long-poll until the log passes `N` |
//! | `PUT /v1/spaces/{space}/min_protocol` | `{"min": 2}`: refuse older clients for this space (never lowered) |
//!
//! Every `/v1` request except `info` needs `Authorization: Bearer <token>`.
//! Clients name their protocol in `Quire-Sync-Protocol` (absent: 1, Quire
//! 0.8). A client older than its space's minimum gets `426` with
//! `{"error": …, "min": N}`.

pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use quire_sync::protocol::{MIN_PROTOCOL, PROTOCOL, PROTOCOL_HEADER, ProtocolRange, ServerInfo};
use quire_sync::{FileId, PushOutcome, Revision, SpaceId, WrappedKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::watch;

pub use store::{Store, StoreError};

/// Largest request body: one revision of a big note.
pub const MAX_BODY: usize = 32 * 1024 * 1024;
/// Most revisions returned by one `changes` call.
pub const MAX_PAGE: u32 = 1000;
/// Longest a `wait` call is held open.
pub const MAX_WAIT: Duration = Duration::from_secs(55);

pub struct AppState {
    store: Store,
    token_hash: [u8; 32],
    /// Latest log position per space, for waking long-polls.
    bells: Mutex<HashMap<String, watch::Sender<u64>>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppState")
    }
}

impl AppState {
    pub fn new(store: Store, token: &str) -> Self {
        Self { store, token_hash: Sha256::digest(token.as_bytes()).into(), bells: Mutex::new(HashMap::new()) }
    }

    fn bell(&self, space: &SpaceId) -> watch::Sender<u64> {
        let mut bells = self.bells.lock().unwrap_or_else(PoisonError::into_inner);
        bells.entry(space.0.clone()).or_insert_with(|| watch::channel(0).0).clone()
    }
}

/// The HTTP application, ready to serve.
pub fn app(state: Arc<AppState>) -> Router {
    let api = routes_0_8()
        .route("/spaces/{space}/min_protocol", axum::routing::put(set_min_protocol))
        .route_layer(middleware::from_fn_with_state(Arc::clone(&state), require_protocol))
        .route_layer(middleware::from_fn_with_state(Arc::clone(&state), require_token))
        .route("/info", get(info));
    Router::new().route("/health", get(|| async { "ok" })).nest("/v1", api).layer(DefaultBodyLimit::max(MAX_BODY)).with_state(state)
}

fn routes_0_8() -> Router<Arc<AppState>> {
    Router::new()
        .route("/spaces", post(create_space).get(list_spaces))
        .route("/spaces/{space}/key", get(get_key).put(put_key))
        .route("/spaces/{space}/changes", get(changes))
        .route("/spaces/{space}/revisions", post(push))
        .route("/spaces/{space}/wait", get(wait))
        .route("/spaces/{space}/files/{file}", get(file_history))
}

/// The application as Quire 0.8 served it (no `/v1/info`, no minimums):
/// for tests of mixed fleets.
#[doc(hidden)]
pub fn app_0_8(state: Arc<AppState>) -> Router {
    let api = routes_0_8().route_layer(middleware::from_fn_with_state(Arc::clone(&state), require_token));
    Router::new().route("/health", get(|| async { "ok" })).nest("/v1", api).layer(DefaultBodyLimit::max(MAX_BODY)).with_state(state)
}

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo { protocol: ProtocolRange { min: MIN_PROTOCOL, max: PROTOCOL }, server: env!("CARGO_PKG_VERSION").to_string() })
}

/// The client's protocol: the header, or 1 (Quire 0.8 sends none).
fn client_protocol(headers: &HeaderMap) -> u32 {
    headers.get(PROTOCOL_HEADER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()).unwrap_or(1)
}

/// Refuse clients older than the space's minimum with `426`.
async fn require_protocol(State(st): State<Arc<AppState>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let space = req.uri().path().strip_prefix("/spaces/").and_then(|r| r.split('/').next()).map(str::to_string);
    if let Some(space) = space.filter(|s| valid_id(s).is_ok()) {
        let min = db(&st, move |s| s.min_protocol(&SpaceId(space))).await.unwrap_or(1);
        if client_protocol(&headers) < min {
            let body = serde_json::json!({ "error": "This vault now needs Quire 0.9 or later on every device.", "min": min });
            return (StatusCode::UPGRADE_REQUIRED, Json(body)).into_response();
        }
    }
    next.run(req).await
}

#[derive(Debug, Deserialize)]
struct MinProtocol {
    min: u32,
}

async fn set_min_protocol(
    State(st): State<Arc<AppState>>,
    Path(space): Path<String>,
    Json(body): Json<MinProtocol>,
) -> Result<Json<serde_json::Value>, ApiError> {
    valid_id(&space)?;
    if body.min == 0 || body.min > PROTOCOL {
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("min must be between 1 and {PROTOCOL}")));
    }
    let now = db(&st, move |s| s.raise_min_protocol(&SpaceId(space), body.min)).await?;
    Ok(Json(serde_json::json!({ "min": now })))
}

async fn require_token(State(st): State<Arc<AppState>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let given =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
    let hash: [u8; 32] = Sha256::digest(given.as_bytes()).into();
    if bool::from(hash.ct_eq(&st.token_hash)) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(ErrorBody { error: "missing or wrong token".into() })).into_response()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSpace {
    pub space: SpaceId,
    pub wrapped_key: WrappedKey,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    pub revisions: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Changes {
    pub revisions: Vec<Revision>,
    /// The log's latest position (may be past the last revision returned).
    pub latest: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Latest {
    pub latest: u64,
}

#[derive(Debug, Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    after: u64,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WaitQuery {
    #[serde(default)]
    after: u64,
    timeout: Option<u64>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let code = match e {
            StoreError::NoSpace => StatusCode::NOT_FOUND,
            StoreError::SpaceExists => StatusCode::CONFLICT,
            StoreError::BadParent => StatusCode::UNPROCESSABLE_ENTITY,
            StoreError::Db(_) | StoreError::Corrupt(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(code, e.to_string())
    }
}

/// Run blocking database work off the async threads.
async fn db<T: Send + 'static>(
    st: &Arc<AppState>,
    f: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, ApiError> {
    let st = Arc::clone(st);
    tokio::task::spawn_blocking(move || f(&st.store))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

fn valid_id(s: &str) -> Result<(), ApiError> {
    if !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(())
    } else {
        Err(ApiError(StatusCode::BAD_REQUEST, "bad id".into()))
    }
}

async fn create_space(State(st): State<Arc<AppState>>, Json(body): Json<CreateSpace>) -> Result<StatusCode, ApiError> {
    valid_id(&body.space.0)?;
    db(&st, move |s| s.create_space(&body.space, &body.wrapped_key)).await?;
    Ok(StatusCode::CREATED)
}

async fn list_spaces(State(st): State<Arc<AppState>>) -> Result<Json<Vec<SpaceInfo>>, ApiError> {
    let list = db(&st, Store::spaces).await?;
    Ok(Json(list.into_iter().map(|(space, created, revisions)| SpaceInfo { space, created, revisions }).collect()))
}

async fn get_key(State(st): State<Arc<AppState>>, Path(space): Path<String>) -> Result<Json<WrappedKey>, ApiError> {
    valid_id(&space)?;
    Ok(Json(db(&st, move |s| s.wrapped_key(&SpaceId(space))).await?))
}

/// Every version of one file, for history and restore.
async fn file_history(State(st): State<Arc<AppState>>, Path((space, file)): Path<(String, String)>) -> Result<Json<Changes>, ApiError> {
    valid_id(&space)?;
    valid_id(&file)?;
    let out = db(&st, move |s| {
        let sp = SpaceId(space);
        Ok(Changes { revisions: s.file_history(&sp, &FileId(file))?, latest: s.latest(&sp)? })
    })
    .await?;
    Ok(Json(out))
}

async fn put_key(State(st): State<Arc<AppState>>, Path(space): Path<String>, Json(key): Json<WrappedKey>) -> Result<StatusCode, ApiError> {
    valid_id(&space)?;
    db(&st, move |s| s.set_wrapped_key(&SpaceId(space), &key)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn changes(
    State(st): State<Arc<AppState>>,
    Path(space): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<Changes>, ApiError> {
    valid_id(&space)?;
    let limit = q.limit.unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE);
    let out = db(&st, move |s| {
        let sp = SpaceId(space);
        Ok(Changes { revisions: s.changes(&sp, q.after, limit)?, latest: s.latest(&sp)? })
    })
    .await?;
    Ok(Json(out))
}

async fn push(State(st): State<Arc<AppState>>, Path(space): Path<String>, Json(rev): Json<Revision>) -> Result<Response, ApiError> {
    valid_id(&space)?;
    for id in [&rev.file.0, &rev.rev.0, &rev.device.0].into_iter().chain(rev.parents.iter().map(|p| &p.0)) {
        valid_id(id)?;
    }
    let sp = SpaceId(space);
    let sp2 = sp.clone();
    let outcome = db(&st, move |s| s.push(&sp2, &rev)).await?;
    Ok(match outcome {
        PushOutcome::Accepted { seq } => {
            st.bell(&sp).send_replace(seq);
            (StatusCode::OK, Json(PushOutcome::Accepted { seq })).into_response()
        }
        c @ PushOutcome::Conflict { .. } => (StatusCode::CONFLICT, Json(c)).into_response(),
    })
}

async fn wait(State(st): State<Arc<AppState>>, Path(space): Path<String>, Query(q): Query<WaitQuery>) -> Result<Json<Latest>, ApiError> {
    valid_id(&space)?;
    let sp = SpaceId(space);
    let sp2 = sp.clone();
    let latest = db(&st, move |s| s.latest(&sp2)).await?;
    if latest > q.after {
        return Ok(Json(Latest { latest }));
    }
    let mut rx = st.bell(&sp).subscribe();
    let timeout = Duration::from_secs(q.timeout.unwrap_or(30)).min(MAX_WAIT);
    let woke = tokio::time::timeout(timeout, rx.wait_for(|&seq| seq > q.after)).await;
    let latest = match woke {
        Ok(Ok(seq)) => *seq,
        _ => latest,
    };
    Ok(Json(Latest { latest }))
}
