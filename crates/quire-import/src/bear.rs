// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bear: `.bear2bk` backups and TextBundle folders.

use std::collections::{HashMap, HashSet};

use quire_vault::{NotePath, sanitize_title};

use crate::{ImportPlan, PlannedFile, PlannedNote, Tree, attachment, file_name, rewrite_links};

pub fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut bundles: Vec<String> = tree
        .paths()
        .filter_map(|p| p.find(".textbundle/").map(|i| p[..i + ".textbundle".len()].to_string()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    bundles.sort();
    let mut taken_files = HashSet::new();
    let mut taken_notes = HashSet::new();
    for bundle in bundles {
        let text = ["text.md", "text.markdown", "text.txt"].iter().find_map(|f| tree.text(&format!("{bundle}/{f}")));
        let Some(text) = text else {
            plan.warnings.push(format!("{bundle}: no text file"));
            continue;
        };
        // Assets get vault-unique names; links are rewritten to embeds.
        let mut assets = HashMap::new();
        let prefix = format!("{bundle}/assets/");
        for (path, bytes) in tree.iter().filter(|(p, _)| p.starts_with(&prefix)) {
            let rel = &path[bundle.len() + 1..];
            let target = attachment(&mut taken_files, rel);
            assets.insert(rel.to_string(), file_name(&target).to_string());
            plan.files.push(PlannedFile { path: NotePath::new(target), bytes: bytes.to_vec() });
        }
        let body = rewrite_links(&text, |l| {
            let target = assets.get(l.dest.trim_start_matches("./"))?;
            Some(if l.image { format!("![[{target}]]") } else { format!("[[{target}|{}]]", l.text) })
        });
        let body = normalize_tags(&body);
        let bundle_name = file_name(&bundle).trim_end_matches(".textbundle");
        let title = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| l.strip_prefix("# "))
            .map(sanitize_title)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| sanitize_title(bundle_name));
        let title = if title.is_empty() { "Untitled".to_string() } else { title };
        let path = crate::unique_name(&mut taken_notes, &format!("{title}.md"));
        plan.notes.push(PlannedNote { path: NotePath::new(path), content: body });
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no TextBundles found".into());
    }
    plan
}

/// Bear's `#multi word tag#` → `#multi-word-tag`. Single-word and nested
/// tags (`#work/q4`) are already valid Quire tags.
pub fn normalize_tags(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_fence = false;
    for line in src.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence || !line.contains('#') {
            out.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let at_boundary = i == 0 || chars[i - 1].is_whitespace();
            if chars[i] == '#' && at_boundary && chars.get(i + 1).is_some_and(|c| !c.is_whitespace() && *c != '#') {
                // Look for a closing '#' preceded by non-space and followed by
                // a boundary, with at least one space inside.
                if let Some(close) = (i + 2..chars.len()).find(|&j| {
                    chars[j] == '#'
                        && !chars[j - 1].is_whitespace()
                        && chars.get(j + 1).is_none_or(|c| c.is_whitespace() || ",.;:!?)".contains(*c))
                }) {
                    let inner: String = chars[i + 1..close].iter().collect();
                    if inner.contains(' ') && !inner.contains('#') {
                        out.push('#');
                        out.push_str(&inner.split_whitespace().collect::<Vec<_>>().join("-"));
                        i = close + 1;
                        continue;
                    }
                }
            }
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_textbundles() {
        let t = Tree::from_files([
            ("Groceries.textbundle/text.md", &b"# Groceries\nBuy milk #home chores# and #errands\n![](assets/photo.jpg)\n"[..]),
            ("Groceries.textbundle/assets/photo.jpg", b"jpg"),
            ("Other.textbundle/text.markdown", b"no heading here #a/b\n![](assets/photo.jpg)\n"),
            ("Other.textbundle/assets/photo.jpg", b"jpg2"),
        ]);
        let p = plan(&t);
        assert_eq!(p.notes[0].path.as_str(), "Groceries.md");
        assert_eq!(p.notes[0].content, "# Groceries\nBuy milk #home-chores and #errands\n![[photo.jpg]]\n");
        assert_eq!(p.notes[1].path.as_str(), "Other.md");
        assert!(p.notes[1].content.contains("![[photo 1.jpg]]"));
        assert_eq!(p.files.len(), 2);
    }

    #[test]
    fn tag_edge_cases() {
        assert_eq!(normalize_tags("# Heading\n#a and #b\n"), "# Heading\n#a and #b\n");
        assert_eq!(normalize_tags("x #big idea#, y"), "x #big-idea, y");
        assert_eq!(normalize_tags("```\n#not a tag#\n```\n"), "```\n#not a tag#\n```\n");
    }
}
