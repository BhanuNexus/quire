// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pandoc-style citations (design §G.3): `[@key]`, `[@key, p. 4]`,
//! `[see @a; @b]`, `[-@key]` (without the author) and `@key` in prose.
//!
//! [`scan`] finds them outside code, math, HTML and link targets.
//! [`render`] rewrites the ones whose keys a [`Formatter`] knows into
//! author-date text and adds a `## References` list; unknown keys stay as
//! written, so nothing is lost.

use std::ops::Range;

/// One work cited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteItem {
    pub key: String,
    /// Text before the key inside brackets (`see`).
    pub prefix: String,
    /// Text after the key (`p. 4`), without the separating comma.
    pub locator: String,
    /// `-@key`: the author is already named in the sentence.
    pub suppress_author: bool,
}

/// A citation as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// Byte range in the source, brackets included.
    pub range: Range<usize>,
    pub items: Vec<CiteItem>,
    /// `@key` in prose (author named in the text) rather than `[@key]`.
    pub in_text: bool,
}

/// Whether `c` may be part of a key (Pandoc: alphanumerics, `_`, and
/// internal punctuation `:.#$%&-+?<>~/`).
fn key_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '#' | '$' | '%' | '&' | '-' | '+' | '?' | '<' | '>' | '~' | '/')
}

/// The key at the start of `s` (after `@`): `{braced}` or bare, without
/// trailing punctuation. Returns the key and its length in bytes.
fn key_at(s: &str) -> Option<(String, usize)> {
    if let Some(rest) = s.strip_prefix('{') {
        let end = rest.find('}')?;
        let key = rest[..end].trim();
        return (!key.is_empty() && !key.contains('\n')).then(|| (key.to_string(), end + 2));
    }
    let first = s.chars().next()?;
    if !(first.is_alphanumeric() || first == '_') {
        return None;
    }
    let mut end = s.find(|c: char| !key_char(c)).unwrap_or(s.len());
    // Trailing punctuation ends a sentence, not the key.
    while end > 0 {
        let last = s[..end].chars().next_back()?;
        if last.is_alphanumeric() || last == '_' {
            break;
        }
        end -= last.len_utf8();
    }
    (end > 0).then(|| (s[..end].to_string(), end))
}

/// Parse the inside of `[...]` as citation items, if it is one.
fn bracket_items(inner: &str) -> Option<Vec<CiteItem>> {
    let mut items = Vec::new();
    for part in inner.split(';') {
        let at = part.find('@')?;
        let mut prefix = part[..at].trim().to_string();
        let mut suppress = false;
        if prefix.ends_with('-') {
            suppress = true;
            prefix.pop();
            prefix = prefix.trim().to_string();
        }
        // `foo@bar` inside a word is an address, not a citation.
        if part[..at].chars().next_back().is_some_and(char::is_alphanumeric) {
            return None;
        }
        let (key, len) = key_at(&part[at + 1..])?;
        let rest = part[at + 1 + len..].trim();
        let locator = rest.strip_prefix(',').unwrap_or(rest).trim().to_string();
        items.push(CiteItem { key, prefix, locator, suppress_author: suppress });
    }
    (!items.is_empty()).then_some(items)
}

/// Every citation in `src`, in order, outside code, math, HTML, front
/// matter and link targets.
pub fn scan(src: &str) -> Vec<Citation> {
    if !src.contains('@') {
        return Vec::new();
    }
    let verbatim = crate::scan::verbatim_ranges(src);
    let skip = |at: usize| verbatim.iter().any(|r| r.contains(&at));
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let b = bytes[i];
        if b == b'[' && !skip(i) {
            // `[[wikilinks]]` are not citations.
            if src[i..].starts_with("[[") {
                i += src[i + 2..].find("]]").map_or(2, |e| e + 4);
                continue;
            }
            if let Some(close) = src[i + 1..].find([']', '\n', '[']).filter(|c| src.as_bytes()[i + 1 + c] == b']')
                && let Some(items) = bracket_items(&src[i + 1..i + 1 + close])
            {
                let end = i + close + 2;
                // `[@key](url)` is a link whose text starts with @.
                if !src[end..].starts_with('(') {
                    out.push(Citation { range: i..end, items, in_text: false });
                    i = end;
                    continue;
                }
            }
        } else if b == b'@' && !skip(i) {
            let before = src[..i].chars().next_back();
            if before.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '"' | '“' | '*' | '_'))
                && let Some((key, len)) = key_at(&src[i + 1..])
            {
                let mut end = i + 1 + len;
                let mut locator = String::new();
                // `@key [p. 33]`: a locator for an in-text citation.
                let rest = &src[end..];
                if let Some(l) = rest.strip_prefix(" [")
                    && let Some(c) = l.find(']')
                    && !l[..c].contains(['@', '\n', '['])
                {
                    locator = l[..c].trim().to_string();
                    end += 2 + c + 1;
                }
                out.push(Citation {
                    range: i..end,
                    items: vec![CiteItem { key, prefix: String::new(), locator, suppress_author: false }],
                    in_text: true,
                });
                i = end;
                continue;
            }
        }
        i += src[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// The citation at byte `at` (for hover, Ctrl+click and completion).
pub fn at(src: &str, at: usize) -> Option<Citation> {
    scan(src).into_iter().find(|c| c.range.start <= at && at <= c.range.end)
}

/// The keys cited in `src`, first use first, each once.
pub fn keys(src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in scan(src) {
        for i in c.items {
            if !out.contains(&i.key) {
                out.push(i.key);
            }
        }
    }
    out
}

/// What [`render`] needs to know about the library.
pub trait Formatter {
    /// Whether `key` is in the library.
    fn has(&self, key: &str) -> bool;
    /// `Lovelace` or `Lovelace and Babbage` or `Lovelace et al.`.
    fn authors(&self, key: &str) -> String;
    /// `1843`, `1843a` or `n.d.`.
    fn year(&self, key: &str) -> String;
    /// The reference-list entry, Markdown.
    fn reference(&self, key: &str) -> String;
    /// The order of the reference list: a sort key.
    fn sort_key(&self, key: &str) -> String;
}

fn item_text(f: &dyn Formatter, i: &CiteItem) -> String {
    let mut s = String::new();
    if !i.prefix.is_empty() {
        s.push_str(&i.prefix);
        s.push(' ');
    }
    if !i.suppress_author {
        s.push_str(&f.authors(&i.key));
        s.push(' ');
    }
    s.push_str(&f.year(&i.key));
    if !i.locator.is_empty() {
        s.push_str(", ");
        s.push_str(&i.locator);
    }
    s
}

/// A citation as author-date text: `(Lovelace 1843, p. 4; Babbage 1864)`,
/// or `Lovelace (1843)` in prose.
pub fn format_citation(f: &dyn Formatter, c: &Citation) -> String {
    if c.in_text {
        let i = &c.items[0];
        let inner = if i.locator.is_empty() { f.year(&i.key) } else { format!("{}, {}", f.year(&i.key), i.locator) };
        return format!("{} ({inner})", f.authors(&i.key));
    }
    format!("({})", c.items.iter().map(|i| item_text(f, i)).collect::<Vec<_>>().join("; "))
}

/// `src` with known citations formatted and a reference list at the end.
///
/// The list goes under an existing `References` or `Bibliography` heading
/// when the note ends with one, else under a new `## References`. Unknown
/// keys and citations in code stay as written. Returns `src` unchanged when
/// nothing is cited.
pub fn render(src: &str, f: &dyn Formatter) -> String {
    let cites = scan(src);
    let known: Vec<&Citation> = cites.iter().filter(|c| c.items.iter().all(|i| f.has(&i.key))).collect();
    if known.is_empty() {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len() + 512);
    let mut pos = 0;
    let mut used: Vec<String> = Vec::new();
    for c in &known {
        out.push_str(&src[pos..c.range.start]);
        out.push_str(&escape_md(&format_citation(f, c)));
        pos = c.range.end;
        for i in &c.items {
            if !used.contains(&i.key) {
                used.push(i.key.clone());
            }
        }
    }
    out.push_str(&src[pos..]);
    used.sort_by_key(|k| f.sort_key(k));
    let list = used.iter().fold(String::new(), |mut s, k| {
        s.push_str(&f.reference(k));
        s.push_str("\n\n");
        s
    });
    let has_heading = out.lines().rev().find(|l| !l.trim().is_empty()).is_some_and(|l| {
        l.starts_with('#') && matches!(l.trim_start_matches('#').trim().to_lowercase().as_str(), "references" | "bibliography")
    });
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if has_heading {
        out.push_str("\n\n");
    } else {
        out.push_str("\n\n## References\n\n");
    }
    out.push_str(list.trim_end());
    out.push('\n');
    out
}

/// Keep Markdown from reading formatted text as emphasis or links.
fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '_' | '[' | ']' | '`' | '<' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lib;
    impl Formatter for Lib {
        fn has(&self, key: &str) -> bool {
            matches!(key, "ada" | "babbage1864")
        }
        fn authors(&self, key: &str) -> String {
            if key == "ada" { "Lovelace".into() } else { "Babbage".into() }
        }
        fn year(&self, key: &str) -> String {
            if key == "ada" { "1843".into() } else { "1864".into() }
        }
        fn reference(&self, key: &str) -> String {
            format!("{} ref", self.authors(key))
        }
        fn sort_key(&self, key: &str) -> String {
            self.authors(key)
        }
    }

    fn keys_of(c: &Citation) -> Vec<&str> {
        c.items.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn scans_the_pandoc_forms() {
        let src = "As [@ada, p. 4] and [see @ada; -@babbage1864] show, @ada [p. 7] said it.\nMail me at x@y.org. `[@code]` [[Note]] [@link](https://x)";
        let c = scan(src);
        assert_eq!(c.len(), 3, "{c:#?}");
        assert_eq!(&src[c[0].range.clone()], "[@ada, p. 4]");
        assert_eq!(c[0].items[0].locator, "p. 4");
        assert_eq!(keys_of(&c[1]), ["ada", "babbage1864"]);
        assert_eq!(c[1].items[0].prefix, "see");
        assert!(c[1].items[1].suppress_author);
        assert!(c[2].in_text);
        assert_eq!(&src[c[2].range.clone()], "@ada [p. 7]");
        assert_eq!(c[2].items[0].locator, "p. 7");
        assert_eq!(
            scan("End of a sentence @ada."),
            vec![Citation {
                range: 18..22,
                items: c[2].items.iter().map(|i| CiteItem { locator: String::new(), ..i.clone() }).collect(),
                in_text: true
            }]
        );
        assert_eq!(keys("[@{weird key}] @ada"), ["weird key", "ada"]);
        assert!(scan("```\n[@ada]\n```\n$@ada$").is_empty());
        assert!(scan("---\nauthor: \"@ada\"\n---\nText").is_empty(), "front matter is skipped");
        assert_eq!(at("x [@ada] y", 4).map(|c| c.range), Some(2..8));
    }

    #[test]
    fn renders_known_citations_and_a_reference_list() {
        let src = "# Notes\n\nUndo is cheap [@ada, p. 4; @babbage1864]. @ada showed it. Unknown [@nobody] stays.\n";
        let out = render(src, &Lib);
        assert!(out.contains("Undo is cheap (Lovelace 1843, p. 4; Babbage 1864)."), "{out}");
        assert!(out.contains("Lovelace (1843) showed it."));
        assert!(out.contains("Unknown [@nobody] stays."));
        assert!(out.ends_with("## References\n\nBabbage ref\n\nLovelace ref\n"), "{out}");
        let with_heading = render("Text [@ada].\n\n## Bibliography\n", &Lib);
        assert!(with_heading.ends_with("## Bibliography\n\nLovelace ref\n"), "{with_heading}");
        assert_eq!(render("No citations here.", &Lib), "No citations here.");
        let star = render("[-@ada]", &Lib);
        assert!(star.starts_with("(1843)"), "{star}");
    }
}
