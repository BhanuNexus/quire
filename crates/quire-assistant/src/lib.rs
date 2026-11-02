// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The in-app assistant (design 0.9 §D.5–§D.7).
//!
//! * [`prompts`] — the built-in prompt texts, shared with MCP `prompts/list`
//! * [`runner`] — the agent loop behind the agent panel
//! * [`acp`] — connecting an external agent (Agent Client Protocol)
//!
//! The loop, the ACP bridge and the wiki workflows arrive with WS-D; the
//! types the desktop and MCP build against are here.

pub mod acp;
pub mod prompts;
pub mod runner;
