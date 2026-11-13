// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cost of one link-graph rebuild (`Index::build_graph`) on a big vault. Run by hand:
//! `cargo test -p quire-index --test graph_timing -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::print_stderr, reason = "test code")]

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
}
