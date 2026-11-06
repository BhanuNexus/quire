// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planning: tasks with dates and recurrence, the agenda, reminders,
//! periodic notes and spaced repetition (design 0.9 §E). Pure logic over
//! parsed notes; no I/O and no UI.
//!
//! * [`recur`] — recurrence rules (an RFC 5545 subset) and next occurrences
//! * [`agenda`] — overdue, today and upcoming tasks
//! * [`reminders`] — when task reminders fire
//! * [`periodic`] — daily, weekly, monthly and yearly notes
//! * [`srs`] — flashcard review logs and the FSRS scheduler
//! * [`tasks`] — completing tasks, recurring ones included
//! * [`task_note`] — task notes, TaskNotes' property names, `[tasks]` prefs
//! * [`time`] — time entries, durations and the weekly report
//! * [`timer`] — the running timer and pomodoro cycles

pub mod agenda;
pub mod periodic;
pub mod recur;
pub mod reminders;
pub mod srs;
pub mod task_note;
pub mod tasks;
pub mod time;
pub mod timer;
