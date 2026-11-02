// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[jobs]` in settings.toml: background work (design §9).
//!
//! Owned by WS-C (shared runner: F0). Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobSettings {
    /// Jobs wait this long after the last keystroke.
    pub pause_on_typing_ms: u64,
    /// Worker threads for background jobs.
    pub max_threads: u32,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for JobSettings {
    fn default() -> Self {
        Self { pause_on_typing_ms: 1500, max_threads: 1, other: toml::Table::new() }
    }
}
