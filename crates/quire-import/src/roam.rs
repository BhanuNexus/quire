// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Roam Research: the JSON export (`Export → JSON`).
//!
//! Each page becomes a note: blocks are a nested list, `{{[[TODO]]}}` and
//! `{{[[DONE]]}}` become tasks, `((block refs))` become the quoted block and
//! a link to its page (as Logseq's import does), `#[[tags]]` and dates in
//! `[[November 10th, 2026]]` links follow Quire's names, and daily pages go
//! to `Daily/YYYY-MM-DD.md`. Attributes (`Status:: done`) stay as inline fields.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use quire_vault::NotePath;
use serde_json::Value;

use crate::convert::{iso, outside_code, page_path};
use crate::logseq::title_date;
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedNote, Stamp, Tree};

/// Nesting deeper than this is written at this depth.
const MAX_DEPTH: usize = 40;

/// Does this JSON file look like a Roam export (an array of pages with titles and blocks)?
pub(crate) fn looks_like(head: &str) -> bool {
    let h = head.trim_start();
    h.starts_with('[')
        && h.contains("\"title\"")
        && (h.contains("\"children\"") || h.contains("\"create-time\"") || h.contains("\"uid\""))
        && !h.contains("\"textContent\"")
}

fn json_files(tree: &Tree) -> Vec<&str> {
    tree.paths()
        .filter(|p| {
            p.to_ascii_lowercase().ends_with(".json")
                && tree.get(p).is_some_and(|b| looks_like(&String::from_utf8_lossy(&b[..b.len().min(4096)])))
        })
        .collect()
}

pub(crate) fn detect(tree: &Tree) -> bool {
    !json_files(tree).is_empty()
}

struct Blocks {
    /// uid → (text, page title)
    by_uid: HashMap<String, (String, String)>,
}

fn collect(children: &[Value], page: &str, out: &mut Blocks) {
    for b in children {
        if let (Some(uid), Some(s)) = (b.get("uid").and_then(Value::as_str), b.get("string").and_then(Value::as_str)) {
            out.by_uid.insert(uid.to_string(), (s.trim().to_string(), page.to_string()));
        }
        if let Some(c) = b.get("children").and_then(Value::as_array) {
            collect(c, page, out);
        }
    }
}

/// `[[Page]]` names written the way the pages are filed.
fn rewrite_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[[") {
        out.push_str(&rest[..i + 2]);
        let after = &rest[i + 2..];
        let Some(e) = after.find("]]") else {
            rest = after;
            continue;
        };
        let (target, alias) = after[..e].split_once('|').map_or((&after[..e], None), |(t, a)| (t, Some(a)));
        let name = title_date(target).or_else(|| page_path(target)).unwrap_or_else(|| target.to_string());
        out.push_str(&name);
        if let Some(a) = alias {
            out.push('|');
            out.push_str(a);
        }
        out.push_str("]]");
        rest = &after[e + 2..];
    }
    out.push_str(rest);
    out
}

/// `__italic__` → `*italic*`, `^^highlight^^` → `==highlight==`.
fn inline_marks(s: &str) -> String {
    let marked = s.replace("^^", "==");
    let mut res = String::with_capacity(marked.len());
    let mut rest = marked.as_str();
    while let Some(i) = rest.find("__") {
        let after = &rest[i + 2..];
        match after.find("__") {
            Some(e) if e > 0 && !after[..e].starts_with(' ') && !after[..e].ends_with(' ') && !after[..e].contains('\n') => {
                res.push_str(&rest[..i]);
                res.push('*');
                res.push_str(&after[..e]);
                res.push('*');
                rest = &after[e + 2..];
            }
            _ => {
                res.push_str(&rest[..i + 2]);
                rest = after;
            }
        }
    }
    res.push_str(rest);
    res
}

struct Cx<'a> {
    blocks: &'a Blocks,
    unresolved: usize,
    macros: HashSet<String>,
}

impl Cx<'_> {
    fn block_ref(&mut self, uid: &str, out: &mut String) {
        if let Some((text, page)) = self.blocks.by_uid.get(uid) {
            let name = title_date(page).or_else(|| page_path(page)).unwrap_or_else(|| page.clone());
            let _ = write!(out, "“{}” ([[{name}]])", text.replace('\n', " "));
        } else {
            self.unresolved += 1;
            let _ = write!(out, "(({uid}))");
        }
    }

    /// `((uid))` and `{{[[embed]]: …}}`, macros, tags, links, marks.
    fn text(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        loop {
            let next = [rest.find("{{"), rest.find("((")].into_iter().flatten().min();
            let Some(i) = next else { break };
            out.push_str(&rest[..i]);
            let after = &rest[i..];
            if let Some(body) = after.strip_prefix("{{") {
                let Some(e) = body.find("}}") else {
                    out.push_str("{{");
                    rest = body;
                    continue;
                };
                // The macro may hold `}}` of a nested `{{ }}`; Roam's own are flat.
                let inner = body[..e].trim();
                let (name, arg) = inner.split_once(':').map_or((inner, ""), |(n, a)| (n.trim(), a.trim()));
                let key = name.trim_start_matches("[[").trim_end_matches("]]").to_ascii_lowercase();
                match key.as_str() {
                    "embed" | "embed-path" | "embed-children" => {
                        if let Some(uid) = arg.strip_prefix("((").and_then(|a| a.strip_suffix("))")) {
                            self.block_ref(uid, &mut out);
                        } else if arg.starts_with("[[") {
                            out.push('!');
                            out.push_str(&rewrite_links(arg));
                        } else {
                            out.push_str(&after[..2 + e + 2]);
                        }
                    }
                    "video" | "youtube" | "iframe" | "pdf" | "audio" => out.push_str(arg),
                    _ => {
                        self.macros.insert(key);
                        out.push_str(&after[..2 + e + 2]);
                    }
                }
                rest = &body[e + 2..];
            } else if let Some(body) = after.strip_prefix("((") {
                match body.find("))") {
                    Some(e) if (1..=40).contains(&e) && body[..e].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) => {
                        self.block_ref(&body[..e], &mut out);
                        rest = &body[e + 2..];
                    }
                    _ => {
                        out.push_str("((");
                        rest = body;
                    }
                }
            }
        }
        out.push_str(rest);
        outside_code(&out, |part| {
            let mut s = part.to_string();
            // `#[[multi word]]` is a tag.
            while let Some(i) = s.find("#[[") {
                let Some(e) = s[i..].find("]]").map(|e| i + e) else { break };
                let tag = crate::convert::tag_name(&s[i + 3..e]);
                s.replace_range(i..e + 2, &format!("#{tag}"));
            }
            // `[text]([[Page]])` is an alias.
            let s = alias_links(&s);
            inline_marks(&rewrite_links(&s))
        })
    }
}

/// `[text]([[Page]])` → `[[Page|text]]`.
fn alias_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("]([[") {
        let found = rest[..i].rfind('[');
        let Some(open) = found else {
            let (head, tail) = rest.split_at(i + 4);
            out.push_str(head);
            rest = tail;
            continue;
        };
        let after = &rest[i + 4..];
        match after.find("]])") {
            Some(e) if !rest[open + 1..i].contains('\n') => {
                out.push_str(&rest[..open]);
                let _ = write!(out, "[[{}|{}]]", &after[..e], &rest[open + 1..i]);
                rest = &after[e + 3..];
            }
            _ => {
                out.push_str(&rest[..i + 4]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn write_blocks(children: &[Value], depth: usize, cx: &mut Cx<'_>, out: &mut String, deep: &mut bool) {
    for b in children {
        let raw = b.get("string").and_then(Value::as_str).unwrap_or_default().trim();
        let level = depth.min(MAX_DEPTH);
        *deep |= depth > MAX_DEPTH;
        let indent = "  ".repeat(level);
        let (task, raw) = if let Some(r) = raw.strip_prefix("{{[[TODO]]}}").or_else(|| raw.strip_prefix("{{TODO}}")) {
            ("[ ] ", r.trim_start())
        } else if let Some(r) = raw.strip_prefix("{{[[DONE]]}}").or_else(|| raw.strip_prefix("{{DONE}}")) {
            ("[x] ", r.trim_start())
        } else {
            ("", raw)
        };
        let heading = match b.get("heading").and_then(Value::as_u64) {
            Some(h @ 1..=3) if !raw.trim_start().starts_with('#') => format!("{} ", "#".repeat(usize::try_from(h).unwrap_or(1))),
            _ => String::new(),
        };
        let text = cx.text(raw);
        let mut lines = text.split('\n');
        let _ = write!(out, "{indent}- {task}{heading}{}", lines.next().unwrap_or_default());
        out.push('\n');
        for l in lines {
            let _ = writeln!(out, "{indent}  {l}");
        }
        if let Some(c) = b.get("children").and_then(Value::as_array) {
            write_blocks(c, depth + 1, cx, out, deep);
        }
    }
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut taken = HashSet::new();
    let mut unresolved = 0;
    let mut macros: HashSet<String> = HashSet::new();
    for file in json_files(tree) {
        let value: Value = match serde_json::from_slice(tree.get(file).unwrap_or_default()) {
            Ok(v) => v,
            Err(e) => {
                plan.warnings.push(format!("{file}: can't be read as JSON ({e})"));
                continue;
            }
        };
        let Some(pages) = value.as_array() else { continue };
        let mut blocks = Blocks { by_uid: HashMap::new() };
        for p in pages {
            if let (Some(t), Some(c)) = (p.get("title").and_then(Value::as_str), p.get("children").and_then(Value::as_array)) {
                collect(c, t, &mut blocks);
            }
        }
        for p in pages {
            let Some(title) = p.get("title").and_then(Value::as_str) else { continue };
            let secs = |k: &str| p.get(k).and_then(Value::as_i64).map(|ms| ms / 1000).filter(|s| *s > 0);
            let (created, updated) = (secs("create-time"), secs("edit-time"));
            let date = title_date(title);
            let rel = match (&date, page_path(title)) {
                (Some(d), _) => format!("Daily/{d}.md"),
                (None, Some(n)) => format!("{n}.md"),
                (None, None) => {
                    plan.warnings.push("a page with no usable title was skipped".into());
                    continue;
                }
            };
            let rel = crate::unique_name(&mut taken, &rel);
            let mut cx = Cx { blocks: &blocks, unresolved: 0, macros: HashSet::new() };
            let mut body = String::new();
            let mut deep = false;
            if let Some(c) = p.get("children").and_then(Value::as_array) {
                write_blocks(c, 0, &mut cx, &mut body, &mut deep);
            }
            if deep {
                plan.warnings.push(format!("{title}: blocks nested more than {MAX_DEPTH} deep are written at that depth"));
            }
            unresolved += cx.unresolved;
            macros.extend(cx.macros);
            let mut fields = Vec::new();
            if let Some(c) = created {
                fields.push(("created".to_string(), Field::One(iso(c))));
            }
            if let Some(u) = updated {
                fields.push(("updated".to_string(), Field::One(iso(u))));
            }
            if date.is_none() && rel.trim_end_matches(".md") != title {
                fields.push(("aliases".to_string(), Field::Many(vec![title.to_string()])));
            }
            plan.notes.push(PlannedNote {
                path: NotePath::new(rel),
                content: format!("{}{body}", front_matter(&fields)),
                times: Stamp { created, modified: updated },
            });
        }
    }
    if unresolved > 0 {
        plan.warnings.push(format!("{unresolved} block references point to blocks that weren't in the export"));
    }
    if !macros.is_empty() {
        let mut m: Vec<_> = macros.into_iter().collect();
        m.sort();
        plan.warnings.push(format!("Roam features with no Quire equivalent were kept as written: {}", m.join(", ")));
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no pages found in the Roam export".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = include_str!("../tests/fixtures/roam.json");

    fn planned() -> ImportPlan {
        plan(&Tree::from_files([("graph.json", EXPORT.as_bytes())]))
    }

    fn note<'a>(p: &'a ImportPlan, path: &str) -> &'a PlannedNote {
        p.notes
            .iter()
            .find(|n| n.path.as_str() == path)
            .unwrap_or_else(|| panic!("{path} missing: {:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>()))
    }

    #[test]
    fn is_detected() {
        assert!(detect(&Tree::from_files([("graph.json", EXPORT.as_bytes())])));
        assert!(!detect(&Tree::from_files([("x.json", &b"{\"a\":1}"[..])])));
        assert!(!detect(&Tree::from_files([("Keep/n.json", &b"[{\"title\":\"x\",\"textContent\":\"y\"}]"[..])])));
    }

    #[test]
    fn pages_become_nested_lists_with_tasks_and_dates() {
        let p = planned();
        let quire = note(&p, "Project/Quire.md");
        assert!(quire.content.starts_with("---\ncreated: 2026-11-01T10:00:00Z\nupdated: 2026-11-02T10:00:00Z\n---\n"), "{}", quire.content);
        assert!(quire.content.contains("- [ ] ship the importer\n"), "{}", quire.content);
        assert!(quire.content.contains("- [x] write tests\n"));
        assert!(quire.content.contains("  - Nested child with *italic* and ==highlight==\n"), "{}", quire.content);
        assert!(quire.content.contains("- # A heading block\n"), "{}", quire.content);
        assert!(quire.content.contains("#big-win"));
        assert!(quire.content.contains("[[2026-11-01]]"), "{}", quire.content);
        assert!(quire.content.contains("[[Rust lang|the language]]"), "{}", quire.content);
        assert!(quire.content.contains("Status:: active"));
        assert_eq!(quire.times.created, Some(1_793_527_200));
        let daily = note(&p, "Daily/2026-11-01.md");
        assert!(daily.content.contains("“Fast editor” ([[Project/Quire]])"), "{}", daily.content);
        assert!(daily.content.contains("![[Project/Quire]]"), "{}", daily.content);
    }

    #[test]
    fn code_is_untouched_and_odd_things_are_reported() {
        let p = planned();
        let quire = note(&p, "Project/Quire.md");
        assert!(quire.content.contains("`__init__` and `((uid))`"), "{}", quire.content);
        assert!(p.warnings.iter().any(|w| w.contains("block references point to blocks")), "{:?}", p.warnings);
        assert!(p.warnings.iter().any(|w| w.contains("kanban")), "{:?}", p.warnings);
    }

    #[test]
    fn hostile_titles_and_deep_nesting_are_safe() {
        let mut deep = String::from("[{\"title\":\"../../etc/evil\",\"children\":");
        let mut s = String::from("[{\"string\":\"x\"}]");
        for _ in 0..60 {
            s = format!("[{{\"string\":\"n\",\"children\":{s}}}]");
        }
        deep.push_str(&s);
        deep.push_str("},{\"title\":\"..\",\"children\":[]},{\"title\":\"CON\",\"children\":[]}]");
        let p = plan(&Tree::from_files([("g.json", deep.as_bytes())]));
        // serde_json refuses nesting past its limit instead of overflowing the stack.
        assert!(p.notes.is_empty() || p.notes.iter().all(|n| !n.path.as_str().contains("..")), "{:?}", p.notes);
        let shallow = "[{\"title\":\"../../etc/evil\",\"children\":[{\"string\":\"x\"}]},{\"title\":\"..\",\"children\":[]}]";
        let p = plan(&Tree::from_files([("g.json", shallow.as_bytes())]));
        assert_eq!(p.notes.len(), 1);
        assert_eq!(p.notes[0].path.as_str(), "etc/evil.md");
    }
}
