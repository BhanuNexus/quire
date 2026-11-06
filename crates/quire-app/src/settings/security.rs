// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[security]` in settings.toml: locked folders (0.11).
//!
//! Keys this version doesn't know are kept in `other`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// Lock unlocked folders after this many minutes without input or
    /// typing; 0 never does.
    pub lock_after_idle_minutes: u32,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self { lock_after_idle_minutes: 15, other: toml::Table::new() }
    }
}
