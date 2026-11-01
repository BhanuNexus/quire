// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Notion: "Markdown & CSV" export (zip or folder).
//!
//! Every page and folder carries a 32-hex id: `Roadmap 0123…cdef.md`.
//! Ids are stripped, links become `[[Title]]`, images become attachments
//! and database CSVs become front matter (one note per row) plus a
//! `quire-view` table over the rows.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use quire_vault::NotePath;

use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Tree, attachment, file_name, rewrite_links};

/// Does any path segment end with a Notion id?
pub(crate) fn has_id(path: &str) -> bool {
    path.split('/').any(|seg| split_id(seg).1.is_some())
}

/// `Roadmap 0123…cdef.md` → (`Roadmap.md`, Some(id)).
fn split_id(seg: &str) -> (String, Option<&str>) {
    let (stem, ext) = match seg.rsplit_once('.') {
        Some((s, e)) if e.len() <= 8 && !e.contains(' ') => (s, Some(e)),
        _ => (seg, None),
    };
    if stem.len() > 33 && stem.is_char_boundary(stem.len() - 33) {
        let (name, id) = stem.split_at(stem.len() - 33);
        let id = &id[1..];
        if stem.as_bytes()[stem.len() - 33] == b' ' && id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            let clean = match ext {
                Some(e) => format!("{}.{e}", name.trim()),
                None => name.trim().to_string(),
            };
            return (clean, Some(id));
        }
    }
    (seg.to_string(), None)
}

fn clean_path(path: &str) -> String {
    path.split('/').map(|s| clean_segment(&split_id(s).0)).collect::<Vec<_>>().join("/")
}

/// Sanitize a file or folder name, keeping its extension.
fn clean_segment(seg: &str) -> String {
    match seg.rsplit_once('.') {
        Some((stem, ext)) if ext.len() <= 8 && !ext.contains(' ') => format!("{}.{ext}", quire_vault::sanitize_title(stem)),
        _ => quire_vault::sanitize_title(seg),
    }
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    // id → clean title, for links.
    let mut titles: HashMap<String, String> = HashMap::new();
    for p in tree.paths().filter(|p| Tree::is_markdown(p)) {
        let seg = file_name(p);
        if let (clean, Some(id)) = split_id(seg) {
            titles.insert(id.to_string(), NotePath::new(clean).stem().to_string());
        }
    }
    let mut taken_files = HashSet::new();
    let mut files: HashMap<String, String> = HashMap::new();
    for (p, bytes) in tree.iter() {
        let l = p.to_ascii_lowercase();
        if Tree::is_markdown(p) || std::path::Path::new(&l).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("csv")) {
            continue;
        }
        let target = attachment(&mut taken_files, &split_id(file_name(p)).0);
        files.insert(p.to_string(), file_name(&target).to_string());
        plan.files.push(PlannedFile { path: NotePath::new(target), bytes: bytes.to_vec() });
    }

    let mut notes: Vec<(String, String)> = Vec::new();
    for p in tree.paths().filter(|p| Tree::is_markdown(p)) {
        let text = tree.text(p).unwrap_or_default();
        let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
        let body = rewrite_links(&text, |l| {
            if l.dest.contains("://") || l.dest.starts_with("mailto:") {
                return None;
            }
            let full = normalize(&if dir.is_empty() { l.dest.clone() } else { format!("{dir}/{}", l.dest) });
            if let Some(name) = files.get(&full) {
                return Some(if l.image { format!("![[{name}]]") } else { format!("[[{name}|{}]]", l.text) });
            }
            let seg = file_name(&l.dest);
            if let (_, Some(id)) = split_id(seg) {
                let title = titles.get(id)?;
                return Some(if l.text == title || l.text.is_empty() { format!("[[{title}]]") } else { format!("[[{title}|{}]]", l.text) });
            }
            None
        });
        notes.push((clean_path(p), body));
    }

    // Databases: `DB <id>.csv` with row pages in the `DB <id>/` folder.
    let csvs: Vec<&str> = tree.paths().filter(|p| p.to_ascii_lowercase().ends_with(".csv") && !p.ends_with("_all.csv")).collect();
    for csv_path in csvs {
        let Some(text) = tree.text(csv_path) else { continue };
        let rows = parse_csv(&text);
        let Some((header, rows)) = rows.split_first() else { continue };
        if header.is_empty() {
            continue;
        }
        let db_path = clean_path(csv_path);
        let db_folder = db_path.trim_end_matches(".csv").trim_end_matches(".CSV").to_string();
        let db_name = file_name(&db_folder).to_string();
        for row in rows {
            let title = row.first().map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("Untitled");
            let fields: Vec<(String, Field)> = header
                .iter()
                .zip(row.iter())
                .skip(1)
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if v.contains(", ") && !v.contains(": ") {
                            Field::Many(v.split(", ").map(str::to_string).collect())
                        } else {
                            Field::One(v.clone())
                        },
                    )
                })
                .collect();
            let fm = front_matter(&fields);
            let row_path = format!("{db_folder}/{}.md", quire_vault::sanitize_title(title));
            if let Some(n) = notes.iter_mut().find(|(p, _)| p.eq_ignore_ascii_case(&row_path)) {
                n.1 = format!("{fm}{}", strip_property_lines(&n.1, header));
            } else {
                notes.push((row_path, format!("{fm}# {title}\n")));
            }
        }
        let fields: Vec<String> = std::iter::once("title".to_string())
            .chain(header.iter().skip(1).take(4).map(|h| h.trim().to_lowercase().replace(' ', "_")))
            .collect();
        let index = format!("{db_folder}.md");
        // Relative, so the view still works when the import goes into a folder.
        let view = format!("```quire-view\nfrom: \"./{db_name}/\"\nlayout: table\nfields: [{}]\n```\n", fields.join(", "));
        if let Some(n) = notes.iter_mut().find(|(p, _)| p.eq_ignore_ascii_case(&index)) {
            let _ = write!(n.1, "\n{view}");
        } else {
            notes.push((index, format!("# {db_name}\n\n{view}")));
        }
    }

    for (p, content) in notes {
        plan.notes.push(PlannedNote { path: NotePath::new(p), content });
    }
    plan
}

/// Resolve `.` and `..` in a relative path.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Notion repeats database columns as `Key: value` lines under the title.
fn strip_property_lines(body: &str, header: &[String]) -> String {
    let mut out = Vec::new();
    let mut lines = body.lines().peekable();
    // Title line and blank lines are kept; then skip the property run.
    while let Some(l) = lines.peek() {
        if l.starts_with("# ") || l.trim().is_empty() {
            out.push(*l);
            lines.next();
        } else {
            break;
        }
    }
    let mut skipped = false;
    while let Some(l) = lines.peek() {
        let is_prop = l.split_once(": ").is_some_and(|(k, _)| header.iter().any(|h| h == k.trim()));
        if is_prop || (skipped && l.trim().is_empty()) {
            skipped = true;
            lines.next();
        } else {
            break;
        }
    }
    out.extend(lines);
    let mut s = out.join("\n");
    if body.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Minimal RFC 4180 parser: quoted fields, doubled quotes, embedded newlines.
#[allow(clippy::match_same_arms, reason = "arm order matters: quoted and unquoted chars are pushed after the special cases")]
pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";
    const C: &str = "11112222333344445555666677778888";
    const D: &str = "99998888777766665555444433332222";

    #[test]
    fn ids_are_detected_and_stripped() {
        assert_eq!(split_id(&format!("Roadmap {A}.md")), ("Roadmap.md".to_string(), Some(A)));
        assert_eq!(split_id(&format!("Roadmap {A}")), ("Roadmap".to_string(), Some(A)));
        assert_eq!(split_id("Roadmap.md").1, None);
        assert!(has_id(&format!("Work {A}/x.md")));
    }

    #[test]
    fn converts_pages_links_images_and_databases() {
        let home =
            format!("# Home\n\nSee [Roadmap](Home%20{A}/Roadmap%20{B}.md) and ![](Home%20{A}/chart.png)\n[web](https://notion.so)\n");
        let roadmap = format!("# Roadmap\n\nBack to [Home](../Home%20{A}.md)\n");
        let csv = "Name,Status,Tags\nShip v3,Done,\"rust, ui\"\n\"Plan \"\"v4\"\"\",Todo,\n";
        let row = "# Ship v3\n\nStatus: Done\nTags: rust, ui\n\nNotes here.\n";
        let t = Tree::from_files([
            (format!("Home {A}.md").as_str(), home.as_bytes()),
            (format!("Home {A}/Roadmap {B}.md").as_str(), roadmap.as_bytes()),
            (format!("Home {A}/chart.png").as_str(), &b"png"[..]),
            (format!("Tasks {C}.csv").as_str(), csv.as_bytes()),
            (format!("Tasks {C}/Ship v3 {D}.md").as_str(), row.as_bytes()),
        ]);
        let p = plan(&t);
        let get = |path: &str| {
            p.notes
                .iter()
                .find(|n| n.path.as_str() == path)
                .map_or_else(|| panic!("{path}: {:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>()), |n| n.content.clone())
        };
        assert_eq!(get("Home.md"), "# Home\n\nSee [[Roadmap]] and ![[chart.png]]\n[web](https://notion.so)\n");
        assert_eq!(get("Home/Roadmap.md"), "# Roadmap\n\nBack to [[Home]]\n");
        let ship = get("Tasks/Ship v3.md");
        assert_eq!(ship, "---\nstatus: Done\ntags: [rust, ui]\n---\n# Ship v3\n\nNotes here.\n");
        assert!(get("Tasks/Plan v4.md").starts_with("---\nstatus: Todo\n---\n# Plan \"v4\""));
        assert!(get("Tasks.md").contains("```quire-view\nfrom: \"./Tasks/\"\nlayout: table\nfields: [title, status, tags]\n```"));
    }

    #[test]
    fn csv_parser() {
        assert_eq!(parse_csv("a,\"b,\nc\",\"d\"\"\"\r\n1,2,3"), vec![vec!["a", "b,\nc", "d\""], vec!["1", "2", "3"]]);
    }
}
