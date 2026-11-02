// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The spell checker: a Hunspell dictionary plus the user's own word lists.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::io::Read as _;
use std::ops::Range;
use std::path::Path;
use std::sync::{OnceLock, PoisonError, RwLock, RwLockReadGuard};

use spellbook::Dictionary;

use crate::SpellError;
use crate::cache::LineCache;
use crate::scan::{self, LineContext};
use crate::system::{self, DictionarySource, Encoding};
use crate::user_dict::{UserDictionary, possessive_stem, straight_apostrophes};

const EN_US_AFF: &str = include_str!("../dictionaries/en_US.aff");
const EN_US_DIC_GZ: &[u8] = include_bytes!("../dictionaries/en_US.dic.gz");
const EN_GB_AFF: &str = include_str!("../dictionaries/en_GB.aff");
const EN_GB_DIC_GZ: &[u8] = include_bytes!("../dictionaries/en_GB.dic.gz");

/// A loaded dictionary plus the personal word list and the session's
/// ignored words.
///
/// `Speller` is `Send + Sync`: load it on a background thread, share it in an
/// `Arc`, and query it from the UI thread. Word-list changes take `&self`.
pub struct Speller {
    lang: String,
    dict: Dictionary,
    scripts: Scripts,
    /// The word list has no accented words (SCOWL's English lists strip
    /// them), so `café` is checked as `cafe`.
    fold_accents: bool,
    personal: RwLock<Personal>,
    cache: LineCache,
}

#[derive(Debug, Default)]
struct Personal {
    user: UserDictionary,
    /// Lowercased words ignored for this session.
    ignored: HashSet<String>,
}

impl Personal {
    fn accepts(&self, word: &str) -> bool {
        if self.user.contains(word) {
            return true;
        }
        if self.ignored.is_empty() {
            return false;
        }
        let lower = word.to_lowercase();
        self.ignored.contains(&lower) || possessive_stem(&lower).is_some_and(|stem| self.ignored.contains(stem))
    }
}

impl fmt::Debug for Speller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Speller").field("lang", &self.lang).field("fold_accents", &self.fold_accents).finish_non_exhaustive()
    }
}

impl Speller {
    /// The bundled US English speller, loaded on first use and shared by the
    /// whole process (including its personal word list).
    pub fn english() -> &'static Self {
        static ENGLISH: OnceLock<Speller> = OnceLock::new();
        ENGLISH.get_or_init(|| Self::load("en_US").expect("the bundled en_US dictionary is valid"))
    }

    /// Loads a dictionary by language tag (`en_US`, `en-GB`, `de_DE`, `fr`...).
    ///
    /// English (`en`, `en_US`, `en_GB`) is bundled; other languages come from
    /// [`system_dictionaries`](crate::system_dictionaries). A bare language
    /// (`de`) picks the first installed regional variant.
    pub fn load(lang: &str) -> Result<Self, SpellError> {
        let wanted = system::normalize_lang(lang);
        match wanted.as_str() {
            "en" | "en_us" => return Self::bundled("en_US", EN_US_AFF, EN_US_DIC_GZ),
            "en_gb" | "en_uk" => return Self::bundled("en_GB", EN_GB_AFF, EN_GB_DIC_GZ),
            _ => {}
        }
        let installed = system::system_dictionaries();
        let prefix = format!("{wanted}_");
        let found = installed
            .iter()
            .find(|d| system::normalize_lang(&d.lang) == wanted)
            .or_else(|| installed.iter().find(|d| system::normalize_lang(&d.lang).starts_with(&prefix)));
        match found.map(|d| &d.source) {
            Some(DictionarySource::Files { aff, dic }) => Self::from_files(aff, dic),
            Some(DictionarySource::Bundled) | None => Err(SpellError::UnknownLanguage(lang.to_owned())),
        }
    }

    /// Loads a Hunspell `.aff` + `.dic` pair from disk. UTF-8, ISO-8859-1 and
    /// ISO-8859-15 files are supported.
    pub fn from_files(aff: impl AsRef<Path>, dic: impl AsRef<Path>) -> Result<Self, SpellError> {
        let (aff, dic) = (aff.as_ref(), dic.as_ref());
        let aff_bytes = std::fs::read(aff).map_err(|source| SpellError::Io { path: aff.to_owned(), source })?;
        let encoding = system::aff_encoding(&aff_bytes);
        let encoding = if encoding == Encoding::Unsupported { Encoding::Utf8 } else { encoding };
        let aff_text = system::read_dictionary_text(aff, encoding)?;
        let dic_text = system::read_dictionary_text(dic, encoding)?;
        let lang = aff.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Self::from_strings(&lang, &system::force_utf8_set(&aff_text), &dic_text)
    }

    /// Builds a speller from UTF-8 `.aff` and `.dic` contents.
    pub fn from_strings(lang: &str, aff: &str, dic: &str) -> Result<Self, SpellError> {
        let dict = Dictionary::new(aff, dic).map_err(|e| SpellError::Parse { lang: lang.to_owned(), message: e.to_string() })?;
        Ok(Self {
            lang: lang.to_owned(),
            dict,
            scripts: Scripts::of(aff, dic),
            fold_accents: dic.is_ascii(),
            personal: RwLock::default(),
            cache: LineCache::default(),
        })
    }

    fn bundled(lang: &str, aff: &str, dic_gz: &[u8]) -> Result<Self, SpellError> {
        let mut dic = String::with_capacity(dic_gz.len() * 3);
        flate2::read::GzDecoder::new(dic_gz)
            .read_to_string(&mut dic)
            .map_err(|e| SpellError::Parse { lang: lang.to_owned(), message: format!("bundled dictionary: {e}") })?;
        Self::from_strings(lang, aff, &dic)
    }

    /// The language tag this speller was loaded for, e.g. `en_US`.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    fn personal(&self) -> RwLockReadGuard<'_, Personal> {
        self.personal.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn edit_personal<T>(&self, f: impl FnOnce(&mut Personal) -> T) -> T {
        let result = f(&mut self.personal.write().unwrap_or_else(PoisonError::into_inner));
        self.cache.clear();
        result
    }

    // ------------------------------------------------------------ words

    /// Whether `word` is spelled correctly: accepted by the dictionary, the
    /// personal word list or the session's ignore list. Curly apostrophes are
    /// treated as straight ones. Empty input counts as correct.
    ///
    /// This checks the word as given; the Markdown and word-shape rules
    /// (acronyms, code, URLs...) only apply in [`misspellings`](Self::misspellings).
    pub fn check(&self, word: &str) -> bool {
        let word = word.trim();
        word.is_empty() || self.check_with(word, &self.personal())
    }

    fn check_with(&self, word: &str, personal: &Personal) -> bool {
        let word = straight_apostrophes(word);
        if self.dict.check(&word) || personal.accepts(&word) {
            return true;
        }
        if self.fold_accents
            && !word.is_ascii()
            && let Some(folded) = fold_accents(&word)
        {
            return self.dict.check(&folded) || personal.accepts(&folded);
        }
        false
    }

    /// Up to `max` corrections for `word`, best first. Close matches from the
    /// personal word list come before dictionary suggestions.
    pub fn suggest(&self, word: &str, max: usize) -> Vec<String> {
        let word = word.trim();
        if max == 0 || word.is_empty() {
            return Vec::new();
        }
        let curly = word.contains('\u{2019}');
        let straight = straight_apostrophes(word);
        let mut out: Vec<String> = {
            let personal = self.personal();
            let limit = if straight.chars().count() <= 4 { 1 } else { 2 };
            let mut near: Vec<(usize, &str)> =
                personal.user.words().filter_map(|w| edit_distance(&straight, w, limit).filter(|&d| d > 0).map(|d| (d, w))).collect();
            near.sort_by_key(|&(d, _)| d);
            near.into_iter().map(|(_, w)| w.to_owned()).collect()
        };
        // Edit-based suggestions are fast (a few ms). The n-gram pass scans the
        // whole word list (20-30 ms), so only run it when edits found nothing.
        let mut from_dict = Vec::new();
        self.dict.suggester().with_ngram_suggestions(false).suggest(&straight, &mut from_dict);
        if from_dict.is_empty() {
            self.dict.suggest(&straight, &mut from_dict);
        }
        rank(&straight, &mut from_dict);
        for s in from_dict {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out.truncate(max);
        if curly {
            for s in &mut out {
                *s = s.replace('\'', "\u{2019}");
            }
        }
        out
    }

    // ------------------------------------------------------- word lists

    /// Replaces the personal word list (e.g. after loading it from disk).
    pub fn set_user_dictionary(&self, dict: UserDictionary) {
        self.edit_personal(|p| p.user = dict);
    }

    /// A copy of the personal word list.
    pub fn user_dictionary(&self) -> UserDictionary {
        self.personal().user.clone()
    }

    /// Adds a word to the personal word list and saves it if the list has a
    /// file. Returns whether the word was new.
    pub fn add_user_word(&self, word: &str) -> Result<bool, SpellError> {
        self.edit_personal(|p| {
            let added = p.user.add(word);
            if added {
                p.user.save()?;
            }
            Ok(added)
        })
    }

    /// Removes a word from the personal word list and saves it if the list
    /// has a file. Returns whether the word was present.
    pub fn remove_user_word(&self, word: &str) -> Result<bool, SpellError> {
        self.edit_personal(|p| {
            let removed = p.user.remove(word);
            if removed {
                p.user.save()?;
            }
            Ok(removed)
        })
    }

    /// Ignores `word` (any capitalization) until [`clear_ignored`](Self::clear_ignored)
    /// or the speller is dropped. Nothing is saved.
    pub fn ignore(&self, word: &str) {
        let word = straight_apostrophes(word.trim()).to_lowercase();
        if !word.is_empty() {
            self.edit_personal(|p| p.ignored.insert(word));
        }
    }

    /// Forgets all ignored words.
    pub fn clear_ignored(&self) {
        self.edit_personal(|p| p.ignored.clear());
    }

    // ------------------------------------------------------------ text

    /// Byte ranges of the misspelled words in one line of Markdown (without
    /// its line break), advancing `ctx` past the line.
    ///
    /// Call it for the lines of a document in order, starting from
    /// [`LineContext::new`]. Results are cached by line text and context
    /// (bounded, cleared when the word lists change), so calling it for every
    /// visible line on every frame is cheap.
    ///
    /// Not checked: front matter, fenced and indented code, `$$` math blocks,
    /// inline code and `$math$`, URLs, bare domains and file names, e-mail
    /// addresses, `[[wikilink]]` targets (aliases are), link destinations
    /// (link text is), HTML tags and comments, `#tags`, `@mentions`,
    /// `^block-ids`, footnote refs, callout types, words with digits or
    /// underscores, ALL-CAPS words, words with internal capitals, single
    /// letters, and words in scripts the dictionary does not cover.
    pub fn misspellings(&self, line: &str, ctx: &mut LineContext) -> Vec<Range<usize>> {
        let before = *ctx;
        if let Some((ranges, after)) = self.cache.get(line, before) {
            *ctx = after;
            return ranges;
        }
        let epoch = self.cache.epoch();
        let ranges = self.misspellings_uncached(line, ctx, &self.personal());
        self.cache.insert(line, before, *ctx, ranges.clone(), epoch);
        ranges
    }

    fn misspellings_uncached(&self, line: &str, ctx: &mut LineContext, personal: &Personal) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        if ctx.advance(line) {
            scan::for_each_word(line, &mut |range| {
                let word = &line[range.clone()];
                if self.scripts.covers(word) && !self.check_with(word, personal) {
                    out.push(range);
                }
            });
        }
        out
    }

    /// Every misspelling in a whole document as `(line index, byte range
    /// within that line)`. Lines are split on `\n`; a trailing `\r` is ignored.
    /// Bypasses the line cache.
    pub fn check_document(&self, text: &str) -> Vec<(usize, Range<usize>)> {
        let personal = self.personal();
        let mut ctx = LineContext::new();
        let mut out = Vec::new();
        for (n, line) in text.split('\n').enumerate() {
            out.extend(self.misspellings_uncached(line, &mut ctx, &personal).into_iter().map(|r| (n, r)));
        }
        out
    }

    /// Drops all cached line results.
    pub fn clear_cache(&self) {
        self.cache.clear();
    }
}

/// Re-orders dictionary suggestions so the most likely fixes come first:
/// a missing apostrophe (`dont` → `don't`), then fewest edits, then same letters in a different order (`tset` → `test`,
/// `ntoe` → `note`: transposed keys are the most common typo), then
/// same first letter, then single words over splits. Ties keep the
/// dictionary's order, which puts common-mistake replacements first.
fn rank(word: &str, suggestions: &mut [String]) {
    let lower = word.to_lowercase();
    let mut letters: Vec<char> = lower.chars().collect();
    letters.sort_unstable();
    let first = lower.chars().next();
    suggestions.sort_by_cached_key(|s| {
        let s_lower = s.to_lowercase();
        let distance = edit_distance(&lower, &s_lower, 3).unwrap_or(4);
        let mut s_letters: Vec<char> = s_lower.chars().collect();
        s_letters.sort_unstable();
        let only_apostrophe_added = s_lower.contains('\'') && s_lower.replace('\'', "") == lower;
        (!only_apostrophe_added, distance, s_letters != letters, s_lower.chars().next() != first, s.contains([' ', '-']))
    });
}

/// Optimal-string-alignment distance (insert, delete, substitute, swap
/// adjacent), or `None` if it exceeds `limit`.
pub(crate) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    let width = b.len() + 1;
    let mut rows = vec![0usize; width * (a.len() + 1)];
    for (j, cell) in rows.iter_mut().take(width).enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        rows[i * width] = i;
        let mut row_min = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (rows[(i - 1) * width + j] + 1).min(rows[i * width + j - 1] + 1).min(rows[(i - 1) * width + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(rows[(i - 2) * width + j - 2] + 1);
            }
            rows[i * width + j] = d;
            row_min = row_min.min(d);
        }
        if row_min > limit {
            return None;
        }
    }
    Some(rows[a.len() * width + b.len()]).filter(|&d| d <= limit)
}

/// `café` → `cafe`, `naïve` → `naive`; `None` if nothing changed.
fn fold_accents(word: &str) -> Option<String> {
    let mut out = String::with_capacity(word.len());
    let mut changed = false;
    for c in word.chars() {
        if scan::is_mark(c) {
            changed = true;
            continue;
        }
        match fold_char(c) {
            Some(s) => {
                out.push_str(s);
                changed = true;
            }
            None => out.push(c),
        }
    }
    changed.then_some(out)
}

fn fold_char(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' => "A",
        'æ' => "ae",
        'Æ' => "Ae",
        'ç' | 'ć' | 'č' => "c",
        'Ç' | 'Ć' | 'Č' => "C",
        'ď' => "d",
        'Ď' => "D",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ė' | 'Ę' | 'Ě' => "E",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => "i",
        'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' | 'Į' => "I",
        'ñ' | 'ń' | 'ň' => "n",
        'Ñ' | 'Ń' | 'Ň' => "N",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ő' => "O",
        'œ' => "oe",
        'Œ' => "Oe",
        'ř' => "r",
        'Ř' => "R",
        'ś' | 'š' | 'ş' => "s",
        'Ś' | 'Š' | 'Ş' => "S",
        'ť' => "t",
        'Ť' => "T",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' | 'Ů' | 'Ű' => "U",
        'ý' | 'ÿ' => "y",
        'Ý' | 'Ÿ' => "Y",
        'ź' | 'ż' | 'ž' => "z",
        'Ź' | 'Ż' | 'Ž' => "Z",
        _ => return None,
    })
}

/// The writing systems a dictionary's words use, so text in other scripts
/// (say, Devanagari in an English note) is left alone instead of flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scripts(Vec<u32>);

impl Scripts {
    fn of(aff: &str, dic: &str) -> Self {
        let mut set = BTreeSet::new();
        let mut add = |text: &str| set.extend(text.chars().filter(|c| c.is_alphabetic()).map(script_of));
        for line in aff.lines() {
            if let Some(letters) = line.strip_prefix("TRY ") {
                add(letters);
            }
        }
        if dic.is_ascii() {
            set.insert(LATIN);
        } else {
            // Skip the count line; take each entry's word, not its flags or morphology.
            for line in dic.lines().skip(1) {
                let word = line.split(['/', '\t', ' ']).next().unwrap_or_default();
                add(word);
            }
        }
        Self(set.into_iter().collect())
    }

    /// True if `word` has letters and all of them are in scripts the
    /// dictionary uses.
    fn covers(&self, word: &str) -> bool {
        let mut any = false;
        for c in word.chars().filter(|c| c.is_alphabetic()) {
            if self.0.binary_search(&script_of(c)).is_err() {
                return false;
            }
            any = true;
        }
        any
    }
}

const LATIN: u32 = 0;

/// A coarse script id: Latin, Greek and Cyrillic (spread over several Unicode
/// blocks) get fixed ids; everything else is identified by its 128-code-point
/// block, which is enough to tell scripts apart.
fn script_of(c: char) -> u32 {
    let cp = u32::from(c);
    match cp {
        0..=0x024F | 0x1E00..=0x1EFF | 0x2C60..=0x2C7F | 0xA720..=0xA7FF | 0xAB30..=0xAB6F | 0xFF21..=0xFF5A => LATIN,
        0x0370..=0x03FF | 0x1F00..=0x1FFF => 1,
        0x0400..=0x052F | 0x1C80..=0x1C8F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => 2,
        _ => 0x100 + (cp >> 7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance() {
        assert_eq!(edit_distance("tset", "test", 2), Some(1));
        assert_eq!(edit_distance("ntoe", "note", 2), Some(1));
        assert_eq!(edit_distance("kitten", "sitting", 2), None);
        assert_eq!(edit_distance("", "ab", 2), Some(2));
    }

    #[test]
    fn folding() {
        assert_eq!(fold_accents("café").as_deref(), Some("cafe"));
        assert_eq!(fold_accents("cafe\u{301}").as_deref(), Some("cafe"));
        assert_eq!(fold_accents("plain"), None);
    }

    #[test]
    fn scripts() {
        let s = Scripts::of("TRY abc", "1\nhello/S");
        assert!(s.covers("héllo"));
        assert!(!s.covers("नमस्ते"));
        assert!(!s.covers("привет"));
        assert!(!s.covers("''"));
        let ru = Scripts::of("SET UTF-8", "1\nпривет/S");
        assert!(ru.covers("привет"));
    }
}
