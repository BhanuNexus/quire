// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[editing]` in settings.toml: editing and navigation (design §F).
//!
//! Named `editing` because `editor` (fast or classic) already exists.
//!
//! Owned by WS-F. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditingSettings {
    /// Vim keys in the editor.
    pub vim: bool,
    /// `"reuse"` (a preview tab is replaced) or `"new"`.
    pub tab_mode: String,
    /// Show `title:` instead of file names.
    pub display_titles: bool,
    /// Note to open when no session is restored.
    pub homepage: String,
    /// The Files sidebar shows folders and notes side by side.
    pub two_pane: bool,
    /// Where Ctrl+N puts a new note: `"current"` (the open note's folder,
    /// so the folder's template applies) or `"root"` (the vault's top).
    pub new_note_folder: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for EditingSettings {
    fn default() -> Self {
        Self {
            vim: false,
            tab_mode: "reuse".into(),
            display_titles: false,
            homepage: String::new(),
            two_pane: false,
            new_note_folder: "current".into(),
            other: toml::Table::new(),
        }
    }
}
