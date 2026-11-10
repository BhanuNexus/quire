// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Importers: turn another app's export into Quire notes.
//!
//! Every importer works in two steps. [`plan`] reads the source (a folder,
//! a `.zip`, a `.jex`, a `.bear2bk`, an `.enex` or a `.json`) into memory
//! and produces an [`ImportPlan`] without touching the vault, so the user can
//! review it. [`apply`] then writes the plan, never overwriting existing
//! notes.
//!
//! Exports are untrusted: loading and applying refuse paths that leave the
//! export or the target folder, links, hidden folders, device names, and
//! files past the [`Limits`] (see `tree.rs`).

mod bear;
mod convert;
mod dayone;
mod evernote;
mod html;
mod htmlfolder;
mod joplin;
mod keep;
mod logseq;
mod markdown;
mod notion;
pub mod obsidian;
mod roam;
mod tree;
pub mod yaml;
// WS-B: URL capture.
pub mod web;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use quire_vault::{NotePath, Vault};

pub use tree::{Limits, Stamp, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Logseq,
    Bear,
    Notion,
    Evernote,
    Markdown,
    Roam,
    Keep,
    Joplin,
    DayOne,
    Html,
}

impl Source {
    /// Every source, in the order the import dialog lists them.
    pub const ALL: [Self; 10] = [
        Self::Logseq,
        Self::Notion,
        Self::Bear,
        Self::Evernote,
        Self::Roam,
        Self::Keep,
        Self::Joplin,
        Self::DayOne,
        Self::Markdown,
        Self::Html,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Logseq => "Logseq",
            Self::Bear => "Bear",
            Self::Notion => "Notion",
            Self::Evernote => "Evernote",
            Self::Markdown => "Markdown folder",
            Self::Roam => "Roam Research",
            Self::Keep => "Google Keep",
            Self::Joplin => "Joplin",
            Self::DayOne => "Day One",
            Self::Html => "HTML files",
        }
    }

    /// What to pick, for the dialog and the CLI.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Logseq => "the graph folder",
            Self::Bear => "a .bear2bk backup or a folder of TextBundles",
            Self::Notion => "the .zip (Markdown & CSV) or its folder",
            Self::Evernote => "an .enex file (or a folder of them)",
            Self::Markdown => "a folder of .md and .txt files",
            Self::Roam => "the .json from Export all → JSON",
            Self::Keep => "the Keep folder of a Google Takeout (or its .zip)",
            Self::Joplin => "a .jex file or a RAW export folder",
            Self::DayOne => "the .zip or Journal.json from Export → JSON",
            Self::Html => "a folder of .html files (Apple Notes and OneNote exports, saved pages)",
        }
    }

    /// The word used on the command line (`quire-cli import --from joplin`).
    pub fn id(self) -> &'static str {
        match self {
            Self::Logseq => "logseq",
            Self::Bear => "bear",
            Self::Notion => "notion",
            Self::Evernote => "evernote",
            Self::Markdown => "markdown",
            Self::Roam => "roam",
            Self::Keep => "keep",
            Self::Joplin => "joplin",
            Self::DayOne => "dayone",
            Self::Html => "html",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id.trim().to_ascii_lowercase().replace([' ', '-', '_'], ""))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedNote {
    pub path: NotePath,
    pub content: String,
    /// When the note was made and last changed in the source; set as the
    /// file's time when written.
    pub times: Stamp,
}

impl PlannedNote {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self { path: NotePath::new(path), content: content.into(), times: Stamp::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: NotePath,
    pub bytes: Vec<u8>,
    pub times: Stamp,
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

    /// Notes and files that would be written, and the notes whose name is
    /// taken in `folder` (they would be saved as `(wanted, saved as)`).
    pub fn clashes(&self, vault: &Vault, folder: &str) -> Vec<(NotePath, NotePath)> {
        let base = folder.trim_matches('/');
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = Vec::new();
        for n in &self.notes {
            let wanted = NotePath::join(base, n.path.as_str());
            if vault.exists(&wanted) || taken.contains(&wanted.as_str().to_lowercase()) {
                let mut i = 1;
                let mut cand = NotePath::join(wanted.folder(), &format!("{} {i}.md", wanted.stem()));
                while vault.exists(&cand) || taken.contains(&cand.as_str().to_lowercase()) {
                    i += 1;
                    cand = NotePath::join(wanted.folder(), &format!("{} {i}.md", wanted.stem()));
                }
                taken.insert(cand.as_str().to_lowercase());
                out.push((wanted, cand));
            } else {
                taken.insert(wanted.as_str().to_lowercase());
            }
        }
        out
    }

    /// The notes' names and attachments that exist in `folder` already.
    pub fn existing_files(&self, vault: &Vault, folder: &str) -> usize {
        let base = folder.trim_matches('/');
        self.files.iter().filter(|f| vault.exists(&NotePath::join(base, f.path.as_str()))).count()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub notes: usize,
    pub files: usize,
    /// Notes written under a different name because the target existed.
    pub renamed: Vec<(NotePath, NotePath)>,
    pub skipped_files: usize,
    /// Attachments that already existed with other content; the existing one was kept.
    pub kept_existing: Vec<NotePath>,
    /// Everything written, in order: the notes, then the files.
    pub created: Vec<NotePath>,
    /// Files whose time was set from the source.
    pub times_set: usize,
    /// Planned notes or files refused as unsafe, with why.
    pub refused: Vec<String>,
    /// The import was stopped before the end.
    pub cancelled: bool,
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
    if dayone::detect(tree) {
        return Source::DayOne;
    }
    if keep::detect(tree) {
        return Source::Keep;
    }
    if roam::detect(tree) {
        return Source::Roam;
    }
    if joplin::detect(tree) {
        return Source::Joplin;
    }
    if tree.paths().any(notion::has_id) {
        return Source::Notion;
    }
    if htmlfolder::detect(tree) {
        return Source::Html;
    }
    Source::Markdown
}

/// Read a source and plan the import, finding out what kind of export it is. Nothing is written.
pub fn plan(source: &Path) -> Result<ImportPlan, String> {
    plan_as(source, None, &Limits::default())
}

/// Read a source as `kind` (or find out what it is) within `limits`.
pub fn plan_as(source: &Path, kind: Option<Source>, limits: &Limits) -> Result<ImportPlan, String> {
    let tree = Tree::load_with(source, limits)?;
    let kind = kind.unwrap_or_else(|| detect(&tree, source));
    let mut plan = match kind {
        Source::Logseq => logseq::plan(&tree),
        Source::Bear => bear::plan(&tree),
        Source::Notion => notion::plan(&tree),
        Source::Evernote => evernote::plan(&tree),
        Source::Markdown => markdown::plan(&tree),
        Source::Roam => roam::plan(&tree),
        Source::Keep => keep::plan(&tree),
        Source::Joplin => joplin::plan(&tree),
        Source::DayOne => dayone::plan(&tree),
        Source::Html => htmlfolder::plan(&tree),
    };
    plan.source = Some(kind);
    plan.warnings.splice(0..0, tree.warnings().iter().take(50).cloned());
    if tree.warnings().len() > 50 {
        plan.warnings.insert(50, format!("… and {} more files were skipped", tree.warnings().len() - 50));
    }
    finish(&mut plan);
    Ok(plan)
}

/// Notes whose front matter has `created` / `updated` get those as their
/// file times too, unless the importer already knew them.
fn finish(plan: &mut ImportPlan) {
    for n in &mut plan.notes {
        if n.times == Stamp::default()
            && let Some(fm) = quire_markdown::frontmatter::parse(&n.content)
        {
            let get = |k: &str| fm.get(k).and_then(quire_markdown::Value::as_str).and_then(convert::parse_time);
            n.times = Stamp { created: get("created"), modified: get("updated").or_else(|| get("modified")) };
        }
    }
}

/// A path safe to create under the vault: plain names, no hidden folders
/// (`.quire`, `.git`, `.trash`), nothing that leaves the folder.
fn safe_target(p: &str) -> Result<(), &'static str> {
    let l = Limits { max_depth: 40, ..Limits::default() };
    let clean = tree::clean_path(p, &l)?;
    if clean != p.replace('\\', "/").trim_start_matches("./") {
        return Err("a path that isn't plain");
    }
    if clean.split('/').any(|s| s.starts_with('.')) {
        return Err("a hidden folder or file");
    }
    Ok(())
}

/// What [`apply_with`] tells its caller as it goes.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

/// Write a plan into `folder` of the vault (`""` for the root).
pub fn apply(plan: &ImportPlan, vault: &Vault, folder: &str) -> Result<ImportReport, String> {
    apply_with(plan, vault, folder, None, &mut |_| {})
}

/// Like [`apply`], reporting progress after every file and stopping at the
/// next file once `cancel` is set (what was written stays; the report says so).
pub fn apply_with(
    plan: &ImportPlan,
    vault: &Vault,
    folder: &str,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(Progress),
) -> Result<ImportReport, String> {
    let mut report = ImportReport::default();
    let base = folder.trim_matches('/');
    if !base.is_empty() {
        safe_target(base).map_err(|why| format!("the folder “{folder}” isn't allowed: {why}"))?;
    }
    let total = plan.files.len() + plan.notes.len();
    let mut done = 0;
    let stop = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    for f in &plan.files {
        if stop() {
            report.cancelled = true;
            return Ok(report);
        }
        done += 1;
        progress(Progress { done, total });
        if let Err(why) = safe_target(f.path.as_str()) {
            report.refused.push(format!("{}: {why}", f.path));
            continue;
        }
        let target = NotePath::join(base, f.path.as_str());
        if vault.exists(&target) {
            if vault.storage().read_bytes(&target).is_ok_and(|b| b != f.bytes) {
                report.kept_existing.push(target);
            }
            report.skipped_files += 1;
            continue;
        }
        vault.storage().write_bytes_atomic(&target, &f.bytes).map_err(|e| e.to_string())?;
        set_times(vault, &target, f.times, &mut report);
        report.created.push(target);
        report.files += 1;
    }
    for n in &plan.notes {
        if stop() {
            report.cancelled = true;
            return Ok(report);
        }
        done += 1;
        progress(Progress { done, total });
        if let Err(why) = safe_target(n.path.as_str()) {
            report.refused.push(format!("{}: {why}", n.path));
            continue;
        }
        let wanted = NotePath::join(base, n.path.as_str());
        let target = if vault.exists(&wanted) {
            vault.unique_path(wanted.folder(), wanted.stem()).map_err(|e| e.to_string())?
        } else {
            wanted.clone()
        };
        vault.write(&target, &n.content).map_err(|e| e.to_string())?;
        set_times(vault, &target, n.times, &mut report);
        if target != wanted {
            report.renamed.push((wanted, target.clone()));
        }
        report.created.push(target);
        report.notes += 1;
    }
    Ok(report)
}

/// Give a written file the time it had in the source (best effort: a
/// vault that isn't a folder on disk, or a locked folder, keeps now).
fn set_times(vault: &Vault, path: &NotePath, t: Stamp, report: &mut ImportReport) {
    let Some(secs) = t.modified.or(t.created).filter(|s| *s > 0) else { return };
    let Some(abs) = vault.storage().absolute(path) else { return };
    let Ok(file) = std::fs::File::options().write(true).open(abs) else { return };
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.unsigned_abs());
    if file.set_modified(when).is_ok() {
        report.times_set += 1;
    }
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
            notes: vec![PlannedNote::new("A.md", "new")],
            files: vec![PlannedFile { path: NotePath::new("attachments/x.png"), bytes: vec![1], times: Stamp::default() }],
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
