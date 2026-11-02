// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[tasks]` in settings.toml: tasks 2.0 (design §E.2).
//!
//! Owned by WS-E. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskSettings {
    /// Show reminder notifications.
    pub reminders: bool,
    /// Where a recurring task's next copy goes: `"above"` or `"below"`.
    pub new_occurrence: String,
    /// Task fields written as `"emoji"` (📅) or `"dataview"` (`[due:: …]`).
    pub format: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for TaskSettings {
    fn default() -> Self {
        Self { reminders: true, new_occurrence: "above".into(), format: "emoji".into(), other: toml::Table::new() }
    }
}
