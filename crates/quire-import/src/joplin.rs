// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Joplin: a JEX archive (`File → Export all → JEX`) or a raw export folder
//! (`RAW - Joplin Export Directory`).
//!
//! Notebooks become folders, tags become tags, pictures and files are kept,
//! `[text](:/id)` links become wikilinks, to-dos say so in front matter, and
//! created and updated times are kept. Notes in a conflict, deleted notes
//! and encrypted ones are left out and counted.

use std::collections::{HashMap, HashSet};

use quire_vault::{NotePath, sanitize_title};

use crate::convert::{iso, parse_time, tag_name};
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Stamp, Tree, attachment, file_name, html, rewrite_links};

/// One Joplin item: its text above the metadata, and the metadata.
struct Item {
    title: String,
    body: String,
    meta: HashMap<String, String>,
}

/// Split a `.md` file into its text and the trailing `key: value` block.
fn parse_item(text: &str) -> Option<Item> {
    let lines: Vec<&str> = text.lines().collect();
    // The metadata is the last block of `key: value` lines, after a blank line.
    let mut i = lines.len();
    while i > 0 && !lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    let (head, tail) = lines.split_at(i);
    let mut meta = HashMap::new();
    for l in tail {
        let (k, v) = l.split_once(':')?;
        if k.is_empty() || k.contains(' ') {
            return None;
        }
        meta.insert(k.to_string(), v.trim().to_string());
    }
    meta.get("type_")?;
    meta.get("id")?;
    let mut head = head.to_vec();
    while head.last().is_some_and(|l| l.trim().is_empty()) {
        head.pop();
    }
    let title = head.first().map(|t| t.trim().to_string()).unwrap_or_default();
    let body = head.get(2..).map(|b| b.join("\n")).unwrap_or_default();
    Some(Item { title, body, meta })
}

fn items(tree: &Tree) -> Vec<Item> {
    tree.iter()
        .filter(|(p, _)| Tree::is_markdown(p) && !p.contains('/'))
        .filter_map(|(p, _)| tree.text(p))
        .filter_map(|t| parse_item(&t))
        .collect()
}

pub(crate) fn detect(tree: &Tree) -> bool {
    tree.iter().filter(|(p, _)| Tree::is_markdown(p) && !p.contains('/')).take(50).any(|(p, _)| {
        tree.text(p).is_some_and(|t| {
            t.rsplit("\n\n").next().is_some_and(|tail| tail.contains("\ntype_:") || tail.starts_with("id:")) && parse_item(&t).is_some()
        })
    })
}

fn flag(item: &Item, key: &str) -> bool {
    item.meta.get(key).is_some_and(|v| !v.is_empty() && v != "0")
}

fn time(item: &Item, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .filter_map(|k| item.meta.get(*k))
        .filter(|v| !v.is_empty() && *v != "0")
        .find_map(|v| parse_time(v).or_else(|| v.parse::<i64>().ok().map(|ms| ms / 1000)))
}

fn by_type<'a>(all: &'a [Item], t: &str) -> Vec<&'a Item> {
    all.iter().filter(|i| i.meta.get("type_").is_some_and(|x| x == t)).collect()
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let all = items(tree);
    // Notebook id → path (parents first; a loop or a very deep chain stops).
    let folders: HashMap<String, &Item> = by_type(&all, "2").into_iter().filter_map(|f| Some((f.meta.get("id")?.clone(), f))).collect();
    let folder_path = |id: &str| {
        let mut id = id.to_string();
        let mut parts: Vec<String> = Vec::new();
        for _ in 0..16 {
            let Some(f) = folders.get(&id) else { break };
            let n = sanitize_title(&f.title);
            parts.push(if n.is_empty() { "Notebook".into() } else { n });
            id = f.meta.get("parent_id").cloned().unwrap_or_default();
        }
        parts.reverse();
        parts.join("/")
    };
    let tags: HashMap<&str, String> =
        by_type(&all, "5").into_iter().filter_map(|t| Some((t.meta.get("id")?.as_str(), tag_name(&t.title).to_lowercase()))).collect();
    let mut note_tags: HashMap<&str, Vec<String>> = HashMap::new();
    for nt in by_type(&all, "6") {
        if let (Some(n), Some(t)) = (nt.meta.get("note_id"), nt.meta.get("tag_id").and_then(|t| tags.get(t.as_str()))) {
            note_tags.entry(n.as_str()).or_default().push(t.clone());
        }
    }
    // Resources: id → stored file name and bytes.
    let mut taken_files = HashSet::new();
    let mut resources: HashMap<String, String> = HashMap::new();
    for r in by_type(&all, "4") {
        let Some(id) = r.meta.get("id") else { continue };
        let bytes = tree.iter().find(|(p, _)| {
            let stem = file_name(p).split('.').next().unwrap_or_default();
            p.starts_with("resources/") && stem == id
        });
        let Some((src, bytes)) = bytes else { continue };
        let name = r
            .meta
            .get("filename")
            .filter(|n| !n.is_empty())
            .cloned()
            .or_else(|| (!r.title.is_empty()).then(|| r.title.clone()))
            .unwrap_or_else(|| file_name(src).to_string());
        let target = attachment(&mut taken_files, &name);
        resources.insert(id.clone(), file_name(&target).to_string());
        plan.files.push(PlannedFile { path: NotePath::new(target), bytes: bytes.to_vec(), times: tree.stamp(src) });
    }
    // First pass: where each note will be, so links can name them.
    let mut taken_notes = HashSet::new();
    let (mut skipped_deleted, mut skipped_encrypted, mut conflicts) = (0, 0, 0);
    let mut planned: Vec<(&Item, String)> = Vec::new();
    for n in by_type(&all, "1") {
        if flag(n, "deleted_time") {
            skipped_deleted += 1;
            continue;
        }
        if flag(n, "encryption_applied") {
            skipped_encrypted += 1;
            continue;
        }
        if flag(n, "is_conflict") {
            conflicts += 1;
        }
        let name = sanitize_title(&n.title);
        let name = if name.is_empty() { "Untitled".to_string() } else { name };
        let dir = n.meta.get("parent_id").map_or_else(String::new, |p| folder_path(p));
        let rel = if dir.is_empty() { format!("{name}.md") } else { format!("{dir}/{name}.md") };
        planned.push((n, crate::unique_name(&mut taken_notes, &rel)));
    }
    let names: HashMap<&str, &str> =
        planned.iter().filter_map(|(n, p)| Some((n.meta.get("id")?.as_str(), p.trim_end_matches(".md")))).collect();
    let mut dangling = 0;
    for (n, rel) in &planned {
        let markup_html = n.meta.get("markup_language").is_some_and(|m| m == "2");
        let raw = if markup_html { html::to_markdown(&n.body, &mut |_| None) } else { n.body.clone() };
        let mut unresolved_here = 0;
        let body = rewrite_links(&raw, |l| {
            let id = l.dest.strip_prefix(":/")?.split(['#', ' ']).next()?;
            if let Some(file) = resources.get(id) {
                return Some(if l.image { format!("![[{file}]]") } else { format!("[[{file}|{}]]", l.text) });
            }
            if let Some(target) = names.get(id) {
                let stem = target.rsplit('/').next().unwrap_or(target);
                return Some(if l.text.is_empty() || l.text == stem {
                    format!("[[{target}]]")
                } else {
                    format!("[[{target}|{}]]", l.text)
                });
            }
            unresolved_here += 1;
            None
        });
        dangling += unresolved_here;
        let mut fields = Vec::new();
        let created = time(n, &["user_created_time", "created_time"]);
        let updated = time(n, &["user_updated_time", "updated_time"]);
        if let Some(c) = created {
            fields.push(("created".to_string(), Field::One(iso(c))));
        }
        if let Some(u) = updated {
            fields.push(("updated".to_string(), Field::One(iso(u))));
        }
        let mut t = n.meta.get("id").and_then(|id| note_tags.get(id.as_str())).cloned().unwrap_or_default();
        t.sort();
        t.dedup();
        if flag(n, "is_conflict") {
            t.push("joplin-conflict".into());
        }
        if !t.is_empty() {
            fields.push(("tags".to_string(), Field::Many(t)));
        }
        if let Some(src) = n.meta.get("source_url").filter(|s| !s.is_empty()) {
            fields.push(("source".to_string(), Field::One(src.clone())));
        }
        if flag(n, "is_todo") {
            fields.push(("todo".to_string(), Field::One("true".into())));
            if let Some(d) = time(n, &["todo_completed"]) {
                fields.push(("completed".to_string(), Field::One(iso(d))));
            }
            if let Some(d) = time(n, &["todo_due"]) {
                fields.push(("due".to_string(), Field::One(iso(d)[..10].to_string())));
            }
        }
        plan.notes.push(PlannedNote {
            path: NotePath::new(rel.clone()),
            content: format!("{}{}\n", front_matter(&fields), body.trim_end()),
            times: Stamp { created, modified: updated },
        });
    }
    if skipped_deleted > 0 {
        plan.warnings.push(format!("{skipped_deleted} deleted notes were left out"));
    }
    if skipped_encrypted > 0 {
        plan.warnings
            .push(format!("{skipped_encrypted} notes are encrypted by Joplin and were left out; turn encryption off and export again"));
    }
    if conflicts > 0 {
        plan.warnings.push(format!("{conflicts} notes were in conflict; they are tagged joplin-conflict"));
    }
    if dangling > 0 {
        plan.warnings.push(format!("{dangling} links point to notes or files that weren't in the export"));
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no Joplin notes found".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::from_files([
            ("11111111111111111111111111111111.md", &include_bytes!("../tests/fixtures/joplin/note1.md")[..]),
            ("22222222222222222222222222222222.md", include_bytes!("../tests/fixtures/joplin/note2.md")),
            ("33333333333333333333333333333333.md", include_bytes!("../tests/fixtures/joplin/folder.md")),
            ("44444444444444444444444444444444.md", include_bytes!("../tests/fixtures/joplin/resource.md")),
            ("55555555555555555555555555555555.md", include_bytes!("../tests/fixtures/joplin/tag.md")),
            ("66666666666666666666666666666666.md", include_bytes!("../tests/fixtures/joplin/note_tag.md")),
            ("77777777777777777777777777777777.md", include_bytes!("../tests/fixtures/joplin/deleted.md")),
            ("resources/44444444444444444444444444444444.png", b"\x89PNG"),
        ])
    }

    #[test]
    fn notebooks_tags_links_and_pictures() {
        assert!(detect(&tree()));
        let p = plan(&tree());
        let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(names, vec!["Work/Plan.md", "Work/Todo.md"], "{names:?}");
        let plan_note = &p.notes[0];
        assert!(
            plan_note.content.starts_with(
                "---\ncreated: 2026-11-01T10:00:00Z\nupdated: 2026-11-02T10:00:00Z\ntags: [rust]\nsource: https://example.com/x\n---\n"
            ),
            "{}",
            plan_note.content
        );
        assert!(plan_note.content.contains("![[diagram.png]]"), "{}", plan_note.content);
        assert!(plan_note.content.contains("[[Work/Todo|the todo]]"), "{}", plan_note.content);
        assert!(plan_note.content.contains("[[Work/Todo]]"), "{}", plan_note.content);
        assert!(plan_note.content.contains("](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)"), "an unknown id link stays");
        let todo = &p.notes[1];
        assert!(todo.content.contains("todo: true\ncompleted: 2026-11-03T10:00:00Z\n"), "{}", todo.content);
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].path.as_str(), "attachments/diagram.png");
        assert_eq!(plan_note.times.modified, Some(1_793_613_600));
        assert!(p.warnings.iter().any(|w| w.contains("deleted")), "{:?}", p.warnings);
        assert!(p.warnings.iter().any(|w| w.contains("links point")), "{:?}", p.warnings);
    }

    #[test]
    fn a_folder_loop_does_not_hang() {
        let a = "A\n\nid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nparent_id: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\ntype_: 2";
        let b = "B\n\nid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nparent_id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ntype_: 2";
        let n = "Note\n\nbody\n\nid: cccccccccccccccccccccccccccccccc\nparent_id: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ntype_: 1";
        let t = Tree::from_files([("a.md", a.as_bytes()), ("b.md", b.as_bytes()), ("c.md", n.as_bytes())]);
        let p = plan(&t);
        assert_eq!(p.notes.len(), 1);
        assert!(!p.notes[0].path.as_str().contains(".."));
    }
}
