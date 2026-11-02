// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task metadata written after a task's text.
//!
//! Dates, recurrence, priority and reminders, in the Obsidian Tasks emoji
//! format (`📅 2026-12-01`) or as Dataview fields (`[due:: 2026-12-01]`).
//! See design §E.2.
//!
//! [`extract`](crate::extract()) calls [`parse_meta`] for every task. Dates
//! stay strings here; `quire-plan` parses them. Until the parser lands (WS-E)
//! every field stays `None`.

use crate::Task;

/// Fill `task`'s metadata fields from its source line. `line` is the whole
/// line the task's checkbox is on, `marker` the byte offset of `[ ]` within it.
pub fn parse_meta(line: &str, marker: usize, task: &mut Task) {
    // WS-E: parse the trailing fields into `task` and set `task.meta_range`.
    let _ = (line, marker, task);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_start_empty() {
        let mut t = Task { done: false, text: "Pay rent".into(), offset: 2, ..Task::default() };
        parse_meta("- [ ] Pay rent 📅 2026-12-01", 2, &mut t);
        assert_eq!(t.due, None);
        assert_eq!(t.meta_range, None);
    }
}
