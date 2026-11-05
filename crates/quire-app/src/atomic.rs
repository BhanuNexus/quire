// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing app state files so a crash or power cut leaves the old file or
//! the new one, never half of each.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Numbers temporary files, so threads writing the same file at once don't collide.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Replace `path` with `bytes` all at once.
///
/// The bytes go to a temporary file in the same folder, are flushed to
/// disk, then renamed over the old file. With `private`, the
/// file is readable only by its owner (Unix) from the moment it exists.
pub fn write(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Unique per call too: two threads saving the same file must not share
    // a temporary file (one rename would find it gone).
    let n = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
        }
        assert!(keep_unreadable(&p).is_some_and(|c| std::fs::read(c).unwrap() == b"two"));
    }
}
