// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-note semantic updates and the background refit
//! (`docs/design/0.14-index-startup.md` §3.3, §6): rows are folded in with the
//! current model, a refit equals a fresh build, quality holds after 10 %
//! of the notes changed, and an agent view's fit-set rule holds across the
//! long window between refits.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use quire_vault::{NotePath, Vault};

use super::*;
use crate::chunk::chunk_note;
use crate::scope::{Visible, visible};
use crate::{IndexService, SearchFilters};

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

/// A small xorshift: tests are deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % n as u64).expect("small")
    }
}

const WORDS: &[&str] = &[
    "rope", "editor", "tree", "buffer", "undo", "markdown", "note", "link", "search", "index", "quick", "local", "graph", "oven", "flour",
    "sugar", "bread", "engine", "road", "vehicle", "garden", "tomato", "harvest", "river", "boat", "music", "piano", "violin",
];

fn text(rng: &mut Rng) -> String {
    let mut s = String::from("# Note\n");
    for para in 0..=rng.next(3) {
        if para > 0 {
            s.push_str("\n## Part\n");
        }
        let words: Vec<&str> = (0..6 + rng.next(14)).map(|_| WORDS[rng.next(WORDS.len())]).collect();
        s.push_str(&words.join(" "));
        s.push_str(".\n");
    }
    s
}

/// Every passage of the notes the service holds, in no order.
fn chunks_now(svc: &IndexService) -> Vec<Chunk> {
    let ix = svc.read();
    ix.paths().iter().flat_map(|p| chunk_note(p, ix.source(p).unwrap_or_default())).collect()
}

/// `docs/design/0.14-index-startup.md` §6, the property: after random edits (no refit) every row is what the
/// current model gives its passage's text, and the rows are the notes'
/// passages; after a refit the index equals a fresh build exactly.
#[test]
fn rows_are_folded_in_and_a_refit_equals_a_fresh_build() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let files: Vec<(String, String)> = (0..40).map(|i| (format!("N{i}.md"), text(&mut rng))).collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let svc = IndexService::spawn();
    svc.load(Vault::in_memory(&refs));
    svc.flush_semantic();
    let fits = svc.semantic_fits();
    assert_eq!(fits, 1);
    for round in 0..60 {
        let path = np(&format!("N{}.md", rng.next(50)));
        if rng.next(5) == 0 {
            svc.remove(path);
        } else {
            svc.upsert(path, text(&mut rng));
        }
        if round % 5 == 4 {
            svc.flush_semantic();
            let sem = svc.semantic().expect("built");
            let ix = svc.read();
            let mut places: Vec<(NotePath, usize)> = Vec::new();
            for (id, p) in sem.passages() {
                let t = sem.passage_text(id, &ix).expect("the passage is in its note");
                assert_eq!(sem.row_bits(id), sem.row_bits_for(&t), "round {round}: {} at {}", p.path, p.range.start);
                // The note → rows map finds the same row (S3).
                let found: Option<Vec<u32>> = sem.vector_of(&p.path, p.range.start).map(|v| v.iter().map(|x| x.to_bits()).collect());
                assert_eq!(found, sem.row_bits(id), "round {round}: vector_of {}", p.path);
                places.push((p.path.clone(), p.range.start));
            }
            places.sort();
            drop(ix);
            let mut want: Vec<(NotePath, usize)> = chunks_now(&svc).into_iter().map(|c| (c.path, c.range.start)).collect();
            want.sort();
            assert_eq!(places, want, "round {round}: the rows are the notes' passages");
        }
    }
    // A refit on demand; the rule may have refitted on the way (a tenth of this small vault changed).
    let before = svc.semantic_fits();
    assert!(before >= fits);
    svc.refit_semantic();
    assert_eq!(svc.semantic_fits(), before + 1);
    let fresh = SemanticIndex::fit_lsa(chunks_now(&svc));
    assert_eq!(svc.semantic().expect("built").rows_text(), fresh.rows_text(), "a refit is a fresh build, bit for bit");
}

/// The refit rule (`docs/design/0.14-index-startup.md` §3.3): 10 % of the passages (at least 8) changed, or
/// idle 10 minutes with the fit older than 6 hours and something changed.
#[test]
fn the_refit_rule() {
    let now = std::time::Instant::now();
    let mut s = crate::actor::refit_probe(now);
    assert!(!s.due(10_000, now), "nothing changed");
    s.since_fit(999);
    assert!(!s.due(10_000, now));
    s.since_fit(1_000);
    assert!(s.due(10_000, now), "10 % of 10,000");
    s.since_fit(7);
    assert!(!s.due(20, now), "at least 8");
    s.since_fit(8);
    assert!(s.due(20, now));
    s.since_fit(1);
    let (min, hour) = (std::time::Duration::from_mins(1), std::time::Duration::from_hours(1));
    let later = now + 6 * hour + std::time::Duration::from_secs(1);
    s.changed_at(now + 5 * hour + 59 * min);
    assert!(!s.due(10_000, later), "not idle: a change a minute ago");
    s.changed_at(now + 5 * hour + 50 * min);
    assert!(s.due(10_000, later), "idle 10 minutes, fit older than 6 hours");
    s.changed_at(now);
    assert!(!s.due(10_000, now + 5 * hour), "idle, but the fit is younger than 6 hours");
    s.since_fit(0);
    assert!(!s.due(10_000, later), "idle and old, but nothing changed");
}

/// Recall@10 of topic queries over passages, relevant = the topic's notes:
/// each of the topic's first `queries` words alone.
fn recall(sem: &SemanticIndex, topics: &[(&str, &[&str])], label: &BTreeMap<NotePath, usize>, queries: usize) -> f32 {
    let (mut total, mut n) = (0.0, 0);
    for (t, (_, words)) in topics.iter().enumerate() {
        for q in &words[..queries] {
            let hits = sem.search(q, 10);
            let good = hits.iter().filter(|(i, _)| sem.passage(*i).is_some_and(|p| label.get(&p.path) == Some(&t))).count();
            total += good as f32 / 10.0;
            n += 1;
        }
    }
    total / n as f32
}

/// `docs/design/0.14-index-startup.md` §6, quality: after 10 % of the notes changed and were folded in (some of
/// their words new to the model), recall@10 of topic queries is within 5
/// points of a fresh fit's.
#[test]
fn quality_after_ten_percent_changed_is_within_five_points() {
    let topics: &[(&str, &[&str])] = &[
        (
            "cars",
            &["automobile", "engine", "vehicle", "road", "tyre", "fuel", "gearbox", "brake", "clutch", "piston", "exhaust", "chassis"],
        ),
        ("bread", &["sourdough", "flour", "oven", "yeast", "crust", "dough", "knead", "starter", "loaf", "proof", "rye", "baguette"]),
        (
            "music",
            &["symphony", "violin", "piano", "melody", "chord", "tempo", "sonata", "cello", "rhythm", "octave", "fugue", "orchestra"],
        ),
        ("garden", &["tomato", "harvest", "soil", "seed", "compost", "weeds", "trowel", "mulch", "seedling", "prune", "hedge", "trellis"]),
        ("sea", &["harbour", "boat", "sail", "tide", "anchor", "wave", "hull", "mast", "keel", "buoy", "jetty", "rudder"]),
        (
            "code",
            &["compiler", "function", "variable", "loop", "debug", "syntax", "closure", "pointer", "module", "lambda", "struct", "macro"],
        ),
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // A note on topic `t` with words `lo..hi` of its list, half of its words off topic.
    let note = |rng: &mut Rng, t: usize, lo: usize, hi: usize| -> String {
        let words = &topics[t].1[lo..hi];
        let w: Vec<&str> =
            (0..30).map(|_| if rng.next(2) == 0 { WORDS[rng.next(WORDS.len())] } else { words[rng.next(words.len())] }).collect();
        format!("# Entry\n{}.\n", w.join(" "))
    };
    let mut label: BTreeMap<NotePath, usize> = BTreeMap::new();
    let mut files: BTreeMap<NotePath, String> = BTreeMap::new();
    for i in 0..400 {
        let t = i % topics.len();
        let p = np(&format!("T{i}.md"));
        files.insert(p.clone(), note(&mut rng, t, 0, 8));
        label.insert(p, t);
    }
    let chunks = |files: &BTreeMap<NotePath, String>| -> Vec<Chunk> { files.iter().flat_map(|(p, s)| chunk_note(p, s)).collect() };
    let before = SemanticIndex::fit_lsa(chunks(&files));
    // 10 % of the notes change topic, half their topic words new to the model, and are folded in.
    let mut changed = Vec::new();
    for i in 0..40 {
        let p = np(&format!("T{}.md", i * 10 + 3));
        let t = (label[&p] + 1 + rng.next(topics.len() - 1)) % topics.len();
        files.insert(p.clone(), note(&mut rng, t, 4, 12));
        label.insert(p.clone(), t);
        changed.push(p);
    }
    let gone = changed.clone();
    let added: Vec<Chunk> = changed.iter().flat_map(|p| chunk_note(p, &files[p])).collect();
    let folded = before.updated(&gone, added, None).expect("lsa").0;
    let fresh = SemanticIndex::fit_lsa(chunks(&files));
    let (r_fold, r_fresh) = (recall(&folded, topics, &label, 8), recall(&fresh, topics, &label, 8));
    eprintln!("recall@10: folded in {r_fold:.3}, fresh fit {r_fresh:.3}");
    assert!(r_fresh > 0.5, "the corpus is searchable: {r_fresh}");
    assert!((r_fold - r_fresh).abs() <= 0.05, "folded in {r_fold}, fresh {r_fresh}");
}

/// f16 rows rank as f32 rows would, but for near-ties: the top hit of each
/// query is the same, and scores stay within f16 rounding.
#[test]
fn compact_rows_score_like_f32() {
    let mut rng = Rng(7);
    let chunks: Vec<Chunk> = (0..120).flat_map(|i| chunk_note(&np(&format!("N{i}.md")), &text(&mut rng))).collect();
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let lsa = Lsa::fit(&texts, 96);
    let sem = SemanticIndex::fit_lsa(chunks.clone());
    for q in ["rope editor buffer", "oven flour bread", "garden tomato harvest", "piano violin"] {
        let qv = sem.embed_query(q);
        let mut exact: Vec<(usize, f32)> = texts.iter().enumerate().map(|(i, t)| (i, dot_f32(&qv, &lsa.project(t)))).collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1));
        let got = sem.search(q, 1);
        assert!((got[0].1 - exact[0].1).abs() < 2e-3, "{q}: {} against {}", got[0].1, exact[0].1);
    }
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn f16_round_trips() {
    for x in [0.0f32, 1.0, -1.0, 0.5, 0.333_333, 65504.0, 1e-5, -2.5e-7, 6.1e-5, 0.099_975_586] {
        let back = f16_value(f16_bits(x));
        let tol = x.abs() * 1e-3 + 6e-8;
        assert!((back - x).abs() <= tol, "{x} -> {back}");
    }
    assert_eq!(f16_bits(1.0), 0x3c00);
    assert_eq!(f16_bits(-2.0), 0xc000);
    assert_eq!(f16_value(0x7bff).to_bits(), 65504.0f32.to_bits());
    assert!(f16_value(f16_bits(1e6)).is_infinite());
}

/// Fable's review, F1 and the long window: a hidden note deleted after the
/// fit loses its rows at once (a per-note update, no refit), but stays in
/// the model's fitted list, so a view that hides it searches by keyword
/// only across any number of later saves, until a refit without it.
#[test]
fn a_removed_hidden_note_stays_fitted_until_a_refit() {
    let svc = IndexService::spawn();
    // Enough notes that 20 saves stay under a tenth of the passages (no refit by the rule).
    let mut files: Vec<(String, String)> = vec![
        ("Cars.md".into(), "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road.".into()),
        ("Autos.md".into(), "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long.".into()),
        ("Bread.md".into(), "# Bread\nSourdough bread needs flour, water and a slow rise in a warm oven.".into()),
        ("Private/s.md".into(), "# S\nThe automobile secret is a vehicle engine road.".into()),
    ];
    let mut rng = Rng(3);
    files.extend((0..400).map(|i| (format!("Filler/F{i}.md"), text(&mut rng))));
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    svc.load(Vault::in_memory(&refs));
    svc.flush_semantic();
    let no_private: Visible = visible(|p: &NotePath, _: &[String]| !p.as_str().starts_with("Private/"));
    assert!(svc.scoped([1; 32], Arc::clone(&no_private)).semantic().is_none());
    svc.remove(np("Private/s.md"));
    svc.flush_semantic();
    let sem = svc.semantic().expect("built");
    assert!(sem.passages().all(|(_, p)| !p.path.as_str().starts_with("Private/")), "its rows went");
    assert!(sem.fitted().iter().any(|p| p.as_str() == "Private/s.md"), "it stays fitted");
    for i in 0..20 {
        svc.upsert(np(&format!("Day {i}.md")), format!("# Day {i}\nThe automobile engine and the road today, entry {i}."));
        svc.flush_semantic();
        for key in [[1; 32], [2 + u8::try_from(i).unwrap_or(0); 32]] {
            let sc = svc.scoped(key, Arc::clone(&no_private));
            assert!(sc.view().hides_nothing());
            assert!(sc.semantic().is_none(), "save {i}: the model still holds the hidden note");
            assert!(sc.search_sections("automobile", &SearchFilters::default(), 5).iter().all(|h| !h.by_meaning));
        }
    }
    assert_eq!(svc.semantic_fits(), 1, "no refit on the save path");
    // The vault loaded again (a new build) while the old model still serves: its fit set is made
    // again from the model's fitted list, not its rows (which no longer hold the note).
    let (go, wait) = crossbeam_channel::unbounded::<()>();
    svc.set_fit_hook(Some(Arc::new(move || {
        let _ = wait.recv();
    })));
    let notes: Vec<(String, String)> = {
        let ix = svc.read();
        ix.paths().iter().map(|p| (p.to_string(), ix.source(p).unwrap_or_default().to_string())).collect()
    };
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    svc.load(Vault::in_memory(&refs));
    svc.flush();
    assert_eq!(svc.semantic_fits(), 1, "the refit waits");
    assert!(svc.scoped([60; 32], Arc::clone(&no_private)).semantic().is_none(), "after the load, still keyword only");
    go.send(()).unwrap();
    go.send(()).unwrap();
    svc.refit_semantic();
    assert!(svc.scoped([1; 32], Arc::clone(&no_private)).semantic().is_some(), "refitted without it");
}

/// Fable's review, F2: a view's answer is kept for the fit set, so a second
/// search asks the predicate nothing about fitted notes that left.
#[test]
fn a_second_search_makes_no_storage_call() {
    let svc = IndexService::spawn();
    svc.load(Vault::in_memory(&[
        ("Cars.md", "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road."),
        ("Autos.md", "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long."),
        ("Gone/a.md", "# A\nThe automobile engine on a long road trip."),
        ("Gone/b.md", "# B\nA vehicle and its engine, on the road."),
    ]));
    svc.flush_semantic();
    svc.remove(np("Gone/a.md"));
    svc.remove(np("Gone/b.md"));
    svc.flush_semantic();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let counting: Visible = Arc::new(move |_: &NotePath, _: &[String], storage: bool| {
        if storage {
            seen.fetch_add(1, Ordering::Relaxed);
        }
        Some(true)
    });
    assert!(svc.scoped([5; 32], Arc::clone(&counting)).semantic().is_some(), "it may read every fitted note");
    let first = calls.load(Ordering::Relaxed);
    assert!(first >= 2, "the two notes that left were asked about: {first}");
    for _ in 0..3 {
        assert!(svc.scoped([5; 32], Arc::clone(&counting)).semantic().is_some());
    }
    assert_eq!(calls.load(Ordering::Relaxed), first, "later searches ask nothing");
}

/// Fable's review of I2a, B2: a model fitted while a folder now locked was
/// open serves no view, though the note's old path is one the view may read
/// (a readable note moved into a locked folder).
#[test]
fn a_tainted_model_serves_no_view() {
    let svc = IndexService::spawn();
    // Made after the service, so dropped first: a held fit is let go if the test fails.
    let (go, wait) = crossbeam_channel::unbounded::<()>();
    svc.set_fit_hook(Some(Arc::new(move || {
        let _ = wait.recv();
    })));
    go.send(()).unwrap();
    let vault = Vault::in_memory(&[
        ("Cars.md", "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road."),
        ("Autos.md", "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long."),
        ("Plan.md", "# Plan\nThe automobile plan: a vehicle engine on the road."),
    ]);
    svc.load(vault.clone());
    svc.flush_semantic();
    let all: Visible = visible(|_: &NotePath, _: &[String]| true);
    assert!(svc.scoped([1; 32], Arc::clone(&all)).semantic().is_some());
    // Plan.md moves into a folder, which is then locked; the index hears of the move after.
    vault.storage().write_bytes_atomic(&np("Locked/Plan.md"), b"# Plan\nThe automobile plan.").unwrap();
    vault.storage().write_bytes_atomic(&np(&format!("Locked/{}", quire_vault::lock::KEY_FILE)), b"{}").unwrap();
    svc.remove(np("Plan.md"));
    svc.upsert(np("Locked/Plan.md"), "# Plan\nThe automobile plan.".into());
    svc.flush();
    // The refit waits for a go: meanwhile the model is tainted.
    let start = std::time::Instant::now();
    while !svc.semantic().is_some_and(|m| m.is_tainted()) {
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "the model is marked tainted");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(svc.semantic().is_some(), "it still serves the person's own search");
    assert!(svc.scoped([2; 32], Arc::clone(&all)).semantic().is_none(), "no view uses a tainted model");
    go.send(()).unwrap();
    svc.flush_semantic();
    assert!(svc.semantic().is_some_and(|m| !m.is_tainted()), "refitted");
    assert!(svc.scoped([3; 32], all).semantic().is_some());
}

/// Fable's review of I2a, S1: a small vault refits once a tenth of its
/// passages changed (the floor is a handful, not 256), so a new vault does
/// not fold everything into the vocabulary of its first few notes.
#[test]
fn a_small_vault_refits_after_a_tenth_changed() {
    let notes: Vec<(String, String)> =
        (0..40).map(|i| (format!("N{i}.md"), format!("# N{i}\nWords about ropes, trees and editors, note number {i}."))).collect();
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let svc = IndexService::spawn();
    svc.load(Vault::in_memory(&refs));
    svc.flush_semantic();
    assert_eq!(svc.semantic_fits(), 1);
    // Ten new notes with new words: a quarter of the passages, over a tenth.
    for i in 0..10 {
        svc.upsert(np(&format!("New {i}.md")), format!("# New {i}\nGardens, tomatoes and the harvest in autumn, entry {i}."));
    }
    svc.flush_semantic();
    assert_eq!(svc.semantic_fits(), 2, "refitted: a tenth of a small vault changed");
    let sem = svc.semantic().expect("built");
    assert!(sem.search("tomatoes harvest", 3).iter().any(|(i, _)| sem.passage(*i).is_some_and(|p| p.path.as_str().starts_with("New "))));
}
