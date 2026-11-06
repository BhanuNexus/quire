// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};
use std::time::SystemTime;

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
/// between its check and its write. Held only for the write itself.
static WRITES: Mutex<()> = Mutex::new(());

fn one_write_at_a_time() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------- LocalFs

#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io_err(root.display().to_string()))?;
        let root = root.canonicalize().map_err(io_err(root.display().to_string()))?;
        Ok(Self { root })
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

    fn walk(&self, dir: &Path, out: &mut Vec<NotePath>) -> Result<()> {
        let entries = std::fs::read_dir(dir).map_err(io_err(dir.display().to_string()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if is_hidden(&rel) {
                continue;
            }
            let ft = entry.file_type().map_err(io_err(rel.clone()))?;
            if ft.is_dir() {
                self.walk(&path, out)?;
            } else if ft.is_file() {
                out.push(NotePath::new(rel));
            }
        }
        Ok(())
    }

    /// Write `contents` to `path` through a temporary file (call with
    /// [`one_write_at_a_time`] held).
    fn persist(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        let abs = self.abs(path)?;
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
        tmp.persist(&abs).map_err(|e| io_err(path.to_string())(e.error))?;
        Ok(())
    }

    fn move_to_trash(&self, path: &NotePath) -> Result<()> {
        let src = self.abs(path)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = self.root.join(".trash").join(format!("{stamp} {}", path.file_name()));
        std::fs::create_dir_all(dest.parent().unwrap_or(&self.root)).map_err(io_err(".trash"))?;
        std::fs::rename(&src, &dest).map_err(io_err(path.to_string()))
    }
}

impl Storage for LocalFs {
    fn list(&self) -> Result<Vec<NotePath>> {
        let mut out = Vec::new();
        self.walk(&self.root, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        let abs = self.abs(path)?;
        std::fs::read(&abs).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { VaultError::NotFound(path.to_string()) } else { io_err(path.to_string())(e) }
        })
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        let _one = one_write_at_a_time();
        self.persist(path, contents)
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        let _one = one_write_at_a_time();
        if !still(read_if_exists(self, path)?.as_deref()) {
            return Ok(false);
        }
        self.persist(path, bytes)?;
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
        if b.exists() {
            return Err(VaultError::Exists(to.to_string()));
        }
        if let Some(dir) = b.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(to.to_string()))?;
        }
        std::fs::rename(&a, &b).map_err(io_err(from.to_string()))
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

// ---------------------------------------------------------------- MemFs

/// In-memory storage for tests, demos and the scratchpad.
#[derive(Debug, Default)]
pub struct MemFs {
    files: RwLock<BTreeMap<NotePath, (Vec<u8>, SystemTime)>>,
    trashed: RwLock<Vec<NotePath>>,
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

    pub fn trashed(&self) -> Vec<NotePath> {
        self.trashed.read().expect("lock").clone()
    }
}

impl Storage for MemFs {
    fn list(&self) -> Result<Vec<NotePath>> {
        Ok(self.files.read().expect("lock").keys().filter(|p| !is_hidden(p.as_str())).cloned().collect())
    }

    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.files.read().expect("lock").get(path).map(|(c, _)| c.clone()).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }

    fn write_bytes_atomic(&self, path: &NotePath, contents: &[u8]) -> Result<()> {
        self.files.write().expect("lock").insert(path.clone(), (contents.to_vec(), SystemTime::now()));
        Ok(())
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        let mut files = self.files.write().expect("lock");
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
}
