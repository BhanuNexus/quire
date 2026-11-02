// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agenda: overdue tasks, today's, and the next seven days (design §E.2).

use chrono::NaiveDate;
use quire_markdown::Task;

/// A task with the note it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgendaItem {
    /// Vault-relative path of the note.
    pub path: String,
    pub task: Task,
    /// The date that put it on the agenda (due, scheduled or start).
    pub date: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Agenda {
    pub overdue: Vec<AgendaItem>,
    pub today: Vec<AgendaItem>,
    /// The next seven days, by date.
    pub upcoming: Vec<AgendaItem>,
}

/// Build the agenda for `today` from every open task. Tasks have no dates
/// until the task parser lands (WS-E), so this is empty for now.
pub fn build<'a>(tasks: impl IntoIterator<Item = (&'a str, &'a Task)>, today: NaiveDate) -> Agenda {
    let _ = today;
    let mut agenda = Agenda::default();
    for (path, task) in tasks {
        let Some(date) = task.due.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else { continue };
        if task.done {
            continue;
        }
        let item = AgendaItem { path: path.to_string(), task: task.clone(), date };
        match date.cmp(&today) {
            std::cmp::Ordering::Less => agenda.overdue.push(item),
            std::cmp::Ordering::Equal => agenda.today.push(item),
            std::cmp::Ordering::Greater if (date - today).num_days() <= 7 => agenda.upcoming.push(item),
            std::cmp::Ordering::Greater => {}
        }
    }
    agenda.upcoming.sort_by_key(|i| i.date);
    agenda
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_by_due_date() {
        let task = |due: &str| Task { due: Some(due.into()), ..Task::default() };
        let (a, b, c, d) = (task("2026-11-01"), task("2026-11-02"), task("2026-11-05"), task("2026-12-24"));
        let today = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let ag = build([("A.md", &a), ("B.md", &b), ("C.md", &c), ("D.md", &d), ("E.md", &Task::default())], today);
        assert_eq!((ag.overdue.len(), ag.today.len(), ag.upcoming.len()), (1, 1, 1));
    }
}
