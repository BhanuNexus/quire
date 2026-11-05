// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli bench`: performance check on synthetic data (design §9 baselines).
//!
//! * `bench [size_mb]`: the readable report (F0).
//! * `bench --suite index,search,sections,semantic,mcp,pdf,mcp-pdfs [--runs N]`:
//!   the §9 measures as JSON lines `{"suite", "metric", "value"}` (ms), each
//!   the median of `N` runs, for the CI `perf` job
//!   (`packaging/perf-check.py`, docs/design/0.10-perf.md).
//! * `bench vault <dir> [notes]`: write a synthetic vault for the desktop
//!   runs (`notes` notes of ~4 KB and `Big.md` of 100 KB).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use quire_agent::DeviceInfo;
use quire_app::index::chunk::chunk_note;
use quire_app::index::semantic::build_lsa_index;
use quire_app::index::{IndexService, SearchFilters};
use quire_app::intel::chat::CancelFlag;
use quire_app::intel::related_notes;
use quire_app::markdown::{blocks, extract, highlight, render};
use quire_app::text::{Buffer, Transaction};
use quire_app::vault::{NotePath, Vault};
use quire_mcp::{Conn, Reply, Server, ServerConfig};
use serde_json::{Value, json};

use super::{CliCommand, load};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "bench",
    rank: 18,
    usage: "  quire-cli bench     [size_mb]               performance check on synthetic data
  quire-cli bench     --suite NAME[,NAME] [--runs N]   design §9 measures as JSON lines (CI)",
    run,
}];

/// Every suite `--suite all` runs.
const SUITES: &[&str] = &["index", "search", "sections", "semantic", "mcp", "pdf", "mcp-pdfs"];

fn run(args: &[String]) -> Result<(), String> {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str);
    if args.get(1).map(String::as_str) == Some("vault") {
        let dir = args.get(2).ok_or("bench vault <dir> [notes]")?;
        let notes = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(10_000);
        return write_vault(std::path::Path::new(dir), notes);
    }
    if let Some(list) = flag("--suite") {
        let runs: usize = flag("--runs").and_then(|n| n.parse().ok()).unwrap_or(5).max(1);
        let names: Vec<&str> = if list == "all" { SUITES.to_vec() } else { list.split(',').collect() };
        for name in names {
            if !SUITES.contains(&name) {
                return Err(format!("no bench suite “{name}” (one of: {}, all)", SUITES.join(", ")));
            }
            suite(name, runs);
        }
        return Ok(());
    }
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

// ---------------------------------------------------------------- suites (CI)

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
}

/// The `p`th percentile (0–100).
fn percentile(mut v: Vec<f64>, p: usize) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    v[(v.len() * p / 100).min(v.len() - 1)]
}

fn emit(suite: &str, metric: &str, value: f64) {
    println!("{}", json!({"suite": suite, "metric": metric, "value": (value * 1000.0).round() / 1000.0}));
}

/// A temporary folder that goes away with it.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(what: &str) -> Self {
        let d = std::env::temp_dir().join(format!("quire-bench-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Self(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_notes(dir: &std::path::Path, notes: usize) -> Result<(), String> {
    let v = Vault::open_local(dir).map_err(|e| e.to_string())?;
    for i in 0..notes {
        v.write(&NotePath::new(format!("Folder {}/Note {i}.md", i % 50)), &synthetic(4096, i as u64)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `bench vault`: the desktop runs' vault.
fn write_vault(dir: &std::path::Path, notes: usize) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_notes(dir, notes)?;
    let big = synthetic(100 * 1024, 99);
    std::fs::write(dir.join("Big.md"), big).map_err(|e| e.to_string())?;
    println!("Wrote {notes} notes and Big.md (100 KB) to {}", dir.display());
    Ok(())
}

fn suite(name: &str, runs: usize) {
    match name {
        "index" => index_suite(runs),
        "search" | "sections" | "semantic" => search_suite(name, runs),
        "mcp" => mcp_suite(runs),
        "pdf" => pdf_suite(runs),
        _ => mcp_pdfs_suite(runs),
    }
}

/// §9: parse and index 2,000 notes (≤ 420 ms).
fn index_suite(runs: usize) {
    let dir = TempDir::new("index");
    if let Err(e) = write_notes(&dir.0, 2_000) {
        eprintln!("bench: {e}");
        return;
    }
    let mut v = Vec::new();
    for _ in 0..runs {
        if let Ok((_, svc, t)) = load(dir.0.to_str().unwrap_or(".")) {
            v.push(t);
            drop(svc);
        }
    }
    emit("index", "build_2000_ms", median(v));
}

const QUERIES: &[&str] = &["rope undo", "editor buffer", "graph link", "plain file sync", "quick search", "markdown tag"];

/// The 2,000-note vault indexed in memory, semantic index included.
fn two_thousand() -> IndexService {
    let notes: Vec<(String, String)> = (0..2_000).map(|i| (format!("Folder {}/Note {i}.md", i % 20), synthetic(4096, i))).collect();
    let files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let svc = IndexService::spawn();
    svc.load(Vault::in_memory(&files));
    svc.flush_semantic();
    svc
}

/// §9: note search ≤ 10 ms, section search ≤ 15 ms (2,000 notes); the
/// semantic scan (part of section search) on its own.
fn search_suite(name: &str, runs: usize) {
    let svc = two_thousand();
    let f = SearchFilters::default();
    // Once each, so caches are warm.
    for q in QUERIES {
        let _ = svc.search_sections(q, &f, 10);
    }
    let mut v = Vec::new();
    for _ in 0..runs {
        for q in QUERIES {
            let t = Instant::now();
            match name {
                "search" => {
                    let _ = svc.read().search(q, 20);
                }
                "sections" => {
                    let _ = svc.search_sections(q, &f, 10);
                }
                _ => {
                    if let Some(sem) = svc.semantic() {
                        let _ = sem.search(q, 80);
                    }
                }
            }
            v.push(ms(t.elapsed()));
        }
    }
    match name {
        "search" => emit("search", "bm25_p50_ms", median(v)),
        "sections" => {
            emit("sections", "p50_ms", median(v.clone()));
            emit("sections", "p95_ms", percentile(v, 95));
        }
        _ => {
            let n = svc.semantic().map_or(0, |s| s.len());
            emit("semantic", "scan_p50_ms", median(v));
            emit("semantic", "passages", n as f64);
        }
    }
}

fn mcp_call(srv: &Server, conn: &Conn, tool: &str, args: &Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args,
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
    match srv.handle(&msg, conn, &CancelFlag::new()) {
        Reply::Response(v) => v["result"].clone(),
        _ => Value::Null,
    }
}

/// §9: MCP read tools on 10,000 notes: `view` ≤ 10 ms, `search` p95 ≤ 50 ms
/// (as `crates/quire-mcp/tests/bench.rs`).
fn mcp_suite(runs: usize) {
    let words = ["rope", "undo", "tree", "editor", "buffer", "cursor", "merge", "sync", "note", "index", "query", "vault"];
    let notes: Vec<(String, String)> = (0..10_000)
        .map(|i| {
            let mut t = format!("# Note {i}\n\n## Part one\n");
            for j in 0..30 {
                let _ = write!(t, "{} {} {} {i} ", words[(i + j) % words.len()], words[(i * 7 + j) % words.len()], words[j % words.len()]);
            }
            t.push_str("\n\n## Part two\nMore text about ropes and undo.\n");
            (format!("Folder {}/Note {i}.md", i % 50), t)
        })
        .collect();
    let files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let srv = Server::with_config(
        Vault::in_memory(&files),
        None,
        ServerConfig { device: Some(DeviceInfo { id: "bench".into(), name: "bench".into() }), ..ServerConfig::default() },
    );
    let conn = Conn::stdio(srv.new_conn());
    let (mut view, mut search) = (Vec::new(), Vec::new());
    for i in 0..(50 * runs) {
        let t = Instant::now();
        let _ = mcp_call(
            &srv,
            &conn,
            "view",
            &json!({"path": format!("Folder {}/Note {}.md", (i * 197 % 10_000) % 50, i * 197 % 10_000), "max_tokens": 4000}),
        );
        view.push(ms(t.elapsed()));
        let t = Instant::now();
        let _ = mcp_call(
            &srv,
            &conn,
            "search",
            &json!({"query": format!("{} {}", words[i % words.len()], words[(i + 3) % words.len()]), "max_tokens": 2000}),
        );
        search.push(ms(t.elapsed()));
    }
    emit("mcp", "view_p50_ms", median(view));
    emit("mcp", "search_p95_ms", percentile(search, 95));
}

/// A PDF of `pages` pages of Helvetica text (as `make_fixtures.py` writes them).
pub(crate) fn synthetic_pdf(pages: usize, lines_per_page: usize, seed: u64) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        String::new(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
    ];
    let mut kids = Vec::new();
    for p in 0..pages {
        let text = synthetic(lines_per_page * 90, seed.wrapping_add(p as u64));
        let mut content = String::from("BT /F1 10 Tf 12 TL 50 760 Td\n");
        for line in text.lines().filter(|l| !l.trim().is_empty()).take(lines_per_page) {
            let line: String = line.chars().filter(|c| c.is_ascii() && !matches!(c, '(' | ')' | '\\')).take(95).collect();
            let _ = writeln!(content, "({line}) Tj T*");
        }
        content.push_str("ET");
        let page_obj = objects.len() + 1;
        kids.push(format!("{page_obj} 0 R"));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            page_obj + 1
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }
    objects[1] = format!("<< /Type /Pages /Kids [{}] /Count {pages} >>", kids.join(" "));
    let mut out = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{body}\nendobj\n", i + 1);
    }
    let xref = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for o in offsets {
        let _ = writeln!(out, "{o:010} 00000 n ");
    }
    let _ = write!(out, "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
    out.into_bytes()
}

/// §9: PDF text extraction ≤ 50 ms a page: a 40-page synthetic PDF, and
/// any PDFs named in `QUIRE_BENCH_PDFS` (a folder; CI points it at the
/// quire-pdf fixtures).
fn pdf_suite(runs: usize) {
    let mut docs = vec![synthetic_pdf(40, 55, 3)];
    if let Some(dir) = std::env::var_os("QUIRE_BENCH_PDFS") {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf"))
                && let Ok(b) = std::fs::read(e.path())
            {
                docs.push(b);
            }
        }
    }
    let mut per_page = Vec::new();
    for _ in 0..runs {
        for bytes in &docs {
            let t = Instant::now();
            let Ok(doc) = quire_pdf::PdfDoc::open(bytes.clone()) else { continue };
            let n = doc.page_count().max(1);
            for i in 0..doc.page_count() {
                let _ = doc.page_text(i).map(|p| p.text());
            }
            per_page.push(ms(t.elapsed()) / n as f64);
        }
    }
    emit("pdf", "text_per_page_p50_ms", median(per_page.clone()));
    emit("pdf", "text_per_page_max_ms", per_page.iter().copied().fold(0.0, f64::max));
}

/// 0.10: `quire-cli mcp`/`serve` with 200 PDFs answers at once; PDFs are
/// read in the background.
fn mcp_pdfs_suite(runs: usize) {
    let (mut first, mut all) = (Vec::new(), Vec::new());
    for r in 0..runs {
        let dir = TempDir::new("pdfs");
        let v = match Vault::open_local(&dir.0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("bench: {e}");
                return;
            }
        };
        let _ = v.write(&NotePath::new("Note.md"), "# Note\n\nRopes.\n");
        for i in 0..200 {
            let _ =
                v.storage().write_bytes_atomic(&NotePath::new(format!("papers/p{i:03}.pdf")), &synthetic_pdf(2, 30, (r * 1000 + i) as u64));
        }
        let t = Instant::now();
        let srv = Server::with_config(
            v,
            None,
            ServerConfig { device: Some(DeviceInfo { id: "bench".into(), name: "bench".into() }), ..ServerConfig::default() },
        );
        let conn = Conn::stdio(srv.new_conn());
        let _ = mcp_call(&srv, &conn, "search", &json!({"query": "rope", "filters": {"kind": "all"}}));
        first.push(ms(t.elapsed()));
        let _ = srv.wait_for_pdfs(Duration::from_mins(5));
        all.push(ms(t.elapsed()));
    }
    emit("mcp-pdfs", "first_answer_200_pdfs_ms", median(first));
    emit("mcp-pdfs", "all_read_200_pdfs_ms", median(all));
}
