// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire over the Model Context Protocol (design 0.9 §B, §4).
//!
//! * [`tools`] — the note tools: one registry shared by MCP, the agent panel and ACP
//! * [`protocol`] — protocol versions, eras (2026-07-28 and older), result shapes
//! * [`server`] — the transport-agnostic server; [`stdio`] and `http` carry it
//! * [`resources`], [`subs`] — resources, prompts and change notifications
//! * [`served`] — the served-version cache behind optimistic concurrency
//! * [`mrtr`] — approval in the agent's client
//! * [`tokens`] — personal and OAuth tokens for HTTP
//! * [`capture`] — web pages into `raw/`
//!
//! Clients of 2024-11-05 to 2025-11-25 get what Quire 0.8 sent, plus what
//! their version defines. A 2026-07-28 client names its version in every
//! request's `_meta` and can start with `server/discover`.

pub mod bounds;
pub mod capture;
pub mod consent;
pub mod edit;
pub mod eval;
#[cfg(feature = "http")]
pub mod http;
pub mod instructions;
pub mod mrtr;
pub mod protocol;
pub mod resources;
pub mod served;
pub mod server;
pub mod stdio;
pub mod subs;
pub mod tokens;
pub mod tools;

pub use server::{Conn, Principal, Reply, Server, ServerConfig};
pub use tools::{Tool, ToolCx, ToolOutput, ToolRegistry, ToolSpec};
