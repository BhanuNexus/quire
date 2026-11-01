// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Evernote `.enex` exports (one or many notes per file).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use base64::Engine;
use quire_vault::{NotePath, sanitize_title};

use crate::html::{to_markdown, unescape};
use crate::yaml::{Field, front_matter};
use crate::{ImportPlan, PlannedFile, PlannedNote, Tree, attachment, file_name};

struct Resource {
    bytes: Vec<u8>,
    mime: String,
    name: Option<String>,
}

pub(crate) fn plan(tree: &Tree) -> ImportPlan {
    let mut plan = ImportPlan::default();
    let mut taken_files = HashSet::new();
    let mut taken_notes = HashSet::new();
    for (path, _) in tree.iter().filter(|(p, _)| p.to_ascii_lowercase().ends_with(".enex")) {
        let xml = tree.text(path).unwrap_or_default();
        // Multi-notebook exports: put each .enex in a folder named after it,
        // unless there's only one.
        let many = tree.paths().filter(|p| p.to_ascii_lowercase().ends_with(".enex")).count() > 1;
        let folder = if many { sanitize_title(file_name(path).trim_end_matches(".enex")) } else { String::new() };
        let mut count = 0;
        for note in elements(&xml, "note") {
            count += 1;
            let title = first(note, "title").map(|t| unescape(t.trim())).filter(|t| !t.is_empty()).unwrap_or_else(|| "Untitled".into());
            let tags: Vec<String> = elements(note, "tag").map(|t| unescape(t.trim())).filter(|t| !t.is_empty()).collect();
            let created = first(note, "created").and_then(|c| enex_time(c.trim()));
            let updated = first(note, "updated").and_then(|c| enex_time(c.trim()));
            let source = first(note, "source-url").map(|s| unescape(s.trim()));

            let mut by_hash: HashMap<String, String> = HashMap::new();
            for res in elements(note, "resource") {
                let Some(r) = resource(res) else {
                    plan.warnings.push(format!("{title}: an attachment couldn't be decoded"));
                    continue;
                };
                let hash = format!("{:x}", md5::compute(&r.bytes));
                let name = r.name.clone().unwrap_or_else(|| format!("{}.{}", &hash[..8], ext_for(&r.mime)));
                let target = attachment(&mut taken_files, &name);
                let embed_name = file_name(&target).to_string();
                let embed = if r.mime.starts_with("image/") { format!("![[{embed_name}]]") } else { format!("[[{embed_name}]]") };
                by_hash.insert(hash, embed);
                plan.files.push(PlannedFile { path: NotePath::new(target), bytes: r.bytes });
            }

            let content = first(note, "content").map(cdata).unwrap_or_default();
            let mut used = HashSet::new();
            let mut media = |h: &str| {
                let e = by_hash.get(h).cloned();
                if e.is_some() {
                    used.insert(h.to_string());
                }
                e
            };
            let mut body = to_markdown(&content, &mut media);
            // Attachments not referenced in the body still belong to the note.
            for (h, e) in &by_hash {
                if !used.contains(h) {
                    let _ = write!(body, "\n{e}\n");
                }
            }

            let mut fields = Vec::new();
            if let Some(c) = created {
                fields.push(("created".to_string(), Field::One(c)));
            }
            if let Some(u) = updated {
                fields.push(("updated".to_string(), Field::One(u)));
            }
            if !tags.is_empty() {
                fields.push(("tags".to_string(), Field::Many(tags.iter().map(|t| t.replace(' ', "-")).collect())));
            }
            if let Some(s) = source.filter(|s| !s.is_empty()) {
                fields.push(("source".to_string(), Field::One(s)));
            }
            let name = sanitize_title(&title);
            let name = if name.is_empty() { "Untitled".into() } else { name };
            let rel = if folder.is_empty() { format!("{name}.md") } else { format!("{folder}/{name}.md") };
            let rel = crate::unique_name(&mut taken_notes, &rel);
            plan.notes.push(PlannedNote { path: NotePath::new(rel), content: format!("{}# {title}\n\n{body}", front_matter(&fields)) });
        }
        if count == 0 {
            plan.warnings.push(format!("{path}: no notes found"));
        }
    }
    plan
}

/// Inner text of every `<name …>…</name>` (non-nested).
fn elements<'a>(xml: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut rest = xml;
    std::iter::from_fn(move || {
        loop {
            let i = rest.find(&open)?;
            let after = &rest[i + open.len()..];
            // `<note>` must not match `<note-attributes>`.
            if !after.starts_with(['>', ' ', '\t', '\n', '\r', '/']) {
                rest = after;
                continue;
            }
            let gt = after.find('>')?;
            if after[..gt].ends_with('/') {
                rest = &after[gt + 1..];
                return Some("");
            }
            let body = &after[gt + 1..];
            let end = find_close(body, &close)?;
            rest = &body[end + close.len()..];
            return Some(&body[..end]);
        }
    })
}

/// Find a closing tag, skipping CDATA sections (ENML lives in one).
fn find_close(body: &str, close: &str) -> Option<usize> {
    let mut offset = 0;
    loop {
        let c = body[offset..].find(close).map(|i| i + offset)?;
        match body[offset..].find("<![CDATA[").map(|i| i + offset) {
            Some(cd) if cd < c => {
                let end = body[cd..].find("]]>")? + cd + 3;
                offset = end;
            }
            _ => return Some(c),
        }
    }
}

fn first<'a>(xml: &'a str, name: &'a str) -> Option<&'a str> {
    elements(xml, name).next()
}

fn cdata(s: &str) -> String {
    let t = s.trim();
    match t.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")) {
        Some(inner) => inner.to_string(),
        None => unescape(t),
    }
}

fn resource(xml: &str) -> Option<Resource> {
    // Drop recognition data (OCR XML) so it can't be mistaken for content.
    let data = first(xml, "data")?;
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(cleaned).ok()?;
    let mime = first(xml, "mime").map(|m| m.trim().to_string()).unwrap_or_default();
    let name = first(xml, "file-name").map(|n| unescape(n.trim())).filter(|n| !n.is_empty());
    Some(Resource { bytes, mime, name })
}

fn ext_for(mime: &str) -> &str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        _ => "bin",
    }
}

/// `20261101T120000Z` → `2026-11-01T12:00:00Z`.
fn enex_time(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if s.len() < 15 || b[8] != b'T' || !s[..8].bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}T{}:{}:{}Z", &s[0..4], &s[4..6], &s[6..8], &s[9..11], &s[11..13], &s[13..15]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_notes_with_resources() {
        let png = b"\x89PNG fake";
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let hash = format!("{:x}", md5::compute(png));
        let enex = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export>
<note><title>Trip &amp; plans</title><created>20261101T120000Z</created><tag>travel</tag><tag>to do</tag>
<note-attributes><source-url>https://example.com</source-url></note-attributes>
<content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note SYSTEM "x"><en-note><div>Pack <b>light</b></div><div><en-todo checked="true"/>passport</div><en-media hash="{hash}" type="image/png"/></en-note>]]></content>
<resource><data encoding="base64">
{b64}
</data><mime>image/png</mime><resource-attributes><file-name>map.png</file-name></resource-attributes></resource>
</note>
<note><title>Second</title><content><![CDATA[<en-note>Hi</en-note>]]></content></note>
</en-export>"#
        );
        let t = Tree::from_files([("export.enex", enex.as_bytes())]);
        let p = plan(&t);
        assert_eq!(p.notes.len(), 2, "{:?}", p.warnings);
        let n = &p.notes[0];
        assert_eq!(n.path.as_str(), "Trip & plans.md");
        assert_eq!(
            n.content,
            "---\ncreated: 2026-11-01T12:00:00Z\ntags: [travel, to-do]\nsource: https://example.com\n---\n# Trip & plans\n\nPack **light**\n- [x] passport\n![[map.png]]\n"
        );
        assert_eq!(p.files[0].path.as_str(), "attachments/map.png");
        assert_eq!(p.files[0].bytes, png);
        assert_eq!(p.notes[1].content, "# Second\n\nHi\n");
    }

    #[test]
    fn element_scan_skips_prefixed_names() {
        let xml = "<note-attributes>x</note-attributes><note>body</note>";
        assert_eq!(elements(xml, "note").collect::<Vec<_>>(), vec!["body"]);
    }
}
