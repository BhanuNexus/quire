// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Day One: the JSON export (`File → Export → JSON`), a zip with
//! `Journal.json` and its `photos`, `videos`, `audios` and `pdfs` folders.
//!
//! Each entry is a note `Journal/2026-11-01 Title.md` with its date and
//! time, tags, starred flag, place and weather in front matter. Pictures
//! and other files are kept and embedded where the entry put them.

use std::collections::HashSet;
use std::fmt::Write as _;

use quire_vault::{NotePath, sanitize_title};
use serde_json::Value;

use crate::convert::{iso, parse_time, shorten, tag_name};
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Stamp, Tree, attachment, file_name};

fn journals(tree: &Tree) -> Vec<&str> {
    tree.paths()
        .filter(|p| p.to_ascii_lowercase().ends_with(".json"))
        .filter(|p| {
            tree.get(p).is_some_and(|b| {
                let head = String::from_utf8_lossy(&b[..b.len().min(8192)]);
                head.contains("\"entries\"") && head.contains("\"metadata\"")
            })
        })
        .collect()
}

pub(crate) fn detect(tree: &Tree) -> bool {
    !journals(tree).is_empty()
}

/// Day One escapes punctuation (`1\.`, `Hello\!`); take away the escapes
/// that can't change what the text means.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            let after = chars.get(i + 2).copied();
            let line_start_digits = out.rsplit('\n').next().is_some_and(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()));
            let plain = match next {
                '.' => !line_start_digits,
                '!' => after != Some('['),
                '(' | ')' => !out.ends_with(']'),
                ',' | ';' | ':' | '\'' | '"' | '=' | '@' | '%' | '&' | '$' | '~' => true,
                _ => false,
            };
            if plain {
                out.push(next);
                i += 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The file for a media item: `photos/<md5>.<type>` and friends.
fn media_file<'a>(tree: &'a Tree, folder: &str, ids: &[&str]) -> Option<(&'a str, &'a [u8])> {
    tree.iter().find(|(p, _)| {
        let stem = file_name(p).rsplit_once('.').map_or_else(|| file_name(p), |(s, _)| s);
        p.split('/').any(|seg| seg == folder) && ids.iter().any(|i| !i.is_empty() && *i == stem)
    })
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut taken_notes = HashSet::new();
    let mut taken_files = HashSet::new();
    let mut missing = 0;
    let many = journals(tree).len() > 1;
    for file in journals(tree) {
        let Ok(v) = serde_json::from_slice::<Value>(tree.get(file).unwrap_or_default()) else {
            plan.warnings.push(format!("{file}: can't be read as JSON"));
            continue;
        };
        let stem = file_name(file).trim_end_matches(".json").trim_end_matches(".JSON");
        let journal = if many || !stem.eq_ignore_ascii_case("journal") { sanitize_title(stem) } else { String::from("Journal") };
        let journal = if journal.is_empty() { "Journal".to_string() } else { journal };
        for e in v.get("entries").and_then(Value::as_array).into_iter().flatten() {
            let text = e.get("text").and_then(Value::as_str).unwrap_or_default();
            let created = e.get("creationDate").and_then(Value::as_str).and_then(parse_time);
            let modified = e.get("modificationDate").and_then(Value::as_str).and_then(parse_time);
            // Pictures and other media: `![](dayone-moment://ID)` and `dayone-moment:/video/ID`.
            let mut kept: Vec<(String, String, bool)> = Vec::new(); // (id, stored name, image)
            for (key, folder, image) in
                [("photos", "photos", true), ("videos", "videos", false), ("audios", "audios", false), ("pdfAttachments", "pdfs", false)]
            {
                for m in e.get(key).and_then(Value::as_array).into_iter().flatten() {
                    let id = m.get("identifier").and_then(Value::as_str).unwrap_or_default();
                    let md5 = m.get("md5").and_then(Value::as_str).unwrap_or_default();
                    let Some((src, bytes)) = media_file(tree, folder, &[md5, id]) else {
                        missing += 1;
                        continue;
                    };
                    let target = attachment(&mut taken_files, file_name(src));
                    kept.push((id.to_string(), file_name(&target).to_string(), image));
                    plan.files.push(PlannedFile {
                        path: NotePath::new(target),
                        bytes: bytes.to_vec(),
                        times: Stamp { created, modified: created },
                    });
                }
            }
            let mut body = unescape(text);
            let mut used: HashSet<String> = HashSet::new();
            for (id, name, image) in &kept {
                for url in [
                    format!("dayone-moment://{id}"),
                    format!("dayone-moment:/video/{id}"),
                    format!("dayone-moment:/audio/{id}"),
                    format!("dayone-moment:/pdfAttachment/{id}"),
                ] {
                    for pat in [format!("![]({url})"), format!("[]({url})")] {
                        if body.contains(&pat) {
                            body = body.replace(&pat, &format!("{}[[{name}]]", if *image { "!" } else { "" }));
                            used.insert(id.clone());
                        }
                    }
                }
            }
            // Media the text doesn't mention still belongs to the entry.
            let mut extra = String::new();
            for (id, name, image) in &kept {
                if !used.contains(id) {
                    let _ = writeln!(extra, "{}[[{name}]]", if *image { "!" } else { "" });
                }
            }
            if !extra.is_empty() {
                let _ = write!(body, "\n\n{extra}");
            }
            let first = body
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| !l.is_empty() && !l.starts_with("![[") && !l.starts_with("[["))
                .map(|l| shorten(l, 50))
                .unwrap_or_default();
            let day = created.map_or_else(|| "undated".to_string(), |c| iso(c)[..10].to_string());
            let title = sanitize_title(&first);
            let title =
                if title.is_empty() { created.map_or_else(|| "Entry".to_string(), |c| iso(c)[11..16].replace(':', ".")) } else { title };
            let rel = crate::unique_name(&mut taken_notes, &format!("{journal}/{day} {title}.md"));
            let mut fields = Vec::new();
            if let Some(c) = created {
                fields.push(("created".to_string(), Field::One(iso(c))));
            }
            if let Some(m) = modified {
                fields.push(("updated".to_string(), Field::One(iso(m))));
            }
            let tags: Vec<String> = e
                .get("tags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(tag_name)
                .filter(|t| !t.is_empty())
                .collect();
            if !tags.is_empty() {
                fields.push(("tags".to_string(), Field::Many(tags)));
            }
            if e.get("starred").and_then(Value::as_bool) == Some(true) {
                fields.push(("starred".to_string(), Field::One("true".into())));
            }
            if let Some(loc) = e.get("location") {
                let place: Vec<&str> = ["placeName", "localityName", "administrativeArea", "country"]
                    .iter()
                    .filter_map(|k| loc.get(*k).and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .collect();
                if !place.is_empty() {
                    fields.push(("location".to_string(), Field::One(place.join(", "))));
                }
                if let (Some(la), Some(lo)) = (loc.get("latitude").and_then(Value::as_f64), loc.get("longitude").and_then(Value::as_f64)) {
                    fields.push(("coordinates".to_string(), Field::One(format!("{la:.5}, {lo:.5}"))));
                }
            }
            if let Some(w) = e.get("weather") {
                let desc = w.get("conditionsDescription").and_then(Value::as_str).unwrap_or_default();
                let temp = w.get("temperatureCelsius").and_then(Value::as_f64).map(|t| format!("{t:.0}°C"));
                let line = [Some(desc.to_string()).filter(|d| !d.is_empty()), temp].into_iter().flatten().collect::<Vec<_>>().join(", ");
                if !line.is_empty() {
                    fields.push(("weather".to_string(), Field::One(line)));
                }
            }
            plan.notes.push(PlannedNote {
                path: NotePath::new(rel),
                content: format!("{}{}\n", front_matter(&fields), body.trim()),
                times: Stamp { created, modified },
            });
        }
    }
    if missing > 0 {
        plan.warnings.push(format!("{missing} pictures or files are named in entries but weren't in the export"));
    }
    if plan.notes.is_empty() {
        plan.warnings.push("no Day One entries found".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::from_files([
            ("Journal.json", &include_bytes!("../tests/fixtures/dayone/Journal.json")[..]),
            ("photos/abc123.jpeg", b"jpg"),
            ("audios/def456.m4a", b"m4a"),
        ])
    }

    #[test]
    fn entries_become_dated_notes_with_media() {
        assert!(detect(&tree()));
        let p = plan(&tree());
        let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(names, vec!["Journal/2026-11-01 A walk by the river.md", "Journal/2026-11-02 09.30.md"], "{names:?}");
        let a = &p.notes[0];
        assert!(a.content.starts_with("---\ncreated: 2026-11-01T10:00:00Z\nupdated: 2026-11-01T11:00:00Z\ntags: [walk, river-day]\nstarred: true\nlocation: Gasworks Park, Seattle, Washington, United States\n"), "{}", a.content);
        assert!(a.content.contains("coordinates: 47.64560, -122.33450"), "{}", a.content);
        assert!(a.content.contains("weather: Clear, 12°C"), "{}", a.content);
        assert!(a.content.contains("# A walk by the river"));
        assert!(a.content.contains("![[abc123.jpeg]]"), "{}", a.content);
        assert!(a.content.contains("Hello! It was 1. nice (really)"), "unescaped: {}", a.content);
        assert!(a.content.contains("1\\. Not a list"), "line-start numbers keep their escape: {}", a.content);
        let b = &p.notes[1];
        assert!(b.content.contains("[[def456.m4a]]"), "unmentioned media is still attached: {}", b.content);
        assert_eq!(p.files.len(), 2);
        assert_eq!(a.times.created, Some(1_793_527_200));
        assert!(p.warnings.iter().any(|w| w.contains("weren't in the export")), "{:?}", p.warnings);
    }
}
