// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The MCP server core, shared by stdio (`quire-cli mcp <vault>`) and
//! Streamable HTTP (`quire-cli mcp serve-http`, the app).
//!
//! Lets AI assistants (Claude Desktop, Claude Code, Cursor…) search, read and
//! change one vault. Nothing runs unless the user adds this command to their
//! client or turns the HTTP server on. Writing needs `--allow-write` (or a
//! token with a write scope), and then goes through the vault's agent policy:
//! direct in agent zones (`wiki/`), suggestions for the person to review
//! everywhere else.
//!
//! Clients that `initialize` (2024-11-05 to 2025-11-25) get the answers Quire
//! 0.8 gave, plus what their version defines. A 2026-07-28 client names the
//! version in each request's `_meta` and may begin with `server/discover`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use quire_agent::{Actor, AgentStore, ClientInfo, Clock, DeviceInfo, Mode, SystemClock, Via};
use quire_app::index::IndexService;
use quire_app::intel::chat::{CancelFlag, ChatModel};
use quire_app::vault::{ContentHash, NotePath, Vault, VaultEvent, VaultWatcher};
use serde_json::{Value, json};

use crate::instructions;
use crate::mrtr::Sealer;
use crate::protocol::{self, Era, LEGACY, META_CLIENT_CAPS, META_CLIENT_INFO, MODERN, SUPPORTED, code};
use crate::resources::{self, PromptDef, ReadCx, ReadError};
use crate::served::ServedCache;
use crate::subs::{Filter, Sink, Subscriptions, Visibility};
use crate::tokens::TokenGrant;
use crate::tools::{CallExt, Grant, ToolCx, ToolOutput, ToolRegistry};

/// How long a client may cache lists (design §4.1).
const TOOLS_TTL_MS: u64 = 300_000;
const DISCOVER_TTL_MS: u64 = 60_000;
/// The background pump's period.
const PUMP_EVERY: Duration = Duration::from_millis(150);

/// What the server is allowed to do, set by whoever starts it.
#[derive(Clone)]
pub struct ServerConfig {
    /// stdio: `--allow-write` (writes per policy). Over HTTP, each token's
    /// scopes decide instead.
    pub allow_write: bool,
    /// Every caller only reads, HTTP tokens included (`quire-cli serve --read-only`).
    pub read_only: bool,
    /// stdio: `--client KEY`, the key Quire gave this client (verified).
    pub client_key: Option<String>,
    /// URL capture may reach private and loopback addresses.
    pub allow_private_urls: bool,
    /// The user's AI provider, for `ask` in `model` mode.
    pub chat: Option<Arc<dyn ChatModel>>,
    /// Built-in prompts (the texts of `quire-assistant::prompts`).
    pub prompts: Vec<PromptDef>,
    /// `<config>/vaults/<key>/` (approvals, MRTR key). `None`: nothing approved.
    pub state_dir: Option<PathBuf>,
    pub device: Option<DeviceInfo>,
    pub clock: Arc<dyn Clock>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            allow_write: false,
            read_only: false,
            client_key: None,
            allow_private_urls: false,
            chat: None,
            prompts: Vec::new(),
            state_dir: None,
            device: None,
            clock: Arc::new(SystemClock),
        }
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").field("allow_write", &self.allow_write).field("client_key", &self.client_key).finish_non_exhaustive()
    }
}

/// Who is on the other end of a connection, as the transport established it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// stdio: the user started the process; [`ServerConfig`] says what it may do.
    Stdio,
    /// HTTP: a valid access token.
    Token(TokenGrant),
}

/// One connection's view of the protocol.
#[derive(Debug, Clone)]
pub struct Conn {
    pub id: u64,
    pub principal: Principal,
    /// stdio: the version `initialize` chose.
    pub legacy: Option<String>,
    /// From `initialize` (legacy clients report it once).
    pub client: Option<ClientInfo>,
    pub client_caps: Value,
    /// HTTP: the `MCP-Protocol-Version` header.
    pub http_version: Option<String>,
    pub http: bool,
}

impl Conn {
    pub const fn stdio(id: u64) -> Self {
        Self { id, principal: Principal::Stdio, legacy: None, client: None, client_caps: Value::Null, http_version: None, http: false }
    }

    pub const fn http(id: u64, grant: TokenGrant, header_version: Option<String>) -> Self {
        Self {
            id,
            principal: Principal::Token(grant),
            legacy: None,
            client: None,
            client_caps: Value::Null,
            http_version: header_version,
            http: true,
        }
    }
}

/// What to do with a message.
#[derive(Debug)]
pub enum Reply {
    /// A notification: nothing to send.
    None,
    /// The JSON-RPC response.
    Response(Value),
    /// `initialize`: the response, and the session it starts (stdio keeps it).
    Initialized { response: Value, version: String, client: Option<ClientInfo>, caps: Value },
    /// `subscriptions/listen`: the transport registers it with a sink and keeps
    /// the request open. `sees` is what the subscriber may hear about.
    Listen { id: Value, filter: Filter, sees: Visibility },
    /// Legacy `resources/subscribe` (`subscribe = true`) or `unsubscribe`.
    Subscribe { response: Value, uri: String, subscribe: bool, sees: Visibility },
}

pub(crate) struct Core {
    vault: Vault,
    index: IndexService,
    watcher: Mutex<Option<VaultWatcher>>,
    registry: ToolRegistry,
    pub(crate) served: ServedCache,
    pub(crate) subs: Subscriptions,
    config: ServerConfig,
    device: DeviceInfo,
    /// One store per (actor, cap), so a grant never leaks to another caller.
    stores: Mutex<HashMap<(String, Option<Mode>), AgentStore>>,
    sealer: Sealer,
    policy_hash: Mutex<Option<ContentHash>>,
    /// stdio without `--client`: the client each connection (one per
    /// process) named in its first request. It can't rename itself later.
    stdio_clients: Mutex<HashMap<u64, Option<ClientInfo>>>,
}

/// The MCP server for one vault. Cheap to clone.
#[derive(Clone)]
pub struct Server {
    pub(crate) core: Arc<Core>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Server")
    }
}

impl Server {
    /// Open the vault at `root` (stdio, 0.8's signature).
    pub fn open(root: &str, allow_write: bool) -> Result<Self, String> {
        Self::open_with(root, ServerConfig { allow_write, ..ServerConfig::default() })
    }

    pub fn open_with(root: &str, mut config: ServerConfig) -> Result<Self, String> {
        let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
        let watcher = VaultWatcher::start(&PathBuf::from(root).canonicalize().map_err(|e| e.to_string())?).ok();
        if config.state_dir.is_none() {
            config.state_dir = instructions::state_dir(&vault);
        }
        Ok(Self::with_config(vault, watcher, config))
    }

    /// A server over any vault (tests use an in-memory one).
    pub fn with_vault(vault: Vault, watcher: Option<VaultWatcher>, allow_write: bool) -> Self {
        Self::with_config(vault, watcher, ServerConfig { allow_write, ..ServerConfig::default() })
    }

    pub fn with_config(vault: Vault, watcher: Option<VaultWatcher>, config: ServerConfig) -> Self {
        let index = IndexService::spawn();
        index.load(vault.clone());
        // PDF pages are searchable too ("paper.pdf p. 3").
        quire_app::pdf_state::index_pdfs(&vault, &index);
        index.flush_semantic();
        let device = config.device.clone().unwrap_or_else(quire_app::device::this_device);
        let sealer = config.state_dir.as_deref().map_or_else(Sealer::ephemeral, |d| Sealer::load_or_create(&d.join("mrtr.key")));
        let core = Core {
            served: ServedCache::new(vault.clone()),
            vault,
            index,
            watcher: Mutex::new(watcher),
            registry: ToolRegistry::standard(),
            subs: Subscriptions::default(),
            config,
            device,
            stores: Mutex::new(HashMap::new()),
            sealer,
            policy_hash: Mutex::new(None),
            stdio_clients: Mutex::new(HashMap::new()),
        };
        let s = Self { core: Arc::new(core) };
        s.core.policy_changed();
        s
    }

    pub fn vault(&self) -> &Vault {
        &self.core.vault
    }

    /// The vault's index, kept fresh by [`Self::start_pump`] (the web app
    /// of `quire-cli serve` searches the same one).
    pub fn index(&self) -> &IndexService {
        &self.core.index
    }

    pub fn config(&self) -> &ServerConfig {
        &self.core.config
    }

    /// A new connection id (for subscriptions).
    pub fn new_conn(&self) -> u64 {
        self.core.subs.conn()
    }

    /// Watch the vault in the background: index edits made elsewhere and send
    /// change notifications. Stops when the last handle to the server goes.
    pub fn start_pump(&self) {
        let weak: Weak<Core> = Arc::downgrade(&self.core);
        let _ = std::thread::Builder::new().name("mcp-pump".into()).spawn(move || {
            let mut ticks = 0u32;
            while let Some(core) = weak.upgrade() {
                core.refresh();
                core.subs.flush(Instant::now());
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(12) && core.policy_changed() {
                    core.subs.tools_changed();
                }
                drop(core);
                std::thread::sleep(PUMP_EVERY);
            }
        });
    }

    /// Register a subscription the transport will keep open.
    pub fn listen(&self, conn: u64, id: Value, filter: &Filter, sink: Sink, sees: Visibility) -> u64 {
        self.core.subs.listen(conn, id, filter, sink, sees)
    }

    pub fn subscribe_legacy(&self, conn: u64, uri: &str, subscribe: bool, sink: Sink, sees: Visibility) {
        if subscribe {
            self.core.subs.subscribe_legacy(conn, uri, sink, sees);
        } else {
            self.core.subs.unsubscribe_legacy(conn, uri);
        }
    }

    pub fn cancel_listen(&self, conn: u64, id: &Value) -> bool {
        self.core.subs.cancel(conn, id)
    }

    /// The access a tool needs, by name.
    pub fn tool_access(&self, name: &str) -> Option<crate::tools::Access> {
        self.core.registry.get(name).map(|t| t.spec().access)
    }

    /// End a connection's subscriptions gracefully.
    pub fn close_conn(&self, conn: u64) {
        self.core.subs.close_conn(conn);
        self.core.stdio_clients.lock().unwrap_or_else(PoisonError::into_inner).remove(&conn);
    }

    pub fn remove_subscription(&self, handle: u64) {
        self.core.subs.remove(handle);
    }

    /// Handle one JSON-RPC message from `conn`.
    pub fn handle(&self, msg: &Value, conn: &Conn, cancel: &CancelFlag) -> Reply {
        self.core.handle(msg, conn, cancel)
    }

    /// The instructions agents see (0.8's sentence, then approved AGENTS.md).
    pub fn instructions(&self, grant: Grant) -> String {
        self.core.instructions(grant)
    }

    /// Serve newline-delimited JSON-RPC on stdio-like streams until the input
    /// closes (see [`crate::stdio`]).
    pub fn serve(&self, input: impl std::io::BufRead, output: impl std::io::Write + Send) -> std::io::Result<()> {
        crate::stdio::serve(self, input, output)
    }
}

/// The actor id for a client key Quire gave out (`--client KEY`).
///
/// `claude-code` acts as `mcp:claude-code`. A key with Quire's `acp:` or
/// `mcp:` prefix is used as is: Quire starts an external agent's MCP server
/// with `--client acp:<name>`, so the agent's tool calls and its file
/// requests are one actor. The CLI checks keys with this too.
pub fn client_actor_id(key: &str) -> Result<String, String> {
    let (prefix, bare) = match key.split_once(':') {
        Some((p @ ("acp" | "mcp"), b)) => (p, b),
        Some(_) => ("", ""),
        None => ("mcp", key),
    };
    if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("--client takes a short key like claude-code (or acp:claude-code), not “{key}”"));
    }
    Ok(format!("{prefix}:{bare}"))
}

fn client_info(v: Option<&Value>) -> Option<ClientInfo> {
    let c = v?;
    Some(ClientInfo {
        name: c.get("name")?.as_str()?.chars().take(80).collect(),
        version: c.get("version").and_then(Value::as_str).unwrap_or_default().chars().take(40).collect(),
        self_reported: true,
    })
}

fn rpc_error(id: Value, err: Value) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("jsonrpc".into(), "2.0".into());
    m.insert("id".into(), id);
    m.insert("error".into(), err);
    Value::Object(m)
}

fn rpc_result(id: Value, result: Value) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("jsonrpc".into(), "2.0".into());
    m.insert("id".into(), id);
    m.insert("result".into(), result);
    Value::Object(m)
}

impl Core {
    fn refresh(&self) {
        let guard = self.watcher.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(w) = guard.as_ref() else { return };
        let mut changed = false;
        let mut list_changed: Vec<String> = Vec::new();
        let now = Instant::now();
        let events: Vec<VaultEvent> = w.events.try_iter().collect();
        drop(guard);
        for ev in events {
            changed = true;
            match ev {
                VaultEvent::Changed(p) => {
                    if let Ok(src) = self.vault.read(&p) {
                        if self.index.read().get(&p).is_none() {
                            list_changed.push(resources::uri_for_path(&p));
                        }
                        self.index.upsert(p.clone(), src);
                    }
                    self.note_changed(&p, now);
                }
                VaultEvent::Removed(p) => {
                    self.index.remove(p.clone());
                    list_changed.push(resources::uri_for_path(&p));
                    self.note_changed(&p, now);
                }
                VaultEvent::AttachmentChanged(p) | VaultEvent::AttachmentRemoved(p) => {
                    quire_app::pdf_state::pdf_changed(&self.vault, &self.index, &p);
                    self.subs.changed(&resources::uri_for_path(&p), now);
                }
            }
        }
        if changed {
            self.index.flush();
        }
        if !list_changed.is_empty() {
            self.subs.resources_list_changed(&list_changed);
        }
    }

    fn note_changed(&self, p: &NotePath, now: Instant) {
        self.subs.changed(&resources::uri_for_path(p), now);
        let lower = p.as_str().to_lowercase();
        if lower == "wiki/index.md" || !lower.contains('/') {
            self.subs.changed("quire://vault/index", now);
        }
        if lower == "agents.md" || lower == "claude.md" {
            self.subs.changed("quire://vault/instructions", now);
        }
    }

    /// Whether the agent policy file changed since last asked.
    fn policy_changed(&self) -> bool {
        let text = self.vault.storage().read(&NotePath::new(quire_agent::policy::POLICY_PATH)).unwrap_or_default();
        let h = ContentHash::of(&text);
        let mut last = self.policy_hash.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = last.as_ref().is_some_and(|l| *l != h);
        *last = Some(h);
        changed
    }

    fn grant(&self, conn: &Conn) -> Grant {
        match &conn.principal {
            Principal::Stdio => Grant::stdio(self.config.allow_write && !self.config.read_only),
            Principal::Token(t) if self.config.read_only => Grant { write: false, network: false, ..t.scopes.grant() },
            Principal::Token(t) => t.scopes.grant(),
        }
    }

    fn cap(&self, conn: &Conn) -> Option<Mode> {
        match &conn.principal {
            Principal::Stdio => (!self.config.allow_write || self.config.read_only).then_some(Mode::Read),
            Principal::Token(_) if self.config.read_only => Some(Mode::Read),
            Principal::Token(t) => t.scopes.cap(),
        }
    }

    /// The actor for a request: the key Quire assigned (`--client`, a token)
    /// or, unverified, the name the client reported in its first request.
    fn actor(&self, conn: &Conn, params: &Value) -> Actor {
        let meta = params.get("_meta");
        let mut client =
            client_info(meta.and_then(|m| m.get(META_CLIENT_INFO).or_else(|| m.get("clientInfo")))).or_else(|| conn.client.clone());
        let mut a = match &conn.principal {
            Principal::Stdio => {
                if let Some(key) = &self.config.client_key {
                    let label = client.as_ref().map_or_else(|| key.clone(), |c| c.name.clone());
                    let id = client_actor_id(key).unwrap_or_else(|_| format!("mcp:{key}"));
                    Actor::agent(id, label, Via::McpStdio, &self.device.id)
                } else {
                    // Fixed at the first request, so a client can't get a fresh
                    // rate limit or another client's ledger by renaming itself.
                    let first = self
                        .stdio_clients
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entry(conn.id)
                        .or_insert_with(|| client.take())
                        .clone();
                    client = first;
                    let name = client.as_ref().map_or_else(|| "unknown".to_string(), |c| c.name.clone());
                    let key: String = name
                        .to_lowercase()
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
                        .collect();
                    // Self-reported: per-client policy overrides can't widen
                    // (design §A.2), and the id ends in `?`, so it is never a
                    // verified client's id.
                    Actor::unverified_agent(&format!("mcp:{key}"), name, Via::McpStdio, &self.device.id)
                }
            }
            Principal::Token(t) => {
                let label = client.as_ref().map_or_else(|| t.label.clone(), |c| c.name.clone());
                Actor::agent(t.actor_id.clone(), label, Via::McpHttp, &self.device.id)
            }
        };
        a.client = client;
        a.trace = meta.and_then(|m| m.get("traceparent")).and_then(Value::as_str).map(|s| s.chars().take(128).collect());
        a
    }

    /// What the caller of a subscription request may hear about.
    fn visibility(&self, conn: &Conn, params: &Value) -> Visibility {
        let actor = self.actor(conn, params);
        let store = self.store(&actor, self.cap(conn));
        Visibility(Arc::new(move |uri: &str| resources::uri_visible(&store, &actor, uri)))
    }

    fn store(&self, actor: &Actor, cap: Option<Mode>) -> AgentStore {
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        stores
            .entry((actor.id.clone(), cap))
            .or_insert_with(|| {
                let s = AgentStore::open(&self.vault, self.device.clone(), Arc::clone(&self.config.clock));
                // The approvals the server was given (the default is the same folder).
                if let Some(d) = &self.config.state_dir {
                    s.set_state_dir(Some(d.clone()));
                }
                if let Some(c) = cap {
                    s.set_cap(&actor.id, c);
                }
                s
            })
            .clone()
    }

    fn instructions(&self, grant: Grant) -> String {
        let n = self.index.read().len();
        let mode = if grant.write {
            "; you may write: directly in agent zones (wiki/), and elsewhere as suggestions the person reviews in Quire; every change is kept in version history"
        } else {
            "; read-only"
        };
        let mut s = format!("Tools for the user's Quire notes vault ({n} notes){mode}. Cite notes as [[Note]] when you use them.");
        if let Some((path, _, text)) = instructions::for_agents(&self.vault, self.config.state_dir.as_deref(), instructions::INLINE_LIMIT) {
            let _ = write!(s, "\n\nVault instructions ({path}):\n{text}");
        }
        s
    }

    fn capabilities() -> Value {
        json!({
            "tools": { "listChanged": true },
            "resources": { "subscribe": true, "listChanged": true },
            "prompts": { "listChanged": false },
        })
    }

    fn handle(&self, msg: &Value, conn: &Conn, cancel: &CancelFlag) -> Reply {
        let Some(obj) = msg.as_object() else {
            return Reply::Response(rpc_error(Value::Null, protocol::error(code::INVALID_REQUEST, "not a JSON-RPC message")));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response from the client: Quire never sends requests, so ignore it.
            return id.map_or(Reply::None, |id| Reply::Response(rpc_error(id, protocol::error(code::INVALID_REQUEST, "missing method"))));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        if method.starts_with("notifications/") {
            return Reply::None;
        }
        let Some(id) = id else { return Reply::None };
        if method == "initialize" {
            return self.initialize(id, &params, conn);
        }
        let era = match Self::era(method, &params, conn) {
            Ok(e) => e,
            Err(e) => return Reply::Response(rpc_error(id, e)),
        };
        match (method, &era) {
            ("subscriptions/listen", Era::Modern) => {
                return Reply::Listen { id, filter: Filter::from_params(&params), sees: self.visibility(conn, &params) };
            }
            ("resources/subscribe" | "resources/unsubscribe", Era::Legacy(_)) => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or_default().to_string();
                let sees = self.visibility(conn, &params);
                let subscribe = method == "resources/subscribe";
                // A note it may not read doesn't exist, for subscriptions too.
                if subscribe && !sees.sees(&uri) {
                    return Reply::Response(rpc_error(id, protocol::resource_not_found(&era, &uri)));
                }
                return Reply::Subscribe { response: json!({"jsonrpc": "2.0", "id": id, "result": {}}), uri, subscribe, sees };
            }
            _ => {}
        }
        self.refresh();
        let result = self.dispatch(&era, method, &params, conn, cancel);
        Reply::Response(match result {
            Ok(r) => rpc_result(id, if era.is_modern() { protocol::modern_result(r) } else { r }),
            Err(e) => rpc_error(id, e),
        })
    }

    /// The legacy handshake, as in 0.8: the client's version if supported,
    /// else the newest legacy one.
    fn initialize(&self, id: Value, params: &Value, conn: &Conn) -> Reply {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        let version = if LEGACY.contains(&asked) { asked } else { LEGACY[0] };
        let client = client_info(params.get("clientInfo"));
        let caps = params.get("capabilities").cloned().unwrap_or(Value::Null);
        let mut c = conn.clone();
        c.client.clone_from(&client);
        // Older clients get no unsolicited list notifications, so none are
        // promised; over HTTP they get no streams at all (design §4.6).
        let result = json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false }, "resources": { "subscribe": !conn.http, "listChanged": false }, "prompts": { "listChanged": false } },
            "serverInfo": { "name": "quire", "version": env!("CARGO_PKG_VERSION") },
            "instructions": self.instructions(self.grant(&c)),
        });
        Reply::Initialized { response: rpc_result(id, result), version: version.to_string(), client, caps }
    }

    /// Which era answers this request (design §4.1).
    fn era(method: &str, params: &Value, conn: &Conn) -> Result<Era, Value> {
        if let Some(v) = protocol::requested_version(params) {
            return protocol::era_for(v);
        }
        if conn.http {
            // HTTP is per request: the header names a legacy version, or none (2025-03-26).
            return match conn.http_version.as_deref() {
                Some(MODERN) => Err(protocol::missing_version()),
                Some(v) => protocol::era_for(v),
                None => Ok(Era::Legacy("2025-03-26".into())),
            };
        }
        if let Some(v) = &conn.legacy {
            return Ok(Era::Legacy(v.clone()));
        }
        // A dual-era client may probe with `server/discover` before choosing.
        if method == "server/discover" {
            return Ok(Era::Modern);
        }
        Err(protocol::missing_version())
    }

    fn dispatch(&self, era: &Era, method: &str, params: &Value, conn: &Conn, cancel: &CancelFlag) -> Result<Value, Value> {
        let lists = |mut v: Value, ttl: u64| {
            if era.is_modern() {
                v["ttlMs"] = ttl.into();
                v["cacheScope"] = "private".into();
            }
            v
        };
        match (method, era) {
            ("server/discover", _) => Ok(self.discover(conn)),
            // Removed in 2026-07-28, kept for older clients.
            ("ping" | "logging/setLevel", Era::Legacy(_)) => Ok(json!({})),
            ("tools/list", _) => Ok(lists(self.tools_list(era, conn), TOOLS_TTL_MS)),
            ("tools/call", _) => Ok(self.tools_call(era, params, conn, cancel)),
            ("resources/list", _) => {
                let actor = self.actor(conn, params);
                let store = self.store(&actor, self.cap(conn));
                let cx = self.read_cx(&store, &actor);
                resources::list(&cx, params).map(|v| lists(v, resources::RESOURCES_TTL_MS)).map_err(|e| self.read_error(era, "", e))
            }
            ("resources/templates/list", _) => Ok(lists(json!({ "resourceTemplates": resources::templates() }), TOOLS_TTL_MS)),
            ("resources/read", _) => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or_default();
                let actor = self.actor(conn, params);
                let store = self.store(&actor, self.cap(conn));
                let cx = self.read_cx(&store, &actor);
                let out = resources::read(&cx, uri).map_err(|e| self.read_error(era, uri, e))?;
                if let Some(p) = out["contents"][0]["_meta"]["path"].as_str().filter(|_| uri.starts_with("quire://note/")) {
                    store.record_read(&actor, &NotePath::new(p));
                }
                Ok(lists(out, resources::RESOURCES_TTL_MS))
            }
            ("prompts/list", _) => Ok(lists(resources::prompts_list(&self.config.prompts), TOOLS_TTL_MS)),
            ("prompts/get", _) => resources::prompts_get(&self.config.prompts, params).map_err(|e| self.read_error(era, "", e)),
            _ => Err(json!({ "code": code::METHOD_NOT_FOUND, "message": format!("Method not found: {method}") })),
        }
    }

    fn read_cx<'a>(&'a self, store: &'a AgentStore, actor: &'a Actor) -> ReadCx<'a> {
        ReadCx {
            vault: &self.vault,
            index: &self.index,
            agent: store,
            actor,
            served: &self.served,
            state_dir: self.config.state_dir.as_deref(),
        }
    }

    fn read_error(&self, era: &Era, uri: &str, e: ReadError) -> Value {
        let _ = self;
        match e {
            ReadError::NotFound if uri.is_empty() => protocol::error(code::INVALID_PARAMS, "Not found"),
            ReadError::NotFound => protocol::resource_not_found(era, uri),
            ReadError::BadRequest(m) => protocol::error(code::INVALID_PARAMS, m),
        }
    }

    fn discover(&self, conn: &Conn) -> Value {
        json!({
            "supportedVersions": SUPPORTED,
            "capabilities": Self::capabilities(),
            "serverInfo": protocol::server_info(),
            "instructions": self.instructions(self.grant(conn)),
            "ttlMs": DISCOVER_TTL_MS,
            "cacheScope": "private",
        })
    }

    fn tools_list(&self, era: &Era, conn: &Conn) -> Value {
        let tools: Vec<Value> = self
            .registry
            .offered_for(self.grant(conn))
            .map(|t| {
                let s = t.spec();
                let mut v = json!({ "name": s.name, "description": s.description, "inputSchema": s.input_schema });
                if era.structured() {
                    v["title"] = s.title.into();
                    if let Some(o) = &s.output_schema {
                        v["outputSchema"] = o.clone();
                    }
                }
                if era.annotations() {
                    v["annotations"] = s.annotations.to_json();
                }
                v
            })
            .collect();
        json!({ "tools": tools })
    }

    fn approve_in_client(&self, era: &Era, store: &AgentStore, actor: &Actor, params: &Value, conn: &Conn) -> bool {
        if !era.is_modern() || actor.unverified {
            return false;
        }
        let caps = params.get("_meta").and_then(|m| m.get(META_CLIENT_CAPS)).unwrap_or(&conn.client_caps);
        if caps.get("elicitation").is_none() {
            return false;
        }
        // In-client approval widens what a client may do, so a policy file
        // only turns it on once the person approved that file here (P7).
        let text = self.vault.storage().read(&NotePath::new(quire_agent::policy::POLICY_PATH)).unwrap_or_default();
        instructions::approved_value(self.config.state_dir.as_deref(), "approved_policy", &ContentHash::of(&text))
            && store.policy().clients.get(&actor.id).is_some_and(|c| c.approve_in_client)
    }

    fn tools_call(&self, era: &Era, params: &Value, conn: &Conn, cancel: &CancelFlag) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let args = params.get("arguments").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        let actor = self.actor(conn, params);
        let cap = self.cap(conn);
        let store = self.store(&actor, cap);
        let grant = self.grant(conn);
        let cx = ToolCx { vault: &self.vault, index: &self.index, agent: &store, actor: &actor, cancel, allow_write: grant.write };
        let ext = CallExt {
            served: &self.served,
            grant,
            approve_in_client: self.approve_in_client(era, &store, &actor, params, conn),
            input_responses: params.get("inputResponses"),
            request_state: params.get("requestState").and_then(Value::as_str),
            sealer: Some(&self.sealer),
            chat: self.config.chat.clone(),
            allow_private_urls: self.config.allow_private_urls,
            method: "tools/call",
            state_dir: self.config.state_dir.as_deref(),
        };
        let out = match self.registry.get(name) {
            Some(t) if grant.allows(t.spec().access) => {
                let out = t.call_with(&cx, &ext, &args);
                if matches!(name, "view" | "read_version")
                    && let ToolOutput::Ok { structured, .. } = &out
                    && let Some(p) = structured.get("path").and_then(Value::as_str).or_else(|| args.get("path").and_then(Value::as_str))
                {
                    store.record_read(&actor, &NotePath::new(p));
                }
                out
            }
            Some(_) if !grant.write => ToolOutput::error("denied_by_policy", crate::tools::legacy::READ_ONLY),
            Some(_) => ToolOutput::error("denied_by_policy", format!("Your access doesn't include {name}.")),
            None => ToolOutput::error("not_found", format!("Unknown tool {name}")),
        };
        tool_result(era, out)
    }
}

/// A tool's output as an MCP result. Legacy clients get 0.8's shape (text
/// content and `isError`) plus what their version defines; modern ones get
/// structured content and links.
pub fn tool_result(era: &Era, out: ToolOutput) -> Value {
    let structured_ok = era.structured();
    let mut v = match out {
        ToolOutput::Ok { text, structured, links } => {
            let mut content = vec![json!({ "type": "text", "text": text })];
            let mut v = json!({ "isError": false });
            if structured_ok {
                content.extend(
                    links.iter().map(|l| json!({ "type": "resource_link", "uri": l.uri, "name": l.name, "mimeType": l.mime_type })),
                );
                if !structured.is_null() {
                    v["structuredContent"] = structured;
                }
            }
            v["content"] = Value::Array(content);
            v
        }
        ToolOutput::Err { code, message, structured } => {
            let mut v = json!({ "content": [{ "type": "text", "text": message }], "isError": true });
            if structured_ok {
                let mut s = if structured.is_object() { structured } else { json!({}) };
                s["error"] = code.into();
                v["structuredContent"] = s;
            }
            v
        }
        // Only offered to modern clients (MRTR); kept safe for others.
        ToolOutput::InputRequired { requests, state } => {
            if !era.is_modern() {
                return json!({ "content": [{ "type": "text", "text": "This change needs approval in Quire." }], "isError": true });
            }
            json!({ "resultType": "input_required", "inputRequests": requests, "requestState": state })
        }
    };
    if era.is_modern() {
        v = protocol::modern_result(v);
    }
    v
}

/// The device-local state folder for a vault on disk, if any.
pub fn state_dir_for(root: &Path) -> Option<PathBuf> {
    Some(quire_app::settings::config_dir()?.join("vaults").join(instructions::vault_key(root)))
}

#[cfg(test)]
mod tests;
