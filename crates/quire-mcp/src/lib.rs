// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire over the Model Context Protocol (design 0.9 §B, §4).
//!
//! * [`tools`] — the note tools: one registry shared by MCP, the agent panel and ACP
//! * [`protocol`] — protocol versions, eras (2026-07-28 and older), result shapes
//! * [`server`] — the JSON-RPC server (`quire-cli mcp` over stdio)
//!
//! Clients of 2024-11-05 to 2025-11-25 get exactly what Quire 0.8 sent. A
//! 2026-07-28 client names its version in every request's `_meta` and can
//! start with `server/discover`.

pub mod protocol;
pub mod server;
pub mod tools;

pub use server::Server;
pub use tools::{Tool, ToolCx, ToolOutput, ToolRegistry, ToolSpec};
