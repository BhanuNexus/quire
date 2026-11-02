// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The MCP server over stdio (`quire-cli mcp <vault>`), moved from the CLI.
//!
//! Lets AI assistants (Claude Desktop, Claude Code, Cursor…) search, read and
//! ask questions of one vault. Nothing runs unless the user adds this command
//! to their client. Writing is off unless started with `--allow-write`, and
//! every write is snapshotted in version history first so it can be undone.
//!
//! Clients that `initialize` (protocol 2024-11-05 to 2025-11-25) get the
//! answers Quire 0.8 gave. A 2026-07-28 client names the version in each
//! request's `_meta` and may begin with `server/discover`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use quire_agent::{Actor, AgentStore, ClientInfo, Mode, SystemClock, Via};
use quire_app::index::IndexService;
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::{Vault, VaultEvent, VaultWatcher};
use serde_json::{Value, json};

use crate::protocol::{self, Era, LEGACY, MODERN, SUPPORTED, code};
use crate::tools::{ToolCx, ToolOutput, ToolRegistry};

/// How long a client may cache lists (design §4.1).
const TOOLS_TTL_MS: u64 = 300_000;
const DISCOVER_TTL_MS: u64 = 60_000;

#[derive(Default)]
struct Session {
    /// Set by `initialize`: this process answers in that legacy version.
    legacy: Option<String>,
    client: Option<ClientInfo>,
}

pub struct Server {
    vault: Vault,
    index: IndexService,
    watcher: Option<VaultWatcher>,
    agent: AgentStore,
    registry: ToolRegistry,
    allow_write: bool,
    session: Mutex<Session>,
}

impl Server {
    pub fn open(root: &str, allow_write: bool) -> Result<Self, String> {
        let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
        let watcher = VaultWatcher::start(&PathBuf::from(root).canonicalize().map_err(|e| e.to_string())?).ok();
        Ok(Self::with_vault(vault, watcher, allow_write))
    }

    /// A server over any vault (tests use an in-memory one).
    pub fn with_vault(vault: Vault, watcher: Option<VaultWatcher>, allow_write: bool) -> Self {
        let index = IndexService::spawn();
        index.load(vault.clone());
        index.flush_semantic();
        let agent = AgentStore::open(&vault, quire_app::device::this_device(), std::sync::Arc::new(SystemClock));
        Self { vault, index, watcher, agent, registry: ToolRegistry::standard(), allow_write, session: Mutex::default() }
    }

    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Who is calling: `mcp:<client name>` as the client reports it
    /// (unverified until WS-B's `--client` keys).
    fn actor(&self, modern_client: Option<ClientInfo>) -> Actor {
        let client = modern_client.or_else(|| self.session().client.clone());
        let name = client.as_ref().map_or_else(|| "unknown".to_string(), |c| c.name.clone());
        let mut a = Actor::agent(format!("mcp:{name}"), name, Via::McpStdio, &self.agent.device().id);
        a.client = client;
        if !self.allow_write {
            self.agent.set_cap(&a.id, Mode::Read);
        }
        a
    }

    /// Pick up edits made in the app or elsewhere since the last call.
    fn refresh(&self) {
        let Some(w) = &self.watcher else { return };
        let mut changed = false;
        for ev in w.events.try_iter() {
            changed = true;
            match ev {
                VaultEvent::Changed(p) => {
                    if let Ok(src) = self.vault.read(&p) {
                        self.index.upsert(p, src);
                    }
                }
                VaultEvent::Removed(p) => self.index.remove(p),
                VaultEvent::AttachmentChanged(_) | VaultEvent::AttachmentRemoved(_) => {}
            }
        }
        if changed {
            self.index.flush();
        }
    }

    fn instructions(&self) -> String {
        format!(
            "Tools for the user's Quire notes vault ({} notes){}. Cite notes as [[Note]] when you use them.",
            self.index.read().len(),
            if self.allow_write { "; edits are allowed and every edit is saved in version history" } else { "; read-only" }
        )
    }

    fn tools_list(&self, era: &Era) -> Value {
        let tools: Vec<Value> = self
            .registry
            .offered(self.allow_write)
            .map(|t| {
                let s = t.spec();
                let mut v = json!({ "name": s.name, "description": s.description, "inputSchema": s.input_schema });
                if era.is_modern() {
                    v["title"] = s.title.into();
                    v["annotations"] = s.annotations.to_json();
                    if let Some(o) = &s.output_schema {
                        v["outputSchema"] = o.clone();
                    }
                }
                v
            })
            .collect();
        if era.is_modern() {
            protocol::modern_result(json!({ "tools": tools, "ttlMs": TOOLS_TTL_MS, "cacheScope": "private" }))
        } else {
            json!({ "tools": tools })
        }
    }

    fn tools_call(&self, era: &Era, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let client = params.get("_meta").and_then(|m| m.get("clientInfo")).and_then(|c| {
            Some(ClientInfo {
                name: c.get("name")?.as_str()?.to_string(),
                version: c.get("version").and_then(Value::as_str).unwrap_or_default().into(),
                self_reported: true,
            })
        });
        let actor = self.actor(client);
        let cancel = CancelFlag::new();
        let cx = ToolCx {
            vault: &self.vault,
            index: &self.index,
            agent: &self.agent,
            actor: &actor,
            cancel: &cancel,
            allow_write: self.allow_write,
        };
        let out = match self.registry.get(name) {
            Some(t) => t.call(&cx, &args),
            None => ToolOutput::error("not_found", format!("Unknown tool {name}")),
        };
        tool_result(era, out)
    }

    fn discover(&self) -> Value {
        protocol::modern_result(json!({
            "supportedVersions": SUPPORTED,
            // Only what is implemented is advertised; resources, prompts and
            // subscriptions join with WS-B.
            "capabilities": { "tools": { "listChanged": false } },
            "instructions": self.instructions(),
            "ttlMs": DISCOVER_TTL_MS,
            "cacheScope": "private",
        }))
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if method.starts_with("notifications/") {
            return None;
        }
        let result = if method == "initialize" {
            Ok(self.initialize(&params))
        } else {
            self.era(method, &params).and_then(|era| self.dispatch(&era, method, &params))
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    /// The legacy handshake, as in 0.8: the client's version if supported,
    /// else the newest legacy one.
    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        let version = if LEGACY.contains(&asked) { asked } else { LEGACY[0] };
        let client = params.get("clientInfo").and_then(|c| {
            Some(ClientInfo {
                name: c.get("name")?.as_str()?.to_string(),
                version: c.get("version").and_then(Value::as_str).unwrap_or_default().into(),
                self_reported: true,
            })
        });
        {
            let mut s = self.session();
            s.legacy = Some(version.to_string());
            s.client = client;
        }
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "quire", "version": env!("CARGO_PKG_VERSION") },
            "instructions": self.instructions(),
        })
    }

    /// Which era answers this request (design §4.1).
    fn era(&self, method: &str, params: &Value) -> Result<Era, Value> {
        if let Some(v) = protocol::requested_version(params) {
            return if v == MODERN {
                Ok(Era::Modern)
            } else if LEGACY.contains(&v) {
                Ok(Era::Legacy(v.to_string()))
            } else {
                Err(protocol::unsupported_version(v))
            };
        }
        let legacy = self.session().legacy.clone();
        if let Some(v) = legacy {
            return Ok(Era::Legacy(v));
        }
        // A dual-era client may probe with `server/discover` before choosing.
        if method == "server/discover" {
            return Ok(Era::Modern);
        }
        Err(protocol::missing_version())
    }

    fn dispatch(&self, era: &Era, method: &str, params: &Value) -> Result<Value, Value> {
        match (method, era) {
            ("server/discover", _) => Ok(self.discover()),
            // Removed in 2026-07-28, kept for older clients.
            ("ping", Era::Legacy(_)) => Ok(json!({})),
            ("tools/list", _) => {
                self.refresh();
                Ok(self.tools_list(era))
            }
            ("tools/call", _) => {
                self.refresh();
                Ok(self.tools_call(era, params))
            }
            _ => Err(json!({ "code": code::METHOD_NOT_FOUND, "message": format!("Method not found: {method}") })),
        }
    }

    /// Serve newline-delimited JSON-RPC until the input closes.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code::PARSE_ERROR, "message": e.to_string() } })),
            };
            if let Some(r) = reply {
                writeln!(output, "{r}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

/// A tool's output as an MCP result. Legacy clients get 0.8's shape (text
/// content and `isError`); modern ones also get structured content and links.
fn tool_result(era: &Era, out: ToolOutput) -> Value {
    let modern = era.is_modern();
    let mut v = match out {
        ToolOutput::Ok { text, structured, links } => {
            let mut content = vec![json!({ "type": "text", "text": text })];
            let mut v = json!({ "isError": false });
            if modern {
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
            if modern {
                let mut s = if structured.is_object() { structured } else { json!({}) };
                s["error"] = code.into();
                v["structuredContent"] = s;
            }
            v
        }
        // Legacy clients can't be asked; WS-B turns this into a proposal for them.
        ToolOutput::InputRequired { requests, state } => {
            if !modern {
                return json!({ "content": [{ "type": "text", "text": "This change needs approval in Quire." }], "isError": true });
            }
            json!({ "resultType": "input_required", "inputRequests": requests, "requestState": state })
        }
    };
    if modern {
        v = protocol::modern_result(v);
    }
    v
}

#[cfg(test)]
mod tests;
