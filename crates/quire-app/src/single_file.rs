// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Opening a Markdown file that was handed to Quire (0.12): `quire notes.md`,
//! a file association, a file dropped on the welcome screen.
//!
//! * A file inside the last vault, or inside any folder that has a `.quire/`
//!   folder above it, opens in that vault.
//! * Any other file opens on its own ([`crate::workspace::Workspace::open_single_file`]):
//!   its folder does not become a vault.
//! * A file that isn't Markdown is not opened.

use std::path::{Path, PathBuf};

use quire_vault::NotePath;

/// Where a file handed to Quire opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTarget {
    /// In this vault, as this note.
    Vault { root: PathBuf, note: NotePath },
    /// On its own, outside any vault.
    Single,
    /// Not a Markdown file.
    NotMarkdown,
}

/// Whether `file` is named like a Markdown note.
pub fn is_markdown(file: &Path) -> bool {
    file.file_name().is_some_and(|n| NotePath::new(n.to_string_lossy()).is_markdown())
}

/// Decide where `file` opens. `last_vault` is the vault Quire would open anyway.
pub fn resolve(file: &Path, last_vault: Option<&Path>) -> FileTarget {
    if !is_markdown(file) {
        return FileTarget::NotMarkdown;
    }
    let Ok(file) = file.canonicalize() else { return FileTarget::Single };
    let inside = |root: &Path| -> Option<NotePath> {
        let rel = file.strip_prefix(root).ok()?;
        // Hidden folders (`.quire/`, `.git/`) are not notes.
        if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
            return None;
        }
        Some(NotePath::new(rel.to_string_lossy().replace('\\', "/")))
    };
    if let Some(root) = last_vault.and_then(|v| v.canonicalize().ok())
        && let Some(note) = inside(&root)
    {
        return FileTarget::Vault { root, note };
    }
    for dir in file.ancestors().skip(1) {
        if dir.join(".quire").is_dir()
            && let Some(note) = inside(dir)
        {
            return FileTarget::Vault { root: dir.to_path_buf(), note };
        }
    }
    FileTarget::Single
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, Workspace};

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "# Hello\n\nbody\n").unwrap();
        }
        dir
    }

    #[test]
    fn a_file_opens_in_its_vault_or_on_its_own() {
        let t = tree(&["vault/.quire/x", "vault/A/b.md", "vault/.quire/c.md", "loose/n.md", "loose/pic.png", "loose/n.txt"]);
        let root = t.path().join("vault").canonicalize().unwrap();
        let note = |p: &str| FileTarget::Vault { root: root.clone(), note: NotePath::new(p) };
        // Inside the last vault, or any folder with a `.quire/` above the file.
        assert_eq!(resolve(&t.path().join("vault/A/b.md"), Some(&root)), note("A/b.md"));
        assert_eq!(resolve(&t.path().join("vault/A/b.md"), None), note("A/b.md"));
        // Outside every vault: on its own, whatever the last vault is.
        assert_eq!(resolve(&t.path().join("loose/n.md"), None), FileTarget::Single);
        assert_eq!(resolve(&t.path().join("loose/n.md"), Some(&root)), FileTarget::Single);
        // App data and other file types are not notes.
        assert_eq!(resolve(&t.path().join("vault/.quire/c.md"), Some(&root)), FileTarget::Single);
        assert_eq!(resolve(&t.path().join("loose/pic.png"), None), FileTarget::NotMarkdown);
        assert_eq!(resolve(&t.path().join("loose/n.txt"), None), FileTarget::NotMarkdown);
        assert_eq!(resolve(&t.path().join("missing.md"), None), FileTarget::Single);
    }

    /// The folder doesn't become a vault: nothing is made next to the file, other
    /// files aren't listed, and saving writes the file and only the file.
    #[test]
    fn single_file_mode_leaves_the_folder_alone() {
        let t = tree(&["loose/n.md", "loose/other.md"]);
        std::fs::write(t.path().join("loose/pic.png"), b"png").unwrap();
        let file = t.path().join("loose/n.md");
        let before = std::fs::read_dir(t.path().join("loose")).unwrap().count();
        let mut w = Workspace::new(Settings::default());
        let id = w.open_single_file(&file).unwrap();
        assert_eq!(w.single_file().map(Path::to_path_buf), file.canonicalize().ok());
        assert_eq!(w.vault_name(), "Single file");
        assert!(w.vault_root().is_none());
        w.index().flush();
        let notes: Vec<String> = w.index().read().records().map(|r| r.path.to_string()).collect();
        assert_eq!(notes, ["n.md"], "the folder isn't scanned");
        // Edit, tick (autosave, background work), save.
        w.activate(id);
        w.insert_text("added ");
        for _ in 0..5 {
            w.tick(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }
        w.save_all();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "added # Hello\n\nbody\n");
        w.shutdown();
        let after: Vec<_> =
            std::fs::read_dir(t.path().join("loose")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(after.len(), before, "nothing new next to the file: {after:?}");
        assert!(!t.path().join("loose/.quire").exists());
        // Other files can be read (a picture beside the note), not made, moved or deleted.
        let v = w.vault().unwrap();
        assert_eq!(v.storage().read_bytes(&NotePath::new("pic.png")).unwrap(), b"png");
        assert!(v.save_attachment("new.png", b"x").is_err());
        assert!(v.write(&NotePath::new("fresh.md"), "x").is_err());
        assert!(v.trash(&NotePath::new("n.md")).is_err());
        assert!(t.path().join("loose/n.md").exists() && !t.path().join("loose/fresh.md").exists());
    }
}
