// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The note tools (design §2.5, §4.2): one registry shared by MCP, the agent
//! panel and ACP, so every agent sees the same tools with the same limits.
//!
//! A tool is a [`ToolSpec`] (name, schema, annotations, what access it
//! needs) and a `call` that returns a [`ToolOutput`]. The registry keeps
//! tools in a fixed order ([`ToolRegistry::standard`]), never sorted at
//! runtime; write tools are left out for clients without a write grant.
//!
//! Each module registers its tools in `standard()`, in its own line:
//! `legacy` (F0, the 0.8 tools), WS-B's tools, `canvas` (WS-G).

pub mod canvas;
pub mod legacy;

use quire_agent::{Actor, AgentStore};
use quire_app::index::IndexService;
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::Vault;
use serde_json::{Value, json};

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
    /// C: only creates.
    pub const CREATE: Self = Self { read_only: Some(false), destructive: Some(false), idempotent: Some(false), open_world: Some(false) };

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
    /// Reaches the network (URL capture).
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
    /// Ask the client first (MRTR, design §4.5).
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
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn call(&self, cx: &ToolCx<'_>, args: &Value) -> ToolOutput;
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

/// `{"type": "object", "properties": …, "required": […]}`.
pub fn schema(props: &Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// Every tool, in the order clients see them.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// The tools Quire offers, in their fixed order.
    pub fn standard() -> Self {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        // ---- F0: the 0.8 tools, unchanged.
        tools.extend(legacy::tools());
        // ---- WS-B: the 0.9 tools (vault_info, search, view, …) go here.
        // ---- WS-G
        tools.extend(canvas::tools());
        Self { tools }
    }

    /// Tools offered to a caller: write tools only with a write grant.
    pub fn offered(&self, allow_write: bool) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(AsRef::as_ref).filter(move |t| allow_write || t.spec().access != Access::Write)
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
        assert_eq!(&names[..3], ["search_notes", "read_note", "list_notes"]);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names are unique");
        assert!(r.offered(false).all(|t| t.spec().access != Access::Write));
        assert!(r.offered(true).count() > r.offered(false).count());
        assert_eq!(Annotations::READ.to_json(), json!({"readOnlyHint": true, "openWorldHint": false}));
    }
}
