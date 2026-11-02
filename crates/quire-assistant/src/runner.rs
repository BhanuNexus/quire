// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agent loop of the in-app panel (design §D.5): model turns, tool
//! calls through Quire's tool registry (the same one MCP serves), writes
//! through `AgentStore::submit`, and a stop button.

use quire_agent::{Actor, ProposalId};
use quire_intel::chat::Usage;

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
    Text(String),
    /// A tool call, summarised ("Searched notes for *ropes* · 6 results").
    Tool {
        name: String,
        summary: String,
        is_error: bool,
    },
    Proposed {
        id: ProposalId,
        title: String,
    },
    Done {
        usage: Usage,
        steps: u32,
    },
    Failed(String),
}

/// Who the panel acts as: `panel:<provider>`.
pub fn panel_actor(provider: &str, device: &str) -> Actor {
    Actor::agent(format!("panel:{provider}"), "Agent panel", quire_agent::Via::Panel, device)
}

/// Run one user message to completion. Not available yet (WS-D): it says
/// so through `sink` and returns.
pub fn run(message: &str, limits: RunLimits, sink: &mut dyn FnMut(RunEvent)) {
    let _ = (message, limits);
    sink(RunEvent::Failed("The agent panel isn't available yet.".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_run_reports_and_ends() {
        let mut events = Vec::new();
        run("hi", RunLimits::default(), &mut |e| events.push(e));
        assert!(matches!(events.as_slice(), [RunEvent::Failed(_)]));
        assert_eq!(panel_actor("anthropic", "d_1").id, "panel:anthropic");
    }
}
