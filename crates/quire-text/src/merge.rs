// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Line-based three-way merge (diff3) for reconciling two edited copies of a
//! note with their common ancestor.
//!
//! Rules, chosen for notes rather than source code:
//! * changes to different lines from both sides are combined;
//! * the same change on both sides is applied once;
//! * both sides **adding** lines at the same place keeps both additions
//!   (mine first), which is the common "appended on two devices" case;
//! * anything else touching the same lines is a conflict, written with
//!   `<<<<<<<` / `=======` / `>>>>>>>` markers so nothing is lost.

use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::Change;

/// Labels used in conflict markers.
pub const MINE: &str = "<<<<<<< This device";
pub const SEPARATOR: &str = "=======";
pub const THEIRS: &str = ">>>>>>> Changed elsewhere";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// Merged text; contains conflict markers when `conflicts > 0`.
    pub text: String,
    pub conflicts: usize,
}

impl MergeResult {
    pub const fn is_clean(&self) -> bool {
        self.conflicts == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Mine,
    Theirs,
}

/// A replaced range of base lines and its new lines on one side.
#[derive(Debug)]
struct Hunk<'a> {
    side: Side,
    start: usize,
    end: usize,
    lines: Vec<&'a str>,
}

fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn hunks<'a>(base: &[&str], other: &[&'a str], side: Side) -> Vec<Hunk<'a>> {
    let mut out: Vec<Hunk<'a>> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, base, other) {
        let (start, end, new) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, .. } => (old_index, old_index + old_len, 0..0),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, old_index, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_index + old_len, new_index..new_index + new_len),
        };
        // Coalesce adjacent ops (e.g. delete followed by insert) into one hunk.
        if let Some(last) = out.last_mut()
            && last.end == start
        {
            last.end = end;
            last.lines.extend_from_slice(&other[new]);
            continue;
        }
        out.push(Hunk { side, start, end, lines: other[new].to_vec() });
    }
    out
}

/// Does `h` overlap the group spanning base lines `[gs, ge)`?
const fn overlaps(gs: usize, ge: usize, h: &Hunk<'_>) -> bool {
    let (s, e) = (h.start, h.end);
    match (gs == ge, s == e) {
        (true, true) => gs == s,            // two insertions at the same point
        (true, false) => s < gs && gs < e,  // group insertion strictly inside h
        (false, true) => gs < s && s < ge,  // h inserts strictly inside the group
        (false, false) => s < ge && gs < e, // ranges share a line
    }
}

/// One side's version of base lines `[gs, ge)`, applying its hunks there.
fn side_version<'a>(base: &[&'a str], gs: usize, ge: usize, group: &[Hunk<'a>], side: Side) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut pos = gs;
    for h in group.iter().filter(|h| h.side == side) {
        out.extend_from_slice(&base[pos..h.start]);
        out.extend_from_slice(&h.lines);
        pos = h.end;
    }
    out.extend_from_slice(&base[pos..ge]);
    out
}

fn push_all(out: &mut String, lines: &[&str]) {
    for l in lines {
        out.push_str(l);
    }
}

/// Ensure a block written between markers ends with a newline.
fn push_block(out: &mut String, lines: &[&str]) {
    push_all(out, lines);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// One stretch of a three-way merge, in document order.
///
/// Lines keep their trailing `\n` (the last line of a text may lack one), so
/// concatenating a side's lines rebuilds that side exactly. `base` is the
/// ancestor's lines for that stretch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Unchanged on both sides.
    Same(Vec<String>),
    /// Only `mine` changed these lines (or added lines here).
    Mine { base: Vec<String>, mine: Vec<String> },
    /// Only `theirs` changed these lines (or added lines here).
    Theirs { base: Vec<String>, theirs: Vec<String> },
    /// Both sides made the same change; these are the resulting lines.
    Both(Vec<String>),
    /// Both sides changed the same lines differently.
    Conflict { base: Vec<String>, mine: Vec<String>, theirs: Vec<String> },
}

impl Region {
    /// This region's lines as `mine` has them.
    pub fn mine_lines(&self) -> &[String] {
        match self {
            Self::Same(l) | Self::Both(l) => l,
            Self::Mine { mine, .. } | Self::Conflict { mine, .. } => mine,
            Self::Theirs { base, .. } => base,
        }
    }

    /// This region's lines as `theirs` has them.
    pub fn theirs_lines(&self) -> &[String] {
        match self {
            Self::Same(l) | Self::Both(l) => l,
            Self::Theirs { theirs, .. } | Self::Conflict { theirs, .. } => theirs,
            Self::Mine { base, .. } => base,
        }
    }
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| (*l).to_string()).collect()
}

/// Push a `Same` region, joining it to a `Same` region before it.
fn push_same(out: &mut Vec<Region>, lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    if let Some(Region::Same(prev)) = out.last_mut() {
        prev.extend(lines.iter().map(|l| (*l).to_string()));
    } else {
        out.push(Region::Same(owned(lines)));
    }
}

/// The regions of a three-way merge of `mine` and `theirs`, both derived
/// from `base`, by the same rules as [`merge3`] (which is built on this).
///
/// Both sides adding different lines at the same place gives a `Mine`
/// region followed by a `Theirs` region, both with an empty `base`: they
/// don't conflict, and `merge3` keeps both, mine first.
pub fn merge3_regions(base: &str, mine: &str, theirs: &str) -> Vec<Region> {
    let (b, m, t) = (lines(base), lines(mine), lines(theirs));
    let mut all: Vec<Hunk<'_>> = hunks(&b, &m, Side::Mine);
    all.extend(hunks(&b, &t, Side::Theirs));
    // Insertions sort before replacements starting at the same line.
    all.sort_by_key(|h| (h.start, h.end));

    let mut out = Vec::new();
    let mut pos = 0;
    let mut i = 0;
    while i < all.len() {
        let (mut gs, mut ge) = (all[i].start, all[i].end);
        let mut j = i + 1;
        while j < all.len() && overlaps(gs, ge, &all[j]) {
            gs = gs.min(all[j].start);
            ge = ge.max(all[j].end);
            j += 1;
        }
        let group = &all[i..j];
        push_same(&mut out, &b[pos..gs]);
        let has_mine = group.iter().any(|h| h.side == Side::Mine);
        let has_theirs = group.iter().any(|h| h.side == Side::Theirs);
        let vm = side_version(&b, gs, ge, group, Side::Mine);
        let vt = side_version(&b, gs, ge, group, Side::Theirs);
        let base_lines = owned(&b[gs..ge]);
        if !has_theirs {
            out.push(Region::Mine { base: base_lines, mine: owned(&vm) });
        } else if vm == vt {
            out.push(Region::Both(owned(&vm)));
        } else if !has_mine {
            out.push(Region::Theirs { base: base_lines, theirs: owned(&vt) });
        } else if gs == ge {
            out.push(Region::Mine { base: Vec::new(), mine: owned(&vm) });
            out.push(Region::Theirs { base: Vec::new(), theirs: owned(&vt) });
        } else {
            out.push(Region::Conflict { base: base_lines, mine: owned(&vm), theirs: owned(&vt) });
        }
        pos = ge;
        i = j;
    }
    push_same(&mut out, &b[pos..]);
    out
}

fn push_owned(out: &mut String, lines: &[String]) {
    for l in lines {
        out.push_str(l);
    }
}

/// Ensure a block written between markers ends with a newline.
fn push_owned_block(out: &mut String, lines: &[String]) {
    push_owned(out, lines);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Merge `mine` and `theirs`, both derived from `base`.
pub fn merge3(base: &str, mine: &str, theirs: &str) -> MergeResult {
    if mine == theirs {
        return MergeResult { text: mine.to_string(), conflicts: 0 };
    }
    if base == mine {
        return MergeResult { text: theirs.to_string(), conflicts: 0 };
    }
    if base == theirs {
        return MergeResult { text: mine.to_string(), conflicts: 0 };
    }
    let regions = merge3_regions(base, mine, theirs);
    let mut out = String::with_capacity(mine.len().max(theirs.len()));
    let mut conflicts = 0;
    for (k, r) in regions.iter().enumerate() {
        match r {
            Region::Same(l) | Region::Both(l) => push_owned(&mut out, l),
            Region::Mine { base, mine } => {
                // Both sides added here: keep mine on its own lines before theirs.
                let both_added = base.is_empty() && matches!(regions.get(k + 1), Some(Region::Theirs { base, .. }) if base.is_empty());
                if both_added { push_owned_block(&mut out, mine) } else { push_owned(&mut out, mine) }
            }
            Region::Theirs { theirs, .. } => push_owned(&mut out, theirs),
            Region::Conflict { mine, theirs, .. } => {
                conflicts += 1;
                out.push_str(MINE);
                out.push('\n');
                push_owned_block(&mut out, mine);
                out.push_str(SEPARATOR);
                out.push('\n');
                push_owned_block(&mut out, theirs);
                out.push_str(THEIRS);
                out.push('\n');
            }
        }
    }
    MergeResult { text: out, conflicts }
}

/// The line edits that turn `old` into `new`.
///
/// They are char-index [`Change`]s against `old`. Applying them (instead of
/// replacing the whole text) keeps the caret, folds and undo history meaningful.
pub fn line_changes(old: &str, new: &str) -> Vec<Change> {
    let (lo, ln) = (lines(old), lines(new));
    // Char offset of the start of each old line, plus the end.
    let mut starts = Vec::with_capacity(lo.len() + 1);
    let mut at = 0;
    for l in &lo {
        starts.push(at);
        at += l.chars().count();
    }
    starts.push(at);
    let mut out = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &lo, &ln) {
        let (s, e, new_range) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, .. } => (old_index, old_index + old_len, 0..0),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, old_index, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_index + old_len, new_index..new_index + new_len),
        };
        out.push(Change::new(starts[s], starts[e], ln[new_range].concat()));
    }
    out
}

/// Merge two versions when no common ancestor is known.
///
/// Lines only one side has are kept (they may be an addition or a deletion;
/// keeping them never loses text). Where both sides have different lines in
/// the same place, nothing says which is newer, so they are marked as a
/// conflict.
pub fn merge2(mine: &str, theirs: &str) -> MergeResult {
    if mine == theirs {
        return MergeResult { text: mine.to_string(), conflicts: 0 };
    }
    let (lm, lt) = (lines(mine), lines(theirs));
    let mut out = String::with_capacity(mine.len().max(theirs.len()));
    let mut conflicts = 0;
    for op in capture_diff_slices(Algorithm::Myers, &lm, &lt) {
        match op {
            DiffOp::Equal { old_index, len, .. } => push_all(&mut out, &lm[old_index..old_index + len]),
            DiffOp::Delete { old_index, old_len, .. } => push_block(&mut out, &lm[old_index..old_index + old_len]),
            DiffOp::Insert { new_index, new_len, .. } => push_all(&mut out, &lt[new_index..new_index + new_len]),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                conflicts += 1;
                out.push_str(MINE);
                out.push('\n');
                push_block(&mut out, &lm[old_index..old_index + old_len]);
                out.push_str(SEPARATOR);
                out.push('\n');
                push_block(&mut out, &lt[new_index..new_index + new_len]);
                out.push_str(THEIRS);
                out.push('\n');
            }
        }
    }
    MergeResult { text: out, conflicts }
}

/// Three-way merge when an ancestor is known, two-way otherwise.
pub fn merge(base: Option<&str>, mine: &str, theirs: &str) -> MergeResult {
    base.map_or_else(|| merge2(mine, theirs), |b| merge3(b, mine, theirs))
}

/// Number of lines that differ between two texts (for picking an ancestor).
pub fn distance(a: &str, b: &str) -> usize {
    let (la, lb) = (lines(a), lines(b));
    capture_diff_slices(Algorithm::Myers, &la, &lb)
        .iter()
        .map(|op| match *op {
            DiffOp::Equal { .. } => 0,
            DiffOp::Delete { old_len, .. } => old_len,
            DiffOp::Insert { new_len, .. } => new_len,
            DiffOp::Replace { old_len, new_len, .. } => old_len.max(new_len),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Buffer, Transaction};

    const BASE: &str = "# Plan\n\n- [ ] one\n- [ ] two\n- [ ] three\n\nNotes.\n";

    #[test]
    fn trivial_cases() {
        assert_eq!(merge3(BASE, BASE, BASE).text, BASE);
        assert_eq!(merge3(BASE, "x\n", BASE).text, "x\n");
        assert_eq!(merge3(BASE, BASE, "y\n").text, "y\n");
        assert_eq!(merge3(BASE, "same\n", "same\n").text, "same\n");
    }

    #[test]
    fn combines_edits_to_different_lines() {
        let mine = BASE.replace("- [ ] one", "- [x] one");
        let theirs = BASE.replace("Notes.", "Notes, updated on the phone.");
        let r = merge3(BASE, &mine, &theirs);
        assert!(r.is_clean());
        assert_eq!(r.text, "# Plan\n\n- [x] one\n- [ ] two\n- [ ] three\n\nNotes, updated on the phone.\n");
    }

    #[test]
    fn adjacent_line_edits_both_apply() {
        let mine = BASE.replace("- [ ] one", "- [x] one");
        let theirs = BASE.replace("- [ ] two", "- [x] two");
        let r = merge3(BASE, &mine, &theirs);
        assert!(r.is_clean(), "{}", r.text);
        assert!(r.text.contains("- [x] one\n- [x] two\n"));
    }

    #[test]
    fn same_change_on_both_sides_applies_once() {
        let both = BASE.replace("two", "TWO");
        let mine = format!("{both}mine\n");
        let r = merge3(BASE, &mine, &both);
        assert!(r.is_clean());
        assert_eq!(r.text.matches("TWO").count(), 1);
        assert!(r.text.ends_with("mine\n"));
    }

    #[test]
    fn both_appending_keeps_both_mine_first() {
        let mine = format!("{BASE}- [ ] from laptop\n");
        let theirs = format!("{BASE}- [ ] from phone\n");
        let r = merge3(BASE, &mine, &theirs);
        assert!(r.is_clean());
        assert!(r.text.ends_with("Notes.\n- [ ] from laptop\n- [ ] from phone\n"), "{}", r.text);
    }

    #[test]
    fn appending_without_trailing_newline_stays_on_separate_lines() {
        let base = "a\n";
        let r = merge3(base, "a\nmine", "a\ntheirs");
        assert_eq!(r.text, "a\nmine\ntheirs");
    }

    #[test]
    fn same_line_changed_differently_is_a_conflict() {
        let mine = BASE.replace("- [ ] two", "- [ ] two (laptop)");
        let theirs = BASE.replace("- [ ] two", "- [ ] two (phone)");
        let r = merge3(BASE, &mine, &theirs);
        assert_eq!(r.conflicts, 1);
        assert!(r.text.contains(&format!("{MINE}\n- [ ] two (laptop)\n{SEPARATOR}\n- [ ] two (phone)\n{THEIRS}\n")), "{}", r.text);
        assert!(r.text.starts_with("# Plan\n\n- [ ] one\n") && r.text.ends_with("- [ ] three\n\nNotes.\n"));
    }

    #[test]
    fn delete_versus_edit_is_a_conflict() {
        let mine = BASE.replace("- [ ] three\n", "");
        let theirs = BASE.replace("three", "three!");
        let r = merge3(BASE, &mine, &theirs);
        assert_eq!(r.conflicts, 1);
        assert!(r.text.contains(&format!("{MINE}\n{SEPARATOR}\n- [ ] three!\n{THEIRS}\n")), "{}", r.text);
    }

    #[test]
    fn several_independent_regions() {
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let mine = "1\n2m\n3\n4\n5\n6\n7m\n8\n";
        let theirs = "1t\n2\n3\n4\n5t\n6\n7\n8\n9t\n";
        let r = merge3(base, mine, theirs);
        assert!(r.is_clean());
        assert_eq!(r.text, "1t\n2m\n3\n4\n5t\n6\n7m\n8\n9t\n");
    }

    /// Tiny deterministic PRNG so the randomised test needs no dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(self.0 >> 33).unwrap_or(0) % n.max(1)
        }
    }

    /// Edit lines in `lo..hi` of `base` at random (replace, delete, insert).
    fn mutate(base: &[String], lo: usize, hi: usize, tag: &str, rng: &mut Lcg) -> Vec<String> {
        let mut out = Vec::new();
        for (i, l) in base.iter().enumerate() {
            if (lo..hi).contains(&i) {
                match rng.next(4) {
                    0 => out.push(format!("{tag}{i}\n")),
                    1 => {}
                    2 => {
                        out.push(l.clone());
                        out.push(format!("{tag}+{i}\n"));
                    }
                    _ => out.push(l.clone()),
                }
            } else {
                out.push(l.clone());
            }
        }
        out
    }

    #[test]
    fn random_disjoint_edits_always_merge_cleanly() {
        let mut rng = Lcg(42);
        for round in 0..500 {
            let n = 6 + rng.next(30);
            let base: Vec<String> = (0..n).map(|i| format!("line {i}\n")).collect();
            // Mine edits the first part, theirs the second, with a one-line gap.
            let cut = 1 + rng.next(n - 2);
            let mine = mutate(&base, 0, cut, "m", &mut rng);
            let theirs = mutate(&base, cut + 1, n, "t", &mut rng);
            // Expected: mine's first part + the untouched gap line + theirs' second part.
            let gap = format!("line {cut}\n");
            let mine_head = mine.iter().take_while(|l| !l.starts_with(&gap));
            let expected: String = mine_head
                .cloned()
                .chain(std::iter::once(base[cut].clone()))
                .chain(theirs.iter().skip_while(|l| **l != base[cut]).skip(1).cloned())
                .collect();
            let r = merge3(&base.concat(), &mine.concat(), &theirs.concat());
            assert!(r.is_clean(), "round {round}: {}", r.text);
            assert_eq!(r.text, expected, "round {round}");
        }
    }

    #[test]
    fn conflicts_never_lose_either_side() {
        let mut rng = Lcg(7);
        for round in 0..300 {
            let n = 4 + rng.next(12);
            let base: Vec<String> = (0..n).map(|i| format!("line {i}\n")).collect();
            let mine = mutate(&base, 0, n, "m", &mut rng).concat();
            let theirs = mutate(&base, 0, n, "t", &mut rng).concat();
            let r = merge3(&base.concat(), &mine, &theirs);
            for l in mine.lines().chain(theirs.lines()).filter(|l| l.starts_with('m') || l.starts_with('t')) {
                assert!(r.text.contains(l), "round {round}: lost {l:?}\n{}", r.text);
            }
        }
    }

    #[test]
    fn regions_rebuild_both_sides() {
        let mut rng = Lcg(99);
        for round in 0..400 {
            let n = 3 + rng.next(14);
            let base: Vec<String> = (0..n).map(|i| format!("line {i}\n")).collect();
            let mine = mutate(&base, 0, n, "m", &mut rng).concat();
            let theirs = mutate(&base, rng.next(n), n, "t", &mut rng).concat();
            let regions = merge3_regions(&base.concat(), &mine, &theirs);
            let m: String = regions.iter().flat_map(Region::mine_lines).map(String::as_str).collect();
            let t: String = regions.iter().flat_map(Region::theirs_lines).map(String::as_str).collect();
            assert_eq!(m, mine, "round {round}");
            assert_eq!(t, theirs, "round {round}");
            for w in regions.windows(2) {
                assert!(!matches!(w, [Region::Same(_), Region::Same(_)]), "round {round}: same regions are joined");
            }
        }
    }

    #[test]
    fn regions_name_who_changed_what() {
        let base = "a\nb\nc\nd\n";
        let r = merge3_regions(base, "a\nB\nc\nd\n", "a\nb\nc\nD\n");
        assert_eq!(
            r,
            vec![
                Region::Same(vec!["a\n".into()]),
                Region::Mine { base: vec!["b\n".into()], mine: vec!["B\n".into()] },
                Region::Same(vec!["c\n".into()]),
                Region::Theirs { base: vec!["d\n".into()], theirs: vec!["D\n".into()] },
            ]
        );
        let r = merge3_regions(base, "a\nX\nc\nd\n", "a\nY\nc\nd\n");
        assert_eq!(r[1], Region::Conflict { base: vec!["b\n".into()], mine: vec!["X\n".into()], theirs: vec!["Y\n".into()] });
        let r = merge3_regions(base, "a\nZ\nc\nd\n", "a\nZ\nc\nd\n");
        assert_eq!(r[1], Region::Both(vec!["Z\n".into()]));
        let r = merge3_regions("a\n", "a\nm\n", "a\nt\n");
        assert_eq!(
            r,
            vec![
                Region::Same(vec!["a\n".into()]),
                Region::Mine { base: vec![], mine: vec!["m\n".into()] },
                Region::Theirs { base: vec![], theirs: vec!["t\n".into()] },
            ]
        );
    }

    #[test]
    fn line_changes_rebuild_the_new_text() {
        for (old, new) in [
            (BASE, "# Plan\n\n- [x] one\n- [ ] three\n\nNotes!\nmore\n"),
            ("", "a\n"),
            ("a\nb", "a\nb\nc"),
            ("héllo\nwörld\n", "héllo\nWORLD\n"),
        ] {
            let mut buf = Buffer::from(old);
            let tx = Transaction::new(line_changes(old, new)).expect("non-overlapping");
            tx.apply(&mut buf).expect("in range");
            assert_eq!(buf.to_string(), new);
        }
    }

    #[test]
    fn ancestor_helpers() {
        assert_eq!(distance("a\nb\n", "a\nc\nd\n"), 2);
        // Without history, one-sided lines are kept…
        let r = merge(None, "# T\nshared\nlaptop line\n", "# T\nphone line\nshared\n");
        assert!(r.is_clean());
        assert_eq!(r.text, "# T\nphone line\nshared\nlaptop line\n");
        // …but a line both sides changed is a conflict, not two lines.
        let r = merge(None, "# T\nShip it.\n", "# T\nShip it on Friday.\n");
        assert_eq!(r.conflicts, 1);
        assert_eq!(r.text, format!("# T\n{MINE}\nShip it.\n{SEPARATOR}\nShip it on Friday.\n{THEIRS}\n"));
        assert_eq!(merge(Some("a\n"), "a\nb\n", "a\n").text, "a\nb\n");
    }
}
