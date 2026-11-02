// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `canvas_read` and `canvas_apply` (design §G.1, §4.2). Owned by WS-G;
//! registered at the end of `ToolRegistry::standard`.

use super::Tool;

/// The canvas tools, in their fixed order. None yet.
pub fn tools() -> Vec<Box<dyn Tool>> {
    Vec::new()
}
