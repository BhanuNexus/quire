// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::Path;
use std::sync::Arc;

use chrono::{Local, NaiveDate};

use crate::refactor::{LinkRewrite, rewrite_links};
use crate::templates::{TemplateVars, expand_template};
use crate::{LocalFs, MemFs, NotePath, Result, Storage, VaultError, sanitize_title};

#[derive(Debug, Clone)]
pub struct VaultConfig {
    pub daily_folder: String,
    /// strftime format for daily note names.
    pub daily_format: String,
    pub templates_folder: String,
    /// Template applied to new daily notes, relative to `templates_folder`.
    pub daily_template: String,
    /// Where pasted and dropped files go.
    pub attachments_folder: String,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            daily_folder: "Daily".into(),
            daily_format: "%Y-%m-%d".into(),
            templates_folder: "Templates".into(),
            daily_template: "Daily.md".into(),
            attachments_folder: "attachments".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameReport {
    pub old_path: NotePath,
    pub new_path: NotePath,
    /// Notes whose links were rewritten (may include the renamed note itself).
    pub updated: Vec<NotePath>,
}

/// Repository over a [`Storage`].
#[derive(Clone)]
pub struct Vault {
    storage: Arc<dyn Storage>,
    pub config: VaultConfig,
}

impl Vault {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage, config: VaultConfig::default() }
    }

    pub fn open_local(root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Arc::new(LocalFs::new(root.as_ref())?)))
    }

    pub fn in_memory(files: &[(&str, &str)]) -> Self {
        Self::new(Arc::new(MemFs::with_files(files.iter().copied())))
    }

    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }

    /// All Markdown notes, excluding templates.
    pub fn notes(&self) -> Result<Vec<NotePath>> {
        let tpl = format!("{}/", self.config.templates_folder);
        Ok(self.storage.list()?.into_iter().filter(|p| p.is_markdown() && !p.as_str().starts_with(&tpl)).collect())
    }

    pub fn read(&self, p: &NotePath) -> Result<String> {
        self.storage.read(p)
    }

    pub fn write(&self, p: &NotePath, content: &str) -> Result<()> {
        self.storage.write_atomic(p, content)
    }

    pub fn exists(&self, p: &NotePath) -> bool {
        self.storage.exists(p)
    }

    /// First free `folder/Title.md`, then `Title 1.md`, `Title 2.md`…
    #[allow(clippy::unnecessary_wraps, reason = "same Result signature as the other path-creating calls it is chained with")]
    pub fn unique_path(&self, folder: &str, title: &str) -> Result<NotePath> {
        let base = sanitize_title(title);
        let base = if base.is_empty() { "Untitled".to_string() } else { base };
        let mut candidate = NotePath::join(folder, &format!("{base}.md"));
        let mut n = 1;
        while self.storage.exists(&candidate) {
            candidate = NotePath::join(folder, &format!("{base} {n}.md"));
            n += 1;
        }
        Ok(candidate)
    }

    pub fn create_note(&self, folder: &str, title: &str, content: &str) -> Result<NotePath> {
        let p = self.unique_path(folder, title)?;
        self.storage.write_atomic(&p, content)?;
        Ok(p)
    }

    /// Resolve a wikilink target the way Obsidian does: exact path first,
    /// then by note name (case-insensitive), preferring `from`'s folder and
    /// then the shortest path.
    pub fn resolve(&self, target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        let notes = self.notes().ok()?;
        resolve_in(&notes, target, from)
    }

    /// Whether `p` is in the templates folder (templates aren't notes:
    /// [`Self::notes`] leaves them out, and so does the index).
    pub fn is_template(&self, p: &NotePath) -> bool {
        p.as_str().strip_prefix(self.config.templates_folder.as_str()).is_some_and(|rest| rest.starts_with('/'))
    }

    pub fn templates(&self) -> Vec<NotePath> {
        let prefix = format!("{}/", self.config.templates_folder);
        self.storage.list().unwrap_or_default().into_iter().filter(|p| p.is_markdown() && p.as_str().starts_with(&prefix)).collect()
    }

    /// Create a note from a template. Returns the path and caret offset.
    pub fn create_from_template(&self, folder: &str, title: &str, template: &NotePath) -> Result<(NotePath, Option<usize>)> {
        let tpl = self.storage.read(template)?;
        let (body, cursor) = expand_template(&tpl, &TemplateVars { title: title.into(), now: Local::now() });
        Ok((self.create_note(folder, title, &body)?, cursor))
    }

    pub fn daily_path(&self, date: NaiveDate) -> NotePath {
        NotePath::join(&self.config.daily_folder, &format!("{}.md", date.format(&self.config.daily_format)))
    }

    /// Open-or-create today's (or any day's) note. Returns (path, created).
    pub fn daily_note(&self, date: NaiveDate) -> Result<(NotePath, bool)> {
        let p = self.daily_path(date);
        if self.storage.exists(&p) {
            return Ok((p, false));
        }
        let title = p.stem().to_string();
        let tpl_path = NotePath::join(&self.config.templates_folder, &self.config.daily_template);
        let body = match self.storage.read(&tpl_path) {
            Ok(tpl) => {
                let now = date.and_hms_opt(9, 0, 0).and_then(|d| d.and_local_timezone(Local).single()).unwrap_or_else(Local::now);
                expand_template(&tpl, &TemplateVars { title, now }).0
            }
            Err(_) => format!("# {title}\n\n"),
        };
        self.storage.write_atomic(&p, &body)?;
        Ok((p, true))
    }

    /// Rename a note and rewrite every link to it across the vault.
    pub fn rename_note(&self, from: &NotePath, new_title: &str) -> Result<RenameReport> {
        let new_stem = sanitize_title(new_title);
        if new_stem.is_empty() {
            return Err(VaultError::InvalidName(new_title.into()));
        }
        let to = from.with_stem(&new_stem);
        if &to == from {
            return Ok(RenameReport { old_path: from.clone(), new_path: to, updated: vec![] });
        }
        if self.storage.exists(&to) && !to.as_str().eq_ignore_ascii_case(from.as_str()) {
            return Err(VaultError::Exists(to.to_string()));
        }
        // Read everything first so a failure leaves the vault untouched.
        let notes = self.notes()?;
        let mut rewrites = Vec::new();
        for p in &notes {
            let src = self.storage.read(p)?;
            let check = |target: &str| resolve_in(&notes, target, Some(p)).as_ref() == Some(from);
            let rw = LinkRewrite {
                old_stem: from.stem(),
                old_path: from.without_ext(),
                new_stem: &new_stem,
                resolves: Some(&check),
                new_path: None,
            };
            if let Some(new_src) = rewrite_links(&src, &rw) {
                rewrites.push((p, new_src));
            }
        }
        self.storage.rename(from, &to)?;
        let _ = self.move_history(from, &to);
        let mut updated = Vec::with_capacity(rewrites.len());
        for (p, content) in rewrites {
            let p = if p == from { to.clone() } else { p.clone() };
            self.storage.write_atomic(&p, &content)?;
            updated.push(p);
        }
        Ok(RenameReport { old_path: from.clone(), new_path: to, updated })
    }

    /// Move a note to another folder (`""` = vault root), keeping its name.
    /// Path-style links to it are rewritten; links by name keep working.
    pub fn move_note(&self, from: &NotePath, to_folder: &str) -> Result<RenameReport> {
        let folder = to_folder.trim().trim_matches('/');
        if folder.split('/').any(|seg| seg != sanitize_title(seg) || seg.starts_with('.')) {
            return Err(VaultError::InvalidName(to_folder.into()));
        }
        let to = NotePath::join(folder, from.file_name());
        if &to == from {
            return Ok(RenameReport { old_path: from.clone(), new_path: to, updated: vec![] });
        }
        if self.storage.exists(&to) {
            return Err(VaultError::Exists(to.to_string()));
        }
        let notes = self.notes()?;
        let mut rewrites = Vec::new();
        for p in &notes {
            let src = self.storage.read(p)?;
            let check = |target: &str| resolve_in(&notes, target, Some(p)).as_ref() == Some(from);
            let rw = LinkRewrite {
                old_stem: from.stem(),
                old_path: from.without_ext(),
                new_stem: from.stem(),
                resolves: Some(&check),
                new_path: Some(to.without_ext()),
            };
            if let Some(new_src) = rewrite_links(&src, &rw) {
                rewrites.push((p, new_src));
            }
        }
        self.storage.rename(from, &to)?;
        let _ = self.move_history(from, &to);
        let mut updated = Vec::with_capacity(rewrites.len());
        for (p, content) in rewrites {
            let p = if p == from { to.clone() } else { p.clone() };
            self.storage.write_atomic(&p, &content)?;
            updated.push(p);
        }
        Ok(RenameReport { old_path: from.clone(), new_path: to, updated })
    }

    /// Folders that contain notes, for "Move to folder" suggestions.
    pub fn folders(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.notes().unwrap_or_default().iter().map(|p| p.folder().to_string()).filter(|f| !f.is_empty()).collect();
        v.sort_by_key(|f| f.to_lowercase());
        v.dedup();
        v
    }

    /// Store bytes as an attachment with a unique, link-safe file name.
    pub fn save_attachment(&self, file_name: &str, bytes: &[u8]) -> Result<NotePath> {
        let (stem, ext) = match file_name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, format!(".{}", e.to_ascii_lowercase())),
            _ => (file_name, String::new()),
        };
        let mut base = sanitize_title(stem);
        if base.is_empty() {
            base = "attachment".into();
        }
        let folder = self.config.attachments_folder.clone();
        let mut candidate = NotePath::join(&folder, &format!("{base}{ext}"));
        let mut n = 1;
        while self.storage.exists(&candidate) {
            candidate = NotePath::join(&folder, &format!("{base} {n}{ext}"));
            n += 1;
        }
        self.storage.write_bytes_atomic(&candidate, bytes)?;
        Ok(candidate)
    }

    /// Find a non-note file by name or path (for `![[image.png]]`).
    pub fn resolve_file(&self, target: &str) -> Option<NotePath> {
        let t = target.trim().trim_start_matches("./").to_lowercase();
        let files = self.storage.list().ok()?;
        files.iter().find(|p| p.as_str().to_lowercase() == t).or_else(|| files.iter().find(|p| p.file_name().to_lowercase() == t)).cloned()
    }

    pub fn trash(&self, p: &NotePath) -> Result<()> {
        self.storage.trash(p)
    }
}

/// Pure resolution over a list of paths (shared with the index).
pub(crate) fn resolve_in(notes: &[NotePath], target: &str, from: Option<&NotePath>) -> Option<NotePath> {
    let target = target.trim().trim_end_matches(".md");
    if target.is_empty() {
        return from.cloned();
    }
    if let Some(exact) = notes.iter().find(|p| p.without_ext().eq_ignore_ascii_case(target)) {
        return Some(exact.clone());
    }
    let name = target.rsplit('/').next().unwrap_or(target);
    let mut candidates: Vec<&NotePath> = notes
        .iter()
        .filter(|p| {
            p.stem().eq_ignore_ascii_case(name)
                && (!target.contains('/') || p.without_ext().to_lowercase().ends_with(&target.to_lowercase()))
        })
        .collect();
    let here = from.map(NotePath::folder);
    candidates.sort_by_key(|p| (Some(p.folder()) != here, p.as_str().len()));
    candidates.first().map(|p| (*p).clone())
}

impl Vault {
    pub fn resolve_among(notes: &[NotePath], target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        resolve_in(notes, target, from)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn move_note_rewrites_path_links_only() {
        let v = Vault::in_memory(&[
            ("Ideas.md", "See [[Ideas]] and [[Inbox/Plan]] and [[Plan#Goal|the plan]]"),
            ("Inbox/Plan.md", "# Plan\nback to [[Ideas]]"),
        ]);
        let r = v.move_note(&NotePath::new("Inbox/Plan.md"), "Projects/2026").unwrap();
        assert_eq!(r.new_path.as_str(), "Projects/2026/Plan.md");
        assert_eq!(v.read(&NotePath::new("Ideas.md")).unwrap(), "See [[Ideas]] and [[Projects/2026/Plan]] and [[Plan#Goal|the plan]]");
        assert!(!v.exists(&NotePath::new("Inbox/Plan.md")));
        assert_eq!(v.folders(), vec!["Projects/2026"]);
        assert!(v.move_note(&NotePath::new("Ideas.md"), "../escape").is_err());
        assert!(v.move_note(&NotePath::new("Ideas.md"), ".hidden").is_err());
        let r = v.move_note(&NotePath::new("Projects/2026/Plan.md"), "").unwrap();
        assert_eq!(r.new_path.as_str(), "Plan.md");
    }
    use super::*;

    fn vault() -> Vault {
        Vault::in_memory(&[
            ("Projects/Ideas.md", "Link to [[Rope Notes]] and [[Rope Notes#Undo]]"),
            ("Root.md", "Ambiguous [[Rope Notes]] resolves to the shortest path"),
            ("Projects/Rope Notes.md", "# Rope Notes\nSelf link [[Rope Notes]]"),
            ("Archive/Rope Notes.md", "old copy"),
            ("Templates/Daily.md", "# {{title}}\n## Tasks\n- [ ] "),
        ])
    }

    #[test]
    fn resolves_like_obsidian() {
        let v = vault();
        let from_projects = NotePath::new("Projects/Other.md");
        assert_eq!(v.resolve("Rope Notes", Some(&from_projects)).unwrap().as_str(), "Projects/Rope Notes.md");
        assert_eq!(v.resolve("archive/rope notes", None).unwrap().as_str(), "Archive/Rope Notes.md");
        assert!(v.resolve("Nope", None).is_none());
        assert!(!v.notes().unwrap().iter().any(|p| p.as_str().starts_with("Templates/")));
        assert!(v.is_template(&NotePath::new("Templates/Daily.md")));
        assert!(!v.is_template(&NotePath::new("Templates.md")) && !v.is_template(&NotePath::new("TemplatesOld/A.md")));
    }

    #[test]
    fn unique_names_and_daily_notes() {
        let v = vault();
        assert_eq!(v.create_note("Projects", "Ideas", "").unwrap().as_str(), "Projects/Ideas 1.md");
        let day = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let (p, created) = v.daily_note(day).unwrap();
        assert!(created);
        assert_eq!(p.as_str(), "Daily/2026-11-01.md");
        assert!(v.read(&p).unwrap().starts_with("# 2026-11-01\n## Tasks"));
        assert!(!v.daily_note(day).unwrap().1);
    }

    #[test]
    fn attachments_get_unique_names() {
        let v = vault();
        let a = v.save_attachment("Screen Shot?.PNG", b"x").unwrap();
        let b = v.save_attachment("Screen Shot?.PNG", b"y").unwrap();
        assert_eq!(a.as_str(), "attachments/Screen Shot.png");
        assert_eq!(b.as_str(), "attachments/Screen Shot 1.png");
        assert_eq!(v.storage().read_bytes(&b).unwrap(), b"y");
        assert_eq!(v.resolve_file("screen shot 1.png"), Some(b));
        assert!(v.notes().unwrap().iter().all(NotePath::is_markdown));
    }

    #[test]
    fn rename_rewrites_backlinks() {
        let v = vault();
        let report = v.rename_note(&NotePath::new("Projects/Rope Notes.md"), "Text Buffers").unwrap();
        assert_eq!(report.new_path.as_str(), "Projects/Text Buffers.md");
        assert_eq!(v.read(&NotePath::new("Projects/Ideas.md")).unwrap(), "Link to [[Text Buffers]] and [[Text Buffers#Undo]]");
        assert_eq!(v.read(&report.new_path).unwrap(), "# Rope Notes\nSelf link [[Text Buffers]]");
        // Root.md's link resolved to Archive/Rope Notes, so it must be untouched.
        assert!(v.read(&NotePath::new("Root.md")).unwrap().contains("[[Rope Notes]]"));
        assert!(matches!(v.rename_note(&report.new_path, "Ideas"), Err(VaultError::Exists(_))));
    }
}
