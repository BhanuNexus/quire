// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Link-preserving refactors.

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
pub fn rewrite_links(src: &str, rw: &LinkRewrite<'_>) -> Option<String> {
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for link in extract(src).links {
        if !link.is_internal() || !rw.matches(&link.target) {
            continue;
        }
        let raw = &src[link.range.clone()];
        let new_target = rw.new_target(&link.target);
        match link.kind {
            LinkKind::Wiki | LinkKind::Embed => {
                let open = if link.kind == LinkKind::Embed { 3 } else { 2 };
                let inner = &raw[open..raw.len() - 2];
                // Replace only the target part, keeping `#heading` and `|alias`.
                let cut = inner.find(['#', '|']).unwrap_or(inner.len());
                let mut s = String::with_capacity(raw.len() + 8);
                s.push_str(&raw[..open]);
                s.push_str(&new_target);
                s.push_str(&inner[cut..]);
                s.push_str("]]");
                edits.push((link.range.clone(), s));
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
                let s = format!("{}]({}{ext}{anchor}{title})", &raw[..paren], encode_url(&new_target));
                edits.push((link.range.clone(), s));
            }
            LinkKind::External => {}
        }
    }
    if edits.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(src.len() + edits.len() * 8);
    let mut pos = 0;
    for (r, s) in edits {
        out.push_str(&src[pos..r.start]);
        out.push_str(&s);
        pos = r.end;
    }
    out.push_str(&src[pos..]);
    Some(out)
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
}
