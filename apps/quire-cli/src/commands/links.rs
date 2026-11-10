// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 typed links.
//!
//! `links` (a note's links and backlinks by type) and `tree` (the
//! hierarchy under a note, and the trail above it).

use std::collections::HashSet;

use quire_app::index::{Hier, Index};
use quire_app::links_state::{self, type_label};
use quire_app::markdown::fields::normalize_key;
use quire_app::vault::NotePath;

use super::{CliCommand, find_note, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "links",
        rank: 48,
        usage: "  quire-cli links     <vault> <note> [--type T]...
                      a note's links and backlinks grouped by type (up, supports…; \"untyped\" for plain links)",
        run: links,
    },
    CliCommand {
        name: "tree",
        rank: 49,
        usage: "  quire-cli tree      <vault> <note> [--depth N]
                      the hierarchy under a note (down links), with the trail above it",
        run: tree,
    },
];

/// Every `--flag value` in `args`.
fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2).filter(|w| w[0] == flag).map(|w| w[1].clone()).collect()
}

/// The lines `links` prints for note `p` (only the types in `only`, if given).
fn links_report(ix: &Index, p: &NotePath, only: &HashSet<String>) -> Vec<String> {
    let keep = |t: &str| only.is_empty() || only.contains(t);
    let mut out = Vec::new();
    let rel = links_state::relations_of(ix, p);
    for g in &rel.groups {
        let key = g.hier.map_or_else(|| g.items.first().map(|i| i.field.clone()).unwrap_or_default(), |h| h.key().to_string());
        let items: Vec<_> = g.items.iter().filter(|i| keep(&key) || keep(&i.field)).collect();
        if items.is_empty() {
            continue;
        }
        out.push(format!("{}:", g.label));
        for i in items {
            let arrow = if i.incoming { "← " } else { "" };
            let how = if i.implied {
                format!(" (implied by its {})", ix.link_config().field_for(g.hier.map_or(Hier::Up, Hier::reverse)))
            } else {
                String::new()
            };
            out.push(format!("  {arrow}{}{how}", i.path));
        }
    }
    if keep("untyped") {
        let mut plain: Vec<String> =
            ix.backlinks(p).iter().filter(|b| b.link.field.is_none()).map(|b| format!("  ← {}  — {}", b.from, b.context)).collect();
        plain.dedup();
        let out_plain: Vec<String> = ix
            .graph()
            .outgoing(p)
            .iter()
            .filter(|o| ix.backlinks(o).iter().any(|b| b.from == *p && b.link.field.is_none()))
            .map(|o| format!("  {o}"))
            .collect();
        if !plain.is_empty() || !out_plain.is_empty() {
            out.push(type_label("untyped") + ":");
            out.extend(out_plain);
            out.extend(plain);
        }
    }
    out
}

fn links(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    let only: HashSet<String> = flag_values(args, "--type").iter().map(|t| normalize_key(t)).collect();
    let lines = links_report(&ix, &p, &only);
    println!("Links of {p}:");
    if lines.is_empty() {
        println!("  (none{})", if only.is_empty() { "" } else { " of these types" });
    }
    for l in lines {
        println!("{l}");
    }
    Ok(())
}

/// The lines `tree` prints: trails down to `p`, then `p`'s subtree.
fn tree_report(ix: &Index, p: &NotePath, depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    for t in ix.relations().trails(p, 4, 32) {
        out.push(t.iter().map(|q| q.stem().to_string()).collect::<Vec<_>>().join(" › "));
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    // Expand every note down to `depth` levels.
    let mut open: HashSet<NotePath> = HashSet::new();
    let mut frontier = vec![p.clone()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for q in frontier {
            if open.insert(q.clone()) {
                next.extend(ix.relations().children(&q).into_iter().cloned());
            }
        }
        frontier = next;
    }
    for row in links_state::tree_rows(ix, std::slice::from_ref(p), &open) {
        let mark = if row.repeat {
            " ↻ (cycle)"
        } else if row.has_children && !open.contains(&row.path) {
            " …"
        } else {
            ""
        };
        out.push(format!("{}{}{mark}", "  ".repeat(row.depth), row.path));
    }
    out
}

fn tree(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let ix = svc.read();
    let p = find_note(&ix, arg(2)?)?;
    let depth = flag_values(args, "--depth").first().and_then(|d| d.parse().ok()).unwrap_or(8usize).clamp(1, 64);
    for l in tree_report(&ix, &p, depth) {
        println!("{l}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_app::index::NoteRecord;

    fn index() -> Index {
        let mut ix = Index::new();
        for (p, s) in [
            ("Maths.md", "# Maths\n"),
            ("Calculus.md", "---\nup: \"[[Maths]]\"\n---\n# Calculus\n"),
            ("Limits.md", "up:: [[Calculus]]\nnext:: [[Series]]\n[supports:: [[Claim]]]\nSee [[Maths]].\n"),
            ("Series.md", "parent:: [[Calculus]]\nup:: [[Limits]]\n"),
            ("Claim.md", "# Claim\n"),
        ] {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p), s), s.to_string());
        }
        ix.refresh();
        ix
    }

    #[test]
    fn links_by_type() {
        let ix = index();
        let all = links_report(&ix, &NotePath::new("Limits.md"), &HashSet::new());
        assert_eq!(
            all,
            [
                "Parents:",
                "  Calculus.md",
                "Children:",
                "  Series.md (implied by its up)",
                "Next:",
                "  Series.md",
                "Supports:",
                "  Claim.md",
                "Untyped:",
                "  Maths.md"
            ]
        );
        let only = links_report(&ix, &NotePath::new("Claim.md"), &HashSet::from(["supports".to_string()]));
        assert_eq!(only, ["Supports:", "  ← Limits.md"]);
    }

    #[test]
    fn tree_with_trail_and_cycle_safe() {
        let ix = index();
        let t = tree_report(&ix, &NotePath::new("Calculus.md"), 8);
        assert_eq!(t, ["Maths › Calculus", "", "Calculus.md", "  Limits.md", "    Series.md", "  Series.md"]);
        let shallow = tree_report(&ix, &NotePath::new("Maths.md"), 1);
        assert_eq!(shallow, ["Maths.md", "  Calculus.md …"]);
    }
}
