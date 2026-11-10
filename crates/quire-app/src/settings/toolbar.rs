// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[toolbar]` in settings.toml: the formatting toolbar above the editor
//! and the small one next to a selection (0.11). Both are off by default.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolbarSettings {
    /// The toolbar above the editor.
    pub show: bool,
    /// A compact toolbar next to the selection while text is selected.
    pub floating: bool,
    /// The buttons, in order. Missing: the default set
    /// ([`default_items`]). Present and empty: no buttons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ToolbarItem>>,
    /// 0.12: the buttons of the toolbar next to a selection, in order.
    /// Missing: the default set ([`FLOATING_ITEMS`]); present and empty: no
    /// buttons (the bar isn't shown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating_items: Option<Vec<ToolbarItem>>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// Which toolbar's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bar {
    /// The toolbar above the editor (`items`).
    #[default]
    Main,
    /// The toolbar next to a selection (`floating_items`).
    Floating,
}

/// One button: a command id (`"Bold"`), a macro (`"macro:Clean up"`) or
/// `"|"` for a separator, with an optional icon (an emoji or a few
/// letters) and label (the tooltip; the command's title when empty).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolbarItem {
    pub command: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl ToolbarItem {
    pub fn new(command: &str) -> Self {
        Self { command: command.into(), ..Self::default() }
    }

    pub fn is_separator(&self) -> bool {
        self.command == SEPARATOR
    }
}

/// The `command` of a separator.
pub const SEPARATOR: &str = "|";

/// The buttons of a new toolbar. `menu:headings` is the heading-level menu.
pub const DEFAULT_ITEMS: &[&str] = &[
    "Bold",
    "Italic",
    "Strikethrough",
    "Highlight",
    "InlineCode",
    SEPARATOR,
    "InsertMarkdownLink",
    "InsertLink",
    SEPARATOR,
    "menu:headings",
    "BulletList",
    "NumberedList",
    "ToggleTask",
    "Quote",
    "InsertCallout",
    SEPARATOR,
    "InsertTable",
    "InlineMath",
    "InsertFootnote",
    "InsertRule",
    "InsertAttachment",
];

/// The buttons of the toolbar next to a selection.
pub const FLOATING_ITEMS: &[&str] =
    &["Bold", "Italic", "Strikethrough", "Highlight", "InlineCode", "InlineMath", "InsertMarkdownLink", "InsertLink"];

pub fn default_items() -> Vec<ToolbarItem> {
    DEFAULT_ITEMS.iter().map(|c| ToolbarItem::new(c)).collect()
}

/// The buttons of a new floating toolbar.
pub fn default_floating_items() -> Vec<ToolbarItem> {
    FLOATING_ITEMS.iter().map(|c| ToolbarItem::new(c)).collect()
}

impl ToolbarSettings {
    /// The buttons to show.
    pub fn items(&self) -> Vec<ToolbarItem> {
        self.items.clone().unwrap_or_else(default_items)
    }

    /// The buttons of the toolbar next to a selection.
    pub fn floating_items(&self) -> Vec<ToolbarItem> {
        self.floating_items.clone().unwrap_or_else(default_floating_items)
    }

    /// The buttons of `bar`.
    pub fn items_of(&self, bar: Bar) -> Vec<ToolbarItem> {
        match bar {
            Bar::Main => self.items(),
            Bar::Floating => self.floating_items(),
        }
    }

    /// The stored list of `bar` (`None`: the defaults).
    pub const fn stored_mut(&mut self, bar: Bar) -> &mut Option<Vec<ToolbarItem>> {
        match bar {
            Bar::Main => &mut self.items,
            Bar::Floating => &mut self.floating_items,
        }
    }

    /// Every stored button of both bars (to rename or remove a macro's).
    pub fn all_stored_mut(&mut self) -> impl Iterator<Item = &mut Vec<ToolbarItem>> {
        self.items.iter_mut().chain(self.floating_items.iter_mut())
    }
}
