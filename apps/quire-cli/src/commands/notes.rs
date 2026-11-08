// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Working with notes: vault-wide replace, export, PDF, publish, HTML for
//! pasting, version history and the calculator. Frozen after F0 (`html`
//! came with issue #17).

use quire_app::find::{FindOptions, compile, replace_in_string};
use quire_app::intel::{Calculator, format_number};
use quire_app::markdown::render;

use super::{CliCommand, find_note, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "replace",
        rank: 4,
        usage: "  quire-cli replace   <vault> <find> <with> [--regex] [--write]   vault-wide replace (dry run unless --write)",
        run: replace,
    },
    CliCommand {
        name: "export",
        rank: 11,
        usage: "  quire-cli export    <vault> <note> [out]    render a note to standalone HTML",
        run: export,
    },
    CliCommand {
        name: "pdf",
        rank: 12,
        usage: "  quire-cli pdf       <vault> <note> [out]    typeset a note as a PDF (math, diagrams, callouts, images)",
        run: pdf,
    },
    CliCommand {
        name: "publish",
        rank: 13,
        usage: "  quire-cli publish   <vault> <out> [--all] [--title T]
                      publish notes marked `publish: true` (or --all) as a static website",
        run: publish,
    },
    CliCommand {
        name: "html",
        rank: 19,
        usage: "  quire-cli html      <vault> <note>          print a note as HTML (as \"Copy as HTML\" copies it)",
        run: html,
    },
    CliCommand { name: "history", rank: 16, usage: "  quire-cli history   <vault> <note>          saved versions of a note", run: history },
    CliCommand { name: "calc", rank: 17, usage: "  quire-cli calc      <expr>                  inline calculator", run: calc },
];

fn replace(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let vault = quire_app::secure_state::open_vault(std::path::Path::new(arg(1)?))?;
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
    Ok(())
}

fn export(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (vault, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    // WS-G: `[@key]` citations as author-date text and a reference list.
    let src = quire_app::citations::render_with_vault(&vault, ix.source(&p).unwrap_or_default());
    let html = render::to_html_document(p.stem(), &src, &|l| {
        ix.resolve(&l.target, Some(&p)).map(|t| format!("{}.html", render::encode_url(t.stem())))
    });
    let out = args.get(3).cloned().unwrap_or_else(|| format!("{}.html", p.stem()));
    std::fs::write(&out, html).map_err(|e| e.to_string())?;
    println!("wrote {out}");
    Ok(())
}

/// The note as an HTML fragment on stdout, rendered as "Copy as HTML" does
/// (`export` writes a standalone page to a file instead).
fn html(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (vault, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    let library = quire_app::citations::vault_library(&vault);
    print!("{}", quire_app::copy_html::render(&vault, &ix, &library, &p, ix.source(&p).unwrap_or_default()));
    Ok(())
}

#[cfg(not(feature = "typeset"))]
fn pdf(_args: &[String]) -> Result<(), String> {
    Err("PDF export comes with the full edition of Quire (this is Quire Lite)".into())
}

#[cfg(feature = "typeset")]
fn pdf(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (v, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    let src = ix.source(&p).unwrap_or_default().to_string();
    let out = args.get(3).cloned().unwrap_or_else(|| format!("{}.pdf", p.stem()));
    // Written into the vault first (so images resolve the same way), then moved.
    let tmp = quire_app::vault::NotePath::new(format!(".quire/export-{}.pdf", std::process::id()));
    let written = quire_app::export_pdf::export_pdf(&v, p.stem(), &src, &tmp)?;
    std::fs::rename(&written, &out)
        .or_else(|_| std::fs::copy(&written, &out).map(|_| ()).and_then(|()| std::fs::remove_file(&written)))
        .map_err(|e| e.to_string())?;
    println!("wrote {out}");
    Ok(())
}

fn publish(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (v, svc, _) = load(arg(1)?)?;
    let out = std::path::Path::new(arg(2)?);
    let title = args.iter().position(|a| a == "--title").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| {
        std::fs::canonicalize(arg(1).unwrap_or("."))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Notes".into())
    });
    let which = if args.iter().any(|a| a == "--all") { quire_app::publish::Selection::All } else { quire_app::publish::Selection::Marked };
    let r = quire_app::publish::publish(&v, &svc.read(), out, &title, which)?;
    println!("published {} pages and {} files to {}", r.pages, r.files, out.display());
    if r.private_links > 0 {
        println!("{} links point at notes that aren't published; they're plain text on the site", r.private_links);
    }
    Ok(())
}

fn history(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let p = find_note(&svc.read(), arg(2)?)?;
    let v = quire_app::secure_state::open_vault(std::path::Path::new(arg(1)?))?;
    for s in v.history(&p) {
        let when = quire_app::chrono_fmt(s.at_ms);
        let len = v.read_snapshot(&s).map_or(0, |t| t.chars().count());
        println!("{when}  {len:>7} chars");
    }
    Ok(())
}

fn calc(args: &[String]) -> Result<(), String> {
    let c = Calculator::new();
    let expr = args[1..].join(" ");
    match c.eval(expr.trim_end_matches('=')) {
        Some(v) => println!("{}", format_number(v)),
        None => return Err(format!("can't evaluate “{expr}”")),
    }
    Ok(())
}
