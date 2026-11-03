// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The wiki's `index.md` and `log.md` (design §D.7), made without a model:
//!
//! * [`index_block`] lists every page in `wiki/` between
//!   `<!-- quire:index:start -->` and `<!-- quire:index:end -->`, grouped by
//!   front-matter `type`, then by folder, one line per page with its
//!   `description` (or its first sentence). Text outside the markers is the
//!   person's and is kept byte for byte ([`with_index`]).
//! * [`log_line`] is one line of `log.md`, appended where quick capture
//!   would put it ([`crate::capture::placement`]).
//!
//! Both are pure functions of their inputs, so the same vault always gives
//! the same files, and a run that rewrote them can be reverted exactly.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use quire_vault::NotePath;

pub const WIKI: &str = "wiki/";
pub const INDEX: &str = "wiki/index.md";
pub const LOG: &str = "wiki/log.md";
pub const START: &str = "<!-- quire:index:start -->";
pub const END: &str = "<!-- quire:index:end -->";

/// Whether a path is a page the index lists (in `wiki/`, not the index or log).
pub fn is_page(p: &NotePath) -> bool {
    let s = p.as_str();
    p.is_markdown() && s.starts_with(WIKI) && s != INDEX && s != LOG
}

/// The heading for a `type`, in the order groups appear.
fn group_of(kind: &str) -> (u8, String) {
    match kind {
        "source" => (0, "Sources".into()),
        "concept" => (1, "Concepts".into()),
        "entity" => (2, "Entities".into()),
        "answer" => (3, "Answers".into()),
        "" => (9, "Other pages".into()),
        other => {
            let mut c = other.chars();
            let title = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
            (5, title)
        }
    }
}

/// One sentence describing a page: its `description`, else the first
/// sentence of its first paragraph of prose (headings, quotes and callouts,
/// footnotes and comments skipped), at most 120 characters.
pub fn describe(src: &str) -> String {
    let fm = quire_markdown::extract(src).front_matter;
    if let Some(d) = fm.as_ref().and_then(|f| f.get("description")).and_then(|v| v.as_str()).map(str::trim).filter(|d| !d.is_empty()) {
        return clip(d);
    }
    let body = &src[fm.map_or(0, |f| f.range.end).min(src.len())..];
    let mut in_code = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code || t.is_empty() || t.starts_with('#') || t.starts_with('>') || t.starts_with("[^") || t.starts_with("<!--") || t == "---"
        {
            continue;
        }
        let t = t.trim_start_matches(['-', '*', '+', ' ']);
        let plain = quire_markdown::extract(t).plain;
        let plain = strip_footnote_refs(plain.trim());
        let first = plain.split_inclusive(['.', '!', '?']).next().unwrap_or_default().trim();
        if !first.is_empty() {
            return clip(first);
        }
    }
    String::new()
}

/// `text[^1]` → `text`.
fn strip_footnote_refs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[^") {
        out.push_str(&rest[..i]);
        if let Some(j) = rest[i..].find(']') {
            rest = &rest[i + j + 1..];
        } else {
            rest = &rest[i..];
            break;
        }
    }
    out.push_str(rest);
    out
}

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= 120 {
        return s;
    }
    let mut out: String = s.chars().take(119).collect();
    if let Some(i) = out.rfind(' ').filter(|&i| i > 60) {
        out.truncate(i);
    }
    out.push('…');
    out
}

/// The listing that goes between the markers.
///
/// `pages` are (path, text) of wiki pages; `all` is every note path in the vault, to tell when a page's
/// name alone would be ambiguous (then the link names its folder).
/// Folder → [(sort key, line)] within one index group.
type Folders = BTreeMap<String, Vec<(String, String)>>;

pub fn index_block(pages: &[(NotePath, String)], all: &[NotePath]) -> String {
    let mut stems: HashMap<String, usize> = HashMap::new();
    for p in all {
        *stems.entry(p.stem().to_lowercase()).or_default() += 1;
    }
    // group → folder → [(sort key, line)]
    let mut groups: BTreeMap<(u8, String), Folders> = BTreeMap::new();
    for (path, src) in pages.iter().filter(|(p, _)| is_page(p)) {
        let fm = quire_markdown::extract(src).front_matter;
        let kind = fm.as_ref().and_then(|f| f.get("type")).and_then(|v| v.as_str()).unwrap_or_default().trim().to_lowercase();
        let stem = path.stem();
        let link = if stems.get(&stem.to_lowercase()).copied().unwrap_or(0) > 1 {
            format!("[[{}|{stem}]]", path.as_str().trim_end_matches(".md"))
        } else {
            format!("[[{stem}]]")
        };
        let desc = describe(src);
        let line = if desc.is_empty() { format!("- {link}") } else { format!("- {link} — {desc}") };
        let folder = path.as_str().rsplit_once('/').map_or("", |(f, _)| f).trim_start_matches("wiki").trim_start_matches('/').to_string();
        groups.entry(group_of(&kind)).or_default().entry(folder).or_default().push((stem.to_lowercase(), line));
    }
    let mut out = String::new();
    for ((_, heading), folders) in groups {
        let _ = write!(out, "\n## {heading}\n");
        let several = folders.len() > 1;
        for (folder, mut lines) in folders {
            lines.sort();
            if several && !folder.is_empty() {
                let _ = write!(out, "\n### {folder}\n");
            }
            out.push('\n');
            for (_, l) in lines {
                out.push_str(&l);
                out.push('\n');
            }
        }
    }
    if out.is_empty() {
        out.push_str("\nNo wiki pages yet.\n");
    }
    out.push('\n');
    out
}

/// `existing` (the current `index.md`, if any) with `block` between the
/// markers. Without markers, they are added at the end; without a file, a
/// new one is started.
pub fn with_index(existing: Option<&str>, block: &str) -> String {
    let Some(text) = existing else {
        return format!(
            "# Wiki index\n\nQuire keeps the list below up to date. Write anything you like outside the markers.\n\n{START}\n{block}{END}\n"
        );
    };
    if let (Some(s), Some(e)) = (text.find(START), text.find(END))
        && s < e
    {
        let inner = s + START.len();
        let mut out = String::with_capacity(text.len() + block.len());
        out.push_str(&text[..inner]);
        out.push('\n');
        out.push_str(block);
        out.push_str(&text[e..]);
        return out;
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let _ = write!(out, "\n{START}\n{block}{END}\n");
    out
}

/// The new `index.md` for this vault, or `None` when it is already current.
pub fn regenerate(existing: Option<&str>, pages: &[(NotePath, String)], all: &[NotePath]) -> Option<String> {
    let next = with_index(existing, &index_block(pages, all));
    (existing != Some(next.as_str())).then_some(next)
}

/// One line of `log.md`: `- 2026-11-02 14:05 · ingest · [[raw/Paper]] → 7 pages · Claude Sonnet (run r_…)`.
pub fn log_line(at: &str, what: &str, detail: &str, by: &str, run: Option<&str>) -> String {
    let mut s = format!("- {at} · {what}");
    if !detail.is_empty() {
        s.push_str(" · ");
        s.push_str(detail);
    }
    if !by.is_empty() {
        s.push_str(" · ");
        s.push_str(by);
    }
    if let Some(r) = run {
        let _ = write!(s, " (run {r})");
    }
    s
}

/// `log.md` with `line` appended (a new file starts with a heading).
pub fn append_log(existing: Option<&str>, line: &str) -> String {
    let base = existing.unwrap_or("# Wiki log\n\nWhat agents and Quire did in the wiki, newest last.\n");
    crate::capture::append_entry(base, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NotePath {
        s.into()
    }

    #[test]
    fn index_groups_by_type_and_folder_and_is_deterministic() {
        let pages = vec![
            (p("wiki/concepts/Ropes.md"), "---\ntype: concept\n---\n# Ropes\nA rope is a tree of strings. It makes edits cheap.".into()),
            (p("wiki/sources/Paper X.md"), "---\ntype: source\ndescription: Boehm et al. on ropes\n---\n# Paper X\n…".into()),
            (p("wiki/concepts/Gap buffers.md"), "---\ntype: concept\n---\nGap buffers keep a hole at the cursor!".into()),
            (p("wiki/people/Ada.md"), "---\ntype: entity\n---\nAda wrote notes.".into()),
            (p("wiki/Misc.md"), "Just a page".into()),
            (p("wiki/index.md"), "ignored".into()),
            (p("wiki/log.md"), "ignored".into()),
        ];
        let all: Vec<NotePath> = pages.iter().map(|(p, _)| p.clone()).chain([p("Ropes.md")]).collect();
        let block = index_block(&pages, &all);
        assert_eq!(
            block,
            "\n## Sources\n\n- [[Paper X]] — Boehm et al. on ropes\n\n## Concepts\n\n- [[Gap buffers]] — Gap buffers keep a hole at the cursor!\n- [[wiki/concepts/Ropes|Ropes]] — A rope is a tree of strings.\n\n## Entities\n\n- [[Ada]] — Ada wrote notes.\n\n## Other pages\n\n- [[Misc]] — Just a page\n\n"
        );
        let mut shuffled = pages;
        shuffled.reverse();
        assert_eq!(index_block(&shuffled, &all), block, "order of input doesn't matter");
    }

    #[test]
    fn only_the_text_between_markers_changes() {
        let mine = format!("# My wiki\nIntro I wrote.\n{START}\nold\n{END}\nFooter I wrote.\n");
        let next = with_index(Some(&mine), "\n- [[A]]\n\n");
        assert_eq!(next, format!("# My wiki\nIntro I wrote.\n{START}\n\n- [[A]]\n\n{END}\nFooter I wrote.\n"));
        let pages = [(p("wiki/A.md"), "# A\n\n> [!ai] Written by a model.\n\nA page[^1] about A. More.\n\n[^1]: x".to_string())];
        let regenerated = regenerate(Some(&next), &pages, &[]).unwrap();
        assert!(regenerated.contains("- [[A]] — A page about A.\n"), "{regenerated}");
        assert_eq!(regenerate(Some(&regenerated), &pages, &[]), None, "nothing to do when current");
        let fresh = with_index(None, "\n- [[A]]\n\n");
        assert!(fresh.starts_with("# Wiki index") && fresh.contains(START) && fresh.ends_with(&format!("{END}\n")));
        assert!(with_index(Some("no markers"), "x\n").ends_with(&format!("no markers\n\n{START}\nx\n{END}\n")));
        assert_eq!(index_block(&[], &[]), "\nNo wiki pages yet.\n\n");
    }

    #[test]
    fn log_lines_append() {
        let l = log_line("2026-11-02 14:05", "ingest", "[[raw/Paper]] → 7 pages", "Claude Sonnet", Some("r_1"));
        assert_eq!(l, "- 2026-11-02 14:05 · ingest · [[raw/Paper]] → 7 pages · Claude Sonnet (run r_1)");
        let a = append_log(None, &l);
        assert!(a.starts_with("# Wiki log") && a.ends_with(&format!("{l}\n")));
        let b = append_log(Some(&a), "- second");
        assert!(b.starts_with(&a) && b.ends_with("- second\n"), "{b}");
        assert_eq!(describe(&format!("# T\n{}", "word ".repeat(60))).chars().count(), 115);
    }
}
