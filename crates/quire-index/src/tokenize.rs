// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Words for search, and diacritic folding (design §C.2).
//!
//! [`fold`] makes text comparable regardless of accents and case: NFKD,
//! then diacritical marks dropped, then lowercase, plus the letters that
//! don't decompose (ß→ss, æ→ae, œ→oe, ø→o, ł→l, đ→d, ı→i). It returns a map
//! back to the original, so snippets and highlights land on the exact text
//! the person wrote. Only marks from the diacritical blocks are dropped:
//! vowel signs of Indic scripts, Hebrew and Arabic points, kana voicing and
//! Hangul are left as they are, so those words keep their meaning. CJK text
//! is untouched.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// Combining marks that are accents on letters (dropped by folding).
const fn is_diacritic(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Characters whose compatibility decomposition is worth taking: Latin,
/// Greek, Cyrillic and other alphabets, ligatures and full-width forms. CJK,
/// kana and Hangul syllables keep their form.
const fn decomposes(c: char) -> bool {
    matches!(c as u32, 0x00A0..=0x2FFF | 0xFB00..=0xFB4F | 0xFF00..=0xFFEF)
}

/// Letters NFKD leaves alone, folded to their usual ASCII spelling.
const fn special(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' | 'ẞ' => "ss",
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        'ø' | 'Ø' => "o",
        'ł' | 'Ł' => "l",
        'đ' | 'Đ' => "d",
        'ı' => "i",
        _ => return None,
    })
}

/// Fold one char, calling `emit` for each folded char.
fn fold_char(c: char, emit: &mut impl FnMut(char)) {
    if c.is_ascii() {
        emit(c.to_ascii_lowercase());
        return;
    }
    let mut one = |d: char| {
        if is_diacritic(d) {
            return;
        }
        for l in d.to_lowercase() {
            if is_diacritic(l) {
                continue;
            }
            match special(l) {
                Some(s) => s.chars().for_each(&mut *emit),
                None => emit(l),
            }
        }
    };
    if let Some(s) = special(c) {
        s.chars().for_each(&mut *emit);
    } else if decomposes(c) {
        unicode_normalization::char::decompose_compatible(c, &mut one);
    } else {
        one(c);
    }
}

/// `s` folded, with a map from each folded char's index to the index of
/// the original char it came from (design §C.2).
pub fn fold(s: &str) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        fold_char(c, &mut |f| {
            out.push(f);
            map.push(i);
        });
    }
    (out, map)
}

/// `s` folded, without the map.
pub fn fold_str(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        fold_char(c, &mut |f| out.push(f));
    }
    out
}

/// Folded text that remembers where each byte came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Folded {
    pub text: String,
    /// For each byte of `text`, the byte range of the original char it came from.
    origin: Vec<(u32, u32)>,
}

impl Folded {
    pub fn new(s: &str) -> Self {
        let mut text = String::with_capacity(s.len());
        let mut origin = Vec::with_capacity(s.len());
        for (at, c) in s.char_indices() {
            let span = (u32::try_from(at).unwrap_or(u32::MAX), u32::try_from(at + c.len_utf8()).unwrap_or(u32::MAX));
            fold_char(c, &mut |f| {
                text.push(f);
                origin.extend(std::iter::repeat_n(span, f.len_utf8()));
            });
        }
        Self { text, origin }
    }

    /// The original byte range a range of the folded text came from. Both
    /// ends land on char boundaries of the original; a range that starts or
    /// ends inside one folded-from char (the second `s` of "ß"→"ss") takes
    /// the whole char.
    pub fn original(&self, r: Range<usize>) -> Range<usize> {
        let Some(&(start, _)) = self.origin.get(r.start) else {
            let end = self.origin.last().map_or(0, |o| o.1 as usize);
            return end..end;
        };
        let end = if r.end > r.start { self.origin.get(r.end - 1).map_or(start, |o| o.1) } else { start };
        start as usize..end as usize
    }

    /// Byte ranges in the original of every match of `needle` (already folded).
    pub fn find_all(&self, needle: &str) -> Vec<Range<usize>> {
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = self.text[from..].find(needle) {
            let s = from + i;
            out.push(self.original(s..s + needle.len()));
            from = s + needle.len();
        }
        out
    }
}

/// Folded, lightly stemmed Unicode words. Works for Latin, Cyrillic,
/// Devanagari, etc.; CJK text is split per ideograph by UAX #29.
pub fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.unicode_words().map(|w| stem(&fold_str(w)))
}

/// Conservative English plural folding so "ropes" finds "rope" and
/// "libraries" finds "library". Non-ASCII words are left untouched.
pub fn stem(w: &str) -> String {
    if !w.is_ascii() || w.len() <= 3 {
        return w.to_string();
    }
    if let Some(base) = w.strip_suffix("ies").filter(|b| b.len() >= 2) {
        return format!("{base}y");
    }
    for suf in ["sses", "shes", "ches", "xes", "zes"] {
        if w.ends_with(suf) {
            return w[..w.len() - 2].to_string();
        }
    }
    if w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "because", "been", "but", "by", "can", "could",
    "do", "does", "for", "from", "get", "had", "has", "have", "he", "her", "his", "how", "i", "if", "in", "into", "is", "it", "its",
    "just", "like", "may", "me", "more", "most", "my", "no", "not", "now", "of", "on", "one", "only", "or", "other", "our", "out", "over",
    "she", "so", "some", "such", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "through",
    "to", "too", "up", "us", "use", "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "why", "will", "with",
    "would", "you", "your",
];

pub fn is_stopword(t: &str) -> bool {
    STOPWORDS.binary_search(&t).is_ok()
}

/// Tokens worth using as content features (no stopwords, no 1-char Latin
/// noise, no pure numbers), folded like [`tokens`].
pub fn content_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.unicode_words().map(fold_str).filter_map(|t| {
        let short_ascii = t.len() < 3 && t.is_ascii();
        (!short_ascii && !is_stopword(&t) && !t.chars().all(|c| c.is_ascii_digit())).then(|| stem(&t))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_sorted() {
        let mut v = STOPWORDS.to_vec();
        v.sort_unstable();
        assert_eq!(v, STOPWORDS);
    }

    #[test]
    fn tokenizes_unicode() {
        let t: Vec<_> = tokens("Rope-based Editors, नमस्ते दुनिया").collect();
        assert_eq!(t, vec!["rope", "based", "editor", "नमस्ते", "दुनिया"]);
        let s: Vec<_> = ["ropes", "libraries", "boxes", "class", "status", "analysis", "branches", "bus"].iter().map(|w| stem(w)).collect();
        assert_eq!(s, vec!["rope", "library", "box", "class", "status", "analysis", "branch", "bus"]);
        let c: Vec<_> = content_tokens("The ropes is in a B-tree 2026 does this").collect();
        assert_eq!(c, vec!["rope", "tree"]);
    }

    #[test]
    fn folds_diacritics_and_special_letters() {
        let cases = [
            ("Café", "cafe"),
            ("Ångström", "angstrom"),
            ("Straße", "strasse"),
            ("Łódź", "lodz"),
            ("Tiếng Việt có dấu", "tieng viet co dau"),
            ("Ærø Œuvre đi ı", "aero oeuvre di i"),
            ("ﬁnal ＡＢＣ", "final abc"),
            ("İstanbul", "istanbul"),
            ("Ελληνικά", "ελληνικα"),
            ("Ёлка", "елка"),
            ("東京 タワー 한국어", "東京 タワー 한국어"),
            ("नमस्ते", "नमस्ते"),
            ("が", "が"),
        ];
        for (s, want) in cases {
            assert_eq!(fold_str(s), want, "{s}");
            assert_eq!(fold(s).0, want, "{s}");
        }
        let words: Vec<_> = tokens("Cafés in Łódź").collect();
        assert_eq!(words, ["cafe", "in", "lodz"]);
    }

    #[test]
    fn maps_back_to_the_original() {
        let (f, map) = fold("aßé");
        assert_eq!(f, "asse");
        assert_eq!(map, [0, 1, 1, 2]);
        let src = "Die Straße in Łódź, Tiếng Việt.";
        let folded = Folded::new(src);
        for (needle, want) in [("strasse", "Straße"), ("lodz", "Łódź"), ("viet", "Việt"), ("s", "S")] {
            let r = folded.find_all(needle)[0].clone();
            assert_eq!(&src[r], want);
        }
        // Half of "ß" takes the whole char.
        let r = folded.find_all("sse")[0].clone();
        assert_eq!(&src[r], "ße");
        // Every range stays in bounds and on char boundaries.
        for a in 0..=folded.text.len() {
            for b in a..=folded.text.len() {
                let r = folded.original(a..b);
                assert!(r.start <= r.end && r.end <= src.len() && src.is_char_boundary(r.start) && src.is_char_boundary(r.end));
            }
        }
        assert_eq!(Folded::new("").original(0..0), 0..0);
    }
}
