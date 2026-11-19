// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The startup-cache measurement (`docs/design/0.14-index-startup.md` §3.6, §8):
//! the codec, the deflate of each section, decode speed and the memory of
//! each part, on the bench vault's notes (the other candidates were measured
//! when the codec was chosen; their numbers are in the design note). Report only; run with
//! `cargo test --release -p quire-index measure -- --ignored --nocapture`
//! (`QUIRE_MEASURE_NOTES`, default 10,000).

use std::io::{Read as _, Write as _};
use std::time::Instant;

use quire_vault::NotePath;

use crate::codec::{Reader, Writer, decode_record, encode_record};
use crate::index::Index;
use crate::record::NoteRecord;

/// `quire-cli bench vault`'s note text (`apps/quire-cli/src/commands/bench.rs`, `synthetic`).
fn synthetic(bytes: usize, seed: u64) -> String {
    use std::fmt::Write as _;
    let words = [
        "rope", "editor", "tree", "buffer", "undo", "markdown", "note", "link", "search", "index", "quick", "fast", "local", "graph",
        "tag", "plain", "file", "sync",
    ];
    let mut s = String::with_capacity(bytes + 256);
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut n = 0;
    while s.len() < bytes {
        n += 1;
        match n % 12 {
            0 => {
                let _ = write!(s, "\n## Section {n}\n\n");
            }
            5 => s.push_str("- [ ] follow up on **this** item with `code` #todo\n"),
            7 => {
                let _ = writeln!(s, "See [[Note {}]] and [[Note {}|alias]].", next() % 500, next() % 500);
            }
            _ => {
                for _ in 0..14 {
                    s.push_str(words[(next() % words.len() as u64) as usize]);
                    s.push(' ');
                }
                s.push_str("*emphasis* end.\n");
            }
        }
    }
    s
}

fn rss_kb(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(field)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

fn deflate(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(1));
    e.write_all(b).expect("in memory");
    e.finish().expect("in memory")
}

fn inflate(b: &[u8], n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    flate2::read::DeflateDecoder::new(b).read_to_end(&mut out).expect("valid");
    out
}

/// A record's content and BM25 terms as segment-local ids.
type Ids = (Vec<(u32, u32)>, Vec<(u32, f32)>);

/// One segment of up to 512 notes, as each candidate encodes section A.
struct Seg {
    table: Vec<String>,
    a: Vec<u8>,
    sources: Vec<u8>,
    n: usize,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "a measurement: minutes in a debug build; see the module docs"]
fn measure() {
    let n: usize = std::env::var("QUIRE_MEASURE_NOTES").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let rss0 = rss_kb("VmRSS:");
    let notes: Vec<(NotePath, String)> = (0..n)
        .map(|i| (NotePath::new(format!("Folder {}/Note {i}.md", i % 50)), synthetic(4096, i as u64)))
        .chain([(NotePath::new("Big.md"), synthetic(100 * 1024, 99))])
        .collect();
    let text: usize = notes.iter().map(|(_, s)| s.len()).sum();
    println!("notes {} text {:.1} MB  RSS after text {} MB", notes.len(), text as f64 / 1e6, (rss_kb("VmRSS:") - rss0) / 1024);

    let t = Instant::now();
    let recs: Vec<NoteRecord> = notes.iter().map(|(p, s)| NoteRecord::from_source(p.clone(), s)).collect();
    println!("parse (one thread): {:.0} ms", ms(t));

    // Segments of 512 notes; each with its own sorted term table.
    let mut t_enc = 0.0f64;
    let mut segs = Vec::new();
    for (part, srcs) in recs.chunks(512).zip(notes.chunks(512)) {
        let mut table: Vec<&str> = part
            .iter()
            .flat_map(|r| r.content.iter().map(|(t, _)| t.as_str()).chain(r.search_terms.0.iter().map(|(t, _)| t.as_str())))
            .collect();
        table.sort_unstable();
        table.dedup();
        let local = |t: &str| u32::try_from(table.binary_search(&t).expect("in table")).expect("small");
        let ids: Vec<Ids> = part
            .iter()
            .map(|r| {
                (r.content.iter().map(|(t, c)| (local(t), *c)).collect(), r.search_terms.0.iter().map(|(t, f)| (local(t), *f)).collect())
            })
            .collect();
        let t = Instant::now();
        let mut w = Writer::default();
        for (r, (c, b)) in part.iter().zip(&ids) {
            encode_record(&mut w, r, c, b, r.search_terms.1);
        }
        t_enc += ms(t);
        let mut sources = Vec::new();
        for (_, s) in srcs {
            sources.extend_from_slice(s.as_bytes());
        }
        segs.push(Seg { table: table.iter().map(|s| (*s).to_string()).collect(), a: w.buf, sources, n: part.len() });
    }
    let table_bytes: usize = segs.iter().map(|s| s.table.iter().map(|t| t.len() + 1).sum::<usize>()).sum();
    println!("segments {}  term tables {:.1} MB", segs.len(), table_bytes as f64 / 1e6);
    println!("| codec | encode ms | section A raw MB | deflated MB | deflate ms | inflate ms | decode ms |");
    let raw: usize = segs.iter().map(|s| s.a.len()).sum();
    let t = Instant::now();
    let deflated: Vec<Vec<u8>> = segs.iter().map(|s| deflate(&s.a)).collect();
    let t_def = ms(t);
    let def: usize = deflated.iter().map(Vec::len).sum();
    let t = Instant::now();
    let inflated: Vec<Vec<u8>> = deflated.iter().zip(&segs).map(|(d, s)| inflate(d, s.a.len())).collect();
    let t_inf = ms(t);
    let t = Instant::now();
    let mut count = 0;
    for (seg, b) in segs.iter().zip(&inflated) {
        let mut r = Reader::new(b);
        for _ in 0..seg.n {
            count += decode_record(&mut r, seg.table.len()).expect("decodes").bm25.len();
        }
    }
    let t_dec = ms(t);
    assert!(count > 0);
    println!("| codec.rs | {t_enc:.0} | {:.1} | {:.1} | {t_def:.0} | {t_inf:.0} | {t_dec:.0} |", raw as f64 / 1e6, def as f64 / 1e6);
    let raw_b: usize = segs.iter().map(|s| s.sources.len()).sum();
    let t = Instant::now();
    let def_b: Vec<Vec<u8>> = segs.iter().map(|s| deflate(&s.sources)).collect();
    let t_def = ms(t);
    let t = Instant::now();
    for (d, s) in def_b.iter().zip(&segs) {
        assert_eq!(inflate(d, s.sources.len()).len(), s.sources.len());
    }
    println!(
        "section B (sources): raw {:.1} MB deflated {:.1} MB, deflate {t_def:.0} ms, inflate {:.0} ms",
        raw_b as f64 / 1e6,
        def_b.iter().map(Vec::len).sum::<usize>() as f64 / 1e6,
        ms(t)
    );

    // The warm path with the hand codec: decode, intern each table once,
    // insert, line starts; then the graph.
    drop(recs);
    let rss1 = rss_kb("VmRSS:");
    let t = Instant::now();
    let mut ix = Index::new();
    for (seg, (_, srcs)) in segs.iter().zip(notes.chunks(512).enumerate()) {
        let table: Vec<&str> = seg.table.iter().map(String::as_str).collect();
        let map = ix.intern_all(&table);
        let mut r = Reader::new(&seg.a);
        for (_, src) in srcs {
            let s = decode_record(&mut r, table.len()).expect("decodes");
            ix.insert_stored(s, &map, src.clone());
        }
    }
    ix.recount();
    let t_install = ms(t);
    let t = Instant::now();
    ix.refresh();
    println!(
        "warm install (one thread, decode + insert + sources): {t_install:.0} ms; graph build {:.0} ms; index RSS +{} MB",
        ms(t),
        rss_kb("VmRSS:").saturating_sub(rss1) / 1024
    );
    let t = Instant::now();
    let mut cold = Index::new();
    for (p, s) in &notes {
        cold.insert_record(NoteRecord::from_source(p.clone(), s), s.clone());
    }
    println!("cold build (one thread, parse + insert): {:.0} ms", ms(t));
    assert_eq!(cold.search("rope undo", 20), ix.search("rope undo", 20));
    drop(cold);

    // Memory by part, from the index just built (bytes of the heap data).
    let src_b: usize = ix.paths().iter().filter_map(|p| ix.source(p)).map(str::len).sum();
    let plain_b: usize = ix.records().map(|r| r.plain.len()).sum();
    let terms_b: usize = ix.records().map(|r| r.terms.len() * 8).sum();
    let ft = ix.fulltext();
    let postings: usize = (0..ft.doc_slots()).map(|d| ft.doc_terms(crate::DocId(d as u32)).len()).sum();
    println!(
        "parts: sources {:.1} MB, plain {:.1} MB, content terms {:.1} MB, postings {} entries (~{:.1} MB with doc_terms), peak RSS {} MB",
        src_b as f64 / 1e6,
        plain_b as f64 / 1e6,
        terms_b as f64 / 1e6,
        postings,
        (postings * (9 + 4)) as f64 / 1e6,
        rss_kb("VmHWM:") / 1024
    );

    // Projecting every passage with a model fitted once (no refit at start).
    let t = Instant::now();
    let chunks: Vec<crate::chunk::Chunk> = notes.iter().flat_map(|(p, s)| crate::chunk::chunk_note(p, s)).collect();
    let t_chunk = ms(t);
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let t = Instant::now();
    let lsa = crate::semantic::Lsa::fit(&texts, 96);
    let t_fit = ms(t);
    let t = Instant::now();
    let dims: usize = texts.iter().map(|x| crate::semantic::Embedder::embed(&lsa, x).len()).sum();
    println!(
        "passages {} ({:.1} a note): chunk {t_chunk:.0} ms, fit {t_fit:.0} ms, project all {:.0} ms ({} floats)",
        chunks.len(),
        chunks.len() as f64 / notes.len() as f64,
        ms(t),
        dims
    );
}

/// Start-up time and peak memory of `IndexService` on a bench vault on disk,
/// one mode per process (peak RSS is per process): `QUIRE_MEASURE_DIR`,
/// `QUIRE_MEASURE_MODE` = `write` (make the vault), `cold` (`load`, no
/// cache), `cached` (`load_cached`; run twice: the first writes the cache).
#[test]
#[ignore = "a measurement on a vault on disk; see the doc comment"]
fn measure_start() {
    let (Ok(dir), Ok(mode)) = (std::env::var("QUIRE_MEASURE_DIR"), std::env::var("QUIRE_MEASURE_MODE")) else { return };
    let dir = std::path::PathBuf::from(dir);
    let n: usize = std::env::var("QUIRE_MEASURE_NOTES").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    if mode == "write" {
        let past = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        for i in 0..n {
            let f = dir.join(format!("Folder {}/Note {i}.md", i % 50));
            std::fs::create_dir_all(f.parent().expect("folder")).expect("folder");
            std::fs::write(&f, synthetic(4096, i as u64)).expect("write");
            std::fs::File::options().write(true).open(&f).and_then(|h| h.set_modified(past)).expect("time");
        }
        return;
    }
    let v = quire_vault::Vault::open_local(&dir).expect("vault");
    let svc = crate::IndexService::spawn();
    let t = Instant::now();
    if mode == "cold" {
        svc.load(v);
    } else {
        svc.load_cached(v.clone(), quire_vault::changes::Manifest::open(&v));
    }
    while svc.version() == 0 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let first = ms(t);
    svc.flush();
    let all = ms(t);
    println!(
        "{mode}: notes {} read {} | first install {first:.0} ms | all (graph too) {all:.0} ms | peak RSS {} MB",
        svc.note_count(),
        svc.notes_read(),
        rss_kb("VmHWM:") / 1024
    );
    let t = Instant::now();
    svc.flush_cache();
    println!("{mode}: cache flush {:.0} ms", ms(t));
}
