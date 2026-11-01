// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Load a folder or archive into memory as relative path → bytes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Tree {
    files: BTreeMap<String, Vec<u8>>,
}

fn is_junk(p: &str) -> bool {
    p.split('/').any(|seg| seg.starts_with('.') || seg == "__MACOSX" || seg == "node_modules") || p.ends_with("Thumbs.db")
}

impl Tree {
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        Self { files: files.into_iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect() }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let mut t = Self::default();
        if path.is_dir() {
            t.add_dir(path, path)?;
        } else {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if bytes.starts_with(b"PK\x03\x04") {
                t.add_zip(&bytes, "")?;
            } else {
                t.files.insert(name, bytes);
            }
        }
        // Notion and Bear sometimes nest archives; unpack one level.
        let nested: Vec<String> = t.files.keys().filter(|k| k.to_ascii_lowercase().ends_with(".zip")).cloned().collect();
        for k in nested {
            if let Some(bytes) = t.files.remove(&k) {
                t.add_zip(&bytes, "")?;
            }
        }
        // Strip a single wrapping folder (common in zips): "Export/x.md" → "x.md".
        let tops: std::collections::BTreeSet<&str> = t.files.keys().map(|k| k.split('/').next().unwrap_or("")).collect();
        if tops.len() == 1 && t.files.keys().all(|k| k.contains('/')) {
            let top = format!("{}/", tops.iter().next().expect("one"));
            if !top.ends_with(".textbundle/") {
                t.files = std::mem::take(&mut t.files).into_iter().map(|(k, v)| (k[top.len()..].to_string(), v)).collect();
            }
        }
        Ok(t)
    }

    fn add_dir(&mut self, root: &Path, dir: &Path) -> Result<(), String> {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            if is_junk(&rel) {
                continue;
            }
            if p.is_dir() {
                self.add_dir(root, &p)?;
            } else {
                self.files.insert(rel, std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?);
            }
        }
        Ok(())
    }

    fn add_zip(&mut self, bytes: &[u8], prefix: &str) -> Result<(), String> {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("bad archive: {e}"))?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().map_err(|e| e.to_string())?.replace('\\', "/");
            if is_junk(&name) {
                continue;
            }
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            self.files.insert(format!("{prefix}{name}"), buf);
        }
        Ok(())
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    pub fn contains(&self, p: &str) -> bool {
        self.files.contains_key(p)
    }

    pub fn get(&self, p: &str) -> Option<&[u8]> {
        self.files.get(p).map(Vec::as_slice)
    }

    pub fn text(&self, p: &str) -> Option<String> {
        let b = self.get(p)?;
        let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
        Some(String::from_utf8_lossy(b).replace("\r\n", "\n"))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    pub fn is_markdown(p: &str) -> bool {
        let l = p.to_ascii_lowercase();
        Path::new(&l).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) || l.ends_with(".markdown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_zip_with_wrapper_folder_and_nested_zip() {
        let mut inner = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        inner.start_file("Page abc.md", zip::write::SimpleFileOptions::default()).unwrap();
        inner.write_all(b"inner").unwrap();
        let inner = inner.finish().unwrap().into_inner();
        let mut outer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        outer.start_file("Export/Part-1.zip", zip::write::SimpleFileOptions::default()).unwrap();
        outer.write_all(&inner).unwrap();
        outer.start_file("__MACOSX/._junk", zip::write::SimpleFileOptions::default()).unwrap();
        outer.write_all(b"x").unwrap();
        let bytes = outer.finish().unwrap().into_inner();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("export.zip");
        std::fs::write(&p, bytes).unwrap();
        let t = Tree::load(&p).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["Page abc.md"]);
        assert_eq!(t.text("Page abc.md").unwrap(), "inner");
    }
}
