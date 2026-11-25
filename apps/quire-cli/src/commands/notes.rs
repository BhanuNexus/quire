// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Working with notes: vault-wide replace, export, PDF, publish, HTML for
//! pasting, version history and the calculator. The `html`
//! command came with issue #17.

use quire_app::find::{FindOptions, compile, replace_in_string};
use quire_app::intel::{Calculator, format_number};
use quire_app::markdown::render;

use super::{CliCommand, find_note, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "replace",
        rank: 4,
        usage: "  quire-cli replace   <vault> <find> <with> [--regex] [--write]   vault-wide replace (dry run unless --write);
                      put -- before a <find> or <with> that starts with a dash",
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
        usage: "  quire-cli publish   <vault> <out> [--all] [--title T] [--typed-links auto|on|off]
                      publish notes marked `publish: true` (or --all) as a static website; pages get a
                      breadcrumbs trail and typed links when the vault uses hierarchy fields (auto)",
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
    let vault = super::open_vault(arg(1)?)?;
    let opts = FindOptions { regex: super::has_flag(args, "--regex"), ..Default::default() };
    let write = super::has_flag(args, "--write");
    // The words that aren't `--regex` / `--write` (those may stand anywhere
    // before a `--`; after it, even they are text).
    let words = super::words_without(args, 0, &["--write", "--regex"]);
    let re = compile(super::arg(&words, 2)?, opts)?;
    let with = super::arg(&words, 3)?;
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
    // `[@key]` citations as author-date text and a reference list.
    let src = quire_app::citations::render_with_vault(&vault, ix.source(&p).unwrap_or_default());
    // 0.12: the note's own HTML is cleaned (no scripts, event handlers or `javascript:` links),
    // so the page is safe to open whatever the note holds.
    let body = quire_app::sanitize::note_html(&render::to_html(&src, &|l| {
        ix.resolve(&l.target, Some(&p)).map(|t| format!("{}.html", render::encode_url(t.stem())))
    }));
    let html = render::html_document(p.stem(), &body);
    let out = args.get(3).cloned().unwrap_or_else(|| format!("{}.html", p.stem()));
    quire_base::fs::write_atomic(std::path::Path::new(&out), html.as_bytes(), quire_base::fs::Options::EXISTING_FOLDER)
        .map_err(|e| e.to_string())?;
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
    // Written into the vault first (so images resolve the same way), then
    // written whole to `out` (another disk is fine) and removed.
    let tmp = quire_app::vault::NotePath::new(format!(".quire/export-{}.pdf", std::process::id()));
    let written = quire_app::export_pdf::export_pdf(&v, p.stem(), &src, &tmp)?;
    let moved = std::fs::read(&written)
        .and_then(|pdf| quire_base::fs::write_atomic(std::path::Path::new(&out), &pdf, quire_base::fs::Options::EXISTING_FOLDER));
    let _ = std::fs::remove_file(&written);
    moved.map_err(|e| e.to_string())?;
    println!("wrote {out}");
    Ok(())
}

fn publish(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (v, svc, _) = load(arg(1)?)?;
    let out = std::path::Path::new(arg(2)?);
    let title = super::flag_value(args, "--title").map_or_else(
        || {
            std::fs::canonicalize(arg(1).unwrap_or("."))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "Notes".into())
        },
        str::to_string,
    );
    let which = if super::has_flag(args, "--all") { quire_app::publish::Selection::All } else { quire_app::publish::Selection::Marked };
    let typed = match super::flag_value(args, "--typed-links") {
        None => quire_app::publish::TypedLinks::Auto,
        Some(v) => quire_app::publish::TypedLinks::parse(v).ok_or("--typed-links takes auto, on or off")?,
    };
    let r = quire_app::publish::publish_with(&v, &svc.read(), out, &title, which, typed)?;
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
    let v = super::open_vault(arg(1)?)?;
    for s in v.history(&p) {
        let when = quire_app::local_time_fmt(s.at_ms);
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
