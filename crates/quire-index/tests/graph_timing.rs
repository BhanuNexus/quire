// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cost of one link-graph rebuild (`Index::build_graph`) on a big vault, and of
//! the in-place update an ordinary edit gets instead (0.13.1). Run by hand:
//! `cargo test -p quire-index --test graph_timing -- --ignored --nocapture`.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::fmt::Write as _;
use std::time::Instant;

use quire_index::{Index, NoteRecord};
use quire_vault::NotePath;

#[test]
#[ignore = "timing, run by hand"]
fn rebuild_time_on_ten_thousand_notes() {
    let n = 10_000;
    let mut ix = Index::new();
    for i in 0..n {
        let mut src = format!("---\nup: \"[[n{}]]\"\ntags: [t{}]\n---\n# Note {i}\n", i / 10, i % 50);
        for k in 1..=8 {
            let _ = writeln!(src, "See [[n{}]] and [[n{}|alias]].", (i * 7 + k * 13) % n, (i + k) % n);
        }
        ix.insert_record(NoteRecord::from_source(NotePath::new(format!("f{}/n{i}.md", i % 40)), &src), src);
    }
    let t = Instant::now();
    let g = ix.build_graph();
    let build = t.elapsed();
    let t = Instant::now();
    ix.install_graph(g);
    eprintln!("{n} notes: build {build:?}, install {:?}", t.elapsed());
    // An edit to one note: its text and links change, its aliases don't.
    let p = NotePath::new("f7/n7.md");
    let src = format!("{}\nNow also [[n42]] and [[nowhere]].\n", ix.source(&p).unwrap());
    let t = Instant::now();
    ix.upsert(p.clone(), &src);
    eprintln!("{n} notes: an edit (parse and patch) {:?}", t.elapsed());
    assert!(ix.backlinks(&NotePath::new("f2/n42.md")).iter().any(|b| b.from == p));
}
