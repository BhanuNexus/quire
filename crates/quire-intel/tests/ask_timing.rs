// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What `ask` costs to retrieve its passages on 10,000 notes with the
//! meaning model on (each keyword candidate gets its stored vector). Run by
//! hand: `cargo test --release -p quire-intel --test ask_timing -- --ignored --nocapture`.

#![allow(
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    clippy::print_stderr,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap to fail the test"
)]

use std::fmt::Write as _;
use std::time::Instant;

use quire_index::IndexService;
use quire_vault::Vault;

/// A note of a few sections of words from a 3,000-word vocabulary, with topics.
fn note(i: usize) -> String {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let topic = next(60);
    let mut s = format!("# Note {i}\n");
    for sec in 0..=next(3) {
        let _ = writeln!(s, "\n## Section {sec}\n");
        for _ in 0..40 + next(80) {
            let w = if next(3) == 0 { topic * 50 + next(50) } else { next(3_000) };
            let _ = write!(s, "w{w} ");
        }
        s.push_str(".\n");
    }
    s
}

#[test]
#[ignore = "timing, run by hand"]
fn ask_retrieval_on_ten_thousand_notes() {
    let n = 10_000;
    let notes: Vec<(String, String)> = (0..n).map(|i| (format!("f{}/n{i}.md", i % 50), note(i))).collect();
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let svc = IndexService::spawn();
    svc.load(Vault::in_memory(&refs));
    svc.flush_semantic();
    let sem = svc.semantic().unwrap();
    let ix = svc.read();
    let mut times = Vec::new();
    for q in 0..40 {
        let question = format!("w{} w{} w{} and w{}", q * 50 + 3, q * 50 + 7, (q * 37) % 3_000, q * 50 + 11);
        let t = Instant::now();
        let got = quire_intel::retrieve(&ix, Some(&sem), &question, 6);
        times.push(t.elapsed());
        assert!(!got.is_empty(), "{question}");
    }
    times.sort();
    eprintln!("ask retrieval, {n} notes: median {:?}, p95 {:?}", times[times.len() / 2], times[times.len() * 95 / 100]);
    // One passage's stored vector: through the note → rows map, and by a scan of every row
    // (how `vector_of` found it before Fable's review of I2a, S3).
    let places: Vec<(quire_vault::NotePath, usize)> =
        sem.passages().step_by(97).take(200).map(|(_, p)| (p.path.clone(), p.range.start)).collect();
    let t = Instant::now();
    for (p, start) in &places {
        assert!(sem.vector_of(p, *start).is_some());
    }
    let mapped = t.elapsed() / u32::try_from(places.len()).unwrap();
    let t = Instant::now();
    for (p, start) in &places {
        assert!(sem.passages().any(|(_, q)| &q.path == p && q.range.start == *start));
    }
    let scanned = t.elapsed() / u32::try_from(places.len()).unwrap();
    eprintln!("a passage's vector, {} passages: {mapped:?} by the map, {scanned:?} by a scan of every row", sem.len());
}
