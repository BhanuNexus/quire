// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hunk review: an edit proposal rebased on the note as it is now, split
//! into hunks the person accepts, rejects or edits one by one (design §A.4).
//!
//! Everything here is pure text work; [`crate::AgentStore`] loads and saves
//! the proposal around it.
//!
//! * The hunks of a change come from a three-way merge of the agent's base
//!   (what it read), the current text (the open note's buffer, or the file)
//!   and the agent's text. Lines only the agent changed are normal hunks;
//!   lines both changed differently are **conflict** hunks ("you changed
//!   this after the agent read it"). Lines only the person changed are left
//!   alone. When the current text equals the base, every hunk is normal.
//! * A hunk id depends only on the path and the hunk's old and new lines,
//!   so a decision stays attached to its hunk when the note is rebased.
//! * Deciding gives three texts: the note with the accepted hunks applied,
//!   and the proposal's new base and new text, so the hunks still pending
//!   come back unchanged (conflicts stay conflicts) at the next review.
//!
//! Lines keep their line endings, so joining lines rebuilds a text exactly.

use std::collections::HashMap;
use std::ops::Range;

use quire_text::merge::{Region, merge3_regions};
use quire_vault::{ContentHash, NotePath};

use crate::proposal::{Hunk, HunkAction, HunkDecision};

/// The lines of `text`, each with its line ending.
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn owned(lines: &[String]) -> Vec<String> {
    lines.to_vec()
}

/// `h_` + 12 hex of SHA-256 over the path, the old lines and the new lines.
///
/// The `n`th repeat of the same change in one file (`n > 0`) gets its
/// number mixed in, so two identical hunks can be decided separately.
pub fn hunk_id(path: &NotePath, old: &[String], new: &[String], n: usize) -> String {
    let mut s = String::with_capacity(64);
    s.push_str(path.as_str());
    s.push('\0');
    for l in old {
        s.push_str(l);
    }
    s.push('\0');
    for l in new {
        s.push_str(l);
    }
    if n > 0 {
        s.push('\0');
        s.push_str(&n.to_string());
    }
    format!("h_{}", &ContentHash::of(&s).hex()[..12])
}

/// The hunks that turn `current` into the agent's `new` text, rebased from
/// `base` (the text the agent read), in document order.
pub fn hunks(path: &NotePath, base: &str, current: &str, new: &str) -> Vec<Hunk> {
    let mine = if current == base { base } else { current };
    let regions = merge3_regions(base, mine, new);
    let mut out = Vec::new();
    let mut pos: u32 = 0;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut push = |pos: u32, base_lines: &[String], old: &[String], new: &[String], conflict: bool, out: &mut Vec<Hunk>| {
        let key = hunk_id(path, old, new, 0);
        let n = seen.entry(key).or_default();
        let id = hunk_id(path, old, new, *n);
        *n += 1;
        let len = u32::try_from(old.len()).unwrap_or(u32::MAX);
        out.push(Hunk {
            id,
            range_in_current: pos..pos.saturating_add(len),
            old_lines: owned(old),
            new_lines: owned(new),
            base_lines: owned(base_lines),
            conflict,
        });
    };
    for r in &regions {
        let len = |l: &[String]| u32::try_from(l.len()).unwrap_or(u32::MAX);
        match r {
            Region::Same(l) | Region::Both(l) => pos = pos.saturating_add(len(l)),
            Region::Mine { mine, .. } => pos = pos.saturating_add(len(mine)),
            Region::Theirs { base, theirs } => {
                push(pos, base, base, theirs, false, &mut out);
                pos = pos.saturating_add(len(base));
            }
            Region::Conflict { base, mine, theirs } => {
                push(pos, base, mine, theirs, true, &mut out);
                pos = pos.saturating_add(len(mine));
            }
        }
    }
    out
}

/// What happens to one hunk when a decision is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Accept,
    /// Accept with the person's own text instead of the agent's.
    Edit(String),
    Reject,
    /// Not decided yet.
    Pending,
}

impl Choice {
    /// The choice a stored decision stands for.
    pub fn of(d: &HunkDecision) -> Self {
        match d.action {
            HunkAction::Accepted => Self::Accept,
            HunkAction::Rejected => Self::Reject,
            HunkAction::Edited => Self::Edit(d.text.clone().unwrap_or_default()),
        }
    }
}

/// The result of applying choices to the current text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The current text with the accepted (and edited) hunks applied.
    pub text: String,
    /// The proposal's new base: what the agent's pending hunks change.
    pub base: String,
    /// The proposal's new text: `text` plus the pending hunks.
    pub new_text: String,
    pub accepted: usize,
    pub rejected: usize,
    pub pending: usize,
    /// The hunks still pending, with their lines in `text`. Reviewing `text`
    /// again gives exactly these (see [`ReviewCache`]), so their ids stay put.
    pub remaining: Vec<Hunk>,
}

/// The edited text as lines; it ends with a newline when the agent's lines did.
fn edited_lines(text: &str, like: &[String]) -> Vec<String> {
    let mut t = text.to_string();
    let want_nl = like.last().is_none_or(|l| l.ends_with('\n'));
    if want_nl && !t.is_empty() && !t.ends_with('\n') {
        t.push('\n');
    }
    split_lines(&t).into_iter().map(str::to_string).collect()
}

/// Apply `choice(hunk)` to every hunk of `current` (hunks as [`hunks`]
/// returned them, in document order).
pub fn apply(current: &str, hunks: &[Hunk], choice: impl Fn(&Hunk) -> Choice) -> Applied {
    let lines = split_lines(current);
    let (mut text, mut base, mut new_text) = (String::new(), String::new(), String::new());
    let (mut accepted, mut rejected, mut pending) = (0, 0, 0);
    let mut remaining = Vec::new();
    // Lines written to `text` so far.
    let mut out_lines = 0_usize;
    let mut cursor = 0_usize;
    let mut order: Vec<&Hunk> = hunks.iter().collect();
    order.sort_by_key(|h| (h.range_in_current.start, h.range_in_current.end));
    let push = |s: &mut String, ls: &[String]| ls.iter().for_each(|l| s.push_str(l));
    for h in order {
        let Range { start, end } = h.range_in_current;
        let (start, end) = (usize::try_from(start).unwrap_or(usize::MAX), usize::try_from(end).unwrap_or(usize::MAX));
        // A hunk that no longer fits (shouldn't happen) is skipped, never half-applied.
        if start < cursor || end > lines.len() || start > end {
            continue;
        }
        for l in &lines[cursor..start] {
            text.push_str(l);
            base.push_str(l);
            new_text.push_str(l);
        }
        out_lines += start - cursor;
        let c = choice(h);
        out_lines += match &c {
            Choice::Accept => h.new_lines.len(),
            Choice::Edit(t) => edited_lines(t, &h.new_lines).len(),
            Choice::Reject | Choice::Pending => h.old_lines.len(),
        };
        match c {
            Choice::Accept => {
                accepted += 1;
                push(&mut text, &h.new_lines);
                push(&mut base, &h.new_lines);
                push(&mut new_text, &h.new_lines);
            }
            Choice::Edit(t) => {
                accepted += 1;
                let e = edited_lines(&t, &h.new_lines);
                push(&mut text, &e);
                push(&mut base, &e);
                push(&mut new_text, &e);
            }
            Choice::Reject => {
                rejected += 1;
                push(&mut text, &h.old_lines);
                push(&mut base, &h.old_lines);
                push(&mut new_text, &h.old_lines);
            }
            Choice::Pending => {
                pending += 1;
                let len = h.old_lines.len();
                let at = u32::try_from(out_lines - len).unwrap_or(u32::MAX);
                let mut moved = h.clone();
                moved.range_in_current = at..at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX));
                remaining.push(moved);
                push(&mut text, &h.old_lines);
                // The agent's view of these lines: for a conflict, what it read,
                // so the hunk is still a conflict next time.
                push(&mut base, &h.base_lines);
                push(&mut new_text, &h.new_lines);
            }
        }
        cursor = end;
    }
    for l in &lines[cursor.min(lines.len())..] {
        text.push_str(l);
        base.push_str(l);
        new_text.push_str(l);
    }
    Applied { text, base, new_text, accepted, rejected, pending, remaining }
}

/// The hunks worked out at the last decision, for the text it produced.
///
/// A line diff can align repeated lines differently once some hunks are
/// applied, which would regroup the remaining hunks under new ids. So the
/// remaining hunks are kept with the proposal, and reused while the note
/// is still exactly that text; once the person edits it, the hunks are
/// worked out again from the base (a rebase).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewCache {
    /// Hash of the text the hunks are for.
    pub current: ContentHash,
    pub hunks: Vec<StoredHunk>,
}

/// A [`Hunk`] as stored in a proposal file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredHunk {
    pub id: String,
    /// First line (0-based) in the text the cache is for.
    pub start: u32,
    pub old: Vec<String>,
    pub new: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
}

impl ReviewCache {
    pub fn new(text: &str, hunks: &[Hunk]) -> Self {
        Self {
            current: ContentHash::of(text),
            hunks: hunks
                .iter()
                .map(|h| StoredHunk {
                    id: h.id.clone(),
                    start: h.range_in_current.start,
                    old: h.old_lines.clone(),
                    new: h.new_lines.clone(),
                    base: if h.base_lines == h.old_lines { Vec::new() } else { h.base_lines.clone() },
                    conflict: h.conflict,
                })
                .collect(),
        }
    }

    /// The hunks, if they are for `current` and still fit it.
    pub fn hunks_for(&self, current: &str) -> Option<Vec<Hunk>> {
        if ContentHash::of(current) != self.current {
            return None;
        }
        let lines = split_lines(current);
        let mut out = Vec::with_capacity(self.hunks.len());
        for s in &self.hunks {
            let start = usize::try_from(s.start).ok()?;
            let end = start.checked_add(s.old.len())?;
            if end > lines.len() || lines[start..end].iter().zip(&s.old).any(|(a, b)| *a != b.as_str()) {
                return None;
            }
            let len = u32::try_from(s.old.len()).ok()?;
            out.push(Hunk {
                id: s.id.clone(),
                range_in_current: s.start..s.start + len,
                old_lines: s.old.clone(),
                new_lines: s.new.clone(),
                base_lines: if s.base.is_empty() && !s.conflict { s.old.clone() } else { s.base.clone() },
                conflict: s.conflict,
            });
        }
        Some(out)
    }
}

/// The hunks of a change for `current`: the cached ones when they are for
/// exactly this text, otherwise rebased from `base`.
pub fn review_hunks(path: &NotePath, base: &str, current: &str, new: &str, cache: Option<&ReviewCache>) -> Vec<Hunk> {
    cache.and_then(|c| c.hunks_for(current)).unwrap_or_else(|| hunks(path, base, current, new))
}

/// The choice for each hunk from `decisions` (by hunk id); others pending.
pub fn choices_from(decisions: &[HunkDecision]) -> impl Fn(&Hunk) -> Choice + '_ {
    move |h: &Hunk| decisions.iter().rev().find(|d| d.hunk == h.id).map_or(Choice::Pending, Choice::of)
}

/// The character range (in chars, for the editor) of a line range of `text`:
/// from the start of line `lines.start` to the start of line `lines.end`.
pub fn char_range_of_lines(text: &str, lines: &Range<u32>) -> Range<usize> {
    let mut start = None;
    let mut chars = 0;
    for (i, l) in split_lines(text).iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        if i == lines.start {
            start = Some(chars);
        }
        if i == lines.end {
            return start.unwrap_or(chars)..chars;
        }
        chars += l.chars().count();
    }
    start.unwrap_or(chars)..chars
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> NotePath {
        NotePath::new("Rope Notes.md")
    }

    fn accept_all(current: &str, hs: &[Hunk]) -> String {
        apply(current, hs, |_| Choice::Accept).text
    }

    #[test]
    fn unchanged_base_gives_plain_hunks() {
        let base = "# Rope\nA\nB\nC\nD\n";
        let new = "# Rope\nA\nB2\nC\nD\nE\n";
        let hs = hunks(&p(), base, base, new);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].range_in_current, 2..3);
        assert_eq!(hs[0].old_lines, ["B\n"]);
        assert_eq!(hs[0].new_lines, ["B2\n"]);
        assert_eq!(hs[1].range_in_current, 5..5);
        assert!(hs.iter().all(|h| !h.conflict && h.id.len() == 14 && h.id.starts_with("h_")));
        assert_eq!(accept_all(base, &hs), new);
        assert_eq!(apply(base, &hs, |_| Choice::Reject).text, base);
        let one = apply(base, &hs, |h| if h.id == hs[1].id { Choice::Accept } else { Choice::Pending });
        assert_eq!(one.text, "# Rope\nA\nB\nC\nD\nE\n");
        assert_eq!(one.new_text, new);
        assert_eq!(one.base, one.text);
        assert_eq!((one.accepted, one.pending), (1, 1));
    }

    #[test]
    fn rebase_keeps_human_edits_and_flags_conflicts() {
        let base = "a\nb\nc\nd\ne\nf\n";
        let agent = "a\nB\nc\nd\ne\nF\n";
        // The person edited line d (no overlap) and line f (overlap) since.
        let current = "a\nb\nc\nd!\ne\nf?\n";
        let hs = hunks(&p(), base, current, agent);
        assert_eq!(hs.len(), 2);
        assert!(!hs[0].conflict);
        assert_eq!(hs[0].old_lines, ["b\n"]);
        assert!(hs[1].conflict);
        assert_eq!(hs[1].old_lines, ["f?\n"]);
        assert_eq!(hs[1].new_lines, ["F\n"]);
        assert_eq!(hs[1].base_lines, ["f\n"]);
        assert_eq!(accept_all(current, &hs), "a\nB\nc\nd!\ne\nF\n");
        // A pending conflict stays a conflict at the next review.
        let step = apply(current, &hs, |h| if h.conflict { Choice::Pending } else { Choice::Accept });
        assert_eq!(step.text, "a\nB\nc\nd!\ne\nf?\n");
        assert_eq!(step.remaining.len(), 1);
        assert_eq!(step.remaining[0].range_in_current, 5..6);
        let again = hunks(&p(), &step.base, &step.text, &step.new_text);
        assert_eq!(again.len(), 1);
        assert!(again[0].conflict);
        assert_eq!(again[0].id, hs[1].id, "ids survive the rebase");
    }

    #[test]
    fn edits_end_like_the_agent_lines_and_ids_are_unique() {
        let base = "x\n";
        let new = "x\nnote\nx\nnote\n";
        let hs = hunks(&p(), base, base, new);
        let ids: Vec<&str> = hs.iter().map(|h| h.id.as_str()).collect();
        let mut dedup = ids.clone();
        dedup.dedup();
        assert_eq!(ids.len(), dedup.len(), "{ids:?}");
        let r = apply(base, &hs, |_| Choice::Edit("mine".into()));
        assert!(r.text.lines().all(|l| l == "x" || l == "mine"), "{:?}", r.text);
        assert!(r.text.ends_with('\n'));
    }

    #[test]
    fn char_ranges_of_lines() {
        let t = "ab\ncdé\nf";
        assert_eq!(char_range_of_lines(t, &(1..2)), 3..7);
        assert_eq!(char_range_of_lines(t, &(2..3)), 7..8);
        assert_eq!(char_range_of_lines(t, &(3..3)), 8..8);
        assert_eq!(char_range_of_lines(t, &(0..0)), 0..0);
    }

    // ---- property tests (seeded, deterministic; design §A.4)

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
        }
    }

    const WORDS: &[&str] = &["alpha", "beta", "gamma", "delta", "x", "", "## Head", "- item", "same"];

    fn random_text(r: &mut Rng) -> String {
        let n = r.below(14);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(WORDS[r.below(WORDS.len())]);
            s.push('\n');
        }
        if r.below(5) == 0 {
            s.push_str("tail");
        }
        s
    }

    fn mutate(r: &mut Rng, text: &str) -> String {
        let mut lines: Vec<String> = split_lines(text).iter().map(|l| (*l).to_string()).collect();
        for _ in 0..=r.below(4) {
            let i = r.below(lines.len() + 1);
            match r.below(3) {
                0 => lines.insert(i, format!("{} {}\n", WORDS[r.below(WORDS.len())], r.below(9))),
                1 if i < lines.len() => {
                    lines.remove(i);
                }
                _ if i < lines.len() => lines[i] = format!("changed {}\n", r.below(99)),
                _ => {}
            }
        }
        lines.concat()
    }

    #[test]
    fn property_accept_all_gives_new_text_and_reject_all_keeps_current() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2_000 {
            let base = random_text(&mut r);
            let new = mutate(&mut r, &base);
            let hs = hunks(&p(), &base, &base, &new);
            assert_eq!(accept_all(&base, &hs), new, "base {base:?}");
            assert_eq!(apply(&base, &hs, |_| Choice::Reject).text, base);
            assert!(hs.iter().all(|h| !h.conflict));
        }
    }

    #[test]
    fn property_any_order_or_subset_matches_applying_it_at_once() {
        let mut r = Rng(42);
        for case in 0..1_500 {
            let base = random_text(&mut r);
            let new = mutate(&mut r, &base);
            let hs = hunks(&p(), &base, &base, &new);
            if hs.is_empty() {
                continue;
            }
            let subset: Vec<String> = hs.iter().filter(|_| r.below(2) == 0).map(|h| h.id.clone()).collect();
            let at_once = apply(&base, &hs, |h| if subset.contains(&h.id) { Choice::Accept } else { Choice::Reject }).text;
            // One at a time, in a random order, rebasing in between.
            let mut ids: Vec<String> = hs.iter().map(|h| h.id.clone()).collect();
            for i in (1..ids.len()).rev() {
                ids.swap(i, r.below(i + 1));
            }
            let (mut cur, mut b, mut n) = (base.clone(), base.clone(), new.clone());
            let mut cache: Option<ReviewCache> = None;
            for id in &ids {
                let now = review_hunks(&p(), &b, &cur, &n, cache.as_ref());
                let Some(h) = now.iter().find(|h| &h.id == id) else { panic!("case {case}: hunk {id} lost; base {base:?} new {new:?}") };
                let c = if subset.contains(id) { Choice::Accept } else { Choice::Reject };
                let target = h.id.clone();
                let step = apply(&cur, &now, |h| if h.id == target { c.clone() } else { Choice::Pending });
                cache = Some(ReviewCache::new(&step.text, &step.remaining));
                (cur, b, n) = (step.text, step.base, step.new_text);
                // Without the cache, a rebase still lands on the same text.
                assert_eq!(
                    apply(&cur, &hunks(&p(), &b, &cur, &n), |_| Choice::Accept).text,
                    apply(&cur, &step.remaining, |_| Choice::Accept).text
                );
            }
            assert_eq!(cur, at_once, "case {case}: base {base:?} new {new:?} subset {subset:?}");
            assert!(hunks(&p(), &b, &cur, &n).is_empty(), "nothing left to review");
        }
    }

    #[test]
    fn property_rebase_never_loses_human_lines_outside_conflicts() {
        let mut r = Rng(7);
        for case in 0..2_000 {
            let base = random_text(&mut r);
            let agent = mutate(&mut r, &base);
            let human = mutate(&mut r, &base);
            let hs = hunks(&p(), &base, &human, &agent);
            // Accept everything that isn't a conflict: the person's lines
            // outside those hunks are all still there, in order.
            let out = apply(&human, &hs, |h| if h.conflict { Choice::Reject } else { Choice::Accept }).text;
            let touched: Vec<Range<u32>> = hs.iter().filter(|h| !h.conflict).map(|h| h.range_in_current.clone()).collect();
            let kept: Vec<&str> = split_lines(&human)
                .into_iter()
                .enumerate()
                .filter(|(i, _)| {
                    let i = u32::try_from(*i).unwrap_or(0);
                    !touched.iter().any(|t| t.contains(&i))
                })
                .map(|(_, l)| l)
                .collect();
            let mut it = split_lines(&out).into_iter();
            for l in &kept {
                assert!(
                    it.any(|o| o == *l || o.trim_end_matches('\n') == l.trim_end_matches('\n')),
                    "case {case}: lost {l:?} from {human:?} → {out:?}"
                );
            }
            // Non-conflict hunks only replace lines the person didn't change.
            for h in hs.iter().filter(|h| !h.conflict) {
                assert_eq!(h.old_lines, h.base_lines, "case {case}");
            }
        }
    }
}
