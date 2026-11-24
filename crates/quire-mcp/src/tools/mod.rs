// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The note tools (`docs/design/0.9.md` §2.5, §4.2): one registry shared by MCP, the agent
//! panel and ACP, so every agent sees the same tools with the same limits.
//!
//! A tool is a [`ToolSpec`] (name, schema, annotations, what access it
//! needs) and a `call` that returns a [`ToolOutput`]. The registry keeps
//! tools in a fixed order ([`ToolRegistry::standard`]), never sorted at
//! runtime; write tools are left out for clients without a write grant.
//!
//! Each module registers its tools in `standard()`, in its own line:
//! the 0.9 read tools, `canvas`, the 0.9 write tools, the 0.12 links.
//!
//! The MCP server calls [`Tool::call_with`], which also passes what only a
//! protocol session knows ([`CallExt`]: the served-version cache, whether
//! the client can approve changes itself, the caller's grants). `call`
//! works without it, with defaults that never widen access.

pub mod canvas;
pub mod common;
// 0.12: typed links and hierarchy.
pub mod links;
pub mod read;
pub mod write;

use std::sync::{Arc, OnceLock};

use quire_agent::{Actor, AgentStore, Visibility};
use quire_app::index::{Index, IndexService};
use quire_app::intel::chat::{CancelFlag, ChatModel};
use quire_app::vault::Vault;
use serde_json::{Value, json};

use crate::mrtr::Sealer;
use crate::served::ServedCache;

/// MCP tool annotations (hints for the client's UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Annotations {
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: Option<bool>,
}

impl Annotations {
    /// R: reads the vault only.
    pub const READ: Self = Self { read_only: Some(true), destructive: None, idempotent: None, open_world: Some(false) };
    /// W: changes notes.
    pub const WRITE: Self = Self { read_only: Some(false), destructive: Some(true), idempotent: Some(false), open_world: Some(false) };
    /// W, but doing it twice is the same as once (`set_properties`).
    pub const WRITE_IDEMPOTENT: Self =
        Self { read_only: Some(false), destructive: Some(true), idempotent: Some(true), open_world: Some(false) };
    /// C: only creates.
    pub const CREATE: Self = Self { read_only: Some(false), destructive: Some(false), idempotent: Some(false), open_world: Some(false) };
    /// N + C: fetches from the web and creates a note.
    pub const NETWORK_CREATE: Self =
        Self { read_only: Some(false), destructive: Some(false), idempotent: Some(false), open_world: Some(true) };

    pub fn to_json(self) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in [
            ("readOnlyHint", self.read_only),
            ("destructiveHint", self.destructive),
            ("idempotentHint", self.idempotent),
            ("openWorldHint", self.open_world),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), v.into());
            }
        }
        Value::Object(m)
    }
}

/// What a tool needs to be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    /// Needs a write grant (`--allow-write`, `notes:propose`).
    Write,
    /// Spends the user's AI provider (`notes:ask`).
    Ask,
    /// Reaches the network (URL capture); also needs a write grant.
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: Annotations,
    pub access: Access,
}

/// A `resource_link` content item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLink {
    pub uri: String,
    pub name: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutput {
    Ok {
        text: String,
        structured: Value,
        links: Vec<ResourceLink>,
    },
    /// `code` is a stable error code (`not_found`, `version_conflict`…).
    Err {
        code: &'static str,
        message: String,
        structured: Value,
    },
    /// Ask the client first (MRTR, `docs/design/0.9.md` §4.5).
    InputRequired {
        requests: Value,
        state: String,
    },
}

impl ToolOutput {
    /// Plain text, no structured content.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Ok { text: s.into(), structured: Value::Null, links: Vec::new() }
    }

    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self::Err { code, message: message.into(), structured: Value::Null }
    }

    /// Text plus structured content.
    pub fn ok(text: impl Into<String>, structured: Value) -> Self {
        Self::Ok { text: text.into(), structured, links: Vec::new() }
    }

    #[must_use]
    pub fn with_links(mut self, more: Vec<ResourceLink>) -> Self {
        if let Self::Ok { links, .. } = &mut self {
            links.extend(more);
        }
        self
    }

    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Err { .. })
    }
}

/// What a tool call may use. One per call.
pub struct ToolCx<'a> {
    pub vault: &'a Vault,
    pub index: &'a IndexService,
    pub agent: &'a AgentStore,
    /// Who is calling (`mcp:claude-code`, `panel:anthropic`).
    pub actor: &'a Actor,
    pub cancel: &'a CancelFlag,
    /// The caller may write (subject to policy).
    pub allow_write: bool,
    /// Which indexed notes the caller may read, made on first use and kept
    /// for the rest of the call ([`Self::visibility`]).
    visibility: OnceLock<Arc<Visibility>>,
}

impl<'a> ToolCx<'a> {
    pub fn new(
        vault: &'a Vault,
        index: &'a IndexService,
        agent: &'a AgentStore,
        actor: &'a Actor,
        cancel: &'a CancelFlag,
        allow_write: bool,
    ) -> Self {
        Self { vault, index, agent, actor, cancel, allow_write, visibility: OnceLock::new() }
    }

    /// Which indexed notes the caller may read, for this call: the rules of
    /// its policy and grant, and the locked folders as they are now (checked
    /// once per folder, so a lock made by another process applies to the next
    /// call). For paths that came out of the index or the vault listing only:
    /// a path an agent named goes through [`common::existing`] or the store's
    /// guard.
    ///
    /// Takes the index's read lock the first time; inside a closure that runs
    /// under that lock, use [`Self::visibility_in`] taken before.
    pub fn visibility(&self) -> &Arc<Visibility> {
        if let Some(v) = self.visibility.get() {
            return v;
        }
        let ix = self.index.read();
        self.visibility_in(&ix)
    }

    /// [`Self::visibility`] with the index already read.
    pub fn visibility_in(&self, ix: &Index) -> &Arc<Visibility> {
        self.visibility.get_or_init(|| {
            let mut folders: Vec<&str> = ix.records().filter_map(|r| r.path.as_str().rsplit_once('/').map(|(f, _)| f)).collect();
            folders.sort_unstable();
            folders.dedup();
            let vis = self.agent.visibility(self.actor, folders);
            // A client with a tag allow list is judged by the index's tags (`docs/design/0.14-agent-tools.md` §3.5).
            let vis = if vis.needs_tags() { vis.with_tags(ix.records().map(|r| (r.path.clone(), r.tags.clone())).collect()) } else { vis };
            Arc::new(vis)
        })
    }
}

/// What a caller was granted, beyond the vault's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// Write tools (proposals or direct writes, per policy).
    pub write: bool,
    /// `ask` in `model` mode may spend the user's AI provider.
    pub ask: bool,
    /// `capture_url` may fetch web pages.
    pub network: bool,
}

impl Grant {
    pub const READ: Self = Self { write: false, ask: false, network: false };

    /// stdio: `--allow-write` grants writing and capture; asking the provider
    /// is the user's own configuration, so it's allowed.
    pub const fn stdio(allow_write: bool) -> Self {
        Self { write: allow_write, ask: true, network: allow_write }
    }

    pub const fn allows(self, access: Access) -> bool {
        match access {
            Access::Read => true,
            Access::Write => self.write,
            Access::Ask => self.ask,
            Access::Network => self.write && self.network,
        }
    }
}

/// What only the protocol session knows, for one call.
pub struct CallExt<'a> {
    pub served: &'a ServedCache,
    pub grant: Grant,
    /// The client declared `elicitation` and the policy lets it approve changes.
    pub approve_in_client: bool,
    /// The retry of an `input_required` result: `inputResponses` and `requestState`.
    pub input_responses: Option<&'a Value>,
    pub request_state: Option<&'a str>,
    pub sealer: Option<&'a Sealer>,
    /// The user's AI provider, for `ask` in `model` mode.
    pub chat: Option<Arc<dyn ChatModel>>,
    /// URL capture may fetch private and loopback addresses (tests, intranets).
    pub allow_private_urls: bool,
    /// The protocol method (`tools/call`), bound into MRTR states.
    pub method: &'a str,
    /// This device's folder for the vault (`<config>/vaults/<key>/`): approvals.
    pub state_dir: Option<&'a std::path::Path>,
    /// `(done, total)` while the server is still reading the vault's PDFs
    /// at start-up: `search` says its PDF hits may be incomplete.
    pub pdf_indexing: Option<(usize, usize)>,
}

impl<'a> CallExt<'a> {
    /// Defaults for callers without a protocol session (the panel, ACP).
    pub const fn basic(served: &'a ServedCache, allow_write: bool) -> Self {
        Self {
            served,
            grant: Grant { write: allow_write, ask: false, network: allow_write },
            approve_in_client: false,
            input_responses: None,
            request_state: None,
            sealer: None,
            chat: None,
            allow_private_urls: false,
            method: "tools/call",
            state_dir: None,
            pdf_indexing: None,
        }
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn call(&self, cx: &ToolCx<'_>, args: &Value) -> ToolOutput;

    /// Call with the session's extras. Tools that don't need them use `call`.
    fn call_with(&self, cx: &ToolCx<'_>, ext: &CallExt<'_>, args: &Value) -> ToolOutput {
        let _ = ext;
        self.call(cx, args)
    }
}

/// A tool made of a spec and a function.
pub struct FnTool {
    pub spec: ToolSpec,
    pub run: fn(&ToolCx<'_>, &Value) -> ToolOutput,
}

impl Tool for FnTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Value) -> ToolOutput {
        (self.run)(cx, args)
    }
}

/// A tool whose function also takes the session's extras.
pub struct ExtTool {
    pub spec: ToolSpec,
    pub run: fn(&ToolCx<'_>, &CallExt<'_>, &Value) -> ToolOutput,
}

impl Tool for ExtTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, cx: &ToolCx<'_>, args: &Value) -> ToolOutput {
        let served = ServedCache::new(cx.vault.clone());
        (self.run)(cx, &CallExt::basic(&served, cx.allow_write), args)
    }

    fn call_with(&self, cx: &ToolCx<'_>, ext: &CallExt<'_>, args: &Value) -> ToolOutput {
        (self.run)(cx, ext, args)
    }
}

/// `{"type": "object", "properties": …, "required": […]}`.
pub fn schema(props: &Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// The 0.8 tool names removed in 0.14 (`docs/design/0.14-agent-tools.md` §3.4).
///
/// Each is the old name, the tool to use instead, and what differs. Nothing lists these;
/// a call to one gets [`removed_tool`]'s answer, over MCP and in the agent panel.
pub const REMOVED_TOOLS: &[(&str, &str, &str)] = &[
    ("search_notes", "search", "ranks sections, not notes; each hit has a line range for `view`"),
    ("read_note", "view", "takes `path`; long notes come in pages (`max_tokens`, `cursor`)"),
    ("list_notes", "list", "`folder`, `tags`, `where`, `sort`, `limit`, `cursor`"),
    ("backlinks", "neighbors", "with `direction: \"in\"`; each edge has the linking line as `context` (or use `links`)"),
    ("ask_notes", "ask", ""),
    ("list_tags", "tags", ""),
    ("create_note", "create", "`path` is `folder/title.md`; it refuses to overwrite"),
    ("append_to_note", "append", ""),
    ("replace_in_note", "str_replace", "with `all: true` to replace every occurrence; there is no regex form"),
];

/// The answer to a call for a removed 0.8 name: `not_found`, naming the tool to use instead.
pub fn removed_tool(name: &str) -> Option<ToolOutput> {
    let (old, new, _) = REMOVED_TOOLS.iter().find(|(old, ..)| *old == name)?;
    Some(ToolOutput::Err {
        code: "not_found",
        message: format!(
            "No tool called “{old}”: it was removed in Quire 0.14. Use `{new}` instead (see \"Removed tool names\" in docs/agents.md). Call tools/list for the names."
        ),
        structured: json!({ "removed": old, "use": new }),
    })
}

/// Every tool, in the order clients see them.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// The tools Quire offers, in their fixed order.
    pub fn standard() -> Self {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        // ---- the 0.9 read tools, in the table's order.
        tools.extend(read::tools_before_canvas());
        // ---- canvas
        tools.extend(canvas::tools());
        // ---- the 0.9 write tools, after the canvas tools (`docs/design/0.9.md` §4.2).
        tools.extend(write::tools());
        // ---- 0.12 typed links
        tools.extend(links::tools());
        Self { tools }
    }

    /// Tools offered to a caller: write tools only with a write grant.
    pub fn offered(&self, allow_write: bool) -> impl Iterator<Item = &dyn Tool> {
        self.offered_for(Grant::stdio(allow_write))
    }

    /// Tools offered to a caller with `grant`.
    pub fn offered_for(&self, grant: Grant) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(AsRef::as_ref).filter(move |t| grant.allows(t.spec().access))
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().map(AsRef::as_ref).find(|t| t.spec().name == name)
    }

    /// Every tool, whatever the grant.
    pub fn all(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(AsRef::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_fixed_and_names_unique() {
        let r = ToolRegistry::standard();
        let names: Vec<&str> = r.all().map(|t| t.spec().name).collect();
        assert_eq!(&names[..3], ["vault_info", "search", "view"]);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names are unique");
        assert!(r.offered(false).all(|t| !matches!(t.spec().access, Access::Write | Access::Network)));
        assert!(r.offered(true).count() > r.offered(false).count());
        assert_eq!(Annotations::READ.to_json(), json!({"readOnlyHint": true, "openWorldHint": false}));
    }

    #[test]
    fn every_0_9_tool_is_there_in_the_designs_order() {
        let r = ToolRegistry::standard();
        let names: Vec<&str> = r.all().map(|t| t.spec().name).collect();
        let canvas: Vec<&str> = canvas::tools().iter().map(|t| t.spec().name).collect();
        let ours: Vec<&str> = names.iter().copied().filter(|n| !canvas.contains(n)).collect();
        assert_eq!(
            ours,
            [
                "vault_info",
                "search",
                "view",
                "outline",
                "list",
                "neighbors",
                "recent_activity",
                "ask",
                "history",
                "read_version",
                "diff",
                "tags",
                "properties",
                "query_view",
                "tasks",
                "proposals",
                "begin_run",
                "create",
                "str_replace",
                "insert",
                "edit_section",
                "append",
                "set_properties",
                "rename",
                "delete",
                "save_answer",
                "capture_url",
                "revert",
                "links",
                "hierarchy",
                "add_link",
            ]
        );
        for t in r.all() {
            let s = t.spec();
            assert_eq!(s.input_schema["type"], "object", "{}", s.name);
            assert!(s.description.len() > 80 && s.description.contains("Example"), "{} needs a description agents can use", s.name);
            assert!(s.output_schema.is_some(), "{} has an output schema", s.name);
        }
    }
}
