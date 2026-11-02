// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Editor decorations: how features draw on a note without owning the
//! editor (design §6.1 F0.12).
//!
//! A feature returns [`Decoration`]s for a document (tinted or struck-through
//! lines, ghost lines with action buttons, or a widget of its own under a
//! line). The desktop paints them; a click on an action comes back as a
//! [`DecorationEvent`], routed by the action's [`Owner`] to that feature's
//! `decoration_event`. Lines are 0-based line indices of the current buffer.

use std::ops::Range;

use crate::commands::Owner;
use crate::document::DocId;

/// A clickable action on a decoration. `id` means whatever the owner wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionId {
    pub owner: Owner,
    pub id: u64,
}

/// A feature-drawn widget under a line (the desktop asks the owner's
/// registered widget painter to draw it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WidgetId {
    pub owner: Owner,
    pub id: u64,
}

/// What a tint means; the theme picks the colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Proposed insertion (green).
    Added,
    /// Proposed removal (red).
    Removed,
    /// Conflicts with the person's own edits (amber).
    Conflict,
    /// Needs attention: drift, staleness (amber, lighter).
    Warning,
    /// Neutral highlight.
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecorationKind {
    /// Tint the background of the lines.
    Tint(Tone),
    /// Strike the lines through.
    Strike,
    /// Read-only lines drawn after `after_line` (not part of the text), with
    /// buttons. `tone` colours them.
    Ghost { after_line: usize, lines: Vec<String>, tone: Tone, actions: Vec<(ActionId, String)> },
    /// A widget the owner draws after `after_line`.
    Widget { after_line: usize, id: WidgetId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoration {
    /// The lines it applies to. For ghosts and widgets: `after_line..after_line + 1`.
    pub lines: Range<usize>,
    pub kind: DecorationKind,
}

impl Decoration {
    pub const fn tint(lines: Range<usize>, tone: Tone) -> Self {
        Self { lines, kind: DecorationKind::Tint(tone) }
    }

    pub const fn strike(lines: Range<usize>) -> Self {
        Self { lines, kind: DecorationKind::Strike }
    }

    pub fn ghost(after_line: usize, lines: Vec<String>, tone: Tone, actions: Vec<(ActionId, String)>) -> Self {
        Self { lines: after_line..after_line + 1, kind: DecorationKind::Ghost { after_line, lines, tone, actions } }
    }

    pub const fn widget(after_line: usize, id: WidgetId) -> Self {
        Self { lines: after_line..after_line + 1, kind: DecorationKind::Widget { after_line, id } }
    }
}

/// A click on a decoration's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecorationEvent {
    pub doc: DocId,
    pub action: ActionId,
    /// The line the decoration was anchored to.
    pub line: usize,
}

/// One item in the status bar contributed by a feature: the inbox badge,
/// Vim mode, due cards, the engine's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItem {
    pub owner: Owner,
    pub text: String,
    pub tooltip: String,
    /// Run on click.
    pub command: Option<crate::Command>,
    /// Draw it in the warning colour.
    pub warn: bool,
}
