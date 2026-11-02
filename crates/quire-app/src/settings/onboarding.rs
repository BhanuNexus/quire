// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[onboarding]` in settings.toml: first-run AI choice (design §7.1).
//!
//! Owned by WS-D. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnboardingSettings {
    /// The AI choice was made (or dismissed).
    pub ai_done: bool,
    /// `""` (not asked yet), `"off"`, `"cloud"`, `"device"` or `"both"`.
    pub ai_choice: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for OnboardingSettings {
    fn default() -> Self {
        Self { ai_done: false, ai_choice: String::new(), other: toml::Table::new() }
    }
}
