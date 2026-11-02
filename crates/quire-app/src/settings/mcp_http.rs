// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[mcp_http]` in settings.toml: MCP over HTTP (design §4.6).
//!
//! Off by default; always needs a token.
//!
//! Owned by WS-B. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpHttpSettings {
    /// Serve MCP over HTTP while Quire runs.
    pub enabled: bool,
    /// Address to listen on.
    pub bind: String,
    /// Port to listen on.
    pub port: u16,
    /// The URL clients use when behind a proxy or tunnel.
    pub public_url: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for McpHttpSettings {
    fn default() -> Self {
        Self { enabled: false, bind: "127.0.0.1".into(), port: 7311, public_url: String::new(), other: toml::Table::new() }
    }
}
