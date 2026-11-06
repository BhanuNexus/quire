// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[commands]` in settings.toml: shortcuts people chose, commands pinned
//! to the top of the palette, and macros (0.11, `keymap.rs`).
//!
//! Commands are named by their stable id (`"Bold"`, see
//! `Command::id`); a macro is `"macro:<name>"`. Ids this version doesn't
//! know are kept as they are, so a newer Quire's choices survive a save.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandSettings {
    /// Shortcuts that replace a command's (or a macro's) defaults, as
    /// `"Ctrl+Shift+B"` (Ctrl is ⌘ on macOS). An empty list removes them.
    pub keys: BTreeMap<String, Vec<String>>,
    /// Commands and macros shown first in the command palette, in order.
    pub pinned: Vec<String>,
    /// Named lists of commands run one after another.
    pub macros: Vec<Macro>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// A macro: commands in order, no scripting.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Macro {
    pub name: String,
    /// Command ids, run in this order.
    pub steps: Vec<String>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// The id settings use for a macro.
pub fn macro_id(name: &str) -> String {
    format!("macro:{name}")
}
