// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing app state files so a crash or power cut leaves the old file or
//! the new one, never half of each.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Replace `path` with `bytes` all at once.
///
/// The bytes go to a temporary file in the same folder, are flushed to
/// disk, then renamed over the old file. The temporary file has a random
/// name and is created new (never one that already exists, so nobody can
/// plant it beforehand), and with `private` it is readable only by its
/// owner (Unix) from the moment it exists.
pub fn write(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let prefix = format!(".{name}.");
    let mut b = tempfile::Builder::new();
    b.prefix(&prefix).suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Created with this mode (less the umask); 0600 for secrets.
        b.permissions(std::fs::Permissions::from_mode(if private { 0o600 } else { 0o666 }));
    }
    #[cfg(not(unix))]
    let _ = private;
    // `tempfile` opens with `create_new` and removes the file if anything fails.
    let mut tmp = b.tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Create `path` with `bytes`, readable only by its owner (Unix).
///
/// Fails if `path` already exists: for a secret written once, such as a
/// recovery key, so it never lands in a file someone else made (or can read).
pub fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    let written = f.write_all(bytes).and_then(|()| f.sync_all());
    if written.is_err() {
        drop(f);
        let _ = std::fs::remove_file(path);
    }
    written
}

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
    fn replaces_whole_files_and_keeps_secrets_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("state.toml");
        write(&p, b"one", false).unwrap();
        write(&p, b"two", true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temporary files left: {leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let q = dir.path().join("secret.toml");
            write(&q, b"k", true).unwrap();
            assert_eq!(std::fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
            // A file planted where the old, predictable temporary name was is
            // neither written to nor used.
            let planted = dir.path().join(format!(".secret.toml.{}.0.tmp", std::process::id()));
            std::fs::write(&planted, b"").unwrap();
            std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
            write(&q, b"key", true).unwrap();
            assert_eq!(std::fs::read(&planted).unwrap(), b"");
            assert_eq!(std::fs::read(&q).unwrap(), b"key");
            assert_eq!(std::fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(keep_unreadable(&p).is_some_and(|c| std::fs::read(c).unwrap() == b"two"));
        // Written once, new, private.
        let rk = dir.path().join("recovery-key.txt");
        write_new_private(&rk, b"RK").unwrap();
        assert!(write_new_private(&rk, b"other").is_err(), "never into a file that exists");
        assert_eq!(std::fs::read(&rk).unwrap(), b"RK");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&rk).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
