// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What was open last time, so the app reopens exactly where you left off.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OpenTab {
    pub path: String,
    pub caret: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Session {
    pub vault: Option<PathBuf>,
    pub tabs: Vec<OpenTab>,
    pub active: usize,
    pub scratchpad: String,
    /// Recently opened notes, most recent first.
    pub recent: Vec<String>,
}

impl Session {
    pub fn path() -> Option<PathBuf> {
        crate::settings::config_dir().map(|d| d.join("session.json"))
    }

    pub fn load_from(p: &Path) -> Self {
        std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save_to(&self, p: &Path) -> std::io::Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)
    }
}
