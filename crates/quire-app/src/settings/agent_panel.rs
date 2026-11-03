// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[agent_panel]` in settings.toml: the in-app agent panel (design §D.5, §D.6).
//!
//! Named `agent_panel` because `assistant` is an old alias of `ai_provider`.
//!
//! Owned by WS-D. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentPanelSettings {
    /// Show the panel command and tab.
    pub enabled: bool,
    /// `"provider"` (the configured AI) or `"acp"` (an external agent).
    pub backend: String,
    /// With both a provider and an on-device model: `""` (the provider when
    /// there is one), `"provider"` or `"device"`.
    pub model: String,
    /// Tool steps per message.
    pub max_steps: u32,
    /// External agent command (ACP).
    pub acp_command: String,
    /// Its arguments.
    pub acp_args: Vec<String>,
    /// The user accepted the external-agent warning.
    pub acp_risk_ack: bool,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for AgentPanelSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "provider".into(),
            model: String::new(),
            max_steps: 20,
            acp_command: String::new(),
            acp_args: Vec::new(),
            acp_risk_ack: false,
            other: toml::Table::new(),
        }
    }
}
