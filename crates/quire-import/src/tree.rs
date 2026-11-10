// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Load a folder or archive into memory as relative path → bytes.
//!
//! Exports are untrusted input. Loading never writes anything, and refuses or
//! skips what could do harm later: paths that leave the export (`../x`,
//! `/etc/x`, `C:\x`), names a file system can't hold, links, files and
//! archives past the [`Limits`], very deep folders and archives that expand
//! far beyond their size. A skipped file is named in [`Tree::warnings`]; too
//! many files or too much data stops the load with an error.

use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;

/// How much of an export is read.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Files in the export.
    pub max_files: usize,
    /// Bytes of all files together.
    pub max_total: u64,
    /// Bytes of any one file; a bigger one is skipped.
    pub max_file: u64,
    /// Folders below the root.
    pub max_depth: usize,
    /// Longest file or folder name, in bytes.
    pub max_name: usize,
    /// A file this many times larger than its compressed size, and over
    /// [`Limits::ratio_floor`] bytes, is an archive bomb and is skipped.
    pub max_ratio: u64,
    pub ratio_floor: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_total: 2 << 30,
            max_file: 256 << 20,
            max_depth: 32,
            max_name: 255,
            max_ratio: 1000,
            ratio_floor: 64 << 20,
        }
    }
}

/// When a source file was made and last changed (unix seconds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stamp {
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct Tree {
    files: BTreeMap<String, Vec<u8>>,
    stamps: BTreeMap<String, Stamp>,
    warnings: Vec<String>,
    total: u64,
}

fn is_junk(p: &str) -> bool {
    p.split('/').any(|seg| (seg.starts_with('.') && seg != ".." && seg != ".") || seg == "__MACOSX" || seg == "node_modules")
        || p.ends_with("Thumbs.db")
}

/// Names Windows treats as devices, whatever the extension.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
}

/// A relative path made of plain names, or why it isn't.
pub(crate) fn clean_path(raw: &str, limits: &Limits) -> Result<String, &'static str> {
    let p = raw.replace('\\', "/");
    if p.chars().any(|c| c == '\0' || c.is_control()) {
        return Err("a control character in its name");
    }
    if p.starts_with('/') || p.as_bytes().get(1) == Some(&b':') {
        return Err("an absolute path");
    }
    let mut parts = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err("a path that leaves the export (..)"),
            s if s.len() > limits.max_name => return Err("a name that is too long"),
            s if reserved(s) => return Err("a name Windows reserves"),
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Err("an empty name");
    }
    if parts.len() > limits.max_depth + 1 {
        return Err("folders nested too deeply");
    }
    Ok(parts.join("/"))
}

/// Unix seconds of a zip entry's time (the zip format has no zone: read as UTC).
fn zip_time(t: Option<zip::DateTime>) -> Option<i64> {
    let t = t?;
    let d = chrono::NaiveDate::from_ymd_opt(i32::from(t.year()), u32::from(t.month()), u32::from(t.day()))?;
    Some(d.and_hms_opt(u32::from(t.hour()), u32::from(t.minute()), u32::from(t.second()))?.and_utc().timestamp())
}

fn system_time(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    let t = t.ok()?;
    Some(match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok()?,
        Err(e) => -i64::try_from(e.duration().as_secs()).ok()?,
    })
}

/// Read at most `max` bytes; `None` if there is more.
fn read_capped(r: &mut dyn Read, max: u64) -> Result<Option<Vec<u8>>, String> {
    let mut buf = Vec::new();
    r.take(max + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok((buf.len() as u64 <= max).then_some(buf))
}

impl Tree {
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        Self { files: files.into_iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect(), ..Self::default() }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_with(path, &Limits::default())
    }

    pub fn load_with(path: &Path, limits: &Limits) -> Result<Self, String> {
        let mut t = Self::default();
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            t.add_dir(path, path, 0, limits)?;
        } else {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut magic = [0u8; 4];
            let n = f.read(&mut magic).map_err(|e| e.to_string())?;
            f.rewind().map_err(|e| e.to_string())?;
            if n == 4 && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06") {
                t.add_zip(f, limits)?;
            } else if is_tar(&mut f)? {
                t.add_tar(f, limits)?;
            } else {
                let len = meta.len();
                if len > limits.max_file {
                    return Err(format!("{name} is {} MB; the limit for one file is {} MB", len >> 20, limits.max_file >> 20));
                }
                let bytes = read_capped(&mut f, limits.max_file)?.ok_or("the file is larger than the limit")?;
                t.stamp_of(&name, system_time(meta.created()), system_time(meta.modified()));
                t.insert(name, bytes, limits)?;
            }
        }
        // Notion and Bear sometimes nest archives; unpack one level.
        let nested: Vec<String> = t.files.keys().filter(|k| k.to_ascii_lowercase().ends_with(".zip")).cloned().collect();
        for k in nested {
            if let Some(bytes) = t.files.remove(&k) {
                t.total = t.total.saturating_sub(bytes.len() as u64);
                if bytes.starts_with(b"PK\x03\x04") {
                    t.add_zip(std::io::Cursor::new(bytes), limits)?;
                } else {
                    t.warnings.push(format!("{k}: not a zip archive; skipped"));
                }
            }
        }
        // Strip a single wrapping folder (common in zips): "Export/x.md" → "x.md".
        let tops: std::collections::BTreeSet<&str> = t.files.keys().map(|k| k.split('/').next().unwrap_or("")).collect();
        if tops.len() == 1 && t.files.keys().all(|k| k.contains('/')) {
            let top = format!("{}/", tops.iter().next().expect("one"));
            if !top.ends_with(".textbundle/") {
                let cut = |k: String| k[top.len()..].to_string();
                t.files = std::mem::take(&mut t.files).into_iter().map(|(k, v)| (cut(k), v)).collect();
                t.stamps = std::mem::take(&mut t.stamps).into_iter().map(|(k, v)| (cut(k), v)).collect();
            }
        }
        Ok(t)
    }

    fn stamp_of(&mut self, name: &str, created: Option<i64>, modified: Option<i64>) {
        if created.is_some() || modified.is_some() {
            self.stamps.insert(name.to_string(), Stamp { created, modified });
        }
    }

    /// Keep a file, within the limits.
    fn insert(&mut self, name: String, bytes: Vec<u8>, limits: &Limits) -> Result<(), String> {
        if self.files.len() >= limits.max_files {
            return Err(format!("the export has more than {} files; Quire reads at most that many at once", limits.max_files));
        }
        self.total += bytes.len() as u64;
        if self.total > limits.max_total {
            return Err(format!("the export is larger than {} MB once unpacked; import it in parts", limits.max_total >> 20));
        }
        self.files.insert(name, bytes);
        Ok(())
    }

    fn add_dir(&mut self, root: &Path, dir: &Path, depth: usize, limits: &Limits) -> Result<(), String> {
        if depth > limits.max_depth {
            self.warnings.push(format!("{}: folders nested too deeply; skipped", dir.strip_prefix(root).unwrap_or(dir).display()));
            return Ok(());
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            if is_junk(&rel) {
                continue;
            }
            // Links are never followed: they could point anywhere on this computer.
            let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
            if meta.file_type().is_symlink() {
                self.warnings.push(format!("{rel}: a link; skipped"));
                continue;
            }
            if meta.is_dir() {
                self.add_dir(root, &p, depth + 1, limits)?;
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let rel = match clean_path(&rel, limits) {
                Ok(r) => r,
                Err(why) => {
                    self.warnings.push(format!("{rel}: {why}; skipped"));
                    continue;
                }
            };
            if meta.len() > limits.max_file {
                self.warnings.push(format!(
                    "{rel}: {} MB, over the {} MB limit for one file; skipped",
                    meta.len() >> 20,
                    limits.max_file >> 20
                ));
                continue;
            }
            let mut f = match std::fs::File::open(&p) {
                Ok(f) => f,
                Err(e) => {
                    self.warnings.push(format!("{rel}: {e}; skipped"));
                    continue;
                }
            };
            let Some(bytes) = read_capped(&mut f, limits.max_file)? else {
                self.warnings.push(format!("{rel}: grew past the file limit while reading; skipped"));
                continue;
            };
            self.stamp_of(&rel, system_time(meta.created()), system_time(meta.modified()));
            self.insert(rel, bytes, limits)?;
        }
        Ok(())
    }

    fn add_zip<R: Read + Seek>(&mut self, src: R, limits: &Limits) -> Result<(), String> {
        let mut z = zip::ZipArchive::new(src).map_err(|e| format!("bad archive: {e}"))?;
        if z.len() > limits.max_files * 2 {
            return Err(format!("the archive lists {} entries; too many", z.len()));
        }
        for i in 0..z.len() {
            let mut f = match z.by_index(i) {
                Ok(f) => f,
                Err(e) => {
                    self.warnings.push(format!("entry {i}: {e}; skipped"));
                    continue;
                }
            };
            if f.is_dir() {
                continue;
            }
            let Ok(raw) = f.name().map(std::borrow::Cow::into_owned) else {
                self.warnings.push(format!("entry {i}: a name that can't be read; skipped"));
                continue;
            };
            let norm = raw.replace('\\', "/");
            if is_junk(&norm) {
                continue;
            }
            if f.is_symlink() {
                self.warnings.push(format!("{norm}: a link; skipped"));
                continue;
            }
            let name = match clean_path(&raw, limits) {
                Ok(n) => n,
                Err(why) => {
                    self.warnings.push(format!("{norm}: {why}; skipped"));
                    continue;
                }
            };
            // Judge by the sizes the archive claims, then by what is actually read.
            let (size, packed) = (f.size(), f.compressed_size().max(1));
            if size > limits.max_file {
                self.warnings.push(format!("{name}: {} MB, over the {} MB limit for one file; skipped", size >> 20, limits.max_file >> 20));
                continue;
            }
            if size > limits.ratio_floor && size / packed > limits.max_ratio {
                self.warnings.push(format!("{name}: expands {}× over its size; skipped as a possible archive bomb", size / packed));
                continue;
            }
            let modified = zip_time(f.last_modified());
            let Some(buf) = read_capped(&mut f, limits.max_file).map_err(|e| format!("{name}: {e}"))? else {
                self.warnings.push(format!("{name}: larger than it said; skipped"));
                continue;
            };
            self.stamp_of(&name, None, modified);
            self.insert(name, buf, limits)?;
        }
        Ok(())
    }

    /// A tar archive (a Joplin `.jex`). Only plain files are read.
    fn add_tar<R: Read>(&mut self, mut r: R, limits: &Limits) -> Result<(), String> {
        let mut long_name: Option<String> = None;
        let mut pax_path: Option<String> = None;
        let mut blocks = 0usize;
        loop {
            let mut h = [0u8; 512];
            match r.read_exact(&mut h) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.to_string()),
            }
            if h.iter().all(|b| *b == 0) {
                break;
            }
            blocks += 1;
            if blocks > limits.max_files * 3 {
                return Err("the archive has too many entries".into());
            }
            let sum: u32 = h.iter().enumerate().map(|(i, b)| u32::from(if (148..156).contains(&i) { b' ' } else { *b })).sum();
            if parse_octal(&h[148..156]) != Some(u64::from(sum)) {
                return Err("bad archive: a damaged tar header".into());
            }
            let size = tar_size(&h[124..136]).ok_or("bad archive: a size that can't be read")?;
            let kind = h[156];
            let padded = size.div_ceil(512) * 512;
            let skip = |r: &mut R, n: u64| std::io::copy(&mut r.by_ref().take(n), &mut std::io::sink()).map_err(|e| e.to_string());
            match kind {
                b'x' | b'L' => {
                    // Extended headers are small; anything big is not one.
                    if size > 1 << 20 {
                        skip(&mut r, padded)?;
                        continue;
                    }
                    let mut data = vec![0u8; usize::try_from(padded).unwrap_or(0)];
                    r.read_exact(&mut data).map_err(|e| e.to_string())?;
                    data.truncate(usize::try_from(size).unwrap_or(0));
                    if kind == b'L' {
                        long_name = Some(String::from_utf8_lossy(&data).trim_end_matches('\0').to_string());
                    } else {
                        pax_path = pax_value(&data, "path");
                    }
                    continue;
                }
                b'0' | 0 => {}
                _ => {
                    // Folders, links, devices, global headers: no data we want.
                    skip(&mut r, padded)?;
                    long_name = None;
                    pax_path = None;
                    continue;
                }
            }
            let mut name = String::from_utf8_lossy(&h[..100]).trim_end_matches('\0').to_string();
            if &h[257..262] == b"ustar" && h[345] != 0 {
                let prefix = String::from_utf8_lossy(&h[345..500]).trim_end_matches('\0').to_string();
                name = format!("{prefix}/{name}");
            }
            if let Some(p) = pax_path.take().or_else(|| long_name.take()) {
                name = p;
            }
            long_name = None;
            let modified = parse_octal(&h[136..148]).and_then(|m| i64::try_from(m).ok());
            let norm = name.replace('\\', "/");
            let clean = if is_junk(&norm) { None } else { Some(clean_path(&name, limits)) };
            let keep = match &clean {
                None => false,
                Some(Err(why)) => {
                    self.warnings.push(format!("{norm}: {why}; skipped"));
                    false
                }
                Some(Ok(_)) if size > limits.max_file => {
                    self.warnings.push(format!(
                        "{norm}: {} MB, over the {} MB limit for one file; skipped",
                        size >> 20,
                        limits.max_file >> 20
                    ));
                    false
                }
                Some(Ok(_)) => true,
            };
            if !keep {
                skip(&mut r, padded)?;
                continue;
            }
            let mut buf = vec![0u8; usize::try_from(size).map_err(|e| e.to_string())?];
            r.read_exact(&mut buf).map_err(|e| e.to_string())?;
            skip(&mut r, padded - size)?;
            if let Some(Ok(name)) = clean {
                self.stamp_of(&name, None, modified.filter(|m| *m > 0));
                self.insert(name, buf, limits)?;
            }
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

    /// A file as text: UTF-8 (with or without a byte order mark), UTF-16
    /// with one, or Windows-1252 when it isn't valid UTF-8. Line endings
    /// become `\n` and NUL characters go.
    pub fn text(&self, p: &str) -> Option<String> {
        self.get(p).map(decode)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// When a file was made and changed, as far as the export says.
    pub fn stamp(&self, p: &str) -> Stamp {
        self.stamps.get(p).copied().unwrap_or_default()
    }

    /// Files that were skipped, and why.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn is_markdown(p: &str) -> bool {
        let l = p.to_ascii_lowercase();
        Path::new(&l).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) || l.ends_with(".markdown")
    }
}

/// Bytes as text, whatever they were written in.
pub(crate) fn decode(b: &[u8]) -> String {
    let s = if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else if b.starts_with(&[0xFF, 0xFE]) {
        encoding_rs::UTF_16LE.decode_without_bom_handling(&b[2..]).0.into_owned()
    } else if b.starts_with(&[0xFE, 0xFF]) {
        encoding_rs::UTF_16BE.decode_without_bom_handling(&b[2..]).0.into_owned()
    } else {
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(b).0.into_owned(),
        }
    };
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "")
}

fn is_tar<R: Read + Seek>(f: &mut R) -> Result<bool, String> {
    let mut h = [0u8; 512];
    let ok = f.read_exact(&mut h).is_ok() && &h[257..262] == b"ustar";
    f.rewind().map_err(|e| e.to_string())?;
    Ok(ok)
}

fn parse_octal(b: &[u8]) -> Option<u64> {
    let s = String::from_utf8_lossy(b);
    let s = s.trim_matches(|c: char| c == '\0' || c == ' ');
    if s.is_empty() { Some(0) } else { u64::from_str_radix(s, 8).ok() }
}

/// A tar size: octal, or base 256 with the top bit set for big files.
fn tar_size(b: &[u8]) -> Option<u64> {
    if b[0] & 0x80 != 0 {
        let mut v = u64::from(b[0] & 0x7F);
        for x in &b[1..] {
            v = v.checked_mul(256)?.checked_add(u64::from(*x))?;
        }
        return Some(v);
    }
    parse_octal(b)
}

/// `key` in a pax extended header (`<len> key=value\n` records).
fn pax_value(data: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.lines().find_map(|l| {
        let rec = l.split_once(' ')?.1;
        rec.strip_prefix(key)?.strip_prefix('=').map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(bytes).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    fn load(bytes: &[u8], name: &str, limits: &Limits) -> Result<Tree, String> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        Tree::load_with(&p, limits)
    }

    #[test]
    fn loads_zip_with_wrapper_folder_and_nested_zip() {
        let inner = zip_of(&[("Page abc.md", b"inner")]);
        let outer = zip_of(&[("Export/Part-1.zip", &inner), ("__MACOSX/._junk", b"x")]);
        let t = load(&outer, "export.zip", &Limits::default()).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["Page abc.md"]);
        assert_eq!(t.text("Page abc.md").unwrap(), "inner");
    }

    #[test]
    fn paths_that_leave_the_export_are_skipped() {
        let z = zip_of(&[
            ("ok.md", b"fine"),
            ("../evil.md", b"x"),
            ("a/../../evil2.md", b"x"),
            ("/etc/passwd", b"x"),
            ("C:\\Windows\\x.md", b"x"),
            ("sub\\..\\..\\evil3.md", b"x"),
            ("NUL.md", b"x"),
            ("bad\0name.md", b"x"),
        ]);
        let t = load(&z, "x.zip", &Limits::default()).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["ok.md"]);
        assert!(t.warnings().len() >= 6, "{:?}", t.warnings());
    }

    #[test]
    fn limits_stop_big_and_many_files() {
        let z = zip_of(&[("a.md", &[b'x'; 5000]), ("b.md", b"small")]);
        let t = load(&z, "x.zip", &Limits { max_file: 1000, ..Limits::default() }).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["b.md"]);
        assert!(t.warnings()[0].contains("a.md"));
        let many: Vec<(String, Vec<u8>)> = (0..20).map(|i| (format!("n{i}.md"), vec![b'x'])).collect();
        let refs: Vec<(&str, &[u8])> = many.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        let err = load(&zip_of(&refs), "x.zip", &Limits { max_files: 10, ..Limits::default() }).unwrap_err();
        assert!(err.contains("more than 10 files"), "{err}");
        let err =
            load(&zip_of(&[("a.md", &[b'x'; 3000]), ("b.md", &[b'y'; 3000])]), "x.zip", &Limits { max_total: 4000, ..Limits::default() })
                .unwrap_err();
        assert!(err.contains("larger than"), "{err}");
    }

    #[test]
    fn an_archive_that_expands_far_beyond_its_size_is_skipped() {
        let z = zip_of(&[("zeros.md", &vec![0u8; 2_000_000]), ("ok.md", b"ok")]);
        let t = load(&z, "x.zip", &Limits { ratio_floor: 1_000_000, max_ratio: 100, ..Limits::default() }).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["ok.md"]);
        assert!(t.warnings()[0].contains("archive bomb"), "{:?}", t.warnings());
    }

    #[test]
    fn deep_folders_and_links_in_a_directory_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..6 {
            deep.push(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.md"), "x").unwrap();
        std::fs::write(dir.path().join("top.md"), "top").unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
            std::os::unix::fs::symlink(outside.path().join("secret.md"), dir.path().join("link.md")).unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("linkdir")).unwrap();
        }
        let t = Tree::load_with(dir.path(), &Limits { max_depth: 3, ..Limits::default() }).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["top.md"]);
        assert!(t.warnings().iter().any(|w| w.contains("nested too deeply")));
        #[cfg(unix)]
        assert!(t.warnings().iter().filter(|w| w.contains("a link")).count() == 2, "{:?}", t.warnings());
    }

    #[test]
    fn odd_encodings_become_text() {
        assert_eq!(decode(b"\xEF\xBB\xBFhi\r\nthere"), "hi\nthere");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
        assert_eq!(decode(b"r\xE9sum\xE9"), "résumé", "Windows-1252");
        assert_eq!(decode(b"a\0b\rc"), "ab\nc");
    }

    fn tar_entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"01234567000");
        h[156] = kind;
        h[257..262].copy_from_slice(b"ustar");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut out = h.to_vec();
        out.extend_from_slice(data);
        out.resize(512 + data.len().div_ceil(512) * 512, 0);
        out
    }

    #[test]
    fn tar_files_load_and_hostile_names_are_skipped() {
        let mut tar = Vec::new();
        tar.extend(tar_entry("note.md", b'0', b"hello"));
        tar.extend(tar_entry("../evil.md", b'0', b"x"));
        tar.extend(tar_entry("/abs.md", b'0', b"x"));
        tar.extend(tar_entry("link.md", b'2', b""));
        tar.extend(tar_entry("resources/pic.png", b'0', &[1, 2, 3]));
        tar.extend(vec![0u8; 1024]);
        let t = load(&tar, "export.jex", &Limits::default()).unwrap();
        assert_eq!(t.paths().collect::<Vec<_>>(), vec!["note.md", "resources/pic.png"]);
        assert_eq!(t.stamp("note.md").modified, Some(0o1_234_567_000));
        assert!(load(b"not a tar at all", "x.jex", &Limits::default()).is_ok(), "plain files load as themselves");
        let mut broken = tar.clone();
        broken[0] ^= 1;
        assert!(load(&broken, "x.tar", &Limits::default()).is_err(), "a damaged header is refused");
    }

    #[test]
    fn zip_times_are_kept() {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts =
            zip::write::SimpleFileOptions::default().last_modified_time(zip::DateTime::from_date_and_time(2024, 5, 6, 7, 8, 10).unwrap());
        z.start_file("a.md", opts).unwrap();
        z.write_all(b"x").unwrap();
        let t = load(&z.finish().unwrap().into_inner(), "x.zip", &Limits::default()).unwrap();
        assert_eq!(t.stamp("a.md").modified, Some(1_714_979_290));
    }
}
