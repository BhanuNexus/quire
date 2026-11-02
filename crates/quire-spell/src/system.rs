// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Finding Hunspell dictionaries that are already installed on the system,
//! and decoding their (sometimes legacy 8-bit) text.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::SpellError;

/// Where a dictionary comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DictionarySource {
    /// Compiled into Quire (see `crates/quire-spell/dictionaries`).
    Bundled,
    /// A Hunspell `.aff` + `.dic` pair on disk.
    Files { aff: PathBuf, dic: PathBuf },
}

/// A dictionary the user can pick.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DictionaryInfo {
    /// Language tag as Hunspell names it, e.g. `en_US`, `de_DE_frami`, `fr`.
    pub lang: String,
    pub source: DictionarySource,
}

/// Languages compiled into this crate.
pub const BUNDLED_LANGUAGES: [&str; 2] = ["en_US", "en_GB"];

/// The bundled dictionaries.
pub fn bundled_dictionaries() -> Vec<DictionaryInfo> {
    BUNDLED_LANGUAGES.iter().map(|&lang| DictionaryInfo { lang: lang.to_owned(), source: DictionarySource::Bundled }).collect()
}

/// Bundled dictionaries first, then installed ones for other languages.
pub fn available_dictionaries() -> Vec<DictionaryInfo> {
    let mut all = bundled_dictionaries();
    let bundled: HashSet<String> = all.iter().map(|d| normalize_lang(&d.lang)).collect();
    all.extend(system_dictionaries().into_iter().filter(|d| !bundled.contains(&normalize_lang(&d.lang))));
    all
}

/// Hunspell dictionaries installed on this machine, sorted by language.
///
/// Looks in `$DICPATH` and the usual system locations. Unreadable folders are
/// skipped; an empty list just means none were found.
pub fn system_dictionaries() -> Vec<DictionaryInfo> {
    let mut found = Vec::new();
    let mut seen_files = HashSet::new();
    let mut seen_langs = HashSet::new();
    for dir in search_dirs() {
        scan_dir(&dir, &mut found, &mut seen_files, &mut seen_langs);
    }
    found.sort_by(|a, b| a.lang.cmp(&b.lang));
    found
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(paths) = std::env::var_os("DICPATH") {
        dirs.extend(std::env::split_paths(&paths));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            dirs.push(home.join("Library/Spelling"));
        }
        dirs.extend(["/Library/Spelling", "/opt/homebrew/share/hunspell", "/usr/local/share/hunspell"].map(PathBuf::from));
    } else if cfg!(windows) {
        // LibreOffice ships each language as an extension folder holding an .aff/.dic pair.
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(var) {
                let extensions = PathBuf::from(root).join("LibreOffice").join("share").join("extensions");
                if let Ok(entries) = std::fs::read_dir(&extensions) {
                    dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
                }
            }
        }
    } else {
        if let Some(home) = &home {
            dirs.push(home.join(".local/share/hunspell"));
        }
        dirs.extend(
            ["/usr/share/hunspell", "/usr/share/myspell", "/usr/share/myspell/dicts", "/usr/local/share/hunspell"].map(PathBuf::from),
        );
    }
    dirs
}

fn scan_dir(dir: &Path, found: &mut Vec<DictionaryInfo>, seen_files: &mut HashSet<PathBuf>, seen_langs: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let aff = entry.path();
        if !aff.extension().is_some_and(|e| e.eq_ignore_ascii_case("aff")) {
            continue;
        }
        let dic = aff.with_extension("dic");
        if !dic.is_file() {
            continue;
        }
        let Some(lang) = aff.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else { continue };
        // Distributions symlink the same files under several names and folders.
        let canonical = std::fs::canonicalize(&dic).unwrap_or_else(|_| dic.clone());
        if !seen_files.insert(canonical) || !seen_langs.insert(lang.clone()) {
            continue;
        }
        found.push(DictionaryInfo { lang, source: DictionarySource::Files { aff, dic } });
    }
}

/// `en-us`, `en_US` and `EN_us` all compare equal.
pub(crate) fn normalize_lang(lang: &str) -> String {
    lang.trim().replace('-', "_").to_ascii_lowercase()
}

/// Reads a dictionary file as UTF-8 text, decoding Latin-1 / Latin-9 files
/// (common in older distributions) according to the `.aff` `SET` line.
pub(crate) fn read_dictionary_text(path: &Path, encoding: Encoding) -> Result<String, SpellError> {
    let bytes = std::fs::read(path).map_err(|source| SpellError::Io { path: path.to_owned(), source })?;
    decode(bytes, encoding).ok_or_else(|| SpellError::Encoding { path: path.to_owned() })
}

/// Text encodings we can read. Hunspell defaults to ISO-8859-1 when no `SET` is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Latin1,
    Latin9,
    Unsupported,
}

/// The encoding named by the `SET` line of raw `.aff` bytes.
pub(crate) fn aff_encoding(aff: &[u8]) -> Encoding {
    for line in aff.split(|&b| b == b'\n') {
        let line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"SET") {
            let name: String = rest.iter().map(|&b| char::from(b)).collect::<String>().trim().to_ascii_uppercase();
            return match name.replace(['-', '_'], "").as_str() {
                "UTF8" => Encoding::Utf8,
                "ISO88591" => Encoding::Latin1,
                "ISO885915" => Encoding::Latin9,
                _ => Encoding::Unsupported,
            };
        }
    }
    Encoding::Latin1
}

fn decode(bytes: Vec<u8>, encoding: Encoding) -> Option<String> {
    // Many "Latin-1" files are really UTF-8 (or plain ASCII); trust valid UTF-8 first.
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => return Some(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text)),
        Err(e) => e.into_bytes(),
    };
    match encoding {
        Encoding::Latin1 => Some(bytes.iter().map(|&b| char::from(b)).collect()),
        Encoding::Latin9 => Some(bytes.iter().map(|&b| latin9(b)).collect()),
        Encoding::Utf8 | Encoding::Unsupported => None,
    }
}

/// ISO-8859-15 differs from Latin-1 in eight positions.
fn latin9(b: u8) -> char {
    match b {
        0xA4 => '€',
        0xA6 => 'Š',
        0xA8 => 'š',
        0xB4 => 'Ž',
        0xB8 => 'ž',
        0xBC => 'Œ',
        0xBD => 'œ',
        0xBE => 'Ÿ',
        _ => char::from(b),
    }
}

/// Rewrites the `SET` line to UTF-8 now that the text has been decoded.
pub(crate) fn force_utf8_set(aff: &str) -> String {
    let mut out = String::with_capacity(aff.len());
    for line in aff.split_inclusive('\n') {
        if line.starts_with("SET ") || line.starts_with("SET\t") {
            out.push_str("SET UTF-8\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(aff_encoding(b"# c\nSET UTF-8\nTRY abc"), Encoding::Utf8);
        assert_eq!(aff_encoding(b"SET ISO8859-1\n"), Encoding::Latin1);
        assert_eq!(aff_encoding(b"SET ISO8859-15\n"), Encoding::Latin9);
        assert_eq!(aff_encoding(b"SET KOI8-R\n"), Encoding::Unsupported);
        assert_eq!(aff_encoding(b"TRY abc\n"), Encoding::Latin1);
        assert_eq!(decode(vec![b'c', b'a', b'f', 0xE9], Encoding::Latin1).as_deref(), Some("café"));
        assert_eq!(decode(vec![0xA4], Encoding::Latin9).as_deref(), Some("€"));
        assert_eq!(decode(vec![0xFF], Encoding::Unsupported), None);
    }

    #[test]
    fn lang_normalization() {
        assert_eq!(normalize_lang(" en-US "), "en_us");
    }
}
