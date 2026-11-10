// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Link-preserving refactors.

use quire_markdown::fields::{FmContext, front_matter_context};
use quire_markdown::frontmatter;
use quire_markdown::render::encode_url;
use quire_markdown::{LinkKind, extract};

/// Describe a rename: links whose target matches `old_stem` (or `old_path`
/// without `.md`) are pointed at `new_stem`.
#[derive(Clone)]
pub struct LinkRewrite<'a> {
    pub old_stem: &'a str,
    pub old_path: &'a str,
    pub new_stem: &'a str,
    /// Optional precise check that a matching target really resolves to the
    /// renamed note (two notes can share a name in different folders).
    pub resolves: Option<&'a dyn Fn(&str) -> bool>,
    /// Set for a move to another folder: only path-style links
    /// (`[[Folder/Note]]`) change, to this path; bare `[[Note]]` links still
    /// resolve by name and are left alone.
    pub new_path: Option<&'a str>,
}

impl LinkRewrite<'_> {
    fn matches(&self, target: &str) -> bool {
        if self.new_path.is_some() {
            return target.contains('/') && target.eq_ignore_ascii_case(self.old_path) && self.resolves.is_none_or(|f| f(target));
        }
        let by_name = target.eq_ignore_ascii_case(self.old_stem) || target.eq_ignore_ascii_case(self.old_path);
        by_name && self.resolves.is_none_or(|f| f(target))
    }

    /// Keep any folder prefix the author wrote; swap only the final component.
    fn new_target(&self, target: &str) -> String {
        if let Some(p) = self.new_path {
            return p.to_string();
        }
        match target.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{}", self.new_stem),
            None => self.new_stem.to_string(),
        }
    }
}

/// Rewrite links in `src`. Returns `None` when nothing changed.
///
/// 0.12: links in front-matter properties (`up: "[[Old]]"`) are rewritten
/// too; only the target inside `[[…]]` changes, so every other byte of the
/// note stays as it was. The new name is written the way YAML needs it
/// where the link sits: a `'` is doubled in single quotes; `\` and `"` are
/// escaped in double quotes. In an unquoted value a name that YAML would
/// read differently (`Smith, John`, `@Work`, `Ada's`) gets the whole value
/// quoted. If the front matter was valid YAML and would not be after the
/// change, its links are left as they were.
pub fn rewrite_links(src: &str, rw: &LinkRewrite<'_>) -> Option<String> {
    let ex = extract(src);
    let fm_end = ex.front_matter.as_ref().map_or(0, |f| f.range.end);
    let (body, front) = edits_for(src, &ex.links, fm_end, rw);
    let mut out = apply(src, body.iter().chain(&front))?;
    if !front.is_empty() && frontmatter::yaml_valid(src) == Some(true) && frontmatter::yaml_valid(&out) != Some(true) {
        out = apply(src, body.iter())?;
    }
    (out != src).then_some(out)
}

type Edits = Vec<(std::ops::Range<usize>, String)>;

fn apply<'a>(src: &str, edits: impl Iterator<Item = &'a (std::ops::Range<usize>, String)>) -> Option<String> {
    let mut edits: Vec<_> = edits.collect();
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(src.len() + edits.len() * 8);
    let mut pos = 0;
    for (r, s) in edits {
        out.push_str(&src[pos..r.start]);
        out.push_str(s);
        pos = r.end;
    }
    out.push_str(&src[pos..]);
    Some(out)
}

/// A name YAML could read differently in an unquoted value.
fn needs_quotes(name: &str) -> bool {
    name.contains([',', ':', '#', '\'', '"', '@', '`', '!', '&', '*', '%', '{', '}', '[', ']', '|', '>', '\\'])
        || name.starts_with(['-', '?', '~', '<', '='])
        || name.contains(" -")
        || name != name.trim()
        || name.chars().any(char::is_control)
}

fn dq(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            c if c.is_control() => {
                let _ = std::fmt::Write::write_fmt(&mut q, format_args!("\\u{:04X}", u32::from(c)));
            }
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// Links in one unquoted value: its byte range, to (link range, new text, name needs quotes).
type PlainGroups = std::collections::BTreeMap<(usize, usize), Vec<(std::ops::Range<usize>, String, bool)>>;

/// The edits for the body and for the front matter, apart.
fn edits_for(src: &str, links: &[quire_markdown::Link], fm_end: usize, rw: &LinkRewrite<'_>) -> (Edits, Edits) {
    let (mut body, mut front): (Edits, Edits) = (Vec::new(), Vec::new());
    // Unquoted values hold links to quote together: scalar range -> (link range, new text).
    let mut plain: PlainGroups = std::collections::BTreeMap::new();
    for link in links {
        if !link.is_internal() || !rw.matches(&link.target) {
            continue;
        }
        let in_front = link.range.end <= fm_end;
        let raw = &src[link.range.clone()];
        let mut new_target = rw.new_target(&link.target);
        let ctx = in_front.then(|| front_matter_context(src, fm_end, link.range.start));
        match &ctx {
            Some(FmContext::Single) => new_target = new_target.replace('\'', "''"),
            Some(FmContext::Double) => new_target = new_target.replace('\\', "\\\\").replace('"', "\\\""),
            // A later line of a multi-line value: only a name that is safe anywhere.
            Some(FmContext::Unknown) if needs_quotes(&new_target) => continue,
            _ => {}
        }
        let text = match link.kind {
            LinkKind::Wiki | LinkKind::Embed => {
                let open = if link.kind == LinkKind::Embed { 3 } else { 2 };
                let inner = &raw[open..raw.len() - 2];
                // Replace only the target part, keeping `#heading` and `|alias`.
                let cut = inner.find(['#', '|']).unwrap_or(inner.len());
                format!("{}{new_target}{}]]", &raw[..open], &inner[cut..])
            }
            LinkKind::Relative => {
                let Some(paren) = raw.rfind("](") else {
                    continue;
                };
                let dest = &raw[paren + 2..raw.len() - 1];
                let (dest_core, title) = match dest.find(" \"") {
                    Some(i) => (&dest[..i], &dest[i..]),
                    None => (dest, ""),
                };
                let anchor = dest_core.find('#').map_or("", |i| &dest_core[i..]);
                let ext = if dest_core.contains(".md") { ".md" } else { "" };
                format!("{}]({}{ext}{anchor}{title})", &raw[..paren], encode_url(&new_target))
            }
            LinkKind::External => continue,
        };
        match ctx {
            Some(FmContext::Flow) if needs_quotes(&new_target) => front.push((link.range.clone(), dq(&text))),
            Some(FmContext::Plain(r)) => {
                plain.entry((r.start, r.end)).or_default().push((link.range.clone(), text, needs_quotes(&new_target)));
            }
            Some(_) => front.push((link.range.clone(), text)),
            None => body.push((link.range.clone(), text)),
        }
    }
    for ((start, end), items) in plain {
        if !items.iter().any(|(_, _, q)| *q) {
            front.extend(items.into_iter().map(|(r, t, _)| (r, t)));
            continue;
        }
        // The whole value, with the new names in it, as one double-quoted scalar.
        let mut text = String::new();
        let mut pos = start;
        for (r, t, _) in &items {
            text.push_str(&src[pos..r.start]);
            text.push_str(t);
            pos = r.end;
        }
        text.push_str(&src[pos..end]);
        front.push((start..end, dq(&text)));
    }
    (body, front)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_all_link_forms() {
        let src = "[[Old Name]] [[old name#Part|alias]] ![[Folder/Old Name]] [md](Folder/Old%20Name.md#x) [[Other]] `[[Old Name]]`";
        let rw = LinkRewrite { old_stem: "Old Name", old_path: "Folder/Old Name", new_stem: "New Name", resolves: None, new_path: None };
        let out = rewrite_links(src, &rw).unwrap();
        assert_eq!(out, "[[New Name]] [[New Name#Part|alias]] ![[Folder/New Name]] [md](Folder/New%20Name.md#x) [[Other]] `[[Old Name]]`");
        assert!(rewrite_links("[[Other]]", &rw).is_none());
    }

    #[test]
    fn rewrites_typed_links_keeping_every_other_byte() {
        let src = "---\r\nup: \"[[Old Name]]\"   # parent\r\nrelated: ['[[Old Name|o]]', \"[[Keep]]\"]\r\ndown:\r\n  - \"[[Folder/Old Name#H]]\"\r\n---\r\nparent:: [[Old Name]]\r\n[supports:: [[old name]]]\r\n";
        let rw = LinkRewrite { old_stem: "Old Name", old_path: "Folder/Old Name", new_stem: "Ada's", resolves: None, new_path: None };
        let out = rewrite_links(src, &rw).unwrap();
        assert_eq!(
            out,
            "---\r\nup: \"[[Ada's]]\"   # parent\r\nrelated: ['[[Ada''s|o]]', \"[[Keep]]\"]\r\ndown:\r\n  - \"[[Folder/Ada's#H]]\"\r\n---\r\nparent:: [[Ada's]]\r\n[supports:: [[Ada's]]]\r\n"
        );
        let fm = frontmatter::parse(&out).unwrap();
        assert_eq!(fm.get("related").unwrap().as_list()[0], "[[Ada's|o]]");
    }

    /// Rename over YAML link values with hostile names: the front matter
    /// always still parses, and only the link target changes.
    #[test]
    fn hostile_new_names_never_break_the_yaml() {
        let names = [
            "Smith, John",
            "@Work",
            "`tick",
            "!bang",
            "Bob's",
            "a: b",
            "say \"hi\"",
            "back\\slash",
            "{curly}",
            "%pct",
            "&amp",
            "*star",
            "- dash",
            "?ask",
            ">fold",
            "x y z",
            "'quoted'",
            "tab\there",
            "Ada's \"best\"",
            "plain",
        ];
        let values = [
            "up: [[Old]]",
            "up: x [[Old]]",
            "up: Bob's [[Old]]",
            "up: \"[[Old]]\"",
            "up: '[[Old]]'",
            "up: [\"[[Old]]\", b]",
            "up: ['[[Old]]', \"[[Keep]]\"]",
            "up: [[[Old]], [[Keep]]]",
            "up:\n  - [[Old]]",
            "up:\n  - \"[[Old]]\"\n  - '[[Old|alias]]'",
            "up: \"[[Old#H]]\"",
            "up: see [[Old]] and [[Old]]",
            "up: |\n  text [[Old]]\n  more",
            "up: [\n  \"[[Old]]\",  # why\n  b\n]",
        ];
        for eol in ["\n", "\r\n"] {
            for v in values {
                for comment in ["", "   # trailing"] {
                    let value = if v.contains('\n') && !comment.is_empty() { v.to_string() } else { format!("{v}{comment}") };
                    let src = format!("---\ntitle: T\n{value}\nz: 1\n---\nbody [[Old]]\n").replace('\n', eol);
                    assert_eq!(frontmatter::yaml_valid(&src), Some(true), "bad template {src:?}");
                    for name in names {
                        let rw = LinkRewrite { old_stem: "Old", old_path: "Old", new_stem: name, resolves: None, new_path: None };
                        let out = rewrite_links(&src, &rw).unwrap();
                        assert_eq!(frontmatter::yaml_valid(&out), Some(true), "YAML broke: {name:?}\nin  {src:?}\nout {out:?}");
                        // The same links, in the same fields, now pointing at the new name.
                        let (b, a) = (extract(&src).links, extract(&out).links);
                        // Two links with a backtick in the name pair up as a code span, which no reader sees as links.
                        if name.contains('`') {
                            continue;
                        }
                        assert_eq!(b.len(), a.len(), "{name:?}\nin  {src:?}\nout {out:?}");
                        for (x, y) in b.iter().zip(&a) {
                            assert_eq!(x.field, y.field, "{name:?}\nout {out:?}");
                            let want = if x.target == "Old" { name } else { x.target.as_str() };
                            assert_eq!(y.target, want, "{name:?}\nin  {src:?}\nout {out:?}");
                            assert_eq!(x.heading, y.heading);
                        }
                        // Nothing else changed: the comment, the other keys and the body.
                        assert!(out.contains("title: T") && out.contains("z: 1"));
                        assert!(out.ends_with(&format!("body [[{name}]]{eol}")), "{out:?}");
                        if v.contains('\n') && v.contains("# why") {
                            assert!(out.contains("# why"));
                        }
                        if !comment.is_empty() && !v.contains('\n') {
                            assert!(out.contains("# trailing"), "{out:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_unquoted_value_is_quoted_whole_when_the_new_name_needs_it() {
        let rw = |n| LinkRewrite { old_stem: "Old", old_path: "Old", new_stem: n, resolves: None, new_path: None };
        assert_eq!(rewrite_links("---\nup: [[Old]]\n---\n", &rw("Smith, John")).unwrap(), "---\nup: \"[[Smith, John]]\"\n---\n");
        assert_eq!(rewrite_links("---\nrel: Bob's [[Old]]\n---\n", &rw("N'M")).unwrap(), "---\nrel: \"Bob's [[N'M]]\"\n---\n");
        // A name that is safe unquoted is written as it is.
        assert_eq!(rewrite_links("---\nup: [[Old]]\n---\n", &rw("New")).unwrap(), "---\nup: [[New]]\n---\n");
        // Comments and code spans are not links.
        assert!(rewrite_links("---\nup: x # see [[Old]]\n---\n", &rw("New")).is_none());
    }
}
