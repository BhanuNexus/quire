// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `docs/design/0.14-index-startup.md` §2: a save's semantic update after the
//! debounce, and the index's peak memory with the LSA (a refit included), at
//! 10,000 and 50,000 notes. Run by hand, one test per process (the peak is
//! the process's):
//! `cargo test --release -p quire-index --test semantic_timing -- --ignored --nocapture --test-threads 1 ten_thousand`
//! (and `fifty_thousand`).

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::print_stderr,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it reads /proc directly"
)]

use std::fmt::Write as _;
use std::time::Instant;

use quire_index::IndexService;
use quire_vault::{NotePath, Vault};

/// A note of a few sections of words from a 3,000-word vocabulary, with
/// topics, so the LSA has structure to find.
fn note(i: usize, salt: usize) -> String {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt as u64;
    let mut next = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let topic = next(60);
    let mut s = format!("---\ntags: [t{topic}]\n---\n# Note {i}\n");
    for sec in 0..1 + next(4) {
        let _ = writeln!(s, "\n## Section {sec}\n");
        for _ in 0..40 + next(80) {
            let w = if next(3) == 0 { topic * 50 + next(50) } else { next(3_000) };
            let _ = write!(s, "w{w} ");
        }
        s.push_str(".\n");
    }
    s
}

/// The process's peak resident memory (`VmHWM`), MB.
fn peak_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<f64>().ok())
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

fn run(n: usize) {
    let notes: Vec<(String, String)> = (0..n).map(|i| (format!("f{}/n{i}.md", i % 50), note(i, 0))).collect();
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let svc = IndexService::spawn();
    let t = Instant::now();
    svc.load(Vault::in_memory(&refs));
    svc.flush();
    let keyword = t.elapsed();
    svc.flush_semantic();
    let fit = t.elapsed();
    let sem = svc.semantic().unwrap();
    eprintln!(
        "{n} notes: keyword {keyword:?}, with the meaning model {fit:?}; {} passages, rows {:.1} MB",
        sem.len(),
        sem.heap_bytes() as f64 / 1e6
    );
    drop(sem);
    // Saves: one note's text changes; the index, then the meaning rows (no refit).
    let mut times = Vec::new();
    for k in 0..20 {
        let i = (k * 7_919) % n;
        let t = Instant::now();
        svc.upsert(NotePath::new(format!("f{}/n{i}.md", i % 50)), note(i, k + 1));
        svc.flush_semantic();
        times.push(t.elapsed());
    }
    times.sort();
    eprintln!(
        "{n} notes: save to semantic updated (index and rows, after the debounce): median {:?}, max {:?}; fits {}",
        times[times.len() / 2],
        times[times.len() - 1],
        svc.semantic_fits()
    );
    assert_eq!(svc.semantic_fits(), 1, "no save refits");
    let t = Instant::now();
    svc.refit_semantic();
    eprintln!("{n} notes: a refit {:?}", t.elapsed());
    eprintln!("{n} notes: peak memory {:.0} MB (the process: notes in memory, index, meaning model, a refit)", peak_mb());
}

#[test]
#[ignore = "timing and memory, run by hand"]
fn ten_thousand() {
    run(10_000);
}

#[test]
#[ignore = "timing and memory, run by hand"]
fn fifty_thousand() {
    run(50_000);
}
