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
pub mod expr;
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
        "due, scheduled, start, done, created, repeat and priority, as emoji or [due:: …] fields, kept as written",
        "task ids, dependencies (blocked by) and on-completion actions are kept but not used",
    ),
    (
        "Kanban boards",
        "columns from ## headings, cards from - [ ] items; dragging moves the item in the file",
        "the archive section, card metadata settings",
    ),
    (
        "Bases (.base)",
        "table, cards, kanban and map views; groupBy; filters (and / or / not, file.inFolder, file.hasTag, comparisons, dates); \
         formulas with arithmetic, if, date, now, today, duration, file fields and string, number, date and list functions; summaries \
         (Sum, Average, Earliest… and the base's own); order, sort, limit; Quire's own card order, swimlanes and column colours",
        "other view types, image, icon, html and other functions not listed",
    ),
    ("JSON Canvas (.canvas)", "the canvas editor", "—"),
    ("Excalidraw (.excalidraw.md)", "shown as Markdown", "drawing"),
];
