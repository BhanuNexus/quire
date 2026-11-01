// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Headless frontend over the same core the desktop app uses.

mod import_cmd;
mod mcp;

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use quire_app::find::{FindOptions, compile, replace_in_string};
use quire_app::index::chunk::chunk_note;
use quire_app::index::semantic::build_lsa_index;
use quire_app::index::{Index, IndexService};
use quire_app::intel::{Calculator, extractive_answer, format_number, related_notes, retrieve, suggest_links, unlinked_mentions};
use quire_app::markdown::{blocks, extract, highlight, render};
use quire_app::text::{Buffer, Transaction};
use quire_app::vault::{NotePath, Vault};

const USAGE: &str = "quire-cli — headless Quire

USAGE:
  quire-cli index     <vault>                 index a vault and print stats
  quire-cli search    <vault> <query…>        keyword + meaning search (BM25, #tag, \"phrase\", -exclude; ≈ = by meaning)
  quire-cli ask       <vault> <question…>     answer from your notes with citations (offline)
  quire-cli replace   <vault> <find> <with> [--regex] [--write]   vault-wide replace (dry run unless --write)
  quire-cli open      <vault> <query>         fuzzy quick-open
  quire-cli backlinks <vault> <note>          links and unlinked mentions pointing at a note
  quire-cli related   <vault> <note>          notes related by content
  quire-cli suggest   <vault> <note>          unlinked note names inside a note
  quire-cli tasks     <vault>                 open tasks across the vault
  quire-cli export    <vault> <note> [out]    render a note to standalone HTML
  quire-cli publish   <vault> <out> [--all] [--title T]
                      publish notes marked `publish: true` (or --all) as a static website
  quire-cli mcp       <vault> [--allow-write] MCP server on stdio for AI assistants (read-only by default)
  quire-cli import    <source> <vault> [--into Folder] [--write]
                      import Logseq, Bear, Notion, Evernote or Markdown (folder, .zip, .enex, .bear2bk; dry run first)
  quire-cli history   <vault> <note>          saved versions of a note
  quire-cli calc      <expr>                  inline calculator
  quire-cli bench     [size_mb]               performance check on synthetic data
  quire-cli --version                         print the version";

fn load(vault: &str) -> Result<(Vault, IndexService, f64), String> {
    let v = Vault::open_local(vault).map_err(|e| e.to_string())?;
    let t = Instant::now();
    let svc = IndexService::spawn();
    svc.load(v.clone());
    svc.flush();
    Ok((v, svc, t.elapsed().as_secs_f64() * 1000.0))
}

fn find_note(ix: &Index, name: &str) -> Result<NotePath, String> {
    ix.resolve(name, None)
        .or_else(|| ix.quick_open(name, 1).into_iter().next().map(|h| h.path))
        .ok_or_else(|| format!("no note matches “{name}”"))
}

fn run(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or_else(|| USAGE.to_string());
    match arg(0)? {
        "--version" | "-V" | "version" => println!("quire-cli {}", env!("CARGO_PKG_VERSION")),
        "--help" | "-h" | "help" => println!("{USAGE}"),
        "index" => {
            let (_, svc, ms) = load(arg(1)?)?;
            let ix = svc.read();
            let links: usize = ix.records().map(|r| r.links.iter().filter(|l| l.is_internal()).count()).sum();
            println!("notes      {}", ix.len());
            println!("words      {}", ix.total_words());
            println!("links      {links}");
            println!("tags       {}", ix.tags().len());
            println!("tasks      {} open", ix.tasks(false).len());
            println!("unresolved {}", ix.graph().unresolved().len());
            println!("orphans    {}", ix.orphans().len());
            println!("indexed in {ms:.1} ms");
        }
        "search" => {
            let (_, svc, _) = load(arg(1)?)?;
            svc.flush_semantic();
            let q = args[2..].join(" ");
            let t = Instant::now();
            let hits = svc.hybrid_search(&q, 20);
            let us = t.elapsed().as_micros();
            for h in &hits {
                let mark = if h.by_meaning { "≈" } else { " " };
                let head = h.heading.as_deref().map(|x| format!(" › {x}")).unwrap_or_default();
                println!("{mark} {:.4}  {}{head}\n        {}", h.score, h.path, h.snippet);
            }
            println!("{} results in {us} µs", hits.len());
        }
        "ask" => {
            let (_, svc, _) = load(arg(1)?)?;
            svc.flush_semantic();
            let q = args[2..].join(" ");
            let sem = svc.semantic();
            let ix = svc.read();
            let sources = retrieve(&ix, sem.as_deref(), &q, 6);
            println!("{}\n", extractive_answer(&q, &sources, sem.as_deref()));
            for s in &sources {
                println!("  {}. {}  ({:.2})", s.n, s.citation(), s.score);
            }
        }
        "replace" => {
            let vault = Vault::open_local(arg(1)?).map_err(|e| e.to_string())?;
            let opts = FindOptions { regex: args.iter().any(|a| a == "--regex"), ..Default::default() };
            let write = args.iter().any(|a| a == "--write");
            let re = compile(arg(2)?, opts)?;
            let with = arg(3)?;
            let mut total = 0;
            for p in vault.notes().map_err(|e| e.to_string())? {
                let src = vault.read(&p).map_err(|e| e.to_string())?;
                let (out, n) = replace_in_string(&src, &re, with, opts);
                if n > 0 {
                    println!("{n:>4}  {p}");
                    total += n;
                    if write {
                        vault.write(&p, &out).map_err(|e| e.to_string())?;
                    }
                }
            }
            println!("{total} matches{}", if write { " replaced" } else { " (dry run; add --write)" });
        }
        "open" => {
            let (_, svc, _) = load(arg(1)?)?;
            let hits = svc.read().quick_open(arg(2)?, 10);
            for h in hits {
                println!("{:>5}  {}", h.score, h.path);
            }
        }
        "backlinks" => {
            let (_, svc, _) = load(arg(1)?)?;
            let ix = svc.read();
            let p = find_note(&ix, arg(2)?)?;
            println!("Backlinks to {p}:");
            for b in ix.backlinks(&p) {
                println!("  {}  — {}", b.from, b.context);
            }
            println!("Unlinked mentions:");
            for m in unlinked_mentions(&ix, &p) {
                println!("  {}  — {}", m.from, m.context);
            }
        }
        "related" => {
            let (_, svc, _) = load(arg(1)?)?;
            let ix = svc.read();
            let p = find_note(&ix, arg(2)?)?;
            for r in related_notes(&ix, &p, 10) {
                println!("{:>5.3}  {}{}  ({})", r.score, r.path, if r.linked { " [linked]" } else { "" }, r.because.join(", "));
            }
        }
        "suggest" => {
            let (_, svc, _) = load(arg(1)?)?;
            let ix = svc.read();
            let p = find_note(&ix, arg(2)?)?;
            let src = ix.source(&p).unwrap_or_default().to_string();
            for s in suggest_links(&src, &ix, Some(&p)) {
                println!("“{}” → [[{}]]", s.text, s.target.stem());
            }
        }
        "tasks" => {
            let (_, svc, _) = load(arg(1)?)?;
            let tasks = svc.read().tasks(false);
            for (p, t) in tasks {
                println!("[ ] {}  ({})", t.text, p.stem());
            }
        }
        "export" => {
            let (_, svc, _) = load(arg(1)?)?;
            let ix = svc.read();
            let p = find_note(&ix, arg(2)?)?;
            let src = ix.source(&p).unwrap_or_default();
            let html = render::to_html_document(p.stem(), src, &|l| {
                ix.resolve(&l.target, Some(&p)).map(|t| format!("{}.html", render::encode_url(t.stem())))
            });
            let out = args.get(3).cloned().unwrap_or_else(|| format!("{}.html", p.stem()));
            std::fs::write(&out, html).map_err(|e| e.to_string())?;
            println!("wrote {out}");
        }
        "publish" => {
            let (v, svc, _) = load(arg(1)?)?;
            let out = std::path::Path::new(arg(2)?);
            let title = args.iter().position(|a| a == "--title").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| {
                std::fs::canonicalize(arg(1).unwrap_or("."))
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "Notes".into())
            });
            let which =
                if args.iter().any(|a| a == "--all") { quire_app::publish::Selection::All } else { quire_app::publish::Selection::Marked };
            let r = quire_app::publish::publish(&v, &svc.read(), out, &title, which)?;
            println!("published {} pages and {} files to {}", r.pages, r.files, out.display());
            if r.private_links > 0 {
                println!("{} links point at notes that aren't published; they're plain text on the site", r.private_links);
            }
        }
        "mcp" => {
            let srv = mcp::Server::open(arg(1)?, args.iter().any(|a| a == "--allow-write"))?;
            let stdin = std::io::stdin();
            srv.serve(stdin.lock(), std::io::stdout()).map_err(|e| e.to_string())?;
        }
        "import" => {
            let into = args.iter().position(|a| a == "--into").and_then(|i| args.get(i + 1)).map_or("", String::as_str);
            import_cmd::run(arg(1)?, arg(2)?, into, args.iter().any(|a| a == "--write"))?;
        }
        "history" => {
            let (_, svc, _) = load(arg(1)?)?;
            let p = find_note(&svc.read(), arg(2)?)?;
            let v = Vault::open_local(arg(1)?).map_err(|e| e.to_string())?;
            for s in v.history(&p) {
                let when = quire_app::chrono_fmt(s.at_ms);
                let len = v.read_snapshot(&s).map_or(0, |t| t.chars().count());
                println!("{when}  {len:>7} chars");
            }
        }
        "calc" => {
            let c = Calculator::new();
            let expr = args[1..].join(" ");
            match c.eval(expr.trim_end_matches('=')) {
                Some(v) => println!("{}", format_number(v)),
                None => return Err(format!("can't evaluate “{expr}”")),
            }
        }
        "bench" => bench(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0)),
        _ => return Err(USAGE.to_string()),
    }
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
