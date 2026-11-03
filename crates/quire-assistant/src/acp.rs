// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! An external agent over the Agent Client Protocol (design §D.6), with
//! Quire as the client: JSON-RPC 2.0 over the agent's stdio, one line per
//! message, protocol version 1.
//!
//! Strictly opt-in, behind a warning: the agent is a program with the
//! person's permissions. Quire controls only what the agent asks of Quire:
//!
//! * `fs/read_text_file` is answered for notes the vault's policy lets
//!   agents read (the store's path guard: deny zones, hidden folders and
//!   links out of the vault or into a deny zone are refused), with the
//!   agent's own pending suggestion applied, so it sees what it wrote.
//!   Untrusted notes (raw/, imported, `trust: untrusted`) come in an
//!   `<untrusted-source>` data block, as over MCP; writing such a block back
//!   unchanged around new text writes the text;
//! * `fs/write_text_file` becomes `AgentStore::submit` as `acp:<name>`, so in
//!   the person's notes it is a suggestion for review;
//! * Quire's MCP server is offered to the agent as `quire-cli mcp <vault>
//!   --allow-write --client acp:<name>`, so its tool calls are the same actor;
//! * `session/request_permission` asks the person; `execute`, `fetch` and
//!   `delete` default to Deny, and "Allow always" is offered only for
//!   `read`, `search` and `think`;
//! * Stop sends `session/cancel`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quire_agent::{Actor, AgentStore, Change, ChangeSet, ContentHash, OpKind, SubmitOutcome, Trust, Via};
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::NotePath;
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u64 = 1;

/// ACP permission kinds that default to Deny and never offer "Allow always".
pub const RISKY_KINDS: &[&str] = &["execute", "fetch", "delete"];
/// The only kinds "Allow always" is offered for.
pub const ALWAYS_OK_KINDS: &[&str] = &["read", "search", "think"];

/// The warning the person accepts before an external agent is connected.
pub const WARNING: &str = "This agent is a program on your computer and runs with your permissions. Quire reviews the edits it makes through Quire, but cannot stop it from using its own tools (shell commands, other files, the network).";

/// Whether "Allow always" may be offered for a permission kind.
pub fn may_allow_always(kind: &str) -> bool {
    ALWAYS_OK_KINDS.contains(&kind)
}

/// Whether a kind starts on Deny in the permission dialog.
pub fn denied_by_default(kind: &str) -> bool {
    RISKY_KINDS.contains(&kind) || !ALWAYS_OK_KINDS.contains(&kind) && kind != "edit"
}

/// A configured external agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcpAgent {
    /// `claude-code-acp`, `codex-acp`, or a custom command.
    pub command: String,
    pub args: Vec<String>,
    /// Shown in the panel's amber chip: "External: Claude Code".
    pub label: String,
    /// Extra environment for the agent's process.
    pub env: Vec<(String, String)>,
}

impl AcpAgent {
    /// `acp:<name>`: the actor its writes are recorded as, and the
    /// `--client` key of the MCP server Quire offers it (letters, digits,
    /// `-`, `_` and `.` only).
    pub fn actor_id(&self) -> String {
        let base = Path::new(&self.command).file_stem().map_or_else(|| "agent".into(), |s| s.to_string_lossy().to_lowercase());
        let base: String = base
            .trim_end_matches("-acp")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
            .collect();
        format!("acp:{}", if base.is_empty() { "agent" } else { &base })
    }

    pub fn actor(&self, device: &str) -> Actor {
        let label = if self.label.is_empty() { self.command.clone() } else { self.label.clone() };
        Actor::agent(self.actor_id(), label, Via::Acp, device)
    }
}

/// The presets offered in Settings (label, command).
pub const PRESETS: &[(&str, &str)] = &[("Claude Code", "claude-code-acp"), ("Codex", "codex-acp")];

/// One option of a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// What the agent asks permission for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub title: String,
    /// `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `other`.
    pub kind: String,
    /// The tool call's content as text (a command, a diff, a URL).
    pub detail: String,
    pub options: Vec<PermissionOption>,
}

impl PermissionRequest {
    /// The options Quire shows: never "always" for kinds outside
    /// [`ALWAYS_OK_KINDS`].
    pub fn offered(&self) -> Vec<&PermissionOption> {
        self.options
            .iter()
            .filter(|o| !o.kind.ends_with("_always") || may_allow_always(&self.kind) || o.kind.starts_with("reject"))
            .collect()
    }

    /// The option chosen when the person just presses Enter (or doesn't answer).
    pub fn default_option(&self) -> Option<&PermissionOption> {
        let deny =
            self.options.iter().find(|o| o.kind == "reject_once").or_else(|| self.options.iter().find(|o| o.kind.starts_with("reject")));
        if denied_by_default(&self.kind) { deny } else { self.options.iter().find(|o| o.kind == "allow_once").or(deny) }
    }
}

/// What the person answered (or `None`: cancelled / no answer).
pub type PermissionAnswer = Option<String>;

/// What the panel shows of an external agent's turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpEvent {
    Text(String),
    Thought(String),
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
    },
    ToolUpdate {
        id: String,
        status: String,
        detail: String,
    },
    Plan(Vec<(String, String)>),
    /// A file write became a suggestion (or was written, in an agent zone).
    Write {
        path: String,
        proposal: Option<String>,
    },
    /// A read or write was refused by the vault's policy.
    Refused {
        path: String,
        why: String,
    },
    Permission {
        title: String,
        kind: String,
        allowed: bool,
    },
}

/// What Quire does for the agent: files and permission questions.
pub trait AcpHost {
    fn read_text_file(&mut self, path: &str, line: Option<u64>, limit: Option<u64>) -> Result<String, String>;
    fn write_text_file(&mut self, path: &str, content: &str) -> Result<(), String>;
    /// Ask the person; returns the chosen option id.
    fn permission(&mut self, req: &PermissionRequest, cancel: &CancelFlag) -> PermissionAnswer;
    fn event(&mut self, ev: AcpEvent);
}

/// The host that applies the vault's policy (files in the vault only,
/// writes through `submit`). Permission questions go to `ask`.
pub struct VaultHost<'a> {
    pub agent: &'a AgentStore,
    pub actor: Actor,
    pub root: PathBuf,
    pub ask: &'a mut dyn FnMut(&PermissionRequest, &CancelFlag) -> PermissionAnswer,
    pub events: &'a mut dyn FnMut(AcpEvent),
}

impl VaultHost<'_> {
    /// The vault path for an absolute (or vault-relative) path, if inside.
    fn note_path(&self, path: &str) -> Result<NotePath, String> {
        let p = Path::new(path);
        let rel = if p.is_absolute() {
            let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
            let full = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            let full = if full.exists() {
                full
            } else {
                p.parent()
                    .and_then(|d| d.canonicalize().ok())
                    .map_or_else(|| p.to_path_buf(), |d| d.join(p.file_name().unwrap_or_default()))
            };
            full.strip_prefix(&root).map(Path::to_path_buf).map_err(|_| "Quire only shares files in this vault".to_string())?
        } else {
            p.to_path_buf()
        };
        let s = rel.to_string_lossy().replace('\\', "/");
        let np = quire_agent::canonical_path(&NotePath::from(s.as_str())).map_err(|e| e.to_string())?;
        Ok(np)
    }
}

impl AcpHost for VaultHost<'_> {
    fn read_text_file(&mut self, path: &str, line: Option<u64>, limit: Option<u64>) -> Result<String, String> {
        let np = match self.note_path(path) {
            Ok(p) => p,
            Err(why) => {
                (self.events)(AcpEvent::Refused { path: path.into(), why: why.clone() });
                return Err(why);
            }
        };
        let np = match self.agent.guard(&self.actor, &np, OpKind::Read) {
            Ok(p) => p,
            Err(e) => {
                let why = match e {
                    quire_agent::WriteError::PermissionDenied { .. } => format!("{np} is private: the vault's agent policy hides it"),
                    other => other.to_string(),
                };
                (self.events)(AcpEvent::Refused { path: np.to_string(), why: why.clone() });
                return Err(why);
            }
        };
        let text = match self.agent.pending_text(&self.actor, &np) {
            Some((t, _)) => t,
            None => self.agent.vault().read(&np).map_err(|e| e.to_string())?,
        };
        self.agent.record_read(&self.actor, &np);
        let start = line.map_or(0, |l| l.saturating_sub(1) as usize);
        let part: String = match (line, limit) {
            (None, None) => text.clone(),
            _ => text.split_inclusive('\n').skip(start).take(limit.map_or(usize::MAX, |l| l as usize)).collect(),
        };
        // Untrusted text reaches the agent as data (P8), as over MCP.
        if self.agent.trust_of(&np, Some(&text)) == Trust::Untrusted {
            let origin =
                quire_app::markdown::frontmatter::parse(&text).and_then(|fm| fm.get("source").and_then(|v| v.as_str().map(str::to_string)));
            return Ok(quire_agent::fence::fence_untrusted(np.as_str(), origin.as_deref(), &part));
        }
        Ok(part)
    }

    fn write_text_file(&mut self, path: &str, content: &str) -> Result<(), String> {
        let np = match self.note_path(path) {
            Ok(p) => p,
            Err(why) => {
                (self.events)(AcpEvent::Refused { path: path.into(), why: why.clone() });
                return Err(why);
            }
        };
        if !np.is_markdown() {
            let why = "Quire takes changes to notes (.md files) only".to_string();
            (self.events)(AcpEvent::Refused { path: np.to_string(), why: why.clone() });
            return Err(why);
        }
        // The data block `read_text_file` wrapped it in, written back: the text inside.
        let unfenced = quire_agent::fence::unfence(np.as_str(), content);
        let content = unfenced.as_deref().unwrap_or(content);
        let vault = self.agent.vault();
        let (change, expected) = match vault.read(&np) {
            Ok(cur) => (Change::Edit { path: np.clone(), new_text: content.to_string() }, vec![(np.clone(), ContentHash::of(&cur))]),
            Err(_) => (Change::Create { path: np.clone(), text: content.to_string() }, Vec::new()),
        };
        let cs = ChangeSet {
            changes: vec![change],
            title: format!("{} edits {}", self.actor.label, np.stem()),
            expected,
            ..ChangeSet::default()
        };
        match self.agent.submit(&self.actor, cs) {
            Ok(SubmitOutcome::Applied { .. }) => {
                (self.events)(AcpEvent::Write { path: np.to_string(), proposal: None });
                Ok(())
            }
            Ok(SubmitOutcome::Proposed { id, .. }) => {
                (self.events)(AcpEvent::Write { path: np.to_string(), proposal: Some(id.to_string()) });
                Ok(())
            }
            Err(e) => {
                (self.events)(AcpEvent::Refused { path: np.to_string(), why: e.to_string() });
                Err(e.to_string())
            }
        }
    }

    fn permission(&mut self, req: &PermissionRequest, cancel: &CancelFlag) -> PermissionAnswer {
        let a = (self.ask)(req, cancel);
        let allowed = a.as_ref().and_then(|id| req.options.iter().find(|o| &o.id == id)).is_some_and(|o| o.kind.starts_with("allow"));
        (self.events)(AcpEvent::Permission { title: req.title.clone(), kind: req.kind.clone(), allowed });
        a
    }

    fn event(&mut self, ev: AcpEvent) {
        (self.events)(ev);
    }
}

// ------------------------------------------------------------------ the connection

type Writer = Box<dyn Write + Send>;

/// A connection to an external agent.
pub struct AcpClient {
    writer: Arc<Mutex<Writer>>,
    incoming: Receiver<Value>,
    next_id: u64,
    child: Option<Child>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    session: Option<String>,
    pub agent_info: Value,
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// How long to wait for `initialize` and `session/new`.
const SETUP_TIMEOUT: Duration = Duration::from_mins(1);

impl AcpClient {
    /// Start the agent in `cwd` (the vault folder).
    pub fn spawn(agent: &AcpAgent, cwd: &Path) -> Result<Self, String> {
        let mut child = Command::new(&agent.command)
            .args(&agent.args)
            .envs(agent.env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("couldn't start {}: {e}", agent.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        if let Some(err) = child.stderr.take() {
            let tail = Arc::clone(&tail);
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines() {
                    let Ok(line) = line else { break };
                    let mut t = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    t.push_back(line.chars().take(300).collect());
                    while t.len() > 5 {
                        t.pop_front();
                    }
                }
            });
        }
        let mut c = Self::with_io(stdout, stdin);
        c.child = Some(child);
        c.stderr = tail;
        Ok(c)
    }

    /// Over any pair of streams (tests use pipes).
    pub fn with_io(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                // Agents sometimes log to stdout; only JSON-RPC lines count.
                if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                    && v.get("jsonrpc").is_some()
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            incoming: rx,
            next_id: 1,
            child: None,
            stderr: Arc::default(),
            session: None,
            agent_info: Value::Null,
        }
    }

    fn send(&self, v: &Value) -> Result<(), String> {
        let mut w = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        writeln!(w, "{v}").and_then(|()| w.flush()).map_err(|e| format!("the agent stopped: {e}"))
    }

    fn ended(&self) -> String {
        let tail = self.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect::<Vec<_>>().join(" ");
        if tail.is_empty() { "the agent stopped".into() } else { format!("the agent stopped: {tail}") }
    }

    /// Send a request and wait for its response, serving the agent's own
    /// requests and notifications meanwhile.
    fn request(
        &mut self,
        method: &str,
        params: &Value,
        host: &mut dyn AcpHost,
        cancel: &CancelFlag,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let started = Instant::now();
        let mut cancel_sent = false;
        loop {
            if cancel.is_cancelled() && !cancel_sent {
                if let Some(s) = &self.session {
                    let _ = self.send(&json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": s } }));
                }
                cancel_sent = true;
            }
            if timeout.is_some_and(|t| started.elapsed() > t) {
                return Err(format!("the agent didn't answer {method} in time"));
            }
            let msg = match self.incoming.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(self.ended()),
            };
            let is_response = msg.get("method").is_none();
            if is_response {
                if msg["id"].as_u64() == Some(id) {
                    if let Some(e) = msg.get("error") {
                        return Err(format!("the agent said: {}", e["message"].as_str().unwrap_or("an error")));
                    }
                    return Ok(msg["result"].clone());
                }
                continue;
            }
            self.serve(&msg, host, cancel)?;
        }
    }

    /// Answer one request from the agent, or take in a notification.
    fn serve(&self, msg: &Value, host: &mut dyn AcpHost, cancel: &CancelFlag) -> Result<(), String> {
        let method = msg["method"].as_str().unwrap_or_default();
        let p = &msg["params"];
        let id = msg.get("id").cloned();
        let reply = |r: Result<Value, String>| -> Result<(), String> {
            let Some(id) = id.clone() else { return Ok(()) };
            let m = match r {
                Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
            };
            self.send(&m)
        };
        match method {
            "session/update" => {
                let u = &p["update"];
                let text = |v: &Value| v["content"]["text"].as_str().map(str::to_string).unwrap_or_default();
                match u["sessionUpdate"].as_str().unwrap_or_default() {
                    "agent_message_chunk" => host.event(AcpEvent::Text(text(u))),
                    "agent_thought_chunk" => host.event(AcpEvent::Thought(text(u))),
                    "tool_call" => host.event(AcpEvent::ToolCall {
                        id: u["toolCallId"].as_str().unwrap_or_default().into(),
                        title: u["title"].as_str().unwrap_or_default().into(),
                        kind: u["kind"].as_str().unwrap_or("other").into(),
                        status: u["status"].as_str().unwrap_or("pending").into(),
                    }),
                    "tool_call_update" => host.event(AcpEvent::ToolUpdate {
                        id: u["toolCallId"].as_str().unwrap_or_default().into(),
                        status: u["status"].as_str().unwrap_or_default().into(),
                        detail: content_text(&u["content"]),
                    }),
                    "plan" => host.event(AcpEvent::Plan(
                        u["entries"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|e| {
                                (
                                    e["content"].as_str().unwrap_or_default().to_string(),
                                    e["status"].as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect(),
                    )),
                    _ => {}
                }
                Ok(())
            }
            "fs/read_text_file" => reply(
                host.read_text_file(p["path"].as_str().unwrap_or_default(), p["line"].as_u64(), p["limit"].as_u64())
                    .map(|c| json!({ "content": c })),
            ),
            "fs/write_text_file" => reply(
                host.write_text_file(p["path"].as_str().unwrap_or_default(), p["content"].as_str().unwrap_or_default())
                    .map(|()| Value::Null),
            ),
            "session/request_permission" => {
                let tc = &p["toolCall"];
                let req = PermissionRequest {
                    title: tc["title"].as_str().unwrap_or("The agent asks for permission").into(),
                    kind: tc["kind"].as_str().unwrap_or("other").into(),
                    detail: content_text(&tc["content"]).chars().take(4000).collect::<String>() + &raw_input(&tc["rawInput"]),
                    options: p["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|o| PermissionOption {
                            id: o["optionId"].as_str().unwrap_or_default().into(),
                            name: o["name"].as_str().unwrap_or_default().into(),
                            kind: o["kind"].as_str().unwrap_or_default().into(),
                        })
                        .collect(),
                };
                let answer = if cancel.is_cancelled() { None } else { host.permission(&req, cancel) };
                // An "always" option Quire didn't offer can't be chosen.
                let answer = answer.filter(|a| req.offered().iter().any(|o| &o.id == a));
                reply(Ok(match answer {
                    Some(a) => json!({ "outcome": { "outcome": "selected", "optionId": a } }),
                    None => json!({ "outcome": { "outcome": "cancelled" } }),
                }))
            }
            // Terminals aren't offered (clientCapabilities.terminal = false).
            m if m.starts_with("terminal/") => reply(Err("Quire doesn't run commands for agents".into())),
            _ => reply(Err(format!("{method} isn't supported by Quire"))),
        }
    }

    /// `initialize`, then `session/new` in `cwd` with Quire's MCP server
    /// (when `quire_cli` is known) so the agent gets the note tools too.
    pub fn start_session(&mut self, cwd: &Path, mcp: Option<(&Path, &str)>, host: &mut dyn AcpHost) -> Result<(), String> {
        let cancel = CancelFlag::new();
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false },
            "clientInfo": { "name": "quire", "version": env!("CARGO_PKG_VERSION") },
        });
        self.agent_info = self.request("initialize", &init, host, &cancel, Some(SETUP_TIMEOUT))?;
        let servers: Vec<Value> = mcp
            .map(|(cli, key)| vec![json!({ "name": "quire", "command": cli.to_string_lossy(), "args": mcp_args(cwd, key), "env": [] })])
            .unwrap_or_default();
        let r = self.request(
            "session/new",
            &json!({ "cwd": cwd.to_string_lossy(), "mcpServers": servers }),
            host,
            &cancel,
            Some(SETUP_TIMEOUT),
        )?;
        self.session = Some(r["sessionId"].as_str().ok_or("the agent didn't start a session")?.to_string());
        Ok(())
    }

    /// Send one message; returns the agent's stop reason (`end_turn`, `cancelled`…).
    pub fn prompt(&mut self, text: &str, host: &mut dyn AcpHost, cancel: &CancelFlag) -> Result<String, String> {
        let session = self.session.clone().ok_or("no session")?;
        let r = self.request(
            "session/prompt",
            &json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
            host,
            cancel,
            None,
        )?;
        Ok(r["stopReason"].as_str().unwrap_or("end_turn").to_string())
    }
}

/// The arguments of the `quire-cli` MCP server offered to an external agent
/// in `cwd` (the vault): `key` is its actor id (`acp:<name>`), which
/// `quire-cli mcp --client` takes as is.
pub fn mcp_args(cwd: &Path, key: &str) -> Vec<String> {
    vec!["mcp".into(), cwd.to_string_lossy().into_owned(), "--allow-write".into(), "--client".into(), key.into()]
}

/// Tool call content (`[{type: "content", content: {type: "text", text}} | {type: "diff", path, newText}]`) as text.
fn content_text(v: &Value) -> String {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|c| match c["type"].as_str() {
            Some("diff") => format!("Change to {}", c["path"].as_str().unwrap_or_default()),
            _ => c["content"]["text"].as_str().or_else(|| c["text"].as_str()).unwrap_or_default().to_string(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn raw_input(v: &Value) -> String {
    if v.is_null() {
        return String::new();
    }
    let s = v.get("command").and_then(Value::as_str).map_or_else(|| v.to_string(), str::to_string);
    format!("\n{}", s.chars().take(2000).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_kinds_never_get_allow_always_and_start_on_deny() {
        assert!(RISKY_KINDS.iter().all(|k| !may_allow_always(k) && denied_by_default(k)));
        assert!(may_allow_always("read") && !denied_by_default("read"));
        let opts = vec![
            PermissionOption { id: "a1".into(), name: "Allow".into(), kind: "allow_once".into() },
            PermissionOption { id: "a2".into(), name: "Always".into(), kind: "allow_always".into() },
            PermissionOption { id: "r1".into(), name: "Reject".into(), kind: "reject_once".into() },
        ];
        let exec = PermissionRequest { title: "rm -rf".into(), kind: "execute".into(), detail: String::new(), options: opts };
        assert_eq!(exec.default_option().unwrap().id, "r1");
        assert!(exec.offered().iter().all(|o| o.kind != "allow_always"));
        let read = PermissionRequest { kind: "read".into(), ..exec };
        assert_eq!(read.default_option().unwrap().id, "a1");
        assert_eq!(read.offered().len(), 3);
        assert_eq!(AcpAgent { command: "/usr/bin/claude-code-acp".into(), ..AcpAgent::default() }.actor_id(), "acp:claude-code");
    }
}
