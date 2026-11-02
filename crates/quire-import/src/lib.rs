// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Importers: turn another app's export into Quire notes.
//!
//! Every importer works in two steps. [`plan`] reads the source (a folder,
//! a `.zip`, a `.bear2bk` or an `.enex`) into memory and produces an
//! [`ImportPlan`] without touching the vault, so the user can review it.
//! [`apply`] then writes the plan, never overwriting existing notes.

mod bear;
mod evernote;
mod html;
mod logseq;
mod markdown;
mod notion;
pub mod obsidian;
mod tree;
pub mod yaml;
// WS-B: URL capture.
pub mod web;

use std::path::Path;

use quire_vault::{NotePath, Vault};

pub use tree::Tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Logseq,
    Bear,
    Notion,
    Evernote,
    Markdown,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Logseq => "Logseq",
            Self::Bear => "Bear",
            Self::Notion => "Notion",
            Self::Evernote => "Evernote",
            Self::Markdown => "Markdown folder",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedNote {
    pub path: NotePath,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: NotePath,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    pub source: Option<Source>,
    pub notes: Vec<PlannedNote>,
    pub files: Vec<PlannedFile>,
    pub warnings: Vec<String>,
}

impl ImportPlan {
    pub fn summary(&self) -> String {
        let label = self.source.map_or("Unknown", Source::label);
        format!("{label}: {} notes, {} attachments, {} warnings", self.notes.len(), self.files.len(), self.warnings.len())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub notes: usize,
    pub files: usize,
    /// Notes written under a different name because the target existed.
    pub renamed: Vec<(NotePath, NotePath)>,
    pub skipped_files: usize,
}

/// Guess what kind of export a loaded tree is.
pub fn detect(tree: &Tree, original: &Path) -> Source {
    let ext = original.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if ext == "enex" || tree.paths().any(|p| p.to_ascii_lowercase().ends_with(".enex")) {
        return Source::Evernote;
    }
    if ext == "bear2bk" || tree.paths().any(|p| p.contains(".textbundle/")) {
        return Source::Bear;
    }
    if tree.contains("logseq/config.edn")
        || (tree.paths().any(|p| p.starts_with("pages/")) && tree.paths().any(|p| p.starts_with("journals/")))
    {
        return Source::Logseq;
    }
    if tree.paths().any(notion::has_id) {
        return Source::Notion;
    }
    Source::Markdown
}

/// Read a source and plan the import. Nothing is written.
pub fn plan(source: &Path) -> Result<ImportPlan, String> {
    let tree = Tree::load(source)?;
    let kind = detect(&tree, source);
    let mut plan = match kind {
        Source::Logseq => logseq::plan(&tree),
        Source::Bear => bear::plan(&tree),
        Source::Notion => notion::plan(&tree),
        Source::Evernote => evernote::plan(&tree),
        Source::Markdown => markdown::plan(&tree),
    };
    plan.source = Some(kind);
    Ok(plan)
}

/// Write a plan into `folder` of the vault (`""` for the root).
pub fn apply(plan: &ImportPlan, vault: &Vault, folder: &str) -> Result<ImportReport, String> {
    let mut report = ImportReport::default();
    let base = folder.trim_matches('/');
    for f in &plan.files {
        let target = NotePath::join(base, f.path.as_str());
        if vault.exists(&target) {
            report.skipped_files += 1;
            continue;
        }
        vault.storage().write_bytes_atomic(&target, &f.bytes).map_err(|e| e.to_string())?;
        report.files += 1;
    }
    for n in &plan.notes {
        let wanted = NotePath::join(base, n.path.as_str());
        let target = if vault.exists(&wanted) {
            vault.unique_path(wanted.folder(), wanted.stem()).map_err(|e| e.to_string())?
        } else {
            wanted.clone()
        };
        vault.write(&target, &n.content).map_err(|e| e.to_string())?;
        if target != wanted {
            report.renamed.push((wanted, target));
        }
        report.notes += 1;
    }
    Ok(report)
}

/// Shared helper: pick `name` or `name 1`, `name 2`… not already in `taken`.
pub(crate) fn unique_name(taken: &mut std::collections::HashSet<String>, path: &str) -> String {
    let (stem, ext) = match path.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !s.ends_with('/') && !e.contains('/') => (s.to_string(), format!(".{e}")),
        _ => (path.to_string(), String::new()),
    };
    let mut candidate = path.to_string();
    let mut n = 1;
    while !taken.insert(candidate.to_lowercase()) {
        candidate = format!("{stem} {n}{ext}");
        n += 1;
    }
    candidate
}

/// A Markdown link or image found by [`rewrite_links`].
pub(crate) struct Link<'a> {
    pub image: bool,
    pub text: &'a str,
    /// Percent-decoded destination, without `<>` or a title.
    pub dest: String,
}

/// Replace `[text](dest)` and `![alt](dest)` wherever `map` returns a
/// replacement. Destinations may contain balanced parentheses.
pub(crate) fn rewrite_links(src: &str, mut map: impl FnMut(&Link<'_>) -> Option<String>) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'[' || (i > 0 && b[i - 1] == b'\\') {
            i += 1;
            continue;
        }
        let image = i > 0 && b[i - 1] == b'!';
        let start = if image { i - 1 } else { i };
        // Matching ']' on the same line, allowing nested brackets.
        let mut depth = 0;
        let mut j = i;
        let mut close = None;
        while j < b.len() && b[j] != b'\n' {
            match b[j] {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let Some(close) = close else {
            i += 1;
            continue;
        };
        if b.get(close + 1) != Some(&b'(') {
            i = close + 1;
            continue;
        }
        let mut k = close + 2;
        let mut parens = 1;
        while k < b.len() && b[k] != b'\n' {
            match b[k] {
                b'(' => parens += 1,
                b')' => {
                    parens -= 1;
                    if parens == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k >= b.len() || b[k] != b')' {
            i = close + 1;
            continue;
        }
        let raw = src[close + 2..k].trim();
        let raw = if let Some(inner) = raw.strip_prefix('<') {
            inner.split('>').next().unwrap_or("")
        } else {
            raw.split(" \"").next().unwrap_or(raw)
        };
        let link = Link { image, text: &src[i + 1..close], dest: percent_decode(raw.trim()) };
        if let Some(rep) = map(&link) {
            out.push_str(&src[copied..start]);
            out.push_str(&rep);
            copied = k + 1;
        }
        i = k + 1;
    }
    out.push_str(&src[copied..]);
    out
}

/// Rewrite Markdown image links whose target satisfies `map` into embeds.
#[cfg(test)]
pub(crate) fn rewrite_images(src: &str, mut map: impl FnMut(&str) -> Option<String>) -> String {
    rewrite_links(src, |l| if l.image { map(&l.dest).map(|n| format!("![[{n}]]")) } else { None })
}

/// Reserve a unique `attachments/<name>` for an imported file.
pub(crate) fn attachment(taken: &mut std::collections::HashSet<String>, name: &str) -> String {
    let name = name.rsplit('/').next().unwrap_or(name);
    let clean: String = name.chars().map(|c| if matches!(c, '[' | ']' | '#' | '^' | '|' | '\\' | ':') { '-' } else { c }).collect();
    let clean = if clean.trim().is_empty() { "file".to_string() } else { clean };
    unique_name(taken, &format!("attachments/{clean}"))
}

pub(crate) fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_never_overwrites() {
        let vault = Vault::in_memory(&[("Imported/A.md", "existing")]);
        let plan = ImportPlan {
            source: Some(Source::Markdown),
            notes: vec![PlannedNote { path: NotePath::new("A.md"), content: "new".into() }],
            files: vec![PlannedFile { path: NotePath::new("attachments/x.png"), bytes: vec![1] }],
            warnings: vec![],
        };
        let r = apply(&plan, &vault, "Imported").unwrap();
        assert_eq!(r.notes, 1);
        assert_eq!(r.renamed[0].1.as_str(), "Imported/A 1.md");
        assert_eq!(vault.read(&NotePath::new("Imported/A.md")).unwrap(), "existing");
        assert!(vault.exists(&NotePath::new("Imported/attachments/x.png")));
    }

    #[test]
    fn helpers() {
        let mut taken = std::collections::HashSet::new();
        assert_eq!(unique_name(&mut taken, "image.png"), "image.png");
        assert_eq!(unique_name(&mut taken, "Image.png"), "Image 1.png");
        let s =
            rewrite_images("a ![x](assets/a%20b.png) b ![y](http://x/y.png) ![z]", |d| (d == "assets/a b.png").then(|| "a b.png".into()));
        assert_eq!(s, "a ![[a b.png]] b ![y](http://x/y.png) ![z]");
    }
}
