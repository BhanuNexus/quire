// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing app state files so a crash or power cut leaves the old file or
//! the new one, never half of each.
//!
//! [`write`] and [`write_new_private`] moved to `quire_base::fs` (0.14), the
//! one atomic writer of the workspace; they stay here for existing callers.

use std::path::{Path, PathBuf};

pub use quire_base::fs::{write, write_new_private};

/// Keep a copy of a file Quire couldn't read (`name.unreadable`), so saving
/// fresh state over it doesn't lose what was there. Returns the copy's path.
pub fn keep_unreadable(path: &Path) -> Option<PathBuf> {
    let mut copy = path.as_os_str().to_owned();
    copy.push(".unreadable");
    let copy = PathBuf::from(copy);
    std::fs::copy(path, &copy).ok().map(|_| copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_copy_of_an_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.toml");
        write(&p, b"two", false).unwrap();
        assert!(keep_unreadable(&p).is_some_and(|c| std::fs::read(c).unwrap() == b"two"));
        assert!(keep_unreadable(&dir.path().join("missing")).is_none());
    }
}
