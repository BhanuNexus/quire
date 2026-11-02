// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The personal word list ("Add to dictionary").

use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::SpellError;

/// Words the user has taught the spell checker, stored as a plain text file
/// with one word per line, sorted.
///
/// Matching: an all-lowercase entry (`quire`) accepts any capitalization
/// (`Quire`, `QUIRE`); an entry with capitals (`McAllister`) must match
/// exactly. A trailing possessive `'s` is accepted too (`Quire's`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDictionary {
    words: BTreeSet<String>,
    /// The all-lowercase entries, for case-insensitive lookups.
    lowercase: HashSet<String>,
    path: Option<PathBuf>,
}

impl UserDictionary {
    /// An empty dictionary that is not backed by a file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the word list at `path`. A missing file is an empty dictionary;
    /// [`save`](Self::save) will create it. Blank lines and lines starting
    /// with `#` are ignored.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, SpellError> {
        let path = path.into();
        let mut dict = Self { path: Some(path.clone()), ..Self::default() };
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(dict),
            Err(source) => return Err(SpellError::Io { path, source }),
        };
        for line in text.lines() {
            let line = line.trim_start_matches('\u{feff}');
            if !line.trim_start().starts_with('#') {
                dict.add(line);
            }
        }
        Ok(dict)
    }

    /// The file this dictionary loads from and saves to, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Writes the word list to [`path`](Self::path) (no-op without one).
    pub fn save(&self) -> Result<(), SpellError> {
        match &self.path {
            Some(path) => self.save_to(path),
            None => Ok(()),
        }
    }

    /// Writes the word list to `path` atomically: readers see the old file or
    /// the new one, never a half-written one.
    pub fn save_to(&self, path: &Path) -> Result<(), SpellError> {
        let io = |source| SpellError::Io { path: path.to_owned(), source };
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(dir).map_err(io)?;
        let mut text = String::with_capacity(self.words.iter().map(|w| w.len() + 1).sum());
        for word in &self.words {
            text.push_str(word);
            text.push('\n');
        }
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(io)?;
        tmp.write_all(text.as_bytes()).map_err(io)?;
        tmp.as_file().sync_all().map_err(io)?;
        tmp.persist(path).map_err(|e| io(e.error))?;
        Ok(())
    }

    /// Adds a word. Returns false if it was already present or is not a
    /// single word (empty, or contains whitespace).
    pub fn add(&mut self, word: &str) -> bool {
        let Some(word) = normalize(word) else { return false };
        if word.chars().all(|c| !c.is_uppercase()) {
            self.lowercase.insert(word.clone());
        }
        self.words.insert(word)
    }

    /// Removes a word (exact spelling). Returns whether it was present.
    pub fn remove(&mut self, word: &str) -> bool {
        let Some(word) = normalize(word) else { return false };
        self.lowercase.remove(&word);
        self.words.remove(&word)
    }

    /// Whether `word` is accepted by this list (see the type docs for the
    /// case rules).
    pub fn contains(&self, word: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        let word = straight_apostrophes(word.trim());
        self.matches(&word) || possessive_stem(&word).is_some_and(|stem| self.matches(stem))
    }

    fn matches(&self, word: &str) -> bool {
        self.words.contains(word) || (!self.lowercase.is_empty() && self.lowercase.contains(&word.to_lowercase()))
    }

    /// The words, sorted.
    pub fn words(&self) -> impl Iterator<Item = &str> {
        self.words.iter().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// Trims the word and turns curly apostrophes straight; rejects anything that
/// is not a single token.
fn normalize(word: &str) -> Option<String> {
    let word = word.trim();
    if word.is_empty() || word.chars().any(char::is_whitespace) {
        return None;
    }
    Some(straight_apostrophes(word).into_owned())
}

/// `it’s` → `it's` (dictionaries and word lists use the ASCII apostrophe).
pub(crate) fn straight_apostrophes(word: &str) -> Cow<'_, str> {
    if word.contains('\u{2019}') { Cow::Owned(word.replace('\u{2019}', "'")) } else { Cow::Borrowed(word) }
}

/// `Quire's` → `Quire`
pub(crate) fn possessive_stem(word: &str) -> Option<&str> {
    word.strip_suffix("'s").or_else(|| word.strip_suffix("'S")).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_rules() {
        let mut d = UserDictionary::new();
        assert!(d.add("quire"));
        assert!(!d.add("quire"));
        assert!(d.add("McAllister"));
        assert!(!d.add("two words"));
        assert!(!d.add("  "));
        assert!(d.contains("quire") && d.contains("Quire") && d.contains("QUIRE"));
        assert!(d.contains("Quire's") && d.contains("Quire\u{2019}s"));
        assert!(d.contains("McAllister"));
        assert!(!d.contains("mcallister"));
        assert!(d.remove("quire"));
        assert!(!d.contains("Quire"));
    }
}
