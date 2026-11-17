// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![expect(
    clippy::disallowed_methods,
    reason = "Storage is the one place that renames notes, moves them to the trash and puts a written file in place"
)]
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use quire_base::fs::{Prepared, REPLACE_PATIENCE, is_busy, is_read_busy, retry_busy};
use quire_base::path::Rule;

use crate::{NotePath, Result, VaultError, io_err};

/// Folders never shown as notes (VCS, app metadata, trash).
pub(crate) fn is_hidden(path: &str) -> bool {
    path.split('/').any(|seg| seg.starts_with('.') || seg == "node_modules")
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
    /// A notice about the listing, once: folders inside the vault that can
    /// never be opened (on Windows a folder whose name ends in a space or a
    /// dot; an automount point whose target is missing) and are left out.
    fn take_listing_notice(&self) -> Option<String> {
        None
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
    /// Folders left out of listings because they can never be opened:
    /// (told already, the notice not yet taken).
    unopenable: std::sync::Arc<Mutex<(std::collections::HashSet<String>, Option<String>)>>,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io_err(root.display().to_string()))?;
        let root = root.canonicalize().map_err(io_err(root.display().to_string()))?;
        Ok(Self { root, locked: std::sync::Arc::default(), unopenable: std::sync::Arc::default() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn abs(&self, p: &NotePath) -> Result<PathBuf> {
        // The last guard before the root is joined: no `..`, nothing absolute.
        if !Rule::InsideRoot.accepts(p.as_str()) {
            return Err(VaultError::InvalidName(p.to_string()));
        }
        Ok(self.root.join(p.as_str()))
    }

    /// Every file under `dir`. A folder below the root that was listed but
    /// then can't be found is added to `vanished` (vault-relative): renamed
    /// or deleted meanwhile, the listing is then not a true picture of the
    /// vault (a renamed folder would look like deleted notes), and
    /// [`Storage::list`] lists again. The root itself must be readable.
    fn walk(&self, dir: &Path, out: &mut Vec<NotePath>, vanished: &mut Vec<String>) -> Result<()> {
        let rel_of = |p: &Path| p.strip_prefix(&self.root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if dir != self.root && e.kind() == std::io::ErrorKind::NotFound => {
                vanished.push(rel_of(dir));
                return Ok(());
            }
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
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    vanished.push(rel.clone());
                    continue;
                }
                Err(e) => return Err(io_err(rel)(e)),
            };
            #[expect(
                clippy::filetype_is_file,
                reason = "a symlinked note or folder is skipped, as the watcher and sync skip it: it could point outside the vault"
            )]
            if ft.is_dir() {
                self.walk(&path, out, vanished)?;
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
    fn prepare(&self, path: &NotePath, contents: &[u8]) -> Result<(PathBuf, Prepared)> {
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
        // `tempfile`'s `.tmp…` name: history skips those as writes in flight.
        Ok((abs, Prepared::adopt(tmp).map_err(io_err(path.to_string()))?))
    }

    /// Put a [`Self::prepare`]d file in place, if `still` (when given)
    /// accepts the file as it is now: the check and the rename are one step
    /// under [`one_write_at_a_time`]. The locked-folder check runs again
    /// there (a key file may have arrived since). Returns whether it wrote.
    fn place(&self, path: &NotePath, abs: &Path, mut pending: Prepared, still: Option<&Still<'_>>) -> Result<bool> {
        retry_unlocked(|| {
            if let Some(still) = still {
                let now = match std::fs::read(abs) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) if is_read_busy(&e) => return Ok(Step::Busy(io_err(path.to_string())(e))),
                    Err(e) => return Err(io_err(path.to_string())(e)),
                };
                if !still(now.as_deref()) {
                    return Ok(Step::Done(false));
                }
            }
            self.no_plain_into_locked(path)?;
            match pending.try_place(abs) {
                Ok(()) => Ok(Step::Done(true)),
                Err(e) if is_busy(&e) => Ok(Step::Busy(io_err(path.to_string())(e))),
                Err(e) => Err(io_err(path.to_string())(e)),
            }
        })
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

    /// Move `path` to the trash (call with [`one_write_at_a_time`] held):
    /// one try, a busy file reported as [`Step::Busy`].
    fn move_to_trash(&self, path: &NotePath) -> Result<Step<()>> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        match self.move_to_trash_stamped(path, &stamp) {
            Ok(()) => Ok(Step::Done(())),
            Err(e) if is_busy(&e) => Ok(Step::Busy(io_err(path.to_string())(e))),
            Err(e) => Err(io_err(path.to_string())(e)),
        }
    }

    /// Move `path` to `.trash/<stamp> <file name>`, or, when that name is
    /// taken (the same name trashed in the same second, from another
    /// folder or another process), `<stamp> <name> (2).<ext>` and so on.
    /// The name is claimed by creating it new first, so nothing already in
    /// the trash is ever replaced.
    fn move_to_trash_stamped(&self, path: &NotePath, stamp: &str) -> std::io::Result<()> {
        let src = self.abs(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let dir = self.root.join(".trash");
        std::fs::create_dir_all(&dir)?;
        let mut n = 1;
        let dest = loop {
            let dest = dir.join(trash_name(stamp, path.file_name(), n));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&dest) {
                Ok(_) => break dest,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => n += 1,
                Err(e) => return Err(e),
            }
        };
        // Over the empty file just made, which only this call knows of.
        let r = std::fs::rename(&src, &dest);
        if r.is_err() {
            let _ = std::fs::remove_file(&dest);
        }
        r
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

/// How many times [`LocalFs`] lists the vault when a folder vanished
/// during a listing, before it gives up with an error.
const LIST_TRIES: usize = 3;

/// The listing `walk` makes, again while a folder vanished during it, up
/// to [`LIST_TRIES`] times.
///
/// Two kinds of "vanished" are told apart. A folder renamed or deleted
/// during the walk gives a different result the next time (the new name
/// appears, or the old one is no longer listed): listed again, and if the
/// vault keeps changing, an error naming the folder, never a short
/// listing. A folder that is listed but can never be opened gives the very
/// same result on the next walk, with nothing else changed: that listing
/// is used without it (as before 0.13.2), and `unopenable` gets each such
/// folder once, to be told.
fn stable_listing(
    mut walk: impl FnMut(&mut Vec<String>) -> Result<Vec<NotePath>>,
    mut unopenable: impl FnMut(&[String]),
) -> Result<Vec<NotePath>> {
    let mut last: Option<(Vec<NotePath>, Vec<String>)> = None;
    for _ in 0..LIST_TRIES {
        let mut vanished = Vec::new();
        let out = walk(&mut vanished)?;
        if vanished.is_empty() {
            return Ok(out);
        }
        vanished.sort();
        if last.as_ref().is_some_and(|(o, v)| *o == out && *v == vanished) {
            unopenable(&vanished);
            return Ok(out);
        }
        last = Some((out, vanished));
    }
    let names = last.map(|(_, v)| v.join(", ")).unwrap_or_default();
    Err(VaultError::Io {
        path: names.clone(),
        source: std::io::Error::other(format!(
            "folders kept being moved or deleted while the vault was listed ({names}); try again in a moment"
        )),
    })
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

/// The check of a compare-and-swap ([`Storage::write_bytes_if`]).
type Still<'a> = dyn Fn(Option<&[u8]>) -> bool + 'a;

/// One try of a write under [`one_write_at_a_time`]: done, or blocked by
/// another program for now (with the error to give if it stays so).
enum Step<T> {
    Done(T),
    Busy(VaultError),
}

/// Run `op` with [`one_write_at_a_time`] held, again after a short wait
/// while it reports [`Step::Busy`], within [`REPLACE_PATIENCE`] in all.
/// The lock is not held while waiting, so other writes go on meanwhile;
/// `op` makes its checks again on each try.
fn retry_unlocked<T>(mut op: impl FnMut() -> Result<Step<T>>) -> Result<T> {
    let start = Instant::now();
    let mut wait = Duration::from_millis(5);
    loop {
        let step = {
            let _one = one_write_at_a_time();
            op()?
        };
        match step {
            Step::Done(t) => return Ok(t),
            Step::Busy(_) if start.elapsed() + wait <= REPLACE_PATIENCE => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(50));
            }
            Step::Busy(e) => return Err(e),
        }
    }
}

impl Storage for LocalFs {
    fn set_locked_folders(&self, folders: &[String]) {
        *self.locked.write().unwrap_or_else(PoisonError::into_inner) = folders.to_vec();
    }

    /// Lists again when a folder vanished while it was being listed (a
    /// folder renamed meanwhile), up to three times, then fails:
    /// a listing missing a folder must not be taken for deleted notes.
    fn list(&self) -> Result<Vec<NotePath>> {
        stable_listing(
            |vanished| {
                let mut out = Vec::new();
                self.walk(&self.root, &mut out, vanished)?;
                out.sort();
                Ok(out)
            },
            |folders| {
                let mut st = self.unopenable.lock().unwrap_or_else(PoisonError::into_inner);
                let new: Vec<&String> = folders.iter().filter(|f| !st.0.contains(*f)).collect();
                if !new.is_empty() {
                    let names = new.iter().map(|f| format!("{f}/")).collect::<Vec<_>>().join(", ");
                    st.1 = Some(format!(
                        "Quire can't open {names} in this vault (a name this system can't use, such as one ending in a space or a dot, \
                         or a drive that isn't there), so the notes in it are left out. Rename it with another program, or move it out of the vault."
                    ));
                    st.0.extend(new.into_iter().cloned());
                }
            },
        )
    }

    fn take_listing_notice(&self) -> Option<String> {
        self.unopenable.lock().unwrap_or_else(PoisonError::into_inner).1.take()
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        let abs = self.abs(path)?;
        // Windows: a log being appended to is locked for that moment.
        retry_busy(|| std::fs::read(&abs), REPLACE_PATIENCE, is_read_busy).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { VaultError::NotFound(path.to_string()) } else { io_err(path.to_string())(e) }
        })
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        let (abs, pending) = self.prepare(path, contents)?;
        self.place(path, &abs, pending, None).map(|_| ())
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        // Written to a temporary file first; dropped (removed) if the check fails.
        let (abs, pending) = self.prepare(path, bytes)?;
        self.place(path, &abs, pending, Some(still))
    }

    fn trash_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool) -> Result<bool> {
        let abs = self.abs(path)?;
        retry_unlocked(|| {
            let now = match std::fs::read(&abs) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) if is_read_busy(&e) => return Ok(Step::Busy(io_err(path.to_string())(e))),
                Err(e) => return Err(io_err(path.to_string())(e)),
            };
            if !still(now.as_deref()) {
                return Ok(Step::Done(false));
            }
            Ok(match self.move_to_trash(path)? {
                Step::Done(()) => Step::Done(true),
                Step::Busy(e) => Step::Busy(e),
            })
        })
    }

    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let (a, b) = (self.abs(from)?, self.abs(to)?);
        retry_unlocked(|| {
            self.no_plain_into_locked(to)?;
            // `meeting.md` → `Meeting.md` on a disk that ignores case: the
            // target "exists" because it is this very file.
            if b.exists() && !Self::case_only_rename(&a, &b) {
                return Err(VaultError::Exists(to.to_string()));
            }
            if let Some(dir) = b.parent() {
                std::fs::create_dir_all(dir).map_err(io_err(to.to_string()))?;
            }
            match std::fs::rename(&a, &b) {
                Ok(()) => Ok(Step::Done(())),
                Err(e) if is_busy(&e) => Ok(Step::Busy(io_err(from.to_string())(e))),
                Err(e) => Err(io_err(from.to_string())(e)),
            }
        })
    }

    fn trash(&self, path: &NotePath) -> Result<()> {
        self.abs(path)?;
        retry_unlocked(|| self.move_to_trash(path))
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
            #[expect(clippy::filetype_is_file, reason = "a symlinked note is skipped, as the vault listing skips it")]
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
        assert!(quire_base::path::leaves_root("/etc/passwd") && quire_base::path::leaves_root("\\x"));
        assert!(!quire_base::path::leaves_root("Meeting: notes.md") && !quire_base::path::leaves_root("a/C:/x.md"));
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

    /// Review D-L5: a write blocked by another program waits without the
    /// process-wide write lock (other writes go on meanwhile), and gives up
    /// within one bound for the whole write.
    #[test]
    fn a_blocked_write_waits_without_the_write_lock_and_within_one_bound() {
        let free_while_waiting = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let t = {
            let (free, stop) = (std::sync::Arc::clone(&free_while_waiting), std::sync::Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    if WRITES.try_lock().is_ok() {
                        free.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let start = Instant::now();
        let r: Result<()> = retry_unlocked(|| {
            // Held here: the lock is taken for each try.
            assert!(WRITES.try_lock().is_err());
            Ok(Step::Busy(VaultError::NotFound("blocked".into())))
        });
        let took = start.elapsed();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        t.join().unwrap();
        assert!(r.is_err());
        // One bound, not one per try: well under twice the patience plus the
        // last sleep. Shared CI runners oversleep (590 ms seen on Windows for
        // a 300 ms patience), so the margin is wide.
        assert!(took >= Duration::from_millis(200) && took < REPLACE_PATIENCE * 3, "{took:?}");
        assert!(free_while_waiting.load(std::sync::atomic::Ordering::SeqCst) > 5, "the lock was free while waiting");
        // Reads retry only on sharing or lock violations, never on access denied.
        let denied = std::io::Error::from_raw_os_error(5);
        assert!(!is_read_busy(&denied));
        assert_eq!(is_read_busy(&std::io::Error::from_raw_os_error(32)), cfg!(windows));
    }

    #[test]
    fn a_failed_replace_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // The destination is a folder: the rename fails.
        std::fs::create_dir_all(dir.path().join("dest/inside")).unwrap();
        let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        tmp.write_all(b"x").unwrap();
        assert!(crate::replace_file(tmp, &dir.path().join("dest")).is_err());
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

    /// Review D-L1: a folder that goes away while the vault is being listed
    /// (renamed meanwhile) makes the listing untrustworthy: it is listed
    /// again, and if that keeps happening the listing fails rather than
    /// leaving the folder's notes out (sync would send them as deleted).
    /// Final review F-I1: a folder that is listed but can never be opened
    /// (the same result on every walk) is left out with a notice, once.
    /// An unreadable vault root is an error too.
    #[test]
    fn a_listing_that_lost_a_folder_is_retried_then_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path().join("v")).unwrap();
        fs.write_atomic(&NotePath::new("Keep/a.md"), "a").unwrap();
        let (mut out, mut vanished) = (Vec::new(), Vec::new());
        fs.walk(&fs.root().join("Gone"), &mut out, &mut vanished).unwrap();
        assert_eq!((out.len(), vanished), (0, vec!["Gone".to_string()]), "a folder gone mid-listing is noticed");
        let none = |_: &[String]| panic!("not an unopenable folder");
        // Renamed during the first walk: the second sees the new name and is used.
        let mut tries = 0;
        let got = stable_listing(
            |v| {
                tries += 1;
                if tries == 1 {
                    v.push("Work".into());
                }
                Ok(if tries == 1 { vec![] } else { vec![NotePath::new("Projects/a.md")] })
            },
            none,
        );
        assert_eq!((got.unwrap(), tries), (vec![NotePath::new("Projects/a.md")], 2));
        // The vault changes on every walk: an error naming the folder, never a short listing.
        let mut tries = 0;
        let got = stable_listing(
            |v| {
                tries += 1;
                v.push(format!("Work {tries}"));
                Ok(vec![])
            },
            none,
        );
        assert!(tries == LIST_TRIES && got.as_ref().is_err_and(|e| e.to_string().contains("Work 3")), "{got:?}");
        // The same folder can't be opened on every walk, nothing else changes: left out, told.
        let mut told = Vec::new();
        let got = stable_listing(
            |v| {
                v.push("Bad. ".into());
                Ok(vec![NotePath::new("Keep/a.md")])
            },
            |f| told.extend_from_slice(f),
        );
        assert_eq!((got.unwrap(), told), (vec![NotePath::new("Keep/a.md")], vec!["Bad. ".to_string()]));
        assert_eq!(fs.list().unwrap(), [NotePath::new("Keep/a.md")]);
        assert!(fs.take_listing_notice().is_none());
        std::fs::remove_dir_all(fs.root()).unwrap();
        assert!(fs.list().is_err(), "the root itself must be readable");
    }
}
