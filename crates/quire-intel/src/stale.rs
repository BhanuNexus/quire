// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Which notes may be out of date (design §C.6). Deterministic and offline.
//!
//! ```text
//! age      = days since max(last human edit, `reviewed:` date)
//! half     = `review_every:` | per folder | 180 days
//! s_age    = clamp(age/half − 1, 0, 1)
//! s_links  = clamp(½ · Σ over links whose target changed after this note's last edit of min(1, changed/lines), 0, 1)
//! s_terms  = clamp(0.25 · time-bound phrases, 0, 1)
//! hard     = 1 if `valid_until` is past or `superseded_by` is set
//! score    = max(hard, 0.45·s_age + 0.35·s_links + 0.20·s_terms); flagged at 0.6
//! ```

use chrono::{Datelike, NaiveDate};
use unicode_segmentation::UnicodeSegmentation;

pub const DEFAULT_HALF_DAYS: u32 = 180;
pub const FLAG_AT: f32 = 0.6;

/// Why a note looks stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Not edited or reviewed for this many days.
    Age {
        days: u32,
    },
    /// A linked note changed a lot since this one was edited.
    LinkedChanged {
        target: String,
    },
    /// "currently" (and how often).
    TimeBound {
        phrase: String,
        count: u32,
    },
    ValidUntil {
        date: String,
    },
    Superseded {
        by: String,
    },
}

/// A link of the note and how much its target changed since the note's last edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkChange {
    pub target: String,
    /// Lines of the target added or removed since then (0 if it didn't change).
    pub changed_lines: u32,
    /// The target's length in lines now.
    pub lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input<'a> {
    pub today: NaiveDate,
    /// The last edit by a person (agents' edits don't count).
    pub last_edit: Option<NaiveDate>,
    /// `reviewed:` in front matter.
    pub reviewed: Option<NaiveDate>,
    /// `review_every:` in front matter, or the folder's setting, in days.
    pub half_days: Option<u32>,
    pub valid_until: Option<NaiveDate>,
    pub superseded_by: Option<String>,
    /// The note's prose.
    pub text: &'a str,
    pub links: Vec<LinkChange>,
}

#[derive(Debug, Clone, PartialEq)]
#[expect(clippy::struct_field_names, reason = "score is the formula's name for the total")]
pub struct Score {
    pub score: f32,
    pub s_age: f32,
    pub s_links: f32,
    pub s_terms: f32,
    pub hard: bool,
    /// The strongest reasons first.
    pub reasons: Vec<Reason>,
}

impl Score {
    pub fn flagged(&self, at: f32) -> bool {
        self.score >= at
    }
}

/// `90d`, `12w`, `6m`, `1y` or a plain number of days.
pub fn parse_days(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = s.find(|c: char| !c.is_ascii_digit()).map_or((s.as_str(), "d"), |i| (&s[..i], s[i..].trim()));
    let n: u32 = num.parse().ok()?;
    let mult = match unit {
        "d" | "day" | "days" => 1,
        "w" | "week" | "weeks" => 7,
        "m" | "month" | "months" => 30,
        "y" | "year" | "years" => 365,
        _ => return None,
    };
    (n > 0).then(|| n.saturating_mul(mult))
}

/// A `YYYY-MM-DD` date (times after it are ignored).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim().get(..10)?, "%Y-%m-%d").ok()
}

const PHRASES: &[&str] = &[
    "currently",
    "at the moment",
    "as of",
    "right now",
    "nowadays",
    "these days",
    "this year",
    "this month",
    "this week",
    "latest",
    "recently",
    "so far",
    "upcoming",
    "soon",
    "next year",
];

/// Words that make a sentence present tense, for dated statements.
const PRESENT: &[&str] = &["am", "are", "has", "have", "is", "remains", "remain", "stays", "uses", "costs", "runs", "lives", "works"];

fn phrase_count(lower: &str, p: &str) -> u32 {
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = lower[from..].find(p) {
        let s = from + i;
        let e = s + p.len();
        let before = lower[..s].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = lower[e..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            n += 1;
        }
        from = e;
    }
    n
}

/// Time-bound phrases in `text` with their counts (design §C.6, English):
/// the list above, plus a year at least a year ago in a present-tense sentence.
pub fn time_bound(text: &str, today: NaiveDate) -> Vec<(String, u32)> {
    let lower = text.to_lowercase();
    let mut out: Vec<(String, u32)> = PHRASES.iter().map(|p| ((*p).to_string(), phrase_count(&lower, p))).filter(|(_, n)| *n > 0).collect();
    let mut dated = 0;
    for sent in text.unicode_sentences() {
        let words: Vec<String> = sent.unicode_words().map(str::to_lowercase).collect();
        if !words.iter().any(|w| PRESENT.contains(&w.as_str())) {
            continue;
        }
        let old_year = words.iter().any(|w| w.len() == 4 && w.parse::<i32>().is_ok_and(|y| (1900..=today.year() - 1).contains(&y)));
        if old_year {
            dated += 1;
        }
    }
    if dated > 0 {
        out.push(("a past year in the present tense".into(), dated));
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// The staleness score of a note.
pub fn score(input: &Input<'_>) -> Score {
    let mut reasons = Vec::new();
    let since = match (input.last_edit, input.reviewed) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let age = since.map_or(0, |d| u32::try_from((input.today - d).num_days().max(0)).unwrap_or(u32::MAX));
    let half = input.half_days.filter(|h| *h > 0).unwrap_or(DEFAULT_HALF_DAYS) as f32;
    let s_age = (age as f32 / half - 1.0).clamp(0.0, 1.0);
    let link_sum: f32 =
        input.links.iter().filter(|l| l.changed_lines > 0 && l.lines > 0).map(|l| (l.changed_lines as f32 / l.lines as f32).min(1.0)).sum();
    let s_links = (0.5 * link_sum).clamp(0.0, 1.0);
    let phrases = time_bound(input.text, input.today);
    let n_phrases: u32 = phrases.iter().map(|p| p.1).sum();
    let s_terms = (0.25 * n_phrases as f32).clamp(0.0, 1.0);
    // Reviewed after it expired: the person has seen it (Mark reviewed).
    let expired = input.valid_until.filter(|d| *d < input.today && input.reviewed.is_none_or(|r| r < *d));
    let hard = expired.is_some() || input.superseded_by.is_some();
    if let Some(by) = &input.superseded_by {
        reasons.push(Reason::Superseded { by: by.clone() });
    }
    if let Some(d) = expired {
        reasons.push(Reason::ValidUntil { date: d.format("%Y-%m-%d").to_string() });
    }
    if s_age > 0.0 {
        reasons.push(Reason::Age { days: age });
    }
    let mut changed: Vec<&LinkChange> = input.links.iter().filter(|l| l.changed_lines > 0 && l.lines > 0).collect();
    changed.sort_by(|a, b| (b.changed_lines * a.lines.max(1)).cmp(&(a.changed_lines * b.lines.max(1))));
    // Only links that changed noticeably are worth naming.
    for l in changed.into_iter().filter(|l| l.changed_lines as f32 / l.lines as f32 >= 0.2).take(3) {
        reasons.push(Reason::LinkedChanged { target: l.target.clone() });
    }
    for (phrase, count) in phrases.into_iter().take(3) {
        reasons.push(Reason::TimeBound { phrase, count });
    }
    let soft = 0.45 * s_age + 0.35 * s_links + 0.20 * s_terms;
    Score { score: if hard { 1.0 } else { soft }, s_age, s_links, s_terms, hard, reasons }
}

/// The reason in a sentence, for the Inbox.
pub fn describe(r: &Reason) -> String {
    match r {
        Reason::Age { days } => {
            if *days >= 730 {
                format!("not edited or reviewed for {} years", days / 365)
            } else if *days >= 60 {
                format!("not edited or reviewed for {} months", days / 30)
            } else {
                format!("not edited or reviewed for {days} days")
            }
        }
        Reason::LinkedChanged { target } => format!("linked note “{target}” changed a lot since"),
        Reason::TimeBound { phrase, count } => {
            let times = match count {
                1 => "once".to_string(),
                2 => "twice".to_string(),
                n => format!("{n} times"),
            };
            if phrase.starts_with("a past year") {
                format!("states a past year as current ({times})")
            } else {
                format!("says “{phrase}” {times}")
            }
        }
        Reason::ValidUntil { date } => format!("valid until {date}, which has passed"),
        Reason::Superseded { by } => format!("marked as superseded by {by}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    fn input(text: &str) -> Input<'_> {
        Input {
            today: d("2026-11-03"),
            last_edit: Some(d("2026-10-01")),
            reviewed: None,
            half_days: None,
            valid_until: None,
            superseded_by: None,
            text,
            links: vec![],
        }
    }

    #[test]
    fn the_formula() {
        let fresh = score(&input("Ropes are trees."));
        assert!(fresh.score.abs() < 1e-6 && fresh.reasons.is_empty());
        // 540 days old with a 180-day half life: s_age = 2 → clamped to 1.
        let old = Input { last_edit: Some(d("2025-05-12")), ..input("Ropes are trees.") };
        let s = score(&old);
        assert!((s.s_age - 1.0).abs() < 1e-6);
        assert!((s.score - 0.45).abs() < 1e-6);
        assert_eq!(s.reasons, [Reason::Age { days: 540 }]);
        // 270 days with review_every: 90d → s_age = 2 → 1; reviewed recently → 0.
        let reviewed = Input { last_edit: Some(d("2026-02-06")), reviewed: Some(d("2026-10-30")), half_days: Some(90), ..input("x") };
        assert!(score(&reviewed).s_age.abs() < 1e-6);
        let half = Input { last_edit: Some(d("2026-02-06")), half_days: Some(180), ..input("x") };
        assert!((score(&half).s_age - 0.5).abs() < 1e-6, "270/180 − 1");
        // Links: two targets, one fully rewritten, one half: ½·(1 + 0.5) = 0.75.
        let links = Input {
            links: vec![
                LinkChange { target: "Pricing".into(), changed_lines: 40, lines: 20 },
                LinkChange { target: "Plan".into(), changed_lines: 5, lines: 10 },
                LinkChange { target: "Same".into(), changed_lines: 0, lines: 10 },
            ],
            ..input("x")
        };
        let s = score(&links);
        assert!((s.s_links - 0.75).abs() < 1e-6);
        assert_eq!(s.reasons[0], Reason::LinkedChanged { target: "Pricing".into() });
        // Terms: "currently" ×3, "as of" ×1 → 4 × 0.25 = 1.
        let terms = input("We currently use X. Currently Y. It is currently Z. As of now, fine.");
        let s = score(&terms);
        assert!((s.s_terms - 1.0).abs() < 1e-6);
        assert!(describe(&s.reasons[0]).contains("“currently” 3 times"), "{:?}", s.reasons);
        // All three at once: 0.45 + 0.35·0.75 + 0.2 = 0.9125 → flagged.
        let all = Input { last_edit: Some(d("2024-01-01")), ..Input { text: terms.text, ..links } };
        let s = score(&all);
        assert!((s.score - 0.9125).abs() < 1e-4 && s.flagged(FLAG_AT));
        // Hard rules.
        let gone = Input { valid_until: Some(d("2026-01-01")), ..input("x") };
        assert!((score(&gone).score - 1.0).abs() < 1e-6);
        let sup = Input { superseded_by: Some("[[New]]".into()), ..input("x") };
        assert_eq!(score(&sup).reasons[0], Reason::Superseded { by: "[[New]]".into() });
        let seen = Input { valid_until: Some(d("2026-01-01")), reviewed: Some(d("2026-02-01")), ..input("x") };
        assert!(!score(&seen).hard, "reviewed after it expired");
        let future = Input { valid_until: Some(d("2027-01-01")), ..input("x") };
        assert!(!score(&future).hard);
    }

    #[test]
    fn time_bound_phrases_and_dated_statements() {
        let today = d("2026-11-03");
        let t = time_bound("The price is 10 euros in 2024. In 2026 it is 12. Nowadays it rises. Currencies change.", today);
        assert!(t.contains(&("a past year in the present tense".into(), 1)));
        assert!(t.contains(&("nowadays".into(), 1)));
        assert!(time_bound("Currencies and soonish things.", today).is_empty(), "whole words only");
        assert_eq!(parse_days("90d"), Some(90));
        assert_eq!(parse_days("2 weeks"), Some(14));
        assert_eq!(parse_days("1y"), Some(365));
        assert_eq!(parse_days("45"), Some(45));
        assert_eq!(parse_days("soon"), None);
        assert_eq!(parse_date("2026-11-03T10:00"), Some(today));
        assert_eq!(describe(&Reason::Age { days: 800 }), "not edited or reviewed for 2 years");
        let mut p = PRESENT.to_vec();
        p.sort_unstable();
        let _ = p;
    }
}
