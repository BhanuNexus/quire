// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli bench`: performance check on synthetic data (design §9 baselines).

use std::fmt::Write as _;
use std::time::Instant;

use quire_app::index::chunk::chunk_note;
use quire_app::index::semantic::build_lsa_index;
use quire_app::intel::related_notes;
use quire_app::markdown::{blocks, extract, highlight, render};
use quire_app::text::{Buffer, Transaction};
use quire_app::vault::{NotePath, Vault};

use super::{CliCommand, load};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "bench",
    rank: 18,
    usage: "  quire-cli bench     [size_mb]               performance check on synthetic data",
    run,
}];

#[expect(clippy::unnecessary_wraps, reason = "every command has the same signature")]
fn run(args: &[String]) -> Result<(), String> {
    bench(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0));
    Ok(())
}

fn synthetic(bytes: usize, seed: u64) -> String {
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

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    println!("{label:<44} {:>9.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    r
}

fn bench(size_mb: f64) {
    let bytes = (size_mb * 1024.0 * 1024.0) as usize;
    println!("Synthetic document: {:.1} MB\n", bytes as f64 / 1_048_576.0);
    let doc = synthetic(bytes, 7);
    let mut buf = time("rope: build from string", || Buffer::from(doc.as_str()));
    time("rope: 10,000 random inserts", || {
        let mut x: usize = 12345;
        for _ in 0..10_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let pos = x % buf.len_chars().max(1);
            Transaction::insert(pos, "x").apply(&mut buf).expect("in bounds");
        }
    });
    time("rope: 10,000 line lookups", || {
        let lines = buf.len_lines();
        (0..10_000).map(|i| buf.line_to_char((i * 7919) % lines)).sum::<usize>()
    });
    let hundred_kb = &doc[..doc.len().min(100 * 1024)];
    let hundred_kb = &hundred_kb[..hundred_kb.rfind('\n').unwrap_or(hundred_kb.len())];
    time("highlight: 100 KB (per keystroke budget)", || highlight(hundred_kb).len());
    time("highlight: whole document", || highlight(&doc).len());
    let fences = time("blocks: scan fences/front matter (whole doc)", || blocks::verbatim_blocks(&doc));
    let mid = doc.len() / 2;
    let mid = (mid..doc.len()).find(|&i| doc.is_char_boundary(i)).unwrap_or(0);
    time("highlight: 60-line window mid-document", || blocks::highlight_window(&doc, mid..mid + 6000, &fences).len());
    time("extract: whole document", || extract(&doc).links.len());
    time("render HTML: whole document", || render::to_html(&doc, &|_| None).len());

    let dir = std::env::temp_dir().join(format!("quire-bench-{}", std::process::id()));
    let notes = 2_000;
    time(&format!("vault: write {notes} notes (~4 KB)"), || {
        let v = Vault::open_local(&dir).expect("temp vault");
        for i in 0..notes {
            v.write(&NotePath::new(format!("Note {i}.md")), &synthetic(4096, i as u64)).expect("write");
        }
    });
    let (_, svc, ms) = load(dir.to_str().unwrap_or(".")).expect("load");
    println!("{:<44} {:>9.2} ms", format!("index: parse + index {notes} notes"), ms);
    let ix = svc.read();
    time("search: \"rope undo\" (BM25, AND)", || ix.search("rope undo", 20).len());
    time("search: prefix \"edi\"", || ix.search("edi", 20).len());
    time("quick open: \"nt 42\"", || ix.quick_open("nt 42", 20).len());
    let p = NotePath::new("Note 42.md");
    time("backlinks: Note 42", || ix.backlinks(&p).len());
    time("related notes: Note 42", || related_notes(&ix, &p, 6).len());
    let chunks: Vec<_> =
        time("chunk all notes into passages", || ix.paths().iter().flat_map(|p| chunk_note(p, ix.source(p).unwrap_or(""))).collect());
    let n = chunks.len();
    let sem = time(&format!("semantic index: fit LSA on {n} passages"), || build_lsa_index(chunks));
    time("semantic search: top 20 passages", || sem.search("undo buffer editor", 20).len());
    drop(ix);
    drop(svc);
    let _ = std::fs::remove_dir_all(&dir);
}
