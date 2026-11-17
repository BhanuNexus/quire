// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing a file atomically: the one implementation in the workspace (P4).
//!
//! Readers see the old file or the new one, never half of either. The bytes
//! go to a temporary file beside the destination, made new (`create_new`,
//! so nobody can plant it beforehand), flushed to disk, then renamed over
//! the destination. On Windows the rename works while others have the file
//! open for reading, and a rename another program blocks for a moment is
//! retried within [`REPLACE_PATIENCE`]. A temporary file is removed when
//! anything fails.
//!
//! - [`write_atomic`] with [`Options`]: app state, config, caches, files in
//!   a shared sync folder.
//! - [`write`](fn@write) and [`write_new_private`]: the old `quire_app::atomic` API.
//! - [`Prepared`] and [`replace_file`]: for a caller that makes its own
//!   temporary file (`Storage` for vault files, which keeps `tempfile`'s
//!   `.tmp…` names that history skips) and only needs the rename.
//! - [`sweep_leftovers`]: temporary files a crash left behind.
//!
//! Vault notes are written through `quire_vault::Storage`, which builds on
//! this module and adds the locked-folder check and one write at a time.

#![expect(clippy::disallowed_methods, reason = "this module is the one place that renames a file into place")]

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one rename or read keeps trying, in all, while another program
/// has the file open in a way that blocks it (Windows only).
pub const REPLACE_PATIENCE: Duration = Duration::from_millis(300);

/// How a file is written by [`write_atomic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Flush the file, and on Unix its folder, so the new file survives a
    /// power cut once the write returns. Off only for counters that are
    /// written very often and may lag after a crash.
    pub durable: bool,
    /// Readable only by its owner (Unix mode 0600) from the moment the
    /// temporary file exists: for keys, tokens and passwords.
    pub private: bool,
    /// Make the destination's folder first. Off in a folder other devices
    /// share, where a missing folder means it was removed on purpose.
    pub create_dirs: bool,
}

impl Options {
    /// App state and config: flushed, the usual permissions, folders made.
    pub const STATE: Self = Self { durable: true, private: false, create_dirs: true };
    /// A secret: as [`Self::STATE`], readable only by its owner.
    pub const SECRET: Self = Self { durable: true, private: true, create_dirs: true };
    /// A file in a folder that must already exist (a shared sync folder).
    pub const EXISTING_FOLDER: Self = Self { durable: true, private: false, create_dirs: false };

    /// The same without flushing to disk.
    #[must_use]
    pub const fn lazy(self) -> Self {
        Self { durable: false, ..self }
    }
}

/// Replace `path` with `bytes` all at once (see the module docs).
///
/// The temporary file is `.<name>.<16 hex digits>.tmp` beside `path`:
/// hidden, so nothing that lists the folder takes it for a file of its
/// own, and of a shape [`sweep_leftovers`] recognises.
pub fn write_atomic(path: &Path, bytes: &[u8], opts: Options) -> std::io::Result<()> {
    Prepared::new(path, bytes, opts)?.place(path)
}

/// [`write_atomic`] for app state ([`Options::STATE`]), or a secret with
/// `private` ([`Options::SECRET`]). The former `quire_app::atomic::write`.
pub fn write(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    write_atomic(path, bytes, if private { Options::SECRET } else { Options::STATE })
}

/// Create `path` with `bytes`, readable only by its owner (Unix).
///
/// Fails if `path` already exists: for a secret written once, such as a
/// recovery key, so it never lands in a file someone else made (or can
/// read). The file is removed again if writing it fails.
pub fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
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
        let _ = fs::remove_file(path);
    }
    written
}

/// Put the finished temporary file `tmp` in place of `dest`, all at once.
///
/// For a caller that writes and flushes its own temporary file in `dest`'s
/// folder. It is renamed with [`std::fs::rename`], which on Windows 10 1607
/// and later replaces a file even while others have it open for reading
/// (the POSIX semantics of `FileRenameInfoEx`); `tempfile`'s own `persist`
/// uses the older `MoveFileExW`, which fails then. A replace that another
/// program blocks (Windows Defender, Search, a sync client, or one of
/// Quire's own readers) is retried within [`REPLACE_PATIENCE`]. The
/// temporary file is removed if the replace fails.
pub fn replace_file(tmp: tempfile::NamedTempFile, dest: &Path) -> std::io::Result<()> {
    Prepared::adopt(tmp)?.place(dest)
}

/// A finished temporary file waiting to be renamed into place. Removed
/// when dropped unless it was placed.
#[derive(Debug)]
pub struct Prepared {
    path: PathBuf,
    /// Flush the folder after the rename (Unix).
    sync_folder: bool,
    placed: bool,
}

impl Prepared {
    /// Write `bytes` to a new temporary file beside `dest` (see
    /// [`write_atomic`] for its name), flushed when `opts.durable`.
    pub fn new(dest: &Path, bytes: &[u8], opts: Options) -> std::io::Result<Self> {
        let dir = folder_of(dest);
        if opts.create_dirs {
            fs::create_dir_all(dir)?;
        }
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut tries = 0;
        let (path, mut file) = loop {
            let tag = crate::rand::bytes::<8>()?;
            let path = dir.join(format!(".{name}.{}.tmp", crate::enc::hex(&tag)));
            let mut o = OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Created with this mode (less the umask); 0600 for secrets.
                o.mode(if opts.private { 0o600 } else { 0o666 });
            }
            match o.open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                Err(e) => return Err(e),
            }
        };
        // From here on, dropping `me` removes the file.
        let me = Self { path, sync_folder: opts.durable, placed: false };
        file.write_all(bytes)?;
        if opts.durable {
            file.sync_all()?;
        }
        Ok(me)
    }

    /// Take over a temporary file the caller wrote and flushed (it becomes
    /// an ordinary file: Windows marks temporary files, and `tempfile` no
    /// longer removes it on drop; this does instead). Its folder is not
    /// flushed after the rename.
    pub fn adopt(tmp: tempfile::NamedTempFile) -> std::io::Result<Self> {
        Ok(Self { path: tmp.into_temp_path().keep().map_err(|e| e.error)?, sync_folder: false, placed: false })
    }

    /// Where the temporary file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One try at renaming the file over `dest`, with no retry: for a
    /// caller that runs its own retry loop ([`is_busy`] tells which errors
    /// are worth one).
    pub fn try_place(&mut self, dest: &Path) -> std::io::Result<()> {
        fs::rename(&self.path, dest)?;
        self.placed = true;
        Ok(())
    }

    /// Rename the file over `dest`, retried while another program blocks
    /// it (within [`REPLACE_PATIENCE`]), then flush the folder when the
    /// file was written durably.
    pub fn place(mut self, dest: &Path) -> std::io::Result<()> {
        retry_busy(|| self.try_place(dest), REPLACE_PATIENCE, is_busy)?;
        #[cfg(unix)]
        if self.sync_folder
            && let Ok(d) = fs::File::open(folder_of(dest))
        {
            // The rename itself; a folder that can't be flushed is no error.
            let _ = d.sync_all();
        }
        Ok(())
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.placed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn folder_of(path: &Path) -> &Path {
    path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

/// Whether a rename failed because another program has the file open right now.
///
/// Windows: access denied, which a file open without delete sharing gives,
/// or a sharing violation; worth a short retry. Never on other systems,
/// where an open file doesn't block a rename.
pub fn is_busy(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32))
}

/// Whether a read failed because another program holds the file right now.
///
/// Windows: a sharing or lock violation, as while a log is appended to.
/// Not access denied: that is also a real permission error, or a folder.
pub fn is_read_busy(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33))
}

/// Run `op` until it succeeds, fails for another reason than `busy`, or
/// `patience` is used up (waits double from 5 ms, at most 50 ms each).
pub fn retry_busy<T>(
    mut op: impl FnMut() -> std::io::Result<T>,
    patience: Duration,
    busy: impl Fn(&std::io::Error) -> bool,
) -> std::io::Result<T> {
    let start = Instant::now();
    let mut wait = Duration::from_millis(5);
    loop {
        match op() {
            Err(e) if busy(&e) && start.elapsed() + wait <= patience => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// Remove temporary files [`write_atomic`] left in `dir` that are older than `age`.
///
/// A crash between creating one and renaming it leaves one behind. Any
/// device's go, since none takes longer than a moment to write. Returns
/// how many went. Only names of exactly that shape are touched.
pub fn sweep_leftovers(dir: &Path, age: Duration) -> usize {
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

/// `.<name>.<16 hex digits>.tmp`, as [`Prepared::new`] names its files.
pub fn is_leftover(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|r| r.strip_suffix(".tmp")) else { return false };
    rest.rsplit_once('.').is_some_and(|(base, tag)| !base.is_empty() && tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn replaces_whole_files_and_keeps_secrets_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("state.toml");
        write(&p, b"one", false).unwrap();
        write(&p, b"two", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(names(p.parent().unwrap()), ["state.toml"], "no temporary files left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let q = dir.path().join("secret.toml");
            write(&q, b"k", true).unwrap();
            assert_eq!(fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
            // A file planted where an old, predictable temporary name was
            // is neither written to nor used.
            let planted = dir.path().join(format!(".secret.toml.{}.0.tmp", std::process::id()));
            fs::write(&planted, b"").unwrap();
            fs::set_permissions(&planted, fs::Permissions::from_mode(0o644)).unwrap();
            write(&q, b"key", true).unwrap();
            assert_eq!(fs::read(&planted).unwrap(), b"");
            assert_eq!(fs::read(&q).unwrap(), b"key");
            assert_eq!(fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_failed_replace_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // The destination is a folder: the rename fails.
        let blocked = dir.path().join("blocked");
        fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(write(&blocked, b"x", false).is_err());
        assert_eq!(names(dir.path()), ["blocked"]);
        // The same through a caller's own temporary file.
        let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        tmp.write_all(b"x").unwrap();
        assert!(replace_file(tmp, &blocked).is_err());
        assert_eq!(names(dir.path()), ["blocked"]);
        // A prepared file never placed is removed when dropped.
        let p = Prepared::new(&dir.path().join("a.txt"), b"x", Options::STATE).unwrap();
        assert!(p.path().exists());
        drop(p);
        assert_eq!(names(dir.path()), ["blocked"]);
    }

    #[test]
    fn replace_file_puts_a_callers_temporary_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("words.dic");
        fs::write(&dest, b"old").unwrap();
        let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        tmp.write_all(b"new").unwrap();
        replace_file(tmp, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["words.dic"]);
    }

    #[test]
    fn writes_whole_and_leaves_no_temporary_file_in_an_existing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("space.json");
        write_atomic(&f, b"one", Options::EXISTING_FOLDER).unwrap();
        write_atomic(&f, b"two", Options::EXISTING_FOLDER.lazy()).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        assert_eq!(names(dir.path()), ["space.json"]);
        // A missing folder is not made again: it was removed on purpose.
        let gone = dir.path().join("missing-folder").join("x.json");
        assert!(write_atomic(&gone, b"x", Options::EXISTING_FOLDER).is_err());
        assert!(!gone.parent().unwrap().exists());
    }

    #[test]
    fn a_leftover_temporary_file_of_another_device_is_never_written_into() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("space.json");
        // Before 0.13.2 every device used this one name.
        fs::write(dir.path().join("space.tmp"), b"another device's half").unwrap();
        write_atomic(&f, b"mine", Options::EXISTING_FOLDER).unwrap();
        assert_eq!(fs::read(dir.path().join("space.tmp")).unwrap(), b"another device's half");
        assert_eq!(fs::read(&f).unwrap(), b"mine");
    }

    #[test]
    fn old_leftovers_are_swept_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join(".space.json.0123456789abcdef.tmp");
        fs::write(&left, b"half").unwrap();
        fs::write(dir.path().join(".space.json.notatag.tmp"), b"x").unwrap();
        fs::write(dir.path().join("space.tmp"), b"x").unwrap();
        assert_eq!(sweep_leftovers(dir.path(), Duration::from_hours(1)), 0, "too new");
        assert_eq!(sweep_leftovers(dir.path(), Duration::ZERO), 1);
        assert!(!left.exists() && dir.path().join("space.tmp").exists());
        // What write_atomic leaves is exactly what the sweep recognises.
        let p = Prepared::new(&dir.path().join("space.json"), b"x", Options::STATE).unwrap();
        assert!(is_leftover(&p.path().file_name().unwrap().to_string_lossy()));
    }

    #[test]
    fn written_once_new_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let rk = dir.path().join("recovery-key.txt");
        write_new_private(&rk, b"RK").unwrap();
        assert!(write_new_private(&rk, b"other").is_err(), "never into a file that exists");
        assert_eq!(fs::read(&rk).unwrap(), b"RK");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&rk).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_replace_blocked_for_a_moment_is_retried_and_one_blocked_for_good_gives_up() {
        let busy = |e: &std::io::Error| e.kind() == std::io::ErrorKind::WouldBlock;
        let blocked = || std::io::Error::from(std::io::ErrorKind::WouldBlock);
        let mut tries = 0;
        let r = retry_busy(
            || {
                tries += 1;
                if tries < 4 { Err(blocked()) } else { Ok(tries) }
            },
            Duration::from_millis(400),
            busy,
        );
        assert_eq!(r.unwrap(), 4);
        let start = Instant::now();
        assert!(retry_busy(|| Err::<(), _>(blocked()), Duration::from_millis(100), busy).is_err());
        assert!(start.elapsed() < Duration::from_millis(400), "bounded: {:?}", start.elapsed());
        // Another error is not retried.
        let mut tries = 0;
        let r = retry_busy(
            || {
                tries += 1;
                Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            },
            Duration::from_millis(400),
            busy,
        );
        assert!(r.is_err() && tries == 1);
        // Reads retry only on sharing or lock violations, never on access denied.
        assert!(!is_read_busy(&std::io::Error::from_raw_os_error(5)));
        assert_eq!(is_read_busy(&std::io::Error::from_raw_os_error(32)), cfg!(windows));
    }

    /// Reliability scan item 4: files are replaced on Windows while
    /// something holds them open for reading.
    #[cfg(windows)]
    #[test]
    fn replaces_a_file_that_is_open_for_reading() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        write(&p, b"one", false).unwrap();
        let held = fs::File::open(&p).unwrap();
        write(&p, b"two", false).unwrap();
        drop(held);
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(names(dir.path()), ["settings.toml"]);
    }
}
