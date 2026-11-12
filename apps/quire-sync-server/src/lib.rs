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
//! | `GET /v1/spaces/{space}/min_protocol` | `{"min": N}` (0.13) |
//! | `POST /v1/spaces/{space}/devices` | `{"device": id}` → `{"token": …}`: a token for one device (server token only; 0.13) |
//! | `GET /v1/spaces/{space}/devices` | Devices: enrolled, removed, newest revision, their own record (0.13) |
//! | `PUT /v1/spaces/{space}/devices/{device}/record` | A device's record about itself, by that device (0.13) |
//! | `DELETE /v1/spaces/{space}/devices/{device}` | Remove a device: its token stops working (0.13) |
//! | `GET /v1/spaces/{space}/keys` | The vault's key records (0.13) |
//! | `PUT /v1/spaces/{space}/keys/{epoch}` | Add the next key record; `409` if another came first (0.13) |
//!
//! Every `/v1` request except `info` needs `Authorization: Bearer <token>`:
//! the server's token, or (0.13) a device token for that space. A space at
//! protocol 3 takes device tokens only, except to make one; a removed
//! device's token gets `403`.
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
use axum::routing::{delete, get, post, put};
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

    /// The database (to compact it).
    pub const fn store(&self) -> &Store {
        &self.store
    }

    fn bell(&self, space: &SpaceId) -> watch::Sender<u64> {
        let mut bells = self.bells.lock().unwrap_or_else(PoisonError::into_inner);
        bells.entry(space.0.clone()).or_insert_with(|| watch::channel(0).0).clone()
    }
}

/// The HTTP application, ready to serve.
pub fn app(state: Arc<AppState>) -> Router {
    let api = routes_0_8()
        .route("/spaces/{space}/min_protocol", put(set_min_protocol).get(get_min_protocol))
        .route("/spaces/{space}/devices", post(enroll).get(list_devices))
        .route("/spaces/{space}/devices/{device}", delete(remove_device))
        .route("/spaces/{space}/devices/{device}/record", put(put_device_record))
        .route("/spaces/{space}/keys", get(key_records))
        .route("/spaces/{space}/keys/{epoch}", put(put_key_record))
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
            let need = if min >= 3 { "0.13" } else { "0.9" };
            let body = serde_json::json!({ "error": format!("This vault now needs Quire {need} or later on every device."), "min": min });
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

/// Who made a request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Caller {
    /// The server's own token.
    Admin,
    /// A device token (0.13), for its space only.
    Device(String),
}

fn deny(code: StatusCode, msg: &str) -> Response {
    (code, Json(ErrorBody { error: msg.into() })).into_response()
}

async fn require_token(State(st): State<Arc<AppState>>, headers: HeaderMap, mut req: Request, next: Next) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let path = req.uri().path().to_string();
    let space = path.strip_prefix("/spaces/").and_then(|r| r.split('/').next()).filter(|s| valid_id(s).is_ok()).map(str::to_string);
    let hash: [u8; 32] = Sha256::digest(given.as_bytes()).into();
    if bool::from(hash.ct_eq(&st.token_hash)) {
        // A 0.13 space takes the server's token only to make a device token.
        let enrolling = req.method() == axum::http::Method::POST && path.ends_with("/devices");
        if let Some(sp) = space.filter(|_| !enrolling) {
            let min = db(&st, move |s| s.min_protocol(&SpaceId(sp))).await.unwrap_or(1);
            if client_protocol(&headers) < min {
                // An older Quire: tell it to update, whatever its token.
                let body = serde_json::json!({ "error": "This vault now needs Quire 0.13 or later on every device.", "min": min });
                return (StatusCode::UPGRADE_REQUIRED, Json(body)).into_response();
            }
            if min >= quire_sync::protocol::HARDENED_PROTOCOL {
                return deny(StatusCode::FORBIDDEN, "This vault takes device tokens only: set this device up for sync again.");
            }
        }
        req.extensions_mut().insert(Caller::Admin);
        return next.run(req).await;
    }
    if given.is_empty() {
        return deny(StatusCode::UNAUTHORIZED, "missing or wrong token");
    }
    let owner = db(&st, move |s| s.token_owner(&given)).await.ok().flatten();
    match owner {
        Some(o) if o.removed => deny(StatusCode::FORBIDDEN, "This device was removed from the sync vault."),
        Some(o) if space.as_deref() == Some(o.space.0.as_str()) => {
            req.extensions_mut().insert(Caller::Device(o.device));
            next.run(req).await
        }
        _ => deny(StatusCode::UNAUTHORIZED, "missing or wrong token"),
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
            StoreError::SpaceExists | StoreError::KeyConflict | StoreError::Enrolled => StatusCode::CONFLICT,
            StoreError::BadParent | StoreError::OldKey => StatusCode::UNPROCESSABLE_ENTITY,
            StoreError::Removed => StatusCode::FORBIDDEN,
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

async fn push(
    State(st): State<Arc<AppState>>,
    caller: Option<axum::Extension<Caller>>,
    Path(space): Path<String>,
    Json(rev): Json<Revision>,
) -> Result<Response, ApiError> {
    valid_id(&space)?;
    for id in
        [&rev.file.0, &rev.rev.0, &rev.device.0].into_iter().chain(rev.parents.iter().map(|p| &p.0)).chain(rev.chain.iter().map(|p| &p.0))
    {
        valid_id(id)?;
    }
    if rev.parents.len() > 2 || rev.chain.len() > quire_sync::protocol::CHAIN {
        return Err(ApiError(StatusCode::BAD_REQUEST, "too many parents".into()));
    }
    // A device token writes as that device only.
    if let Some(axum::Extension(Caller::Device(d))) = &caller
        && *d != rev.device.0
    {
        return Err(ApiError(StatusCode::FORBIDDEN, "a device token writes as its own device only".into()));
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

async fn get_min_protocol(State(st): State<Arc<AppState>>, Path(space): Path<String>) -> Result<Json<serde_json::Value>, ApiError> {
    valid_id(&space)?;
    let min = db(&st, move |s| s.min_protocol(&SpaceId(space))).await?;
    Ok(Json(serde_json::json!({ "min": min })))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Enroll {
    pub device: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Enrolled {
    pub token: String,
}

/// A token for one device (server token only).
async fn enroll(
    State(st): State<Arc<AppState>>,
    caller: Option<axum::Extension<Caller>>,
    Path(space): Path<String>,
    Json(body): Json<Enroll>,
) -> Result<Json<Enrolled>, ApiError> {
    valid_id(&space)?;
    valid_id(&body.device)?;
    if !matches!(caller, Some(axum::Extension(Caller::Admin))) {
        return Err(ApiError(StatusCode::FORBIDDEN, "device tokens are made with the server's token".into()));
    }
    let token = db(&st, move |s| s.enroll(&SpaceId(space), &body.device)).await?;
    Ok(Json(Enrolled { token }))
}

async fn list_devices(State(st): State<Arc<AppState>>, Path(space): Path<String>) -> Result<Json<Vec<store::DeviceRow>>, ApiError> {
    valid_id(&space)?;
    Ok(Json(db(&st, move |s| s.devices(&SpaceId(space))).await?))
}

async fn put_device_record(
    State(st): State<Arc<AppState>>,
    caller: Option<axum::Extension<Caller>>,
    Path((space, device)): Path<(String, String)>,
    Json(record): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    valid_id(&space)?;
    valid_id(&device)?;
    if matches!(&caller, Some(axum::Extension(Caller::Device(d))) if *d != device) {
        return Err(ApiError(StatusCode::FORBIDDEN, "a device publishes its own record only".into()));
    }
    let json = record.to_string();
    if json.len() > 64 * 1024 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "record too large".into()));
    }
    db(&st, move |s| s.set_device_record(&SpaceId(space), &device, &json)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a device: its token is refused from now on.
async fn remove_device(
    State(st): State<Arc<AppState>>,
    caller: Option<axum::Extension<Caller>>,
    Path((space, device)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    valid_id(&space)?;
    valid_id(&device)?;
    if matches!(&caller, Some(axum::Extension(Caller::Device(d))) if *d == device) {
        return Err(ApiError(StatusCode::FORBIDDEN, "a device can't remove itself; turn sync off there".into()));
    }
    db(&st, move |s| s.remove_device(&SpaceId(space), &device)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn key_records(State(st): State<Arc<AppState>>, Path(space): Path<String>) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    valid_id(&space)?;
    Ok(Json(db(&st, move |s| s.key_records(&SpaceId(space))).await?))
}

async fn put_key_record(
    State(st): State<Arc<AppState>>,
    Path((space, epoch)): Path<(String, u32)>,
    Json(record): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    valid_id(&space)?;
    if record.get("epoch").and_then(serde_json::Value::as_u64) != Some(u64::from(epoch))
        || record.get("space").and_then(|v| v.as_str()) != Some(space.as_str())
    {
        return Err(ApiError(StatusCode::BAD_REQUEST, "the record names another epoch or vault".into()));
    }
    let json = record.to_string();
    if json.len() > 1024 * 1024 {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "record too large".into()));
    }
    db(&st, move |s| s.put_key_record(&SpaceId(space), epoch, &json)).await?;
    Ok(StatusCode::CREATED)
}
