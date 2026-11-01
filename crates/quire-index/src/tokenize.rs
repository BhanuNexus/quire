// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use unicode_segmentation::UnicodeSegmentation;

/// Lowercased, lightly stemmed Unicode words. Works for Latin, Cyrillic,
/// Devanagari, etc.; CJK text is split per ideograph by UAX #29.
pub fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.unicode_words().map(|w| stem(&w.to_lowercase()))
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

/// Tokens worth using as content features (no stopwords, no 1-char Latin noise, no pure numbers).
pub fn content_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.unicode_words().map(str::to_lowercase).filter_map(|t| {
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
}
