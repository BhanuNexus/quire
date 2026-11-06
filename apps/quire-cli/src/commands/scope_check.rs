// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Warnings for `--only` and `--exclude` folders that match nothing on disk
//! (0.10.1 security review, hardening note).
//!
//! Selective sync compares folder names exactly, letter case and Unicode
//! form included. A mistyped `--exclude journal` (the folder is `Journal`)
//! leaves nothing out: the folder syncs to the server's decrypted copy.
//! Quire still takes the folder as typed (it may be made later), but says
//! so, and names the folder it probably means.

use std::path::Path;

use quire_app::vault::NotePath;

/// A folder name as compared loosely: NFC and lower case.
fn loose(s: &str) -> String {
    quire_agent::canonical_path(&NotePath::new(s)).map_or_else(|_| s.to_lowercase(), |p| p.as_str().to_lowercase())
}

/// Where `folder` (`Work/Old/`, as `Selection` keeps it) is in `vault`:
/// `Ok(())` when it exists exactly, `Err(Some(near))` when only a folder
/// differing in letter case or Unicode form does, `Err(None)` otherwise.
fn find(vault: &Path, folder: &str) -> Result<(), Option<String>> {
    let mut at = vault.to_path_buf();
    let mut near: Vec<String> = Vec::new();
    let mut exact = true;
    for seg in folder.trim_end_matches('/').split('/').filter(|s| !s.is_empty()) {
        let names: Vec<String> = std::fs::read_dir(&at)
            .map(|r| r.flatten().filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().to_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let found = if names.iter().any(|n| n == seg) {
            seg.to_string()
        } else if let Some(n) = names.iter().find(|n| loose(n) == loose(seg)) {
            exact = false;
            n.clone()
        } else {
            return Err(None);
        };
        at.push(&found);
        near.push(found);
    }
    if exact { Ok(()) } else { Err(Some(format!("{}/", near.join("/")))) }
}

/// The warnings for `flag`'s folders that match no folder in `vault`.
pub(crate) fn warnings(vault: &Path, flag: &str, folders: &[String]) -> Vec<String> {
    folders
        .iter()
        .filter_map(|f| match find(vault, f) {
            Ok(()) => None,
            Err(Some(near)) => Some(format!(
                "warning: {flag} “{f}” matches no folder here; did you mean “{near}”? Folder names are compared exactly, letter case included."
            )),
            Err(None) => Some(format!("warning: {flag} “{f}” matches no folder in {} (yet).", vault.display())),
        })
        .collect()
}

/// Print [`warnings`] for a selection's folders to stderr.
pub(crate) fn warn(vault: &Path, only: &[String], exclude: &[String]) {
    for w in warnings(vault, "--only", only).into_iter().chain(warnings(vault, "--exclude", exclude)) {
        eprintln!("{w}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mistyped_folder_is_named_with_the_one_meant() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Journal/2026")).unwrap();
        std::fs::create_dir_all(dir.path().join("Café")).unwrap();
        std::fs::write(dir.path().join("Notes.md"), "x").unwrap();
        assert!(warnings(dir.path(), "--exclude", &["Journal/".into(), "Journal/2026/".into()]).is_empty());
        let w = warnings(dir.path(), "--exclude", &["journal/".into()]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("did you mean “Journal/”") && w[0].contains("letter case"), "{w:?}");
        let w = warnings(dir.path(), "--exclude", &["JOURNAL/2026/".into()]);
        assert!(w[0].contains("“Journal/2026/”"), "{w:?}");
        // Another Unicode form of the same name (e + combining accent).
        let w = warnings(dir.path(), "--only", &["Cafe\u{301}/".into()]);
        assert!(w[0].contains("did you mean") && w[0].starts_with("warning: --only"), "{w:?}");
        let w = warnings(dir.path(), "--exclude", &["Private/".into(), "Notes.md/".into()]);
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|x| x.contains("matches no folder in")), "{w:?}");
    }
}
