// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[appearance]` in settings.toml: themes and accent colour (design §F.4).
//!
//! Owned by WS-F. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Theme file name in `<config>/themes/`; empty for the built-in theme.
    pub theme_file: String,
    /// Accent colour `#RRGGBB`; empty for the theme's.
    pub accent: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self { theme_file: String::new(), accent: String::new(), other: toml::Table::new() }
    }
}
