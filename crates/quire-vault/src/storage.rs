// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::{NotePath, Result, VaultError, io_err};

/// Folders never shown as notes (VCS, app metadata, trash).
pub(crate) fn is_hidden(path: &str) -> bool {
    path.split('/').any(|seg| seg.starts_with('.') || seg == "node_modules")
}

/// A path that `root.join` would not keep under `root` on some system:
/// rooted (`/x`, `\\x`), a drive (`C:`, `C:/x`, `c:x`) or another prefix
/// (`\\\\?\\…`). On Windows `root.join("C:/…")` replaces the root.
pub(crate) fn leaves_root(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    drive
        || p.starts_with('/')
        || p.starts_with('\\')
        || Path::new(p).components().any(|c| matches!(c, std::path::Component::Prefix(_) | std::path::Component::RootDir))
}

/// Storage port. Paths are vault-relative.
pub trait Storage: Send + Sync {
    /// Every visible file (not just Markdown), sorted.
    fn list(&self) -> Result<Vec<NotePath>>;
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>>;
    /// Crash-safe write: readers see the old or the new content, never half.
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()>;
    /// Text read: strips a UTF-8 BOM and decodes lossily so one stray byte never blocks opening a note.
    fn read(&self, path: &NotePath) -> Result<String> {
        let bytes = self.read_bytes(path)?;
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
    fn write_atomic(&self, path: &NotePath, contents: &str) -> Result<()> {
        self.write_bytes_atomic(path, contents.as_bytes())
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()>;
    /// Soft delete into `.trash/`.
    fn trash(&self, path: &NotePath) -> Result<()>;
    fn exists(&self, path: &NotePath) -> bool;
    fn modified(&self, path: &NotePath) -> Result<SystemTime>;
    /// Absolute location on disk, for adapters that have one.
    fn absolute(&self, path: &NotePath) -> Option<PathBuf>;
    /// Files under a folder, including hidden app folders like `.quire/`.
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>>;
    /// Permanently delete (used for app data such as pruned history, never notes).
    fn remove(&self, path: &NotePath) -> Result<()>;
    /// Append one line (a `\n` is added) to an app-data log such as a
    /// `.jsonl` file, creating it. `line` must not contain a newline.
    /// [`LocalFs`] locks the file around a single write, so lines written by
    /// several processes never interleave.
    fn append_line(&self, path: &NotePath, line: &str) -> Result<()> {
        let mut bytes = match self.read_bytes(path) {
            Ok(b) => b,
            Err(VaultError::NotFound(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.write_bytes_atomic(path, &bytes)
    }
    /// Write `bytes` only if the file is still what `still` accepts (its
    /// bytes now; `None` when it doesn't exist): a compare-and-swap. Returns
    /// whether it wrote. [`LocalFs`] checks and writes as one step against
    /// every other write through this process's storage (sync, the web app,
    /// agents, the editor); other programs can't be held off. `still` must
    /// not write to storage itself.
    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        if !still(read_if_exists(self, path)?.as_deref()) {
            return Ok(false);
        }
        self.write_bytes_atomic(path, bytes)?;
        Ok(true)
    }
    /// The locked folders the vault remembers apart from their key files
    /// ([`crate::lock::Known`]): plain writes there stay refused even while
    /// a key file is missing. Adapters that refuse plain writes into locked
    /// folders keep the list; the default ignores it.
    fn set_locked_folders(&self, _folders: &[String]) {}
    /// [`Self::trash`] only if the file is still what `still` accepts, as
    /// one step like [`Self::write_bytes_if`]. Returns whether it did.
    fn trash_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool) -> Result<bool> {
        if !still(read_if_exists(self, path)?.as_deref()) {
            return Ok(false);
        }
        self.trash(path)?;
        Ok(true)
    }
}

/// A file's bytes, `None` when it doesn't exist.
fn read_if_exists(s: &(impl Storage + ?Sized), path: &NotePath) -> Result<Option<Vec<u8>>> {
    match s.read_bytes(path) {
        Ok(b) => Ok(Some(b)),
        Err(VaultError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Text as [`Storage::read`] gives it: a UTF-8 BOM dropped, decoded lossily.
pub(crate) fn decode(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))
}

/// One write to vault files at a time in this process, so a write that
/// checks the file first ([`Storage::write_bytes_if`]) can't be overtaken
/// between its check and its write. Held only for the check and the
/// rename: the bytes are written and flushed to a temporary file before.
static WRITES: Mutex<()> = Mutex::new(());

fn one_write_at_a_time() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------- LocalFs

#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
    /// [`Storage::set_locked_folders`].
    locked: std::sync::Arc<RwLock<Vec<String>>>,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io_err(root.display().to_string()))?;
        let root = root.canonicalize().map_err(io_err(root.display().to_string()))?;
        Ok(Self { root, locked: std::sync::Arc::default() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn abs(&self, p: &NotePath) -> Result<PathBuf> {
        if p.as_str().split('/').any(|s| s == "..") || leaves_root(p.as_str()) {
            return Err(VaultError::InvalidName(p.to_string()));
        }
        Ok(self.root.join(p.as_str()))
    }

    /// Every file under `dir`. The root must be readable; a folder below it
    /// that disappears between being listed and being read (moved or
    /// deleted meanwhile) is left out, not an error for the whole listing.
    fn walk(&self, dir: &Path, out: &mut Vec<NotePath>) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if dir != self.root && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(dir.display().to_string())(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if is_hidden(&rel) {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(rel)(e)),
            };
            if ft.is_dir() {
                self.walk(&path, out)?;
            } else if ft.is_file() {
                out.push(NotePath::new(rel));
            }
        }
        Ok(())
    }

    /// The bytes of `path` in a flushed temporary file beside it, ready to
    /// be put in place with [`Self::place`]. Done without
    /// [`one_write_at_a_time`] held, so a slow disk's flush doesn't hold
    /// up every other write.
    fn prepare(&self, path: &NotePath, contents: &[u8]) -> Result<(PathBuf, tempfile::NamedTempFile)> {
        let abs = self.abs(path)?;
        self.no_plain_into_locked(path)?;
        let dir = abs.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir).map_err(io_err(path.to_string()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io_err(path.to_string()))?;
        tmp.write_all(contents).map_err(io_err(path.to_string()))?;
        tmp.as_file().sync_data().map_err(io_err(path.to_string()))?;
        // Temp files are created 0600; keep the original file's mode, or use a
        // normal 0644 for new files, so saving never changes who can read a note.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&abs).map_or(0o644, |m| m.permissions().mode());
            let _ = tmp.as_file().set_permissions(std::fs::Permissions::from_mode(mode));
        }
        Ok((abs, tmp))
    }

    /// Put a [`Self::prepare`]d file in place (call with
    /// [`one_write_at_a_time`] held). The locked-folder check runs again
    /// here: a key file may have arrived since.
    fn place(&self, path: &NotePath, abs: &Path, tmp: tempfile::NamedTempFile) -> Result<()> {
        self.no_plain_into_locked(path)?;
        replace_file(tmp, abs).map_err(io_err(path.to_string()))
    }

    /// Refuse a plain file in a locked folder (0.11): whoever writes, the
    /// bytes of a note there are only ever stored encrypted.
    fn no_plain_into_locked(&self, path: &NotePath) -> Result<()> {
        let known = self.locked.read().unwrap_or_else(PoisonError::into_inner);
        match crate::lock::refuses_plain(path.as_str(), |p| self.root.join(p).exists(), &known) {
            Some(f) => Err(VaultError::Locked(f.path)),
            None => Ok(()),
        }
    }

    fn move_to_trash(&self, path: &NotePath) -> Result<()> {
        self.move_to_trash_stamped(path, &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string())
    }

    /// Move `path` to `.trash/<stamp> <file name>`, or, when that name is
    /// taken (the same name trashed in the same second, from another
    /// folder or another process), `<stamp> <name> (2).<ext>` and so on.
    /// The name is claimed by creating it new first, so nothing already in
    /// the trash is ever replaced.
    fn move_to_trash_stamped(&self, path: &NotePath, stamp: &str) -> Result<()> {
        let src = self.abs(path)?;
        let dir = self.root.join(".trash");
        std::fs::create_dir_all(&dir).map_err(io_err(".trash"))?;
        let mut n = 1;
        let dest = loop {
            let dest = dir.join(trash_name(stamp, path.file_name(), n));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&dest) {
                Ok(_) => break dest,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => n += 1,
                Err(e) => return Err(io_err(".trash")(e)),
            }
        };
        // Over the empty file just made, which only this call knows of.
        retry_busy(|| std::fs::rename(&src, &dest), REPLACE_PATIENCE, is_busy).map_err(|e| {
            let _ = std::fs::remove_file(&dest);
            io_err(path.to_string())(e)
        })
    }

    /// Whether `to` names the same file as `from` only in another case, on
    /// a disk that ignores case (macOS, Windows): `to` "exists", but no
    /// entry of its folder has exactly its name.
    fn case_only_rename(from: &Path, to: &Path) -> bool {
        let (Some(fd), Some(td), Some(fname), Some(tname)) = (from.parent(), to.parent(), from.file_name(), to.file_name()) else {
            return false;
        };
        let (fname, tname) = (fname.to_string_lossy(), tname.to_string_lossy());
        if fd != td || fname == tname || fname.to_lowercase() != tname.to_lowercase() {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(td) else { return false };
        let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.iter().any(|n| *n == fname) && !names.iter().any(|n| *n == tname)
    }
}

/// The name of the `n`th file trashed as `name` at `stamp`.
fn trash_name(stamp: &str, name: &str, n: usize) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let suffix = if n <= 1 { String::new() } else { format!(" ({n})") };
    // A name has at most 255 bytes on most disks (255 UTF-16 units on
    // Windows, never fewer than the bytes): a long note name is shortened,
    // on a character boundary, to leave room for the date (0.13.2). The
    // extension stays; ` (n)` keeps two shortened names apart.
    let room = TRASH_NAME_MAX.saturating_sub(stamp.len() + 1 + suffix.len() + ext.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{stamp} {}{suffix}{ext}", &stem[..cut])
}

/// The longest name [`trash_name`] makes, in bytes.
const TRASH_NAME_MAX: usize = 255;

/// How long a replace keeps trying while another program has the file
/// open in a way that blocks it (Windows only).
const REPLACE_PATIENCE: Duration = Duration::from_millis(400);

/// Whether `e` means another program has the file open right now (Windows:
/// access denied, sharing violation, lock violation): worth a short retry.
/// Never on other systems, where an open file doesn't block a rename.
fn is_busy(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Run `op` until it succeeds, fails for another reason than `busy`, or
/// `patience` is used up (waits double from 5 ms, at most 50 ms each).
fn retry_busy<T>(
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

/// Put the finished temporary file `tmp` in place of `dest`, all at once.
///
/// It is renamed with [`std::fs::rename`], which on Windows 10 1607 and
/// later replaces a file even while others have it open for reading (the
/// POSIX semantics of `FileRenameInfoEx`); `tempfile`'s own `persist` uses
/// the older `MoveFileExW`, which fails then. A replace that another
/// program blocks (Windows Defender, Search, a sync client, or one of
/// Quire's own readers) is retried for a few hundred milliseconds. The
/// temporary file is removed if the replace fails.
pub fn replace_file(tmp: tempfile::NamedTempFile, dest: &Path) -> std::io::Result<()> {
    // Closes the file; `keep` makes it an ordinary file (Windows marks
    // temporary files) that is no longer removed on drop: removed below on failure.
    let path = tmp.into_temp_path().keep().map_err(|e| e.error)?;
    retry_busy(|| std::fs::rename(&path, dest), REPLACE_PATIENCE, is_busy).inspect_err(|_| {
        let _ = std::fs::remove_file(&path);
    })
}

impl Storage for LocalFs {
    fn set_locked_folders(&self, folders: &[String]) {
        *self.locked.write().unwrap_or_else(PoisonError::into_inner) = folders.to_vec();
    }

    fn list(&self) -> Result<Vec<NotePath>> {
        let mut out = Vec::new();
        self.walk(&self.root, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        let abs = self.abs(path)?;
        // Windows: a log being appended to is locked for that moment.
        retry_busy(|| std::fs::read(&abs), REPLACE_PATIENCE, is_busy).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { VaultError::NotFound(path.to_string()) } else { io_err(path.to_string())(e) }
        })
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        let (abs, tmp) = self.prepare(path, contents)?;
        let _one = one_write_at_a_time();
        self.place(path, &abs, tmp)
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        // Written to a temporary file first; dropped (removed) if the check fails.
        let (abs, tmp) = self.prepare(path, bytes)?;
        let _one = one_write_at_a_time();
        if !still(read_if_exists(self, path)?.as_deref()) {
            return Ok(false);
        }
        self.place(path, &abs, tmp)?;
        Ok(true)
    }

    fn trash_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool) -> Result<bool> {
        let _one = one_write_at_a_time();
        if !still(read_if_exists(self, path)?.as_deref()) {
            return Ok(false);
        }
        self.move_to_trash(path)?;
        Ok(true)
    }

    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let (a, b) = (self.abs(from)?, self.abs(to)?);
        let _one = one_write_at_a_time();
        self.no_plain_into_locked(to)?;
        // `meeting.md` → `Meeting.md` on a disk that ignores case: the
        // target "exists" because it is this very file.
        if b.exists() && !Self::case_only_rename(&a, &b) {
            return Err(VaultError::Exists(to.to_string()));
        }
        if let Some(dir) = b.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(to.to_string()))?;
        }
        retry_busy(|| std::fs::rename(&a, &b), REPLACE_PATIENCE, is_busy).map_err(io_err(from.to_string()))
    }

    fn trash(&self, path: &NotePath) -> Result<()> {
        let _one = one_write_at_a_time();
        self.move_to_trash(path)
    }

    fn exists(&self, path: &NotePath) -> bool {
        self.abs(path).is_ok_and(|p| p.exists())
    }

    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        let abs = self.abs(path)?;
        std::fs::metadata(&abs).and_then(|m| m.modified()).map_err(io_err(path.to_string()))
    }

    fn absolute(&self, path: &NotePath) -> Option<PathBuf> {
        self.abs(path).ok()
    }

    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        let dir = self.abs(&NotePath::new(folder))?;
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(out) };
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                out.push(NotePath::join(folder, &e.file_name().to_string_lossy()));
            }
        }
        out.sort();
        Ok(out)
    }

    fn remove(&self, path: &NotePath) -> Result<()> {
        let abs = self.abs(path)?;
        let _one = one_write_at_a_time();
        std::fs::remove_file(&abs).map_err(io_err(path.to_string()))
    }

    fn append_line(&self, path: &NotePath, line: &str) -> Result<()> {
        let abs = self.abs(path)?;
        self.no_plain_into_locked(path)?;
        if let Some(dir) = abs.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(path.to_string()))?;
        }
        // Read access too: on Windows an append-only handle can't take a
        // file lock (LockFileEx needs read or write access, and append is
        // neither), so every append would fail there.
        let mut f = std::fs::OpenOptions::new().create(true).append(true).read(true).open(&abs).map_err(io_err(path.to_string()))?;
        f.lock().map_err(io_err(path.to_string()))?;
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        let written = f.write_all(buf.as_bytes()).map_err(io_err(path.to_string()));
        let _ = f.unlock();
        written
    }
}

// ---------------------------------------------------------------- SingleFile

/// One Markdown file that is not in a vault (0.12 "single-file mode").
///
/// `quire notes.md`, a file association and a dropped file open the file
/// without making its folder a vault: the folder is not scanned, and no
/// `.quire/` is made in it. Only this file is ever written (when you save).
/// The app's own data (history, the agent ledger, indexes) stays in memory
/// and goes when Quire closes. Other files in the folder can be read, so
/// `![[picture.png]]` next to the note still shows; none can be made.
#[derive(Debug)]
pub struct SingleFile {
    local: LocalFs,
    name: NotePath,
    /// App data under `.quire/`: kept in memory.
    side: MemFs,
}

impl SingleFile {
    /// `file` must exist and be a regular file.
    pub fn open(file: &Path) -> Result<Self> {
        let file = file.canonicalize().map_err(io_err(file.display().to_string()))?;
        if !file.is_file() {
            return Err(VaultError::NotFound(file.display().to_string()));
        }
        let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
            return Err(VaultError::InvalidName(file.display().to_string()));
        };
        Ok(Self { local: LocalFs::new(dir)?, name: NotePath::new(name.to_string_lossy()), side: MemFs::new() })
    }

    /// The note's path in this storage (its file name).
    pub fn note(&self) -> NotePath {
        self.name.clone()
    }

    /// The folder the file is in.
    pub fn folder(&self) -> &Path {
        self.local.root()
    }

    fn is_main(&self, p: &NotePath) -> bool {
        *p == self.name
    }

    fn is_app(p: &NotePath) -> bool {
        p.as_str().starts_with('.')
    }

    /// Read from the folder: the file, and other files that aren't app data.
    fn on_disk(&self, p: &NotePath) -> bool {
        self.is_main(p) || !Self::is_app(p)
    }

    fn refuse(p: &NotePath, what: &str) -> VaultError {
        VaultError::Io {
            path: p.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("this file is open on its own, outside a vault, so Quire can't {what}; open its folder as a vault to do that"),
            ),
        }
    }
}

impl Storage for SingleFile {
    fn list(&self) -> Result<Vec<NotePath>> {
        Ok(if self.local.exists(&self.name) { vec![self.name.clone()] } else { Vec::new() })
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        if self.is_main(path) {
            return self.local.read_bytes(path);
        }
        match self.side.read_bytes(path) {
            Err(VaultError::NotFound(_)) if self.on_disk(path) => self.local.read_bytes(path),
            r => r,
        }
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        if self.is_main(path) {
            self.local.write_bytes_atomic(path, contents)
        } else if Self::is_app(path) {
            self.side.write_bytes_atomic(path, contents)
        } else {
            Err(Self::refuse(path, "make other files"))
        }
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        if self.is_main(path) {
            self.local.write_bytes_if(path, still, bytes)
        } else if Self::is_app(path) {
            self.side.write_bytes_if(path, still, bytes)
        } else {
            Err(Self::refuse(path, "make other files"))
        }
    }

    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        if Self::is_app(from) && Self::is_app(to) { self.side.rename(from, to) } else { Err(Self::refuse(from, "rename or move it")) }
    }

    fn trash(&self, path: &NotePath) -> Result<()> {
        if Self::is_app(path) { self.side.trash(path) } else { Err(Self::refuse(path, "delete it")) }
    }

    fn exists(&self, path: &NotePath) -> bool {
        self.side.exists(path) || (self.on_disk(path) && self.local.exists(path))
    }

    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        if self.on_disk(path) { self.local.modified(path) } else { self.side.modified(path) }
    }

    fn absolute(&self, path: &NotePath) -> Option<PathBuf> {
        self.on_disk(path).then(|| self.local.absolute(path)).flatten()
    }

    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        if folder.starts_with('.') { self.side.list_under(folder) } else { Ok(Vec::new()) }
    }

    fn remove(&self, path: &NotePath) -> Result<()> {
        if Self::is_app(path) { self.side.remove(path) } else { Err(Self::refuse(path, "delete files")) }
    }

    fn append_line(&self, path: &NotePath, line: &str) -> Result<()> {
        if Self::is_app(path) { self.side.append_line(path, line) } else { Err(Self::refuse(path, "write to other files")) }
    }
}

// ---------------------------------------------------------------- MemFs

/// In-memory storage for tests, demos and the scratchpad.
#[derive(Debug, Default)]
pub struct MemFs {
    files: RwLock<BTreeMap<NotePath, (Vec<u8>, SystemTime)>>,
    trashed: RwLock<Vec<NotePath>>,
    locked: RwLock<Vec<String>>,
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let fs = Self::new();
        for (p, c) in files {
            fs.write_atomic(&NotePath::new(p), c).expect("memfs write");
        }
        fs
    }

    /// Every file, hidden ones (`.quire/`) included.
    pub fn list_all(&self) -> Vec<NotePath> {
        self.files.read().expect("lock").keys().cloned().collect()
    }

    pub fn trashed(&self) -> Vec<NotePath> {
        self.trashed.read().expect("lock").clone()
    }
}

/// [`LocalFs`]'s refusal of plain files in a locked folder, for [`MemFs`].
fn mem_no_plain_into_locked(files: &BTreeMap<NotePath, (Vec<u8>, SystemTime)>, known: &[String], path: &NotePath) -> Result<()> {
    match crate::lock::refuses_plain(path.as_str(), |p| files.contains_key(&NotePath::new(p)), known) {
        Some(f) => Err(VaultError::Locked(f.path)),
        None => Ok(()),
    }
}

impl MemFs {
    fn known(&self) -> Vec<String> {
        self.locked.read().expect("lock").clone()
    }
}

impl Storage for MemFs {
    fn set_locked_folders(&self, folders: &[String]) {
        *self.locked.write().expect("lock") = folders.to_vec();
    }

    fn list(&self) -> Result<Vec<NotePath>> {
        Ok(self.files.read().expect("lock").keys().filter(|p| !is_hidden(p.as_str())).cloned().collect())
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.files.read().expect("lock").get(path).map(|(c, _)| c.clone()).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        let mut files = self.files.write().expect("lock");
        mem_no_plain_into_locked(&files, &self.known(), path)?;
        files.insert(path.clone(), (contents.to_vec(), SystemTime::now()));
        Ok(())
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        let mut files = self.files.write().expect("lock");
        mem_no_plain_into_locked(&files, &self.known(), path)?;
        if !still(files.get(path).map(|(c, _)| c.as_slice())) {
            return Ok(false);
        }
        files.insert(path.clone(), (bytes.to_vec(), SystemTime::now()));
        Ok(true)
    }

    fn trash_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool) -> Result<bool> {
        let mut files = self.files.write().expect("lock");
        if !still(files.get(path).map(|(c, _)| c.as_slice())) {
            return Ok(false);
        }
        files.remove(path).ok_or_else(|| VaultError::NotFound(path.to_string()))?;
        self.trashed.write().expect("lock").push(path.clone());
        Ok(true)
    }

    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let mut files = self.files.write().expect("lock");
        mem_no_plain_into_locked(&files, &self.known(), to)?;
        if files.contains_key(to) {
            return Err(VaultError::Exists(to.to_string()));
        }
        let v = files.remove(from).ok_or_else(|| VaultError::NotFound(from.to_string()))?;
        files.insert(to.clone(), v);
        Ok(())
    }

    fn trash(&self, path: &NotePath) -> Result<()> {
        self.files.write().expect("lock").remove(path).ok_or_else(|| VaultError::NotFound(path.to_string()))?;
        self.trashed.write().expect("lock").push(path.clone());
        Ok(())
    }

    fn exists(&self, path: &NotePath) -> bool {
        self.files.read().expect("lock").contains_key(path)
    }

    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        self.files.read().expect("lock").get(path).map(|(_, t)| *t).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }

    fn absolute(&self, _path: &NotePath) -> Option<PathBuf> {
        None
    }

    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        Ok(self
            .files
            .read()
            .expect("lock")
            .keys()
            .filter(|p| p.as_str().strip_prefix(&prefix).is_some_and(|rest| !rest.contains('/')))
            .cloned()
            .collect())
    }

    fn remove(&self, path: &NotePath) -> Result<()> {
        self.files.write().expect("lock").remove(path).map(|_| ()).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fs_refuses_drive_and_rooted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        for p in ["C:/Users/me/Startup/x.cmd", "c:x.md", "C:\\x.md"] {
            assert!(fs.absolute(&NotePath::new(p)).is_none(), "{p}");
            assert!(fs.write_atomic(&NotePath::new(p), "x").is_err(), "{p}");
        }
        // A UNC or rooted path is read as a path inside the vault.
        for p in ["\\\\server\\share\\x.md", "/etc/x.md"] {
            assert!(fs.absolute(&NotePath::new(p)).unwrap().starts_with(fs.root()), "{p}");
        }
        assert!(leaves_root("/etc/passwd") && leaves_root("\\x"));
        assert!(!leaves_root("Meeting: notes.md") && !leaves_root("a/C:/x.md"));
        assert!(fs.absolute(&NotePath::new("Meeting: notes.md")).is_some());
    }

    #[test]
    fn local_fs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let p = NotePath::new("a/b/Note.md");
        fs.write_atomic(&p, "hello").unwrap();
        fs.write_atomic(&NotePath::new(".git/config"), "x").unwrap();
        assert_eq!(fs.read(&p).unwrap(), "hello");
        assert_eq!(fs.list().unwrap(), vec![p.clone()]);
        let q = NotePath::new("Renamed.md");
        fs.rename(&p, &q).unwrap();
        assert!(!fs.exists(&p) && fs.exists(&q));
        fs.trash(&q).unwrap();
        assert!(fs.list().unwrap().is_empty());
        assert!(fs.read(&NotePath::new("../escape.md")).is_err());
        let log = NotePath::new(".quire/agent/log.jsonl");
        fs.append_line(&log, "{\"v\":1}").unwrap();
        fs.append_line(&log, "{\"v\":2}").unwrap();
        assert_eq!(fs.read(&log).unwrap(), "{\"v\":1}\n{\"v\":2}\n");
        let mem = MemFs::new();
        mem.append_line(&log, "a").unwrap();
        mem.append_line(&log, "b").unwrap();
        assert_eq!(mem.read(&log).unwrap(), "a\nb\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = NotePath::new("perm.md");
            fs.write_atomic(&p, "a").unwrap();
            let abs = fs.absolute(&p).unwrap();
            assert_eq!(std::fs::metadata(&abs).unwrap().permissions().mode() & 0o777, 0o644);
            std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o640)).unwrap();
            fs.write_atomic(&p, "b").unwrap();
            assert_eq!(std::fs::metadata(&abs).unwrap().permissions().mode() & 0o777, 0o640);
        }
    }

    #[test]
    fn writes_that_check_first_never_overwrite_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalFs::new(dir.path()).unwrap();
        let mem = MemFs::new();
        for fs in [&local as &dyn Storage, &mem] {
            let p = NotePath::new("Plan.md");
            let is = |want: &'static str| move |now: Option<&[u8]>| now == Some(want.as_bytes());
            // Must not exist yet.
            assert!(fs.write_bytes_if(&p, &|now| now.is_none(), b"one").unwrap());
            assert!(!fs.write_bytes_if(&p, &|now| now.is_none(), b"two").unwrap());
            assert!(fs.write_bytes_if(&p, &is("one"), b"two").unwrap());
            assert!(!fs.write_bytes_if(&p, &is("one"), b"three").unwrap());
            assert_eq!(fs.read(&p).unwrap(), "two");
            assert!(!fs.trash_if(&p, &is("one")).unwrap());
            assert!(fs.trash_if(&p, &is("two")).unwrap());
            assert!(!fs.exists(&p));
        }
    }

    #[test]
    fn a_check_and_its_write_are_one_step_against_other_writes() {
        // Many threads add one line each through a read-check-write loop:
        // with the check and the write as one step, no line is lost.
        let dir = tempfile::tempdir().unwrap();
        let fs = std::sync::Arc::new(LocalFs::new(dir.path()).unwrap());
        let p = NotePath::new("Log.md");
        fs.write_atomic(&p, "").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (fs, p) = (std::sync::Arc::clone(&fs), p.clone());
                std::thread::spawn(move || {
                    for j in 0..10 {
                        loop {
                            let now = fs.read_bytes(&p).unwrap();
                            let mut next = now.clone();
                            next.extend_from_slice(format!("{i}-{j}\n").as_bytes());
                            if fs.write_bytes_if(&p, &|cur| cur == Some(now.as_slice()), &next).unwrap() {
                                break;
                            }
                        }
                        // Plain writes elsewhere go on meanwhile.
                        fs.write_atomic(&NotePath::new(format!("other {i}.md").as_str()), "x").unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(fs.read(&p).unwrap().lines().count(), 80);
    }

    /// Every file under `dir` (hidden ones too), with its text.
    fn trash_of(root: &Path) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = std::fs::read_dir(root.join(".trash"))
            .unwrap()
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap()))
            .collect();
        v.sort();
        v
    }

    /// Reliability scan item 2: two files of the same name trashed in the
    /// same second both stay in the trash.
    #[test]
    fn the_trash_never_replaces_a_file_trashed_in_the_same_second() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        fs.write_atomic(&NotePath::new("A/notes.md"), "from A").unwrap();
        fs.write_atomic(&NotePath::new("B/notes.md"), "from B").unwrap();
        fs.move_to_trash_stamped(&NotePath::new("A/notes.md"), "20261116-120000").unwrap();
        fs.move_to_trash_stamped(&NotePath::new("B/notes.md"), "20261116-120000").unwrap();
        assert_eq!(
            trash_of(fs.root()),
            [
                ("20261116-120000 notes (2).md".to_string(), "from B".to_string()),
                ("20261116-120000 notes.md".to_string(), "from A".to_string())
            ]
        );
        // Many in a loop, through the real clock: none lost.
        for i in 0..40 {
            let p = NotePath::new(format!("F{i}/README").as_str());
            fs.write_atomic(&p, &format!("readme {i}")).unwrap();
            fs.trash(&p).unwrap();
        }
        let all = trash_of(fs.root());
        assert_eq!(all.len(), 42, "{all:?}");
        for i in 0..40 {
            assert!(all.iter().any(|(_, t)| *t == format!("readme {i}")), "readme {i}: {all:?}");
        }
        // Nothing to trash: no empty file is left behind.
        assert!(fs.trash(&NotePath::new("missing.md")).is_err());
        assert_eq!(trash_of(fs.root()).len(), 42);
        assert_eq!(trash_name("s", "README", 3), "s README (3)");
        assert_eq!(trash_name("s", ".env", 2), "s .env (2)");
    }

    /// Review D-L4 (0.13.2): a note whose name leaves no room for the date
    /// still goes to the trash, under a shortened name that keeps its
    /// extension, and two such notes don't take each other's place.
    #[test]
    fn a_note_with_a_very_long_name_still_goes_to_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let (a, b) = (format!("{}a.md", "n".repeat(240)), format!("{}b.md", "n".repeat(240)));
        for (p, t) in [(&a, "first"), (&b, "second")] {
            fs.write_atomic(&NotePath::new(p.as_str()), t).unwrap();
            fs.trash(&NotePath::new(p.as_str())).unwrap();
        }
        let all = trash_of(fs.root());
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all.iter().all(|(n, _)| n.len() <= 255 && Path::new(n).extension().is_some_and(|e| e == "md")), "{all:?}");
        assert!(all.iter().any(|(_, t)| t == "first") && all.iter().any(|(_, t)| t == "second"));
        let wide = trash_name("20261116-120000", &format!("{}.md", "語".repeat(90)), 7);
        assert!(wide.len() <= 255 && wide.ends_with(" (7).md"), "{wide}");
    }

    #[test]
    fn the_trash_is_safe_from_several_writers_at_once() {
        let dir = tempfile::tempdir().unwrap();
        // Two storages on one folder stand for two processes.
        let (a, b) = (LocalFs::new(dir.path()).unwrap(), LocalFs::new(dir.path()).unwrap());
        for i in 0..20 {
            a.write_atomic(&NotePath::new(format!("a{i}/x.md").as_str()), &format!("a{i}")).unwrap();
            b.write_atomic(&NotePath::new(format!("b{i}/x.md").as_str()), &format!("b{i}")).unwrap();
        }
        let t = {
            let a = a.clone();
            std::thread::spawn(move || {
                for i in 0..20 {
                    a.move_to_trash_stamped(&NotePath::new(format!("a{i}/x.md").as_str()), "same").unwrap();
                }
            })
        };
        for i in 0..20 {
            b.move_to_trash_stamped(&NotePath::new(format!("b{i}/x.md").as_str()), "same").unwrap();
        }
        t.join().unwrap();
        let mut texts: Vec<String> = trash_of(a.root()).into_iter().map(|(_, t)| t).collect();
        texts.sort();
        assert_eq!(texts.len(), 40);
        texts.dedup();
        assert_eq!(texts.len(), 40);
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
    }

    #[test]
    fn a_failed_replace_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // The destination is a folder: the rename fails.
        std::fs::create_dir_all(dir.path().join("dest/inside")).unwrap();
        let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        tmp.write_all(b"x").unwrap();
        assert!(replace_file(tmp, &dir.path().join("dest")).is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["dest"], "{names:?}");
        // And a check that fails drops its prepared file.
        let fs = LocalFs::new(dir.path().join("v")).unwrap();
        let p = NotePath::new("A.md");
        fs.write_atomic(&p, "one").unwrap();
        assert!(!fs.write_bytes_if(&p, &|now| now.is_none(), b"two").unwrap());
        let names: Vec<_> = std::fs::read_dir(fs.root()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["A.md"], "{names:?}");
    }

    /// Reliability scan item 4: on Windows a file open for reading (by an
    /// indexer, an antivirus, or Quire's own background readers) can still
    /// be replaced, and a file locked for a moment is waited for.
    #[cfg(windows)]
    #[test]
    fn replacing_a_file_that_is_open_for_reading() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let p = NotePath::new("Open.md");
        fs.write_atomic(&p, "one").unwrap();
        let abs = fs.absolute(&p).unwrap();
        let held = std::fs::File::open(&abs).unwrap();
        fs.write_atomic(&p, "two").unwrap();
        drop(held);
        assert_eq!(fs.read(&p).unwrap(), "two");
        // Opened without delete sharing (as some programs do), for 100 ms: waited for.
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&abs).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        fs.write_atomic(&p, "three").unwrap();
        t.join().unwrap();
        assert_eq!(fs.read(&p).unwrap(), "three");
        // Held for good: the write fails in bounded time and leaves no temporary file.
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&abs).unwrap();
        let start = Instant::now();
        assert!(fs.write_atomic(&p, "four").is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        assert_eq!(fs.read(&p).unwrap(), "three");
        let names: Vec<_> = std::fs::read_dir(fs.root()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["Open.md"], "{names:?}");
    }

    /// Reliability scan item 12: on a disk that ignores case, a note can be
    /// renamed to the same name in another case.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn a_case_only_rename_works_where_case_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let (a, b) = (NotePath::new("meeting.md"), NotePath::new("Meeting.md"));
        fs.write_atomic(&a, "x").unwrap();
        fs.write_atomic(&NotePath::new("other.md"), "y").unwrap();
        fs.rename(&a, &b).unwrap();
        assert_eq!(fs.list().unwrap(), [b.clone(), NotePath::new("other.md")]);
        // Another file of that name in another case is still refused.
        assert!(matches!(fs.rename(&NotePath::new("other.md"), &NotePath::new("MEETING.md")), Err(VaultError::Exists(_))));
    }

    #[test]
    fn case_only_renames_are_told_from_name_clashes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("meeting.md"), "x").unwrap();
        let (a, b) = (dir.path().join("meeting.md"), dir.path().join("Meeting.md"));
        // On a case-sensitive disk `Meeting.md` doesn't exist, so this is a rename like any other.
        assert_eq!(
            LocalFs::case_only_rename(&a, &b),
            !std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| e.file_name() == "Meeting.md")
        );
        assert!(!LocalFs::case_only_rename(&a, &dir.path().join("Other.md")));
        assert!(!LocalFs::case_only_rename(&a, &a));
    }

    /// For the sync team: a folder that goes away while the vault is being
    /// listed is left out; an unreadable vault root is still an error.
    #[test]
    fn listing_skips_a_folder_that_vanishes_but_not_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path().join("v")).unwrap();
        fs.write_atomic(&NotePath::new("Keep/a.md"), "a").unwrap();
        let mut out = Vec::new();
        fs.walk(&fs.root().join("Gone"), &mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(fs.list().unwrap(), [NotePath::new("Keep/a.md")]);
        std::fs::remove_dir_all(fs.root()).unwrap();
        assert!(fs.list().is_err(), "the root itself must be readable");
    }
}
