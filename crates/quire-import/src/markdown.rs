// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Any folder of Markdown: copy notes and files, keeping relative paths so
//! relative image links keep working.

use quire_vault::NotePath;

use crate::{ImportPlan, PlannedFile, PlannedNote, Tree};

pub fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    for (path, bytes) in tree.iter() {
        if Tree::is_markdown(path) {
            let content = tree.text(path).unwrap_or_default();
            let path =
                if path.to_ascii_lowercase().ends_with(".markdown") { format!("{}.md", &path[..path.len() - 9]) } else { path.to_string() };
            plan.notes.push(PlannedNote { path: NotePath::new(path), content });
        } else if path.to_ascii_lowercase().ends_with(".txt") {
            let content = tree.text(path).unwrap_or_default();
            plan.notes.push(PlannedNote { path: NotePath::new(format!("{}.md", &path[..path.len() - 4])), content });
        } else {
            plan.files.push(PlannedFile { path: NotePath::new(path), bytes: bytes.to_vec() });
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
