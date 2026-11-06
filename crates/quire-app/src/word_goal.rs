// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A note's word-count goal in the status bar (issue #16).
//!
//! `goal: 1500` in a note's properties shows as `640 / 1,500 words · 43%`,
//! and "✓ goal" once it is reached. Words are counted as everywhere else
//! ([`quire_markdown::Stats`]).

use quire_intel::format_number;

/// What the status bar shows when the goal is reached (in the success
/// colour), if the font has the check mark; [`REACHED_TEXT`] otherwise.
pub const REACHED: &str = "✓ goal";
/// [`REACHED`] without the check mark.
pub const REACHED_TEXT: &str = "goal reached";

/// The word count part of the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordCount {
    /// `640 words`, `640 / 1,500 words · 43%`, or `1,520 / 1,500 words`
    /// once the goal is reached (then [`REACHED`] follows it).
    pub text: String,
    /// The note has a goal and at least that many words.
    pub reached: bool,
}

/// A whole number with thousands grouped (`1,500`), as the calculator writes it.
pub fn count(n: usize) -> String {
    format_number(n as f64)
}

/// How far `words` is towards `goal`, rounded, but never 100% before the
/// goal is reached.
pub fn percent(words: usize, goal: usize) -> usize {
    let goal = goal.max(1) as u128;
    let p = (words as u128 * 100 + goal / 2) / goal;
    let p = if (words as u128) < goal { p.min(99) } else { p };
    usize::try_from(p).unwrap_or(usize::MAX)
}

/// The status bar's word count for `words` and the note's goal, if any.
pub fn word_count(words: usize, goal: Option<usize>) -> WordCount {
    match goal.filter(|g| *g > 0) {
        None => WordCount { text: format!("{} words", count(words)), reached: false },
        Some(g) if words >= g => WordCount { text: format!("{} / {} words", count(words), count(g)), reached: true },
        Some(g) => WordCount { text: format!("{} / {} words · {}%", count(words), count(g), percent(words, g)), reached: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_text() {
        assert_eq!(word_count(640, Some(1500)), WordCount { text: "640 / 1,500 words · 43%".into(), reached: false });
        assert_eq!(word_count(0, Some(1500)).text, "0 / 1,500 words · 0%");
        assert_eq!(word_count(1499, Some(1500)).text, "1,499 / 1,500 words · 99%", "never 100% too early");
        assert_eq!(word_count(1500, Some(1500)), WordCount { text: "1,500 / 1,500 words".into(), reached: true });
        assert_eq!(word_count(12_345, Some(1500)), WordCount { text: "12,345 / 1,500 words".into(), reached: true });
        assert_eq!(word_count(1234, None), WordCount { text: "1,234 words".into(), reached: false });
        assert_eq!(word_count(5, Some(0)).text, "5 words", "no goal");
        assert_eq!(percent(usize::MAX, 1), usize::MAX, "no overflow");
    }

    #[test]
    fn a_goal_from_the_properties() {
        let src = "---\ngoal: 10\n---\nOne two three four five.\n";
        let ex = quire_markdown::extract(src);
        let goal = ex.front_matter.as_ref().and_then(quire_markdown::FrontMatter::word_goal);
        assert_eq!(goal, Some(10));
        assert_eq!(ex.stats.words, 5, "the properties aren't counted");
        assert_eq!(word_count(ex.stats.words, goal).text, "5 / 10 words · 50%");
        let src = "---\ngoal: soon\n---\nOne two.\n";
        let ex = quire_markdown::extract(src);
        assert_eq!(ex.front_matter.as_ref().and_then(quire_markdown::FrontMatter::word_goal), None);
    }
}
