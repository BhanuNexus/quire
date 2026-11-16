// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing small files whole (0.13.2).
//!
//! `quire_app::atomic::write` does the same for the app; this crate can't
//! depend on `quire-app`, so it has its own until both share one.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

/// Write `bytes` to `path` so that readers see the old file or the new
/// one, never half of either, and the new one survives a power cut once
/// this returns.
///
/// The bytes go to a temporary file of a random name, made with
/// `create_new` next to `path` (so two devices writing the same file in a
/// shared cloud folder never write into each other's), flushed to disk,
/// then renamed over `path`. Temporary names start with a dot and end in
/// `.tmp`, so nothing that reads the folder takes them for its files.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write(path, bytes, true)
}

/// [`write_atomically`] without flushing to disk: for this device's own
/// counters that are written for every revision and can lag after a power
/// cut (a flush each would make a big push many times slower).
pub(crate) fn write_quickly(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write(path, bytes, false)
}

fn write(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tries = 0;
    let (tmp, mut file) = loop {
        let tag = crate::crypto::random_bytes::<8>().map_err(|e| std::io::Error::other(e.to_string()))?;
        let tmp = dir.join(format!(".{name}.{}.tmp", crate::crypto::hex(&tag)));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    let written = file.write_all(bytes).and_then(|()| if durable { file.sync_all() } else { Ok(()) });
    drop(file);
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename itself, on systems where a folder can be flushed.
    #[cfg(unix)]
    if durable && let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Remove temporary files [`write_atomically`] left in `dir` (a crash
/// between creating one and renaming it) that are older than `age`: any
/// device's, since none takes longer than a moment to write. Returns how
/// many went. Only names of exactly that shape are touched.
pub(crate) fn sweep(dir: &Path, age: std::time::Duration) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let now = std::time::SystemTime::now();
    let mut gone = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_leftover(&name) {
            continue;
        }
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok()).is_some_and(|d| d > age);
        if old && e.file_type().is_ok_and(|t| t.is_file()) && fs::remove_file(e.path()).is_ok() {
            gone += 1;
        }
    }
    gone
}

/// `.<name>.<16 hex digits>.tmp`, as [`write()`] names its temporary files.
fn is_leftover(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|r| r.strip_suffix(".tmp")) else { return false };
    rest.rsplit_once('.').is_some_and(|(base, tag)| !base.is_empty() && tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_whole_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("space.json");
        write_atomically(&f, b"one").unwrap();
        write_atomically(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        let names: Vec<String> =
            fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["space.json"]);
    }

    #[test]
    fn a_leftover_temporary_file_of_another_device_is_never_written_into() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("space.json");
        // Before 0.13.2 every device used this one name.
        fs::write(dir.path().join("space.tmp"), b"another device's half").unwrap();
        write_atomically(&f, b"mine").unwrap();
        assert_eq!(fs::read(dir.path().join("space.tmp")).unwrap(), b"another device's half");
        assert_eq!(fs::read(&f).unwrap(), b"mine");
    }

    #[test]
    fn a_failed_write_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("missing-folder").join("x.json");
        assert!(write_atomically(&f, b"x").is_err());
        assert!(!f.exists());
    }

    #[test]
    fn old_leftovers_are_swept_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join(".space.json.0123456789abcdef.tmp");
        fs::write(&left, b"half").unwrap();
        fs::write(dir.path().join(".space.json.notatag.tmp"), b"x").unwrap();
        fs::write(dir.path().join("space.tmp"), b"x").unwrap();
        assert_eq!(sweep(dir.path(), std::time::Duration::from_hours(1)), 0, "too new");
        assert_eq!(sweep(dir.path(), std::time::Duration::ZERO), 1);
        assert!(!left.exists() && dir.path().join("space.tmp").exists());
    }
}
