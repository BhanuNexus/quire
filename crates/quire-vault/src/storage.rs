// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

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
        if p.as_str().split('/').any(|s| s == "..") {
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

    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let (a, b) = (self.abs(from)?, self.abs(to)?);
        if b.exists() {
            return Err(VaultError::Exists(to.to_string()));
        }
        if let Some(dir) = b.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(to.to_string()))?;
        }
        std::fs::rename(&a, &b).map_err(io_err(from.to_string()))
    }

    fn trash(&self, path: &NotePath) -> Result<()> {
        let src = self.abs(path)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = self.root.join(".trash").join(format!("{stamp} {}", path.file_name()));
        std::fs::create_dir_all(dest.parent().unwrap_or(&self.root)).map_err(io_err(".trash"))?;
        std::fs::rename(&src, &dest).map_err(io_err(path.to_string()))
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
}
