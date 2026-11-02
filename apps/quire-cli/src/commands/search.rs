// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reading the vault: index stats, search, ask, quick open, backlinks,
//! related notes and link suggestions. Frozen after F0 (WS-C changes the
//! engines underneath, not these commands).

use std::time::Instant;

use quire_app::intel::{extractive_answer, related_notes, retrieve, suggest_links, unlinked_mentions};

use super::{CliCommand, find_note, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand { name: "index", rank: 1, usage: "  quire-cli index     <vault>                 index a vault and print stats", run: index },
    CliCommand {
        name: "search",
        rank: 2,
        usage: "  quire-cli search    <vault> <query…>        keyword + meaning search (BM25, #tag, \\\"phrase\\\", -exclude; ≈ = by meaning)",
        run: search,
    },
    CliCommand {
        name: "ask",
        rank: 3,
        usage: "  quire-cli ask       <vault> <question…>     answer from your notes with citations (offline)",
        run: ask,
    },
    CliCommand { name: "open", rank: 5, usage: "  quire-cli open      <vault> <query>         fuzzy quick-open", run: open_note },
    CliCommand {
        name: "backlinks",
        rank: 6,
        usage: "  quire-cli backlinks <vault> <note>          links and unlinked mentions pointing at a note",
        run: backlinks,
    },
    CliCommand { name: "related", rank: 7, usage: "  quire-cli related   <vault> <note>          notes related by content", run: related },
    CliCommand {
        name: "suggest",
        rank: 8,
        usage: "  quire-cli suggest   <vault> <note>          unlinked note names inside a note",
        run: suggest,
    },
];

fn index(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
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
    Ok(())
}

fn search(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
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
    Ok(())
}

fn ask(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
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
    Ok(())
}

fn open_note(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let hits = svc.read().quick_open(arg(2)?, 10);
    for h in hits {
        println!("{:>5}  {}", h.score, h.path);
    }
    Ok(())
}

fn backlinks(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
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
    Ok(())
}

fn related(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    for r in related_notes(&ix, &p, 10) {
        println!("{:>5.3}  {}{}  ({})", r.score, r.path, if r.linked { " [linked]" } else { "" }, r.because.join(", "));
    }
    Ok(())
}

fn suggest(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    let src = ix.source(&p).unwrap_or_default().to_string();
    for s in suggest_links(&src, &ix, Some(&p)) {
        println!("“{}” → [[{}]]", s.text, s.target.stem());
    }
    Ok(())
}
