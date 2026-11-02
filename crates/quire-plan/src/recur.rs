// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recurrence rules (design §E.2): `every month on the 1st`, `every 2 weeks
//! on Mon, Thu`, or `RRULE:` text, as one [`Rule`] type. Parsing and the
//! next-occurrence calculation arrive with WS-E.

use chrono::{NaiveDate, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub freq: Freq,
    pub interval: u32,
    /// (ordinal within the month or year, weekday); `None` = every such weekday.
    pub by_day: Vec<(Option<i8>, Weekday)>,
    /// Days of the month; `-1` is the last day.
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<u8>,
    pub until: Option<NaiveDate>,
    /// Count from the completion date instead of the due date.
    pub when_done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurError {
    /// The text isn't a rule this version understands.
    Unsupported(String),
}

impl std::fmt::Display for RecurError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(t) => write!(f, "can't read the repeat rule “{t}” yet"),
        }
    }
}

impl std::error::Error for RecurError {}

/// Parse natural-language or `RRULE:` text.
pub fn parse(text: &str) -> Result<Rule, RecurError> {
    Err(RecurError::Unsupported(text.trim().to_string()))
}

/// The first date the rule produces strictly after `base`; `None` when the
/// rule has ended (or, today, always: WS-E implements it).
pub fn next_occurrence(rule: &Rule, base: NaiveDate) -> Option<NaiveDate> {
    let _ = (rule, base);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_say_so() {
        let e = parse("every month on the 1st").unwrap_err();
        assert!(e.to_string().contains("every month on the 1st"));
        let rule =
            Rule { freq: Freq::Daily, interval: 1, by_day: vec![], by_month_day: vec![], by_month: vec![], until: None, when_done: false };
        assert_eq!(next_occurrence(&rule, NaiveDate::default()), None);
    }
}
