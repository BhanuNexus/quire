// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The scanners must do work in proportion to the note: hostile one-line and many-line
//! inputs, and a note that is nearly all verbatim ranges (`docs/design/0.14-index-startup.md` §3.7).
//!
//! Each check counts the work (needle searches and the bytes they read, ranges an
//! overlap question looks at) for `n` and for `2n`: linear work is about 2x, the old
//! quadratic scans were about 4x or worse. A count is the same on every machine and
//! under every test runner, so unlike a timing it cannot flake. The times, and the
//! absolute budgets of `docs/design/0.14-build-plan.md` (I0a, I0b), are only printed,
//! in the ignored `report_budgets` of `tests/scan_perf.rs`.

use crate::linter::{Cx, Options, lint};
use crate::scan::work::measure;
use crate::test_notes::{dense_note, open_brackets, open_links, open_marks};
use crate::{cite, extract, render, scan_inlines, srs};

/// What the larger run may add to the work in proportion to its size.
const SLACK: f64 = 64.0;

/// How far over the work in proportion to the size is allowed: linear work stays within
/// a few percent (a generated note is a little over its target size), quadratic work
/// would be at about twice the proportion.
const TOLERANCE: f64 = 1.1;

/// `f` over `make(n)` and `make(2n)` does at most about twice the work on the larger
/// (the work for each byte of the text does not grow).
fn doubles(what: &str, n: usize, make: impl Fn(usize) -> String, f: impl Fn(&str)) {
    let (small, large) = (make(n), make(2 * n));
    let a = measure(|| f(&small));
    let b = measure(|| f(&large));
    assert!(a.searched + a.steps + a.scanned > 0, "{what}: nothing was counted, so the check checks nothing");
    let grown = large.len() as f64 / small.len() as f64;
    let linear = |x: u64, y: u64| y as f64 <= x as f64 * grown * TOLERANCE + SLACK;
    assert!(
        linear(a.searched, b.searched) && linear(a.steps, b.steps) && linear(a.scanned, b.scanned),
        "{what}: size {n} -> {} did {a:?} -> {b:?}",
        2 * n
    );
}

fn sink<T>(x: T) {
    std::hint::black_box(x);
}

fn lint_all(s: &str) {
    lint(s, &Options::default(), &Cx::default());
}

fn resolve(_: &crate::WikiLink) -> Option<String> {
    None
}

#[test]
fn inline_scan_is_linear_on_open_brackets() {
    doubles("scan_inlines", 50_000, open_brackets, |s| sink(scan_inlines(s)));
    doubles("extract", 50_000, open_brackets, |s| sink(extract(s)));
    doubles("lint", 50_000, open_brackets, lint_all);
}

#[test]
fn inline_scan_is_linear_on_many_open_links() {
    doubles("scan_inlines", 25_000, open_links, |s| sink(scan_inlines(s)));
    doubles("extract", 25_000, open_links, |s| sink(extract(s)));
    doubles("lint", 25_000, open_links, lint_all);
}

#[test]
fn citation_scan_is_linear_on_many_open_links() {
    doubles("cite::scan", 25_000, |n| format!("@a\n{}", open_links(n)), |s| sink(cite::scan(s)));
}

#[test]
fn highlight_scan_is_linear_on_open_marks() {
    doubles("scan_inlines", 100_000, open_marks, |s| sink(scan_inlines(s)));
    doubles("extract", 100_000, open_marks, |s| sink(extract(s)));
}

#[test]
fn verbatim_range_walks_are_linear_on_a_big_note() {
    doubles("srs::cards", 200_000, dense_note, |s| sink(srs::cards(s)));
    doubles("to_html", 200_000, dense_note, |s| sink(render::to_html(s, &resolve)));
    doubles("for_preview", 200_000, dense_note, |s| sink(render::for_preview(s, &|_| None)));
    doubles("cite::scan", 200_000, dense_note, |s| sink(cite::scan(s)));
    doubles("find_block", 200_000, dense_note, |s| sink(crate::callout::find_block(s, "nope")));
}

/// Many paragraphs with a highlight each (the cloze cards of `srs::cards`): the highlights
/// of a paragraph are found by position, not by a pass over all of them.
#[test]
fn cards_are_linear_on_many_highlighted_paragraphs() {
    doubles("srs::cards", 200_000, |n| "a ==m== b\n\n".repeat(n / 12), |s| sink(srs::cards(s)));
}

/// Front matter with one property whose value is `n` bytes of `piece`: every link
/// asks what quote is open and whether it is in a code span.
fn one_line_of_links(piece: &'static str) -> impl Fn(usize) -> String {
    move |n| format!("---\nrelated: {}\n---\nbody\n", piece.repeat(n / piece.len()))
}

/// Many links on one front-matter line: the quote open at each link is read on from the
/// last one, not from the start of the value, and the code spans are walked once.
#[test]
fn front_matter_links_are_linear_on_one_line_of_many_links() {
    let links = |s: &str| sink(crate::fields::front_matter_links(s, s.len()));
    doubles("front_matter_links", 40_000, one_line_of_links("[[x]] "), links);
    doubles("front_matter_links, quoted", 40_000, one_line_of_links("'[[x]]' "), links);
    doubles("front_matter_links, in code spans", 40_000, one_line_of_links("`c` [[x]] "), links);
    doubles("extract", 40_000, one_line_of_links("[[x]] "), |s| sink(extract(s)));
}

/// Exact counts on small fixed inputs: a counter that runs backwards, stands still or
/// explodes changes them, and so fails here and in the doubling tests.
#[test]
fn the_work_on_small_inputs_is_counted_exactly() {
    use crate::scan::work::Work;
    let work = |f: &dyn Fn()| measure(f);
    // The value reader counts each character it reads, and stops at a comment.
    let read = |v: &'static str| work(&|| sink(crate::fields::yaml_quote_at(v)));
    assert_eq!(read("ab c").scanned, 4);
    assert_eq!(read("a #b").scanned, 3);
    // Two paragraphs with a highlight each: the cards walk the highlights of a paragraph.
    let cards = work(&|| sink(srs::cards("a ==m== b\n\nc ==n== d")));
    assert_eq!(cards, Work { searched: 12, steps: 6, scanned: 0 }, "searches and steps of two highlighted paragraphs");
    // A front-matter line of five links and three code spans: each link asks the cursor once.
    let src = "---\nup: `[[a]]` [[b]] `x` `[[c]]` [[d]] [[e]]\n---\nbody\n";
    let fm = work(&|| sink(crate::fields::front_matter_links(src, src.len() - 5)));
    assert_eq!(fm, Work { searched: 53, steps: 8, scanned: 71 }, "five links ask once each, and three spans are stepped over");
}
