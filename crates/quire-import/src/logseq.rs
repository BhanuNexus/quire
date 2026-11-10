// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Logseq (Markdown graph): `pages/`, `journals/`, `assets/`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use quire_vault::NotePath;

use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Stamp, Tree, attachment, file_name, percent_decode, rewrite_links};

/// One note before link rewriting.
struct Page {
    path: String,
    title: String,
    props: Vec<(String, Field)>,
    body: String,
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut pages = Vec::new();
    for path in tree.paths().filter(|p| Tree::is_markdown(p)) {
        let Some(text) = tree.text(path) else { continue };
        let target = if let Some(rest) = path.strip_prefix("journals/") {
            let stem = rest.trim_end_matches(".md").trim_end_matches(".markdown");
            match journal_date(stem) {
                Some(d) => format!("Daily/{d}.md"),
                None => format!("Daily/{stem}.md"),
            }
        } else if let Some(rest) = path.strip_prefix("pages/") {
            format!("{}.md", page_name(rest.trim_end_matches(".md").trim_end_matches(".markdown")))
        } else if path.starts_with("logseq/") || path.starts_with("draws/") || path.starts_with("whiteboards/") {
            continue;
        } else {
            path.to_string()
        };
        let (props, body) = split_page_properties(&text);
        let mut title = target.trim_end_matches(".md").trim_start_matches("Daily/").to_string();
        let mut target = target;
        let mut fields = Vec::new();
        for (k, v) in props {
            match k.as_str() {
                "title" => {
                    // Logseq's title:: wins over the file name, but keep the folder.
                    if !path.starts_with("journals/") {
                        let named = page_name(&v.replace('/', "%2F"));
                        if !named.is_empty() {
                            target = format!("{named}.md");
                        }
                    }
                    title.clone_from(&v);
                }
                "tags" | "alias" | "aliases" => {
                    let key = if k == "tags" { "tags" } else { "aliases" };
                    fields.push((key.to_string(), Field::Many(list_value(&v))));
                }
                "id" | "collapsed" | "public" | "filters" => {}
                _ => fields.push((k, Field::One(strip_brackets(&v)))),
            }
        }
        pages.push(Page { path: target, title, props: fields, body });
    }

    // Block ids → (text, page title), for ((uuid)) refs.
    let mut blocks: HashMap<String, (String, String)> = HashMap::new();
    for p in &pages {
        let mut last_text = String::new();
        for line in p.body.lines() {
            let t = line.trim_start();
            if let Some(id) = t.strip_prefix("id:: ") {
                blocks.insert(id.trim().to_string(), (last_text.clone(), p.title.clone()));
            } else if let Some(text) = t.strip_prefix("- ") {
                last_text = strip_marker(text).trim().to_string();
            } else if !t.is_empty() && !is_property_line(t) {
                last_text = t.to_string();
            }
        }
    }

    let mut taken = HashSet::new();
    let mut assets: HashMap<String, String> = HashMap::new();
    for (path, bytes) in tree.iter() {
        if let Some(rest) = path.strip_prefix("assets/") {
            let target = attachment(&mut taken, rest);
            assets.insert(rest.to_string(), file_name(&target).to_string());
            plan.files.push(PlannedFile { path: NotePath::new(target), bytes: bytes.to_vec(), times: tree.stamp(path) });
        }
    }

    let mut unresolved = 0;
    for p in pages {
        let mut body = convert_body(&p.body, &blocks, &mut unresolved);
        body = rewrite_links(&body, |l| {
            let rel = l.dest.trim_start_matches("../").trim_start_matches("./");
            let name = rel.strip_prefix("assets/")?;
            let target = assets.get(name)?;
            Some(if l.image {
                format!("![[{target}]]")
            } else if l.text.is_empty() || l.text == name {
                format!("[[{target}]]")
            } else {
                format!("[[{target}|{}]]", l.text)
            })
        });
        let fm = front_matter(&p.props);
        plan.notes.push(PlannedNote { path: NotePath::new(p.path), content: format!("{fm}{body}"), times: Stamp::default() });
    }
    if unresolved > 0 {
        plan.warnings.push(format!("{unresolved} block references point to blocks that weren't in the export"));
    }
    plan
}

/// `2026_11_01` → `2026-11-01`.
fn journal_date(stem: &str) -> Option<String> {
    let parts: Vec<&str> = stem.split(['_', '-']).collect();
    (parts.len() == 3 && parts[0].len() == 4 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())))
        .then(|| format!("{}-{:0>2}-{:0>2}", parts[0], parts[1], parts[2]))
}

/// File name → page path: `a___b` and `a%2Fb` are namespaces (`a/b`).
fn page_name(stem: &str) -> String {
    let decoded = percent_decode(&stem.replace("___", "%2F"));
    decoded.split('/').map(quire_vault::sanitize_title).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

fn is_property_line(t: &str) -> bool {
    match t.split_once(":: ") {
        Some((k, _)) => !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
        None => t.ends_with("::") && t.len() > 2 && t[..t.len() - 2].chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
    }
}

/// Leading `key:: value` lines (optionally as a first `- ` block) are page properties.
fn split_page_properties(text: &str) -> (Vec<(String, String)>, String) {
    let mut props = Vec::new();
    let mut lines = text.lines().peekable();
    let mut consumed_first_bullet = false;
    while let Some(line) = lines.peek() {
        let t = line.trim();
        let t2 = if !consumed_first_bullet && props.is_empty() { t.strip_prefix("- ").unwrap_or(t) } else { t };
        if is_property_line(t2) {
            if t2 != t {
                consumed_first_bullet = true;
            }
            let (k, v) = t2.split_once("::").expect("property");
            props.push((k.trim().to_lowercase(), v.trim().to_string()));
            lines.next();
        } else if t.is_empty() && !props.is_empty() {
            lines.next();
            break;
        } else {
            break;
        }
    }
    let body: Vec<&str> = lines.collect();
    (props, body.join("\n") + if text.ends_with('\n') { "\n" } else { "" })
}

fn strip_brackets(v: &str) -> String {
    v.replace("[[", "").replace("]]", "")
}

/// `a, [[b c]], #d` → `[a, b c, d]`.
fn list_value(v: &str) -> Vec<String> {
    v.split(',').map(|s| strip_brackets(s.trim()).trim_start_matches('#').trim().to_string()).filter(|s| !s.is_empty()).collect()
}

const MARKERS: [(&str, &str); 7] = [
    ("TODO ", "[ ] "),
    ("LATER ", "[ ] "),
    ("NOW ", "[ ] "),
    ("DOING ", "[ ] "),
    ("DONE ", "[x] "),
    ("CANCELED ", "[x] ~~"),
    ("CANCELLED ", "[x] ~~"),
];

fn strip_marker(text: &str) -> &str {
    MARKERS.iter().find_map(|(m, _)| text.strip_prefix(m)).unwrap_or(text)
}

fn convert_body(body: &str, blocks: &HashMap<String, (String, String)>, unresolved: &mut usize) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.starts_with("```") || trimmed.starts_with("- ```") {
            in_fence = !in_fence;
        }
        if in_fence && !trimmed.starts_with("- ```") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // Block properties (id::, collapsed::, …) are Logseq bookkeeping.
        if is_property_line(trimmed) {
            continue;
        }
        let mut line = line.to_string();
        if let Some(text) = trimmed.strip_prefix("- ") {
            let mut converted = None;
            for (m, rep) in MARKERS {
                if let Some(rest) = text.strip_prefix(m) {
                    let close = if rep.ends_with("~~") { "~~" } else { "" };
                    converted = Some(format!("{indent}- {rep}{rest}{close}"));
                    break;
                }
            }
            if let Some(c) = converted {
                line = c;
            } else if indent.is_empty() && text.starts_with('#') && text.trim_start_matches('#').starts_with(' ') {
                // Top-level heading blocks become real headings.
                line = text.to_string();
            }
        }
        line = inline(&line, blocks, unresolved);
        out.push_str(&line);
        out.push('\n');
    }
    if !body.ends_with('\n') {
        out.pop();
    }
    out
}

fn inline(line: &str, blocks: &HashMap<String, (String, String)>, unresolved: &mut usize) -> String {
    let mut s = line.to_string();
    // {{embed ((uuid))}} / {{embed [[page]]}}
    while let Some(i) = s.find("{{embed ") {
        let Some(end) = s[i..].find("}}").map(|e| i + e + 2) else { break };
        let inner = s[i + 8..end - 2].trim();
        let rep = if let Some(uuid) = inner.strip_prefix("((").and_then(|x| x.strip_suffix("))")) {
            if let Some((text, page)) = blocks.get(uuid) {
                format!("{text} ([[{page}]])")
            } else {
                *unresolved += 1;
                String::new()
            }
        } else if let Some(page) = inner.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
            format!("![[{page}]]")
        } else {
            inner.to_string()
        };
        // Embed with a missing nested `}}`: avoid an infinite loop.
        if end <= i + 8 {
            break;
        }
        s.replace_range(i..end, &rep);
    }
    // ((uuid)) block refs.
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("((") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find("))") {
            Some(e) if is_uuid(&after[..e]) => {
                if let Some((text, page)) = blocks.get(&after[..e]) {
                    let _ = write!(out, "“{text}” ([[{page}]])");
                } else {
                    *unresolved += 1;
                    out.push_str(&rest[i..i + 2 + e + 2]);
                }
                rest = &after[e + 2..];
            }
            _ => {
                out.push_str("((");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    // #[[multi word]] tags and journal-title date links.
    let mut s = out;
    while let Some(i) = s.find("#[[") {
        let Some(e) = s[i..].find("]]").map(|e| i + e) else { break };
        let tag = s[i + 3..e].trim().replace(' ', "-");
        s.replace_range(i..e + 2, &format!("#{tag}"));
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find("[[") {
        out.push_str(&rest[..i + 2]);
        let after = &rest[i + 2..];
        match after.find("]]") {
            Some(e) => {
                let target = &after[..e];
                out.push_str(&title_date(target).unwrap_or_else(|| target.to_string()));
                out.push_str("]]");
                rest = &after[e + 2..];
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    out
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Logseq's default journal title `Nov 1st, 2026` → `2026-11-01`.
pub(crate) fn title_date(s: &str) -> Option<String> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let (md, year) = s.split_once(", ")?;
    let (month, day) = md.split_once(' ')?;
    let m = MONTHS.iter().position(|x| month.to_ascii_lowercase().starts_with(x))? + 1;
    let day: u32 = day.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse().ok()?;
    let year: u32 = year.trim().parse().ok()?;
    (year > 999 && (1..=31).contains(&day)).then(|| format!("{year}-{m:02}-{day:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Tree {
        Tree::from_files([
            ("logseq/config.edn", &b"{}"[..]),
            ("journals/2026_11_01.md", b"- Met [[Ana]] about [[Project/Quire]]\n- TODO ship importers\n- DONE write tests\n- See ((6543a1b2-0000-4000-8000-000000000001))\n"),
            ("pages/Project___Quire.md", b"tags:: project, [[rust lang]]\nstatus:: active\n\n- # Goals\n- Fast editor\n  id:: 6543a1b2-0000-4000-8000-000000000001\n  collapsed:: true\n- Shipped on [[Nov 1st, 2026]] #[[big win]]\n- ![diagram](../assets/diagram_1.png)\n"),
            ("pages/Ana.md", b"- Friend\n- {{embed [[Project/Quire]]}}\n"),
            ("assets/diagram_1.png", b"\x89PNG"),
        ])
    }

    #[test]
    fn converts_a_graph() {
        let p = plan(&graph());
        let get = |path: &str| {
            p.notes.iter().find(|n| n.path.as_str() == path).map_or_else(
                || panic!("{path} missing: {:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>()),
                |n| n.content.clone(),
            )
        };
        let daily = get("Daily/2026-11-01.md");
        assert!(daily.contains("- [ ] ship importers"), "{daily}");
        assert!(daily.contains("- [x] write tests"));
        assert!(daily.contains("[[Project/Quire]]"));
        assert!(daily.contains("“Fast editor” ([[Project/Quire]])"), "{daily}");
        let page = get("Project/Quire.md");
        assert!(page.starts_with("---\ntags: [project, rust lang]\nstatus: active\n---\n# Goals\n- Fast editor\n"), "{page}");
        assert!(!page.contains("id::") && !page.contains("collapsed::"));
        assert!(page.contains("[[2026-11-01]] #big-win"), "{page}");
        assert!(page.contains("![[diagram_1.png]]"), "{page}");
        assert!(get("Ana.md").contains("![[Project/Quire]]"));
        assert_eq!(p.files[0].path.as_str(), "attachments/diagram_1.png");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn helpers() {
        assert_eq!(journal_date("2026_11_01").as_deref(), Some("2026-11-01"));
        assert_eq!(page_name("a%2Fb"), "a/b");
        assert_eq!(title_date("Nov 1st, 2026").as_deref(), Some("2026-11-01"));
        assert_eq!(title_date("Notes, misc"), None);
    }
}
