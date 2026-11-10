// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A folder of `.html` files: saved pages, and the exports of apps that
//! only write HTML (Apple Notes through an exporter, the "Save as web page"
//! of Microsoft's notebook app, Tomboy, Zoho).
//!
//! Each file becomes a note named from its `<title>` (or first heading, or
//! file name). Links between the pages become wikilinks, pictures next to
//! them are kept in `attachments/`, and the file's date is kept.

use std::collections::{HashMap, HashSet};

use quire_vault::{NotePath, sanitize_title};

use crate::convert::{iso, join_rel, shorten};
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Stamp, Tree, attachment, file_name, html, percent_decode, rewrite_links, unique_name};

fn is_html(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    [".html", ".htm", ".xhtml"].iter().any(|e| l.ends_with(e))
}

pub(crate) fn detect(tree: &Tree) -> bool {
    tree.paths().any(is_html) && !tree.paths().any(Tree::is_markdown)
}

/// Text between `<tag …>` and `</tag>` (first one, case-insensitive).
fn element<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find(&format!("</{tag}"))?;
    html.get(start..end)
}

fn plain(s: &str) -> String {
    html::unescape(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let pages: Vec<&str> = tree.paths().filter(|p| is_html(p)).collect();
    let mut taken_notes = HashSet::new();
    let mut taken_files = HashSet::new();
    // First pass: each page's note name, so links between pages can be rewritten.
    let mut names: HashMap<&str, String> = HashMap::new();
    let mut texts: HashMap<&str, String> = HashMap::new();
    for p in &pages {
        let src = tree.text(p).unwrap_or_default();
        let title = element(&src, "title")
            .map(plain)
            .filter(|t| !t.is_empty())
            .or_else(|| element(&src, "h1").map(|h| plain(&html::to_markdown(h, &mut |_| None))))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| file_name(p).rsplit_once('.').map_or_else(|| file_name(p), |(s, _)| s).to_string());
        let name = sanitize_title(&shorten(&title, 100));
        let name = if name.is_empty() { "Untitled".to_string() } else { name };
        let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
        let rel = if dir.is_empty() { format!("{name}.md") } else { format!("{dir}/{name}.md") };
        names.insert(p, unique_name(&mut taken_notes, &rel));
        texts.insert(p, src);
    }
    let by_path: HashMap<String, &str> = pages.iter().map(|p| (p.to_ascii_lowercase(), *p)).collect();
    let mut attached: HashMap<String, String> = HashMap::new();
    let mut unresolved = 0;
    for p in &pages {
        let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
        let src = &texts[p];
        let body_html = element(src, "body").unwrap_or(src);
        let md = html::to_markdown(body_html, &mut |_| None);
        let body = rewrite_links(&md, |l| {
            let dest = l.dest.split('#').next().unwrap_or_default();
            if dest.is_empty() || dest.contains("://") || dest.starts_with("mailto:") || dest.starts_with("data:") {
                return None;
            }
            let full = join_rel(dir, &percent_decode(dest))?;
            if let Some(target) = by_path.get(&full.to_ascii_lowercase()).and_then(|k| names.get(k)) {
                let t = target.trim_end_matches(".md");
                let stem = t.rsplit('/').next().unwrap_or(t);
                return Some(if l.text.is_empty() || l.text == stem { format!("[[{t}]]") } else { format!("[[{t}|{}]]", l.text) });
            }
            if let Some(bytes) = tree.get(&full) {
                let stored = attached.entry(full.clone()).or_insert_with(|| {
                    let target = attachment(&mut taken_files, file_name(&full));
                    plan.files.push(PlannedFile { path: NotePath::new(target.clone()), bytes: bytes.to_vec(), times: tree.stamp(&full) });
                    file_name(&target).to_string()
                });
                return Some(if l.image { format!("![[{stored}]]") } else { format!("[[{stored}|{}]]", l.text) });
            }
            unresolved += 1;
            None
        });
        let stamp = tree.stamp(p);
        let mut fields = Vec::new();
        if let Some(c) = stamp.created.or(stamp.modified) {
            fields.push(("created".to_string(), Field::One(iso(c))));
        }
        if let Some(m) = stamp.modified {
            fields.push(("updated".to_string(), Field::One(iso(m))));
        }
        let note = &names[p];
        plan.notes.push(PlannedNote {
            path: NotePath::new(note.clone()),
            content: format!("{}{}\n", front_matter(&fields), body.trim()),
            times: Stamp { created: stamp.created.or(stamp.modified), modified: stamp.modified },
        });
    }
    if unresolved > 0 {
        plan.warnings.push(format!("{unresolved} links or pictures point to files that weren't in the folder"));
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no HTML files found".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::from_files([
            ("notes/First.html", &b"<html><head><title>First &amp; best</title><style>p{}</style></head><body><h1>Heading</h1><p>See <a href=\"Second.html#x\">the other</a> and <a href=\"../About%20us.html\">About us</a><img src=\"img/pic.png\"> <a href=\"https://x.org\">web</a> <a href=\"missing.html\">gone</a></p></body></html>"[..]),
            ("notes/Second.html", b"<html><body><h1>Second page</h1><p>text</p></body></html>"),
            ("About us.html", b"<html><head><title>About</title></head><body><p>Hi</p></body></html>"),
            ("notes/img/pic.png", b"\x89PNG"),
        ])
    }

    #[test]
    fn pages_links_and_pictures() {
        assert!(detect(&tree()));
        let p = plan(&tree());
        let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(names, vec!["About.md", "notes/First & best.md", "notes/Second page.md"]);
        let first = &p.notes[1].content;
        assert!(first.contains("[[notes/Second page|the other]]"), "{first}");
        assert!(first.contains("[[About|About us]]"), "{first}");
        assert!(first.contains("![[pic.png]]"), "{first}");
        assert!(first.contains("[web](https://x.org)"), "{first}");
        assert!(!first.contains("<style>") && !first.contains("p{}"), "{first}");
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].path.as_str(), "attachments/pic.png");
        assert!(p.warnings.iter().any(|w| w.contains("weren't in the folder")), "{:?}", p.warnings);
    }

    #[test]
    fn links_cannot_reach_outside_the_folder() {
        let t = Tree::from_files([("a.html", &b"<p><a href=\"../../etc/passwd\">x</a><img src=\"../../secret.png\"></p>"[..])]);
        let p = plan(&t);
        assert!(p.files.is_empty());
        assert_eq!(p.notes.len(), 1);
    }
}
