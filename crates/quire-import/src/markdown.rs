// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Any folder of Markdown: copy notes and files, keeping relative paths so
//! relative image links keep working.

use quire_vault::NotePath;

use std::collections::HashSet;

use crate::convert::{join_rel, outside_code};
use crate::{ImportPlan, PlannedFile, PlannedNote, Tree, percent_decode, rewrite_links};

/// `[text](other.md#part)` between notes of the folder becomes `[[other#part|text]]`.
fn wikilinks(content: &str, dir: &str, notes: &HashSet<String>) -> String {
    outside_code(content, |part| {
        rewrite_links(part, |l| {
            if l.image {
                return None;
            }
            let (dest, heading) = l.dest.split_once('#').map_or((l.dest.as_str(), None), |(d, h)| (d, Some(h)));
            if dest.is_empty() || dest.contains("://") || dest.starts_with("mailto:") {
                return None;
            }
            let full = join_rel(dir, &percent_decode(dest))?;
            if !notes.contains(&full.to_ascii_lowercase()) {
                return None;
            }
            let target = full.rsplit_once('.').map_or(full.as_str(), |(s, _)| s);
            let stem = target.rsplit('/').next().unwrap_or(target);
            let heading = heading.map(|h| format!("#{}", percent_decode(h))).unwrap_or_default();
            Some(if l.text.is_empty() || l.text == stem {
                format!("[[{target}{heading}]]")
            } else {
                format!("[[{target}{heading}|{}]]", l.text)
            })
        })
    })
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let notes: HashSet<String> =
        tree.paths().filter(|p| Tree::is_markdown(p) || p.to_ascii_lowercase().ends_with(".txt")).map(str::to_ascii_lowercase).collect();
    for (src, bytes) in tree.iter() {
        let path = src;
        if Tree::is_markdown(path) {
            let content = wikilinks(&tree.text(path).unwrap_or_default(), src.rsplit_once('/').map_or("", |(d, _)| d), &notes);
            let path =
                if path.to_ascii_lowercase().ends_with(".markdown") { format!("{}.md", &path[..path.len() - 9]) } else { path.to_string() };
            plan.notes.push(PlannedNote { path: NotePath::new(path), content, times: tree.stamp(src) });
        } else if path.to_ascii_lowercase().ends_with(".txt") {
            let content = tree.text(path).unwrap_or_default();
            plan.notes.push(PlannedNote {
                path: NotePath::new(format!("{}.md", &path[..path.len() - 4])),
                content,
                times: tree.stamp(src),
            });
        } else {
            plan.files.push(PlannedFile { path: NotePath::new(path), bytes: bytes.to_vec(), times: tree.stamp(src) });
        }
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no Markdown or text files found".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_notes_and_files() {
        let t = Tree::from_files([("a.md", &b"# A"[..]), ("sub/b.markdown", b"B"), ("sub/img.png", b"\x89PNG"), ("c.txt", b"plain")]);
        let p = plan(&t);
        let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(names, vec!["a.md", "c.md", "sub/b.md"]);
        assert_eq!(p.files[0].path.as_str(), "sub/img.png");
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn links_between_notes_become_wikilinks() {
        let t = Tree::from_files([
            ("a.md", &b"See [B](sub/b.md#top), [the C](sub/../c.md), [web](https://x.org/a.md), [gone](nope.md) and ![](img.png)\n```\n[B](sub/b.md)\n```\n"[..]),
            ("sub/b.md", b"back to [A](../a.md)"),
            ("c.md", b"c"),
        ]);
        let p = plan(&t);
        assert_eq!(
            p.notes[0].content,
            "See [[sub/b#top|B]], [[c|the C]], [web](https://x.org/a.md), [gone](nope.md) and ![](img.png)\n```\n[B](sub/b.md)\n```\n"
        );
        assert_eq!(p.notes[2].content, "back to [[a|A]]");
    }
}
