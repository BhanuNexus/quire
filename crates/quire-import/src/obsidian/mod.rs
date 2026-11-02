// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian compatibility (design §E.5): what Obsidian plugins keep in a
//! vault, read by Quire without running anything.
//!
//! * [`base`] — Obsidian Bases (`.base` YAML) as `quire-view` specs;
//! * [`kanban`] — Kanban plugin boards (`kanban-plugin: basic`), read from
//!   their `## Column` headings and `- [ ] card` lists, with card moves
//!   written back as moved list items.
//!
//! Dataview and Tasks query blocks are translated by
//! `quire_index::views::{dataview, tasks_query}`, next to the views they
//! become. `DataviewJS` is never run.

pub mod base;
pub mod kanban;

/// What converts, for Settings → About → Compatibility.
pub const COMPATIBILITY: &[(&str, &str, &str)] = &[
    (
        "Dataview queries",
        "TABLE, LIST and TASK with FROM folders and #tags, WHERE (and, comparisons, contains, date(today)), SORT, LIMIT",
        "DataviewJS, CALENDAR, GROUP BY expressions, FLATTEN, other functions",
    ),
    (
        "Tasks queries",
        "done / not done, due, scheduled, starts and done dates, has/no date, path, description and tag includes, priority is, is recurring, sort by, group by due, path or priority, limit",
        "custom JavaScript filters, boolean combinations, explain",
    ),
    (
        "Tasks emoji and Dataview task fields",
        "📅 ⏳ 🛫 ✅ ➕ 🔁 priorities, [due:: …] and the rest, kept as written",
        "🆔 ⛔ 🏁 are kept but not used",
    ),
    (
        "Kanban boards",
        "columns from ## headings, cards from - [ ] items; dragging moves the item in the file",
        "the archive section, card metadata settings",
    ),
    (
        "Bases (.base)",
        "table and cards views; and / or filters with file.inFolder, file.hasTag and comparisons; order, sort, limit",
        "formulas, summaries, not, other functions",
    ),
    ("JSON Canvas (.canvas)", "the canvas editor", "—"),
    ("Excalidraw (.excalidraw.md)", "shown as Markdown", "drawing"),
];
