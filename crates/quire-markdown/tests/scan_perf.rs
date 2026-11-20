// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The times of the scanners on hostile one-line and many-line inputs and on a generated
//! 5 MB note (`docs/design/0.14-index-startup.md` §3.7), for a person to read.
//!
//! That the work grows in proportion to the note is checked by counting the work, not by
//! timing it, in `src/perf_tests.rs`: a timed check flakes on a loaded runner. The absolute
//! budgets of `docs/design/0.14-build-plan.md` (I0a, I0b) depend on the machine, so they
//! only print here, in the ignored `report_budgets`.
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
