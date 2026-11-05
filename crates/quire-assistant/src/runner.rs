// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agent loop of the in-app panel (design §D.5).
//!
//! Model turns, tool calls through Quire's tool registry (the same one MCP serves), writes
//! through `AgentStore::submit` (so they are suggestions in the person's own
//! notes), a step cap, a token budget and Stop.
//!
//! [`ToolHost`] holds what tools need (the vault, an index of it, the agent
//! store) for the panel's background thread. [`run`] takes one message to
//! the end and reports through [`RunEvent`]s, which the panel draws.

use std::fmt::Write as _;
use std::path::Path;

use quire_agent::{Actor, AgentStore, Mode, Via};
use quire_app::index::IndexService;
use quire_app::intel::chat::{
    CancelFlag, ChatEvent, ChatMessage, ChatModel, ChatRequest, Locality, Part, Role, StopReason, ToolDef, Usage,
};
use quire_app::intel::llm::LlmError;
use quire_app::vault::{Vault, VaultEvent, VaultWatcher};
use quire_mcp::served::ServedCache;
use quire_mcp::tools::{Access, CallExt, Grant, ToolCx, ToolOutput, ToolRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the panel lets a model do, by how capable it is (design §D.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Chat about the open note; no tools (small on-device models).
    ChatOnly,
    /// Read tools and one change per message (the balanced on-device model).
    ReadOneWrite,
    /// Every tool (larger on-device models get the core set to save context).
    Full,
}

impl Capability {
    /// Shown in the panel's header.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatOnly => "Chat about this note",
            Self::ReadOneWrite => "Reads notes · one change per message",
            Self::Full => "Reads and edits notes",
        }
    }

    /// What a model can do: no tools means chat only; on this computer the
    /// tier decides; servers and the cloud get everything.
    pub fn for_model(model: &dyn ChatModel, device_tier: Option<quire_ai::catalog::Tier>) -> Self {
        use quire_ai::catalog::Tier;
        if !model.supports_tools() {
            return Self::ChatOnly;
        }
        match (model.locality(), device_tier) {
            (Locality::Device, Some(Tier::Small)) => Self::ChatOnly,
            (Locality::Device, Some(Tier::Balanced)) => Self::ReadOneWrite,
            _ => Self::Full,
        }
    }
}

/// Tools offered to small context windows (on this computer): the ones
/// that matter most, with short schemas.
const CORE_TOOLS: &[&str] =
    &["search", "view", "outline", "list", "neighbors", "create", "str_replace", "append", "edit_section", "set_properties"];

/// Limits for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub max_steps: u32,
    /// Input plus output tokens; 0 = no limit.
    pub max_tokens: u32,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self { max_steps: 20, max_tokens: 0 }
    }
}

/// What the panel shows while a run goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    /// A piece of the reply.
    Text(String),
    /// A tool call starts ("Searching notes for *ropes*").
    ToolStarted {
        id: String,
        name: String,
        summary: String,
    },
    /// It ended: a one-line summary and the text the model saw.
    ToolFinished {
        id: String,
        summary: String,
        detail: String,
        is_error: bool,
    },
    /// A change became a suggestion in the Inbox.
    Proposed {
        proposal: String,
        path: String,
    },
    /// A change was written directly (an agent zone).
    Applied {
        path: String,
    },
    /// The run ended normally.
    Done {
        usage: Usage,
        steps: u32,
    },
    /// Stop was pressed.
    Stopped,
    Failed(String),
}

/// Who the panel acts as: `panel:<provider>`.
pub fn panel_actor(provider: &str, label: &str, model: Option<&str>, device: &str) -> Actor {
    let mut a = Actor::agent(format!("panel:{provider}"), label, Via::Panel, device);
    a.model = model.map(str::to_string);
    a
}

// ------------------------------------------------------------------ tools

/// The vault, an index of it, the agent store and the tool registry, for
/// the panel's thread. Opening it indexes the vault (do it off the UI thread).
pub struct ToolHost {
    vault: Vault,
    index: IndexService,
    agent: AgentStore,
    served: ServedCache,
    registry: ToolRegistry,
    watcher: Option<VaultWatcher>,
}

impl ToolHost {
    /// `root` is the vault folder, watched for changes made elsewhere.
    pub fn open(vault: Vault, root: Option<&Path>, agent: AgentStore) -> Self {
        let index = IndexService::spawn();
        index.load(vault.clone());
        index.flush();
        let watcher = root.and_then(|r| VaultWatcher::start(r).ok());
        Self { served: ServedCache::new(vault.clone()), vault, index, agent, registry: ToolRegistry::standard(), watcher }
    }

    pub const fn vault(&self) -> &Vault {
        &self.vault
    }

    pub const fn agent(&self) -> &AgentStore {
        &self.agent
    }

    /// Take in edits made since the last call (the editor, other agents).
    pub fn refresh(&self) {
        let Some(w) = &self.watcher else { return };
        let events: Vec<VaultEvent> = w.events.try_iter().collect();
        if events.is_empty() {
            return;
        }
        for ev in events {
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
        self.index.flush();
    }

    /// The tools a model may call at `cap`. `core` keeps to the short list.
    pub fn defs(&self, cap: Capability, core: bool) -> Vec<ToolDef> {
        if cap == Capability::ChatOnly {
            return Vec::new();
        }
        let grant = Grant { write: true, ask: false, network: false };
        self.registry
            .offered_for(grant)
            .filter(|t| !core || CORE_TOOLS.contains(&t.spec().name))
            .filter(|t| t.spec().access == Access::Read || t.spec().access == Access::Write)
            .map(|t| {
                let s = t.spec();
                ToolDef { name: s.name.into(), description: s.description.into(), input_schema: s.input_schema.clone() }
            })
            .collect()
    }

    pub fn is_write(&self, name: &str) -> bool {
        self.registry.get(name).is_some_and(|t| t.spec().access != Access::Read)
    }

    /// Call a tool as `actor` (writes per the vault's policy).
    pub fn call(&self, actor: &Actor, name: &str, args: &Value, cancel: &CancelFlag) -> ToolOutput {
        let Some(tool) = self.registry.get(name) else { return ToolOutput::error("not_found", format!("there is no tool called {name}")) };
        if matches!(tool.spec().access, Access::Network | Access::Ask) {
            return ToolOutput::error("denied_by_policy", "this tool isn't available in the agent panel");
        }
        self.refresh();
        let cx = ToolCx { vault: &self.vault, index: &self.index, agent: &self.agent, actor, cancel, allow_write: true };
        let ext = CallExt::basic(&self.served, true);
        tool.call_with(&cx, &ext, args)
    }
}

// ------------------------------------------------------------------ summaries

fn arg<'a>(args: &'a Value, k: &str) -> &'a str {
    args[k].as_str().unwrap_or_default()
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

/// "Searching notes for *ropes*", before the call returns.
pub fn describe_call(name: &str, args: &Value) -> String {
    match name {
        "search" => format!("Searching notes for *{}*", arg(args, "query")),
        "view" => format!("Reading *{}*", stem(arg(args, "path"))),
        "outline" => format!("Looking at the headings of *{}*", stem(arg(args, "path"))),
        "list" => "Listing notes".into(),
        "neighbors" => format!("Following links from *{}*", stem(arg(args, "path"))),
        "create" => format!("Writing *{}*", stem(arg(args, "path"))),
        "str_replace" | "insert" | "edit_section" | "append" | "set_properties" => format!("Changing *{}*", stem(arg(args, "path"))),
        "rename" => format!("Renaming *{}*", stem(arg(args, "from"))),
        "delete" => format!("Deleting *{}*", stem(arg(args, "path"))),
        other => format!("Using {other}"),
    }
}

/// "Searched notes for *ropes* · 6 results", after it returns.
pub fn describe_result(name: &str, args: &Value, out: &ToolOutput) -> String {
    let structured = match out {
        ToolOutput::Ok { structured, .. } => structured,
        ToolOutput::Err { message, .. } => return format!("{} failed: {message}", describe_call(name, args)),
        ToolOutput::InputRequired { .. } => return format!("{} needs approval", describe_call(name, args)),
    };
    let path = stem(arg(args, "path"));
    match name {
        "search" => {
            let n = structured["hits"].as_array().map_or(0, Vec::len);
            format!("Searched notes for *{}* · {n} result{}", arg(args, "query"), if n == 1 { "" } else { "s" })
        }
        "view" => match structured["lines"].as_array().map(|l| (l[0].as_u64().unwrap_or(0), l[1].as_u64().unwrap_or(0))) {
            Some((s, e)) if e > 0 => format!("Read *{path}* (lines {s}–{e})"),
            _ => format!("Read *{path}*"),
        },
        "list" => format!("Listed {} notes", structured["notes"].as_array().map_or(0, Vec::len)),
        "outline" => format!("Looked at the headings of *{path}*"),
        "neighbors" => format!("Followed links from *{path}* · {} notes", structured["nodes"].as_array().map_or(0, Vec::len)),
        _ if structured["outcome"] == "proposed" => format!("Suggested a change to *{path}* · review it in the Inbox"),
        _ if structured["outcome"] == "applied" && name == "create" => format!("Wrote *{path}*"),
        _ if structured["outcome"] == "applied" => format!("Changed *{path}*"),
        _ => format!("Used {name}"),
    }
}

// ------------------------------------------------------------------ the loop

/// Tool results the model sees are cut to about this many bytes (≈ the
/// MCP token bound of 4,000 tokens).
pub const MAX_RESULT_BYTES: usize = 12_000;

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut; ask for a smaller part with `lines` or `section`)", &s[..end])
}

/// One run's settings.
pub struct RunSpec<'a> {
    pub model: &'a dyn ChatModel,
    pub host: Option<&'a ToolHost>,
    pub actor: &'a Actor,
    pub capability: Capability,
    /// Use the core tool list (small context windows).
    pub core_tools: bool,
    /// Ask mode: read tools only.
    pub read_only: bool,
    pub limits: RunLimits,
    pub system: String,
    pub max_reply_tokens: u32,
}

/// Run one message to the end. `conversation` holds the turns so far and
/// gets this run's turns appended (the user's message must already be in it).
pub fn run(spec: &RunSpec<'_>, conversation: &mut Vec<ChatMessage>, cancel: &CancelFlag, sink: &mut dyn FnMut(RunEvent)) {
    let tools: Vec<ToolDef> = spec
        .host
        .map(|h| h.defs(spec.capability, spec.core_tools).into_iter().filter(|t| !spec.read_only || !h.is_write(&t.name)).collect())
        .unwrap_or_default();
    let mut total = Usage::default();
    let mut writes = 0u32;
    let mut repaired = false;
    for step in 1..=spec.limits.max_steps.max(1) {
        let req = ChatRequest {
            system: spec.system.clone(),
            messages: conversation.clone(),
            tools: tools.clone(),
            max_tokens: spec.max_reply_tokens,
            temperature: 0.3,
            json: false,
        };
        let mut text = String::new();
        let mut calls: Vec<(String, String, Value)> = Vec::new();
        let mut stop = StopReason::EndTurn;
        let r = spec.model.chat(&req, cancel, &mut |ev| match ev {
            ChatEvent::Text { text: t } => {
                text.push_str(&t);
                sink(RunEvent::Text(t));
            }
            ChatEvent::ToolUse { id, name, input } => calls.push((id, name, input)),
            ChatEvent::Done { stop: s, usage } => {
                stop = s;
                total.input_tokens += usage.input_tokens;
                total.output_tokens += usage.output_tokens;
            }
        });
        match r {
            Ok(()) => {}
            Err(LlmError::Cancelled) => {
                if !text.is_empty() {
                    conversation.push(ChatMessage::assistant(text));
                }
                sink(RunEvent::Stopped);
                return;
            }
            Err(e) => {
                sink(RunEvent::Failed(e.to_string()));
                return;
            }
        }
        // A call that couldn't be read is never run, nor is anything else in
        // the same turn: the model gets one short correction and tries again.
        if let Some(why) = unreadable(&calls) {
            let raw = calls.iter().find_map(|(_, _, i)| i[quire_ai::toolcall::UNPARSED].as_str()).unwrap_or_default();
            let id = format!("repair_{step}");
            sink(RunEvent::ToolStarted { id: id.clone(), name: String::new(), summary: "Reading the model's tool call".into() });
            if repaired {
                sink(RunEvent::ToolFinished {
                    id,
                    summary: "The model's tool call couldn't be read · nothing was run".into(),
                    detail: cut(raw, 2_000),
                    is_error: true,
                });
                sink(RunEvent::Failed(
                    "The model wrote a tool call Quire couldn't read, twice. Nothing was run; try again or rephrase.".into(),
                ));
                return;
            }
            repaired = true;
            sink(RunEvent::ToolFinished {
                id,
                summary: "The model's tool call couldn't be read · asked it to write it again".into(),
                detail: cut(raw, 2_000),
                is_error: true,
            });
            if !text.is_empty() {
                conversation.push(ChatMessage::assistant(text));
            }
            conversation.push(ChatMessage::user(repair_message(&why)));
            continue;
        }
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(Part::text(text));
        }
        parts.extend(calls.iter().map(|(id, name, input)| Part::ToolUse { id: id.clone(), name: name.clone(), input: input.clone() }));
        if !parts.is_empty() {
            conversation.push(ChatMessage { role: Role::Assistant, parts });
        }
        if calls.is_empty() {
            if stop == StopReason::MaxTokens {
                sink(RunEvent::Text("\n\n*(The reply was cut at its length limit.)*".into()));
            }
            sink(RunEvent::Done { usage: total, steps: step });
            return;
        }
        let mut results = Vec::new();
        for (id, name, input) in calls {
            if cancel.is_cancelled() {
                // Answer every call so the conversation stays valid.
                results.push(Part::ToolResult { id, content: "Stopped by the person.".into(), is_error: true });
                continue;
            }
            sink(RunEvent::ToolStarted { id: id.clone(), name: name.clone(), summary: describe_call(&name, &input) });
            let out = match spec.host {
                None => ToolOutput::error("denied_by_policy", "tools aren't available in this mode"),
                Some(_) if spec.capability == Capability::ChatOnly => {
                    ToolOutput::error("denied_by_policy", "tools aren't available in this mode")
                }
                Some(h) if h.is_write(&name) && spec.read_only => {
                    ToolOutput::error("denied_by_policy", "this is Ask mode: describe the change instead of making it")
                }
                Some(h) if h.is_write(&name) && spec.capability == Capability::ReadOneWrite && writes >= 1 => {
                    ToolOutput::error("budget_exceeded", "one change per message with this model; say what else you would change")
                }
                Some(h) => {
                    if h.is_write(&name) {
                        writes += 1;
                    }
                    h.call(spec.actor, &name, &input, cancel)
                }
            };
            let summary = describe_result(&name, &input, &out);
            let (content, is_error) = match &out {
                ToolOutput::Ok { text, structured, .. } => {
                    let path = structured["uri"].as_str().map_or_else(|| arg(&input, "path").to_string(), uri_path);
                    match structured["outcome"].as_str() {
                        Some("proposed") => {
                            sink(RunEvent::Proposed { proposal: structured["proposal"].as_str().unwrap_or_default().into(), path });
                        }
                        Some("applied") => sink(RunEvent::Applied { path }),
                        _ => {}
                    }
                    (cut(text, MAX_RESULT_BYTES), false)
                }
                ToolOutput::Err { code, message, .. } => (format!("Error ({code}): {message}"), true),
                ToolOutput::InputRequired { .. } => ("This change needs the person's approval; it was not made.".to_string(), true),
            };
            sink(RunEvent::ToolFinished { id: id.clone(), summary, detail: cut(&content, 2_000), is_error });
            results.push(Part::ToolResult { id, content, is_error });
        }
        conversation.push(ChatMessage { role: Role::Tool, parts: results });
        if cancel.is_cancelled() {
            sink(RunEvent::Stopped);
            return;
        }
        if spec.limits.max_tokens > 0 && total.input_tokens + total.output_tokens > spec.limits.max_tokens {
            sink(RunEvent::Failed(format!("Stopped: this message used its budget of {} tokens.", spec.limits.max_tokens)));
            return;
        }
    }
    sink(RunEvent::Text(format!("\n\n*(Stopped after {} steps. Say \"go on\" to continue.)*", spec.limits.max_steps)));
    sink(RunEvent::Done { usage: total, steps: spec.limits.max_steps });
}

/// Why a turn's calls can't be run, if one of them couldn't be read: no
/// name, input that isn't an object, or the engine's `_unparsed` marker.
fn unreadable(calls: &[(String, String, Value)]) -> Option<String> {
    calls.iter().find_map(|(_, name, input)| {
        if quire_ai::toolcall::is_unparsed(input) {
            Some(input["_error"].as_str().unwrap_or("it isn't valid JSON").to_string())
        } else if name.trim().is_empty() {
            Some("it has no tool name".to_string())
        } else if !input.is_object() {
            Some(format!("the arguments of {name} aren't a JSON object"))
        } else {
            None
        }
    })
}

/// The one correction sent after a tool call that couldn't be read.
fn repair_message(why: &str) -> String {
    format!(
        "Your last tool call couldn't be read ({why}), so nothing was run. Write it again as \
         <tool_call>{{\"name\": \"<tool>\", \"arguments\": {{…}}}}</tool_call> with double-quoted JSON \
         and nothing else inside the tags, or answer in plain text."
    )
}

/// `quire://note/Rope%20Notes.md` → `Rope Notes.md`.
fn uri_path(uri: &str) -> String {
    let p = uri.strip_prefix("quire://note/").unwrap_or(uri);
    let mut out = Vec::new();
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&p[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The panel's system prompt: Quire's conventions, the vault's zones, and
/// its `AGENTS.md` when this device approved it.
pub fn system_for(agent: &AgentStore, may_write: bool, tools: bool) -> String {
    use quire_agent::policy::ZoneRole;
    let mut s = crate::prompts::panel_system(may_write, tools);
    let policy = agent.policy();
    let list = |role: ZoneRole| policy.zones.iter().filter(|z| z.role == role).map(|z| z.path.clone()).collect::<Vec<_>>().join(", ");
    let (own, raw) = (list(ZoneRole::Agent), list(ZoneRole::Raw));
    if tools {
        s.push_str("\n\nIn this vault:");
        if !own.is_empty() {
            let _ = write!(s, "\n- You may write directly in: {own}");
        }
        if !raw.is_empty() {
            let _ = write!(s, "\n- Sources (add files, never edit): {raw}");
        }
        s.push_str("\n- Everywhere else your changes become suggestions.");
    }
    let state = quire_mcp::instructions::state_dir(agent.vault());
    if let Some((path, true, text)) =
        quire_mcp::instructions::for_agents(agent.vault(), state.as_deref(), quire_mcp::instructions::INLINE_LIMIT)
    {
        let _ = write!(s, "\n\nThe vault's own instructions ({path}):\n<instructions>\n{text}\n</instructions>");
    }
    s
}

/// Cap a write grant on the store for this actor: read-only models can't
/// write even if a tool slipped through.
pub fn grant(agent: &AgentStore, actor: &Actor, cap: Capability) {
    agent.set_cap(&actor.id, if cap == Capability::ChatOnly { Mode::Read } else { Mode::Direct });
}

/// The context line for the open note and selection (the panel's chips).
pub fn context_message(note: Option<&str>, selection: Option<&str>, note_text: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(n) = note {
        let _ = writeln!(s, "The person is looking at [[{}]] ({}).", stem(n), quire_mcp::resources::uri_for_path(&n.into()));
    }
    if let Some(sel) = selection.filter(|s| !s.trim().is_empty()) {
        s.push_str("They selected:\n");
        for l in sel.lines() {
            s.push_str("> ");
            s.push_str(l);
            s.push('\n');
        }
    }
    if let Some(t) = note_text {
        s.push_str("The note's text:\n<note>\n");
        s.push_str(&cut(t, 9_000));
        s.push_str("\n</note>\n");
    }
    s
}

/// Notes and tags the person mentioned (`@note`, `#tag` chips in the
/// composer). `texts` holds each note's text when the model has no tools
/// (in the same order as `notes`; empty otherwise).
pub fn mentions_message(notes: &[&str], tags: &[&str], texts: &[String]) -> String {
    let mut s = String::new();
    for (i, n) in notes.iter().enumerate() {
        let _ = writeln!(s, "They mention [[{}]] ({}).", stem(n), quire_mcp::resources::uri_for_path(&(*n).into()));
        if let Some(t) = texts.get(i) {
            s.push_str("Its text:\n<note>\n");
            s.push_str(&cut(t, 6_000));
            s.push_str("\n</note>\n");
        }
    }
    if !tags.is_empty() {
        let list = tags.iter().map(|t| format!("#{}", t.trim_start_matches('#'))).collect::<Vec<_>>().join(", ");
        let _ = writeln!(s, "They mean the notes tagged {list} (search for the tag).");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_app::intel::chat::ScriptedChat;
    use serde_json::json;

    #[test]
    fn mentions_name_notes_and_tags() {
        let m = mentions_message(&["Notes/Rope Notes.md"], &["#ropes", "undo"], &[]);
        assert!(m.contains("They mention [[Rope Notes]] (quire://"), "{m}");
        assert!(m.contains("#ropes, #undo"), "{m}");
        let m = mentions_message(&["A.md"], &[], &["body".to_string()]);
        assert!(m.contains("<note>\nbody\n</note>"), "{m}");
        assert!(mentions_message(&[], &[], &[]).is_empty());
    }

    #[test]
    fn summaries_read_naturally() {
        let args = json!({"query": "ropes", "path": "Notes/Rope Notes.md"});
        assert_eq!(describe_call("search", &args), "Searching notes for *ropes*");
        let out = ToolOutput::ok("…", json!({"hits": [1, 2, 3, 4, 5, 6]}));
        assert_eq!(describe_result("search", &args, &out), "Searched notes for *ropes* · 6 results");
        let out = ToolOutput::ok("…", json!({"outcome": "proposed", "proposal": "p_1"}));
        assert_eq!(describe_result("str_replace", &args, &out), "Suggested a change to *Rope Notes* · review it in the Inbox");
        assert_eq!(
            describe_result("view", &args, &ToolOutput::error("not_found", "no such note")),
            "Reading *Rope Notes* failed: no such note"
        );
        assert_eq!(uri_path("quire://note/A%20B/C%C3%A9.md"), "A B/Cé.md");
        assert!(cut(&"é".repeat(10), 5).starts_with("éé\n…"));
    }

    #[test]
    fn chat_only_runs_without_tools_and_stop_is_heard() {
        let model = ScriptedChat::texts(["Hello."]);
        let actor = panel_actor("device", "Agent panel", None, "d_1");
        let spec = RunSpec {
            model: &model,
            host: None,
            actor: &actor,
            capability: Capability::ChatOnly,
            core_tools: true,
            read_only: false,
            limits: RunLimits::default(),
            system: "sys".into(),
            max_reply_tokens: 100,
        };
        let mut conv = vec![ChatMessage::user("hi")];
        let mut events = Vec::new();
        run(&spec, &mut conv, &CancelFlag::new(), &mut |e| events.push(e));
        assert_eq!(events, vec![RunEvent::Text("Hello.".into()), RunEvent::Done { usage: Usage::default(), steps: 1 }]);
        assert_eq!(conv.len(), 2);
        assert!(model.requests()[0].tools.is_empty());
        let stopped = CancelFlag::new();
        stopped.cancel();
        let model = ScriptedChat::texts(["never"]);
        let spec = RunSpec { model: &model, ..spec };
        let mut events = Vec::new();
        run(&spec, &mut conv, &stopped, &mut |e| events.push(e));
        assert_eq!(events, vec![RunEvent::Stopped]);
    }

    #[test]
    fn an_unreadable_call_is_never_run_and_gets_one_correction() {
        let bad = || ChatEvent::ToolUse {
            id: "call_1".into(),
            name: String::new(),
            input: json!({"_unparsed": "{\"name\": search", "_error": "the tool call isn't valid JSON"}),
        };
        let half = || ChatEvent::ToolUse { id: "call_2".into(), name: "delete".into(), input: json!({"path": "A.md"}) };
        let actor = panel_actor("device", "Agent panel", None, "d_1");
        let model = ScriptedChat::new(vec![
            vec![ChatEvent::Text { text: "Let me look.".into() }, half(), bad()],
            vec![ChatEvent::Text { text: "Rope is a data structure.".into() }],
        ]);
        let spec = RunSpec {
            model: &model,
            host: None,
            actor: &actor,
            capability: Capability::Full,
            core_tools: true,
            read_only: false,
            limits: RunLimits::default(),
            system: "sys".into(),
            max_reply_tokens: 100,
        };
        let mut conv = vec![ChatMessage::user("what is a rope?")];
        let mut events = Vec::new();
        run(&spec, &mut conv, &CancelFlag::new(), &mut |e| events.push(e));
        assert!(
            events.iter().any(
                |e| matches!(e, RunEvent::ToolFinished { summary, is_error: true, .. } if summary.contains("asked it to write it again"))
            ),
            "{events:?}"
        );
        assert!(matches!(events.last(), Some(RunEvent::Done { steps: 2, .. })), "{events:?}");
        // The readable call in the same turn wasn't run either.
        assert!(!events.iter().any(|e| matches!(e, RunEvent::ToolStarted { name, .. } if name == "delete")));
        let second = &model.requests()[1].messages;
        assert!(
            second.last().is_some_and(|m| m.role == Role::User && m.text().contains("couldn't be read (the tool call isn't valid JSON)")),
            "{second:?}"
        );
        assert!(second.iter().all(|m| m.parts.iter().all(|p| !matches!(p, Part::ToolUse { .. }))), "no unanswered tool_use is sent back");

        let model = ScriptedChat::new(vec![vec![bad()], vec![bad()]]);
        let spec = RunSpec { model: &model, ..spec };
        let mut events = Vec::new();
        run(&spec, &mut vec![ChatMessage::user("hi")], &CancelFlag::new(), &mut |e| events.push(e));
        assert!(matches!(events.last(), Some(RunEvent::Failed(m)) if m.contains("Nothing was run")), "{events:?}");
        assert_eq!(model.remaining(), 0);
    }
}
