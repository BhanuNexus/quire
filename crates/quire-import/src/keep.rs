// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Google Keep: the `Keep` folder of a Google Takeout.
//!
//! Each note is a `.json` file (the `.html` copies next to them are ignored).
//! Text notes keep their text; lists become task lists; labels become tags;
//! pinned, archived and coloured notes say so in front matter; pictures and
//! audio are kept next to the notes they belong to; links Keep found in a
//! note are listed under it. Trashed notes are left out.

use std::collections::HashSet;
use std::fmt::Write as _;

use quire_vault::{NotePath, sanitize_title};
use serde_json::Value;

use crate::convert::{iso, shorten, tag_name};
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Stamp, Tree, attachment, file_name};

/// Does this JSON file look like a Keep note?
pub(crate) fn looks_like(head: &str) -> bool {
    head.contains("\"userEditedTimestampUsec\"")
        && (head.contains("\"isTrashed\"") || head.contains("\"textContent\"") || head.contains("\"listContent\""))
}

fn notes(tree: &Tree) -> Vec<&str> {
    tree.paths()
        .filter(|p| {
            p.to_ascii_lowercase().ends_with(".json")
                && tree.get(p).is_some_and(|b| looks_like(&String::from_utf8_lossy(&b[..b.len().min(8192)])))
        })
        .collect()
}

pub(crate) fn detect(tree: &Tree) -> bool {
    !notes(tree).is_empty()
}

fn usec(v: &Value, key: &str) -> Option<i64> {
    // Keep writes microseconds as a number; Takeout sometimes as a string.
    let n = v.get(key).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))?;
    (n > 0).then_some(n / 1_000_000)
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut taken_notes = HashSet::new();
    let mut taken_files = HashSet::new();
    let (mut trashed, mut unreadable, mut missing) = (0, 0, 0);
    for file in notes(tree) {
        let Ok(v) = serde_json::from_slice::<Value>(tree.get(file).unwrap_or_default()) else {
            unreadable += 1;
            continue;
        };
        if v.get("isTrashed").and_then(Value::as_bool) == Some(true) {
            trashed += 1;
            continue;
        }
        let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
        let text = v.get("textContent").and_then(Value::as_str).unwrap_or_default();
        let mut body = String::new();
        if let Some(items) = v.get("listContent").and_then(Value::as_array) {
            for it in items {
                let t = it.get("text").and_then(Value::as_str).unwrap_or_default().replace('\n', " ");
                if t.trim().is_empty() {
                    continue;
                }
                let done = it.get("isChecked").and_then(Value::as_bool) == Some(true);
                let _ = writeln!(body, "- [{}] {}", if done { 'x' } else { ' ' }, t.trim());
            }
        } else {
            body.push_str(text.trim_end());
            body.push('\n');
        }
        // Pictures and recordings the note has.
        let mut embeds = String::new();
        for a in v.get("attachments").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = a.get("filePath").and_then(Value::as_str) else { continue };
            let src = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
            let Some(bytes) = tree.get(&src) else {
                missing += 1;
                continue;
            };
            let target = attachment(&mut taken_files, name);
            let stored = file_name(&target).to_string();
            let image = a.get("mimetype").and_then(Value::as_str).is_some_and(|m| m.starts_with("image/"));
            let _ = writeln!(embeds, "{}[[{stored}]]", if image { "!" } else { "" });
            plan.files.push(PlannedFile { path: NotePath::new(target), bytes: bytes.to_vec(), times: tree.stamp(&src) });
        }
        if !embeds.is_empty() {
            body = format!("{embeds}\n{body}");
        }
        let links: Vec<String> = v
            .get("annotations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|a| {
                let url = a.get("url").and_then(Value::as_str)?;
                let title = a.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(url);
                Some(format!("- [{}]({url})", title.replace(['[', ']'], "")))
            })
            .collect();
        if !links.is_empty() {
            let _ = write!(body, "\n{}\n", links.join("\n"));
        }

        let title = v.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let created = usec(&v, "createdTimestampUsec");
        let edited = usec(&v, "userEditedTimestampUsec");
        let name = title
            .clone()
            .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| shorten(l, 60)))
            .or_else(|| {
                v.get("listContent")
                    .and_then(Value::as_array)
                    .and_then(|l| l.iter().find_map(|i| i.get("text").and_then(Value::as_str)))
                    .map(|t| shorten(t.trim(), 60))
            })
            .map(|t| sanitize_title(&t))
            .filter(|t| !t.is_empty())
            .or_else(|| edited.map(|e| format!("Keep note {}", &iso(e)[..10])))
            .unwrap_or_else(|| "Keep note".into());
        let mut fields = Vec::new();
        if let Some(c) = created {
            fields.push(("created".to_string(), Field::One(iso(c))));
        }
        if let Some(e) = edited {
            fields.push(("updated".to_string(), Field::One(iso(e))));
        }
        let tags: Vec<String> = v
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("name").and_then(Value::as_str))
            .map(tag_name)
            .filter(|t| !t.is_empty())
            .collect();
        if !tags.is_empty() {
            fields.push(("tags".to_string(), Field::Many(tags)));
        }
        for (k, key) in [("pinned", "isPinned"), ("archived", "isArchived")] {
            if v.get(key).and_then(Value::as_bool) == Some(true) {
                fields.push((k.to_string(), Field::One("true".into())));
            }
        }
        if let Some(c) = v.get("color").and_then(Value::as_str).filter(|c| !c.is_empty() && *c != "DEFAULT") {
            fields.push(("color".to_string(), Field::One(c.to_ascii_lowercase())));
        }
        // A note named from its own first line keeps the full title in front matter when it was cut.
        let heading = if title.is_some() { format!("# {}\n\n", title.as_deref().unwrap_or_default()) } else { String::new() };
        let rel = crate::unique_name(&mut taken_notes, &format!("{name}.md"));
        plan.notes.push(PlannedNote {
            path: NotePath::new(rel),
            content: format!("{}{heading}{body}", front_matter(&fields)),
            times: Stamp { created, modified: edited },
        });
    }
    if trashed > 0 {
        plan.warnings.push(format!("{trashed} notes in Keep's trash were left out"));
    }
    if unreadable > 0 {
        plan.warnings.push(format!("{unreadable} files looked like notes but couldn't be read"));
    }
    if missing > 0 {
        plan.warnings.push(format!("{missing} attachments are named in notes but weren't in the export"));
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no Keep notes found; pick the Keep folder of a Takeout".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::from_files([
            ("Keep/Groceries.json", &include_bytes!("../tests/fixtures/keep/Groceries.json")[..]),
            ("Keep/Idea.json", include_bytes!("../tests/fixtures/keep/Idea.json")),
            ("Keep/Old.json", include_bytes!("../tests/fixtures/keep/Old.json")),
            ("Keep/Idea.html", b"<html>copy</html>"),
            ("Keep/photo.jpg", b"jpg"),
            ("Keep/Labels.txt", b"Home\nIdeas\n"),
        ])
    }

    #[test]
    fn lists_text_labels_and_attachments() {
        let p = plan(&tree());
        let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(names, vec!["Groceries.md", "An idea about importers.md"], "trashed note and html copies are left out");
        let g = &p.notes[0];
        assert!(g.content.starts_with("---\ncreated: 2026-11-01T10:00:00Z\nupdated: 2026-11-02T10:00:00Z\ntags: [Home]\npinned: true\ncolor: blue\n---\n# Groceries\n\n"), "{}", g.content);
        assert!(g.content.contains("- [ ] milk\n- [x] bread\n"), "{}", g.content);
        assert_eq!(g.times.modified, Some(1_793_613_600));
        let i = &p.notes[1];
        assert!(i.content.contains("![[photo.jpg]]\n"), "{}", i.content);
        assert!(i.content.contains("An idea about importers"));
        assert!(i.content.contains("- [Rust book](https://doc.rust-lang.org/book/)"), "{}", i.content);
        assert!(i.content.contains("archived: true"));
        assert_eq!(i.path.as_str(), "An idea about importers.md", "a note without a title is named from its first line");
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].path.as_str(), "attachments/photo.jpg");
        assert!(p.warnings.iter().any(|w| w.contains("trash")), "{:?}", p.warnings);
    }

    #[test]
    fn detection_ignores_other_json() {
        assert!(detect(&tree()));
        assert!(!detect(&Tree::from_files([("a.json", &b"{\"x\":1}"[..])])));
    }
}
