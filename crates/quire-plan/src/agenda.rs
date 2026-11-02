// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agenda: overdue tasks, today's, the next seven days, and today's
//! time blocks (design §E.2).

use chrono::{Duration, NaiveDate, NaiveTime};
use quire_markdown::Task;
use quire_markdown::tasks::priority_rank;

use crate::recur;

/// Which date put a task on the agenda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Due,
    Scheduled,
    Start,
}

impl When {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::Scheduled => "scheduled",
            Self::Start => "starts",
        }
    }
}

/// A task with the note it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgendaItem {
    /// Vault-relative path of the note.
    pub path: String,
    pub task: Task,
    /// The date that put it on the agenda (due, scheduled or start).
    pub date: NaiveDate,
    pub when: When,
    /// For a recurring task: the occurrence after this one.
    pub next: Option<NaiveDate>,
}

/// A time block (`14:00 - 15:30 Write report`) on today's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub path: String,
    pub task: Task,
    pub start: NaiveTime,
    pub end: NaiveTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Agenda {
    /// Due before today, oldest first.
    pub overdue: Vec<AgendaItem>,
    /// Due, scheduled or starting today, plus scheduled earlier and still open.
    pub today: Vec<AgendaItem>,
    /// The next seven days, by date.
    pub upcoming: Vec<AgendaItem>,
    /// Today's time blocks, by start time.
    pub blocks: Vec<Block>,
}

impl Agenda {
    pub fn is_empty(&self) -> bool {
        self.overdue.is_empty() && self.today.is_empty() && self.upcoming.is_empty() && self.blocks.is_empty()
    }
}

/// `YYYY-MM-DD` (a time after it is ignored).
pub fn parse_date(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?.get(..10)?, "%Y-%m-%d").ok()
}

/// `H:MM` or `HH:MM`.
pub fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// The date a task is planned for, and which field it came from.
pub fn task_date(t: &Task) -> Option<(NaiveDate, When)> {
    parse_date(t.due.as_deref())
        .map(|d| (d, When::Due))
        .or_else(|| parse_date(t.scheduled.as_deref()).map(|d| (d, When::Scheduled)))
        .or_else(|| parse_date(t.start.as_deref()).map(|d| (d, When::Start)))
}

/// A task's time block as times; a block without an end lasts 30 minutes.
pub fn block_times(t: &Task) -> Option<(NaiveTime, NaiveTime)> {
    let (a, b) = t.time_block.as_ref()?;
    let start = parse_time(a)?;
    let end = parse_time(b).filter(|e| *e > start).unwrap_or_else(|| start + Duration::minutes(30));
    Some((start, end))
}

/// Build the agenda for `today` from tasks. `daily_note` is today's daily
/// note: its time blocks go on the timeline, done or not.
pub fn build<'a>(tasks: impl IntoIterator<Item = (&'a str, &'a Task)>, today: NaiveDate, daily_note: Option<&str>) -> Agenda {
    let mut agenda = Agenda::default();
    for (path, task) in tasks {
        if let Some((start, end)) = block_times(task)
            && (daily_note == Some(path) || task_date(task).is_some_and(|(d, _)| d == today))
        {
            agenda.blocks.push(Block { path: path.to_string(), task: task.clone(), start, end });
        }
        if task.done {
            continue;
        }
        let Some((date, when)) = task_date(task) else { continue };
        let next = task.recurrence.as_deref().and_then(|r| recur::parse(r).ok()).and_then(|r| recur::next_occurrence(&r, date));
        let item = AgendaItem { path: path.to_string(), task: task.clone(), date, when, next };
        let days = (date - today).num_days();
        match (days, when) {
            (..0, When::Due) => agenda.overdue.push(item),
            (..0, When::Scheduled) | (0, _) => agenda.today.push(item),
            (1..=7, _) => agenda.upcoming.push(item),
            _ => {}
        }
    }
    let by_date = |a: &AgendaItem, b: &AgendaItem| {
        a.date
            .cmp(&b.date)
            .then(priority_rank(a.task.priority).cmp(&priority_rank(b.task.priority)))
            .then_with(|| a.task.text.cmp(&b.task.text))
    };
    agenda.overdue.sort_by(by_date);
    agenda.upcoming.sort_by(by_date);
    agenda.today.sort_by(|a, b| {
        let t = |i: &AgendaItem| block_times(&i.task).map(|(s, _)| s);
        (t(a).is_none(), t(a))
            .cmp(&(t(b).is_none(), t(b)))
            .then(priority_rank(a.task.priority).cmp(&priority_rank(b.task.priority)))
            .then_with(|| a.task.text.cmp(&b.task.text))
    });
    agenda.blocks.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    agenda
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn groups_by_date() {
        let task = |due: &str| Task { due: Some(due.into()), ..Task::default() };
        let (a, b, c, e) = (task("2026-11-01"), task("2026-11-02"), task("2026-11-05"), task("2026-12-24"));
        let sched = Task { scheduled: Some("2026-10-30".into()), text: "sched".into(), ..Task::default() };
        let start_past = Task { start: Some("2026-10-30".into()), ..Task::default() };
        let done = Task { done: true, due: Some("2026-11-02".into()), ..Task::default() };
        let ag = build(
            [
                ("A.md", &a),
                ("B.md", &b),
                ("C.md", &c),
                ("D.md", &e),
                ("E.md", &Task::default()),
                ("F.md", &sched),
                ("G.md", &start_past),
                ("H.md", &done),
            ],
            d("2026-11-02"),
            None,
        );
        assert_eq!((ag.overdue.len(), ag.today.len(), ag.upcoming.len()), (1, 2, 1));
        assert_eq!(ag.today.iter().map(|i| i.path.as_str()).collect::<Vec<_>>(), ["B.md", "F.md"]);
        assert_eq!(ag.today[1].when, When::Scheduled);
    }

    #[test]
    fn recurring_tasks_show_their_next_date_and_blocks_fill_the_timeline() {
        let rent = Task { due: Some("2026-11-02".into()), recurrence: Some("every month".into()), ..Task::default() };
        let block = Task { time_block: Some(("14:00".into(), "15:30".into())), text: "Write".into(), ..Task::default() };
        let early = Task { time_block: Some(("9:00".into(), String::new())), done: true, ..Task::default() };
        let elsewhere = Task { time_block: Some(("8:00".into(), String::new())), ..Task::default() };
        let ag = build(
            [("R.md", &rent), ("Daily/2026-11-02.md", &block), ("Daily/2026-11-02.md", &early), ("X.md", &elsewhere)],
            d("2026-11-02"),
            Some("Daily/2026-11-02.md"),
        );
        assert_eq!(ag.today[0].next, Some(d("2026-12-02")));
        assert_eq!(ag.blocks.len(), 2);
        assert_eq!(ag.blocks[0].end, parse_time("9:30").unwrap());
        assert_eq!(ag.blocks[1].start, parse_time("14:00").unwrap());
    }
}
