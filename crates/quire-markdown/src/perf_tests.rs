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
