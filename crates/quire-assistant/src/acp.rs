// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! An external agent over the Agent Client Protocol (design §D.6).
//!
//! Strictly opt-in, behind a warning: the agent is a program with the
//! user's permissions. Its file writes become proposals; `execute`, `fetch`
//! and `delete` permission requests default to Deny.

/// A configured external agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcpAgent {
    /// `claude-code-acp`, `codex-acp`, or a custom command.
    pub command: String,
    pub args: Vec<String>,
    /// Shown in the panel's amber chip: "External: Claude Code".
    pub label: String,
}

/// ACP permission kinds that default to Deny and never offer "Allow always".
pub const RISKY_KINDS: &[&str] = &["execute", "fetch", "delete"];
/// The only kinds "Allow always" is offered for.
pub const ALWAYS_OK_KINDS: &[&str] = &["read", "search", "think"];

/// Whether "Allow always" may be offered for a permission kind.
pub fn may_allow_always(kind: &str) -> bool {
    ALWAYS_OK_KINDS.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_kinds_never_get_allow_always() {
        assert!(RISKY_KINDS.iter().all(|k| !may_allow_always(k)));
        assert!(may_allow_always("read"));
    }
}
