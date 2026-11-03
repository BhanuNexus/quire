// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The in-app assistant (design 0.9 §D.5–§D.7).
//!
//! * [`prompts`] — the built-in prompt texts, shared with MCP `prompts/list`
//! * [`runner`] — the agent loop behind the agent panel, over Quire's tools
//! * [`conversation`] — the panel's saved conversations
//! * [`acp`] — connecting an external agent (Agent Client Protocol)
//! * [`wiki`] — "Ingest into wiki…"

pub mod acp;
pub mod conversation;
pub mod prompts;
pub mod runner;
pub mod wiki;
