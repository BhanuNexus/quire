// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! When task reminders (`⏰ 2026-11-30 09:00`) fire (design §E.2). The
//! schedule is rebuilt from the index; firing and snoozing live in the app.

use chrono::NaiveDateTime;

/// One reminder: when, and which task line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reminder {
    pub at: NaiveDateTime,
    pub path: String,
    /// Byte offset of the task's checkbox.
    pub offset: usize,
    pub text: String,
}

/// Reminders of open tasks, soonest first.
pub fn schedule<'a>(tasks: impl IntoIterator<Item = (&'a str, &'a quire_markdown::Task)>) -> Vec<Reminder> {
    let mut v: Vec<Reminder> = tasks
        .into_iter()
        .filter(|(_, t)| !t.done)
        .filter_map(|(path, t)| {
            let at = NaiveDateTime::parse_from_str(t.reminder.as_deref()?, "%Y-%m-%d %H:%M").ok()?;
            Some(Reminder { at, path: path.to_string(), offset: t.offset, text: t.text.clone() })
        })
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soonest_first() {
        let t = |r: &str| quire_markdown::Task { reminder: Some(r.into()), ..Default::default() };
        let (a, b) = (t("2026-11-30 09:00"), t("2026-11-02 18:30"));
        let s = schedule([("A.md", &a), ("B.md", &b), ("C.md", &quire_markdown::Task::default())]);
        assert_eq!(s.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["B.md", "A.md"]);
    }
}
