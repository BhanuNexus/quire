// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The scanners must take time in proportion to the note: hostile one-line and
//! many-line inputs, and a generated 5 MB note (`docs/design/0.14-index-startup.md` §3.7).
//!
//! Each check compares the time for `2n` with the time for `n` (best of a few
//! runs, so it does not depend on how fast the runner is): linear work is about
//! 2x, the old quadratic scans were about 4x. The absolute budgets of
//! `docs/design/0.14-build-plan.md` (I0a, I0b) depend on the runner, so they only
//! print, in the ignored `report_budgets`.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::print_stdout,
    reason = "an integration test: its tests sit at the top level, and the report prints on purpose"
)]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use quire_markdown::linter::{Cx, Options, lint};
use quire_markdown::{cite, extract, render, scan_inlines, srs};

/// One line of `n` `[`.
fn open_brackets(n: usize) -> String {
    "[".repeat(n)
}

/// `n` lines of `[[x`.
fn open_links(n: usize) -> String {
    "[[x\n".repeat(n)
}

/// One line of about `n` bytes of `== a ==`-less marks: each `==` opens a body that starts with a space.
fn open_marks(n: usize) -> String {
    "== a ".repeat(n / 5)
}

/// A note of about `bytes` bytes with code, highlights, links, tags, citations,
/// callouts, block ids, flash cards and fenced blocks on every few lines, so it
/// has one verbatim range for every line or two.
fn big_note(bytes: usize) -> String {
    let mut s = String::from("---\ntitle: Big\ntags: [a, b]\n---\n\n");
    let mut i = 0;
    while s.len() < bytes {
        write!(
            s,
            "## Heading {i}\n\nSome prose with `code {i}` and ==mark {i}== and [[Note {i}|alias]] and #tag/{i} and @key{i}.\n\
             A second line with $x_{i}$ and [text](https://example.com/{i}) and a citation [@key{i}, p. 3] ^b{i}\n\n\
             > [!note] Title {i}\n> body of the callout\n\n\
             ```rust\nlet x = [[{i}]];\n```\n\n\
             Question {i}\n?\nAnswer {i}\n\n"
        )
        .unwrap();
        i += 1;
    }
    s
}

fn time<T>(f: impl FnOnce() -> T) -> Duration {
    let t = Instant::now();
    std::hint::black_box(f());
    t.elapsed()
}

/// The best of `runs` timings of `f`.
fn best(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs).map(|_| time(&mut f)).min().unwrap()
}

/// `f` over `make(n)` and `make(2n)`: the larger takes under 2.5x the smaller.
fn doubles(what: &str, n: usize, make: impl Fn(usize) -> String, f: impl Fn(&str)) {
    let (small, large) = (make(n), make(2 * n));
    let a = best(5, || f(&small));
    let b = best(5, || f(&large));
    // Too quick to measure: far from quadratic whatever the ratio is.
    if b < Duration::from_millis(2) {
        return;
    }
    let ratio = b.as_secs_f64() / a.as_secs_f64().max(1e-6);
    assert!(ratio < 2.5, "{what}: {n} -> {} took {a:?} -> {b:?} ({ratio:.1}x)", 2 * n);
}

/// Uses a result so the work is not optimised away.
fn sink<T>(x: T) {
    std::hint::black_box(x);
}

/// A line of the report: what, on which input, and how to run it.
type Row<'a> = (&'a str, &'a str, Box<dyn Fn() + 'a>);

fn lint_all(s: &str) {
    lint(s, &Options::default(), &Cx::default());
}

fn resolve(_: &quire_markdown::WikiLink) -> Option<String> {
    None
}

#[test]
fn inline_scan_is_linear_on_open_brackets() {
    doubles("scan_inlines", 40_000, open_brackets, |s| sink(scan_inlines(s)));
    doubles("extract", 40_000, open_brackets, |s| sink(extract(s)));
    doubles("lint", 40_000, open_brackets, lint_all);
}

#[test]
fn inline_scan_is_linear_on_many_open_links() {
    doubles("scan_inlines", 20_000, open_links, |s| sink(scan_inlines(s)));
    doubles("extract", 20_000, open_links, |s| sink(extract(s)));
    doubles("lint", 20_000, open_links, lint_all);
}

#[test]
fn highlight_scan_is_linear_on_open_marks() {
    doubles("scan_inlines", 100_000, open_marks, |s| sink(scan_inlines(s)));
    doubles("extract", 100_000, open_marks, |s| sink(extract(s)));
}

/// The budgets of the build plan, for a person to read; run with
/// `cargo test --release -p quire-markdown --test scan_perf -- --ignored --nocapture`.
#[test]
#[ignore = "report only: the absolute times depend on the machine"]
fn report_budgets() {
    let line = open_brackets(100_000);
    let links = open_links(80_000);
    let note = big_note(5_000_000);
    println!("5 MB note: {} bytes", note.len());
    let rows: Vec<Row<'_>> = vec![
        ("extract (<= 20 ms)", "100k [", Box::new(|| sink(extract(&line)))),
        ("lint (<= 100 ms)", "100k [", Box::new(|| lint_all(&line))),
        ("scan_inlines", "100k [", Box::new(|| sink(scan_inlines(&line)))),
        ("extract", "80k [[x lines", Box::new(|| sink(extract(&links)))),
        ("lint", "80k [[x lines", Box::new(|| lint_all(&links))),
        ("scan_inlines", "80k [[x lines", Box::new(|| sink(scan_inlines(&links)))),
        ("srs::cards (<= 300 ms)", "5 MB", Box::new(|| sink(srs::cards(&note)))),
        ("to_html (<= 400 ms)", "5 MB", Box::new(|| sink(render::to_html(&note, &resolve)))),
        ("for_preview", "5 MB", Box::new(|| sink(render::for_preview(&note, &|_| None)))),
        ("cite::scan", "5 MB", Box::new(|| sink(cite::scan(&note)))),
        ("find_block", "5 MB", Box::new(|| sink(quire_markdown::callout::find_block(&note, "nope")))),
        ("extract", "5 MB", Box::new(|| sink(extract(&note)))),
    ];
    for (what, input, f) in rows {
        println!("{what:28} {input:16} {:?}", best(3, &f));
    }
}
