// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unlinked mentions and link suggestions (Obsidian's "unlinked mentions",
//! Roam's automatic linking — but as suggestions, never silent edits).

use std::ops::Range;

use quire_index::Index;
use quire_markdown::verbatim_ranges;
use quire_vault::NotePath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSuggestion {
    /// Byte range in the source of the mention.
    pub range: Range<usize>,
    pub text: String,
    pub target: NotePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkedMention {
    pub from: NotePath,
    pub range: Range<usize>,
    pub context: String,
}

const MIN_NAME_CHARS: usize = 4;

fn blocked_ranges(src: &str, index: &Index, path: Option<&NotePath>) -> Vec<Range<usize>> {
    let mut blocked = verbatim_ranges(src);
    // Existing links and headings shouldn't be suggested again.
    let ex = match path.and_then(|p| index.get(p)) {
        Some(rec) => rec.links.iter().map(|l| l.range.clone()).collect::<Vec<_>>(),
        None => quire_markdown::extract(src).links.into_iter().map(|l| l.range).collect(),
    };
    blocked.extend(ex);
    if let Some(fm) = quire_markdown::frontmatter::parse(src) {
        blocked.push(fm.range);
    }
    for line_start in std::iter::once(0).chain(src.match_indices('\n').map(|(i, _)| i + 1)) {
        if src[line_start..].starts_with('#') {
            let end = src[line_start..].find('\n').map_or(src.len(), |e| line_start + e);
            blocked.push(line_start..end);
        }
    }
    blocked.sort_by_key(|r| r.start);
    blocked
}

fn is_blocked(blocked: &[Range<usize>], r: &Range<usize>) -> bool {
    blocked.iter().any(|b| b.start < r.end && r.start < b.end)
}

fn word_boundary(src: &str, r: &Range<usize>) -> bool {
    let before = src[..r.start].chars().next_back();
    let after = src[r.end..].chars().next();
    before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
}

/// Case-insensitive occurrences of `needle` (ASCII-case folding keeps byte offsets stable).
fn find_all(hay_lower: &str, needle_lower: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay_lower[from..].find(needle_lower) {
        let s = from + i;
        out.push(s..s + needle_lower.len());
        from = s + needle_lower.len();
    }
    out
}

/// Names of other notes that appear unlinked in `src`.
pub fn suggest_links(src: &str, index: &Index, self_path: Option<&NotePath>) -> Vec<LinkSuggestion> {
    let lower = src.to_ascii_lowercase();
    let blocked = blocked_ranges(src, index, self_path);
    let mut out: Vec<LinkSuggestion> = Vec::new();
    // Longer names first so "Rope Notes" wins over "Rope".
    let mut names: Vec<(String, NotePath)> = index
        .records()
        .filter(|r| Some(&r.path) != self_path)
        .flat_map(|r| std::iter::once(r.name.clone()).chain(r.aliases.iter().cloned()).map(move |n| (n, r.path.clone())))
        .filter(|(n, _)| n.chars().count() >= MIN_NAME_CHARS)
        .collect();
    names.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
    for (name, target) in names {
        for r in find_all(&lower, &name.to_ascii_lowercase()) {
            if !word_boundary(src, &r) || is_blocked(&blocked, &r) || out.iter().any(|s| s.range.start < r.end && r.start < s.range.end) {
                continue;
            }
            out.push(LinkSuggestion { text: src[r.clone()].to_string(), range: r, target: target.clone() });
        }
    }
    out.sort_by_key(|s| s.range.start);
    out
}

/// Other notes that mention `target`'s name (or aliases) without linking it.
pub fn unlinked_mentions(index: &Index, target: &NotePath) -> Vec<UnlinkedMention> {
    let Some(rec) = index.get(target) else {
        return Vec::new();
    };
    let names: Vec<String> = std::iter::once(rec.name.clone())
        .chain(rec.aliases.iter().cloned())
        .filter(|n| n.chars().count() >= MIN_NAME_CHARS)
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let mut out = Vec::new();
    for other in index.records().filter(|r| &r.path != target) {
        let Some(src) = index.source(&other.path) else {
            continue;
        };
        let lower = src.to_ascii_lowercase();
        if !names.iter().any(|n| lower.contains(n.as_str())) {
            continue;
        }
        let blocked = blocked_ranges(src, index, Some(&other.path));
        for n in &names {
            for r in find_all(&lower, n) {
                if word_boundary(src, &r) && !is_blocked(&blocked, &r) {
                    let ls = src[..r.start].rfind('\n').map_or(0, |i| i + 1);
                    let le = src[r.end..].find('\n').map_or(src.len(), |i| r.end + i);
                    out.push(UnlinkedMention { from: other.path.clone(), range: r, context: src[ls..le].trim().to_string() });
                }
            }
        }
    }
    out.sort_by(|a, b| a.from.cmp(&b.from).then(a.range.start.cmp(&b.range.start)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix() -> Index {
        let mut ix = Index::new();
        ix.upsert(NotePath::new("Rope.md"), "# Rope\nA data structure.");
        ix.upsert(NotePath::new("Rope Notes.md"), "---\naliases: [rope journal]\n---\nnotes");
        ix.upsert(
            NotePath::new("Draft.md"),
            "# Rope\nI read Rope Notes and the rope journal. Also [[Rope]] linked. `rope` code. Ropes plural.",
        );
        ix
    }

    #[test]
    fn suggests_longest_unlinked_names() {
        let ix = ix();
        let src = ix.source(&NotePath::new("Draft.md")).unwrap().to_string();
        let s = suggest_links(&src, &ix, Some(&NotePath::new("Draft.md")));
        let got: Vec<_> = s.iter().map(|s| (s.text.as_str(), s.target.as_str())).collect();
        assert_eq!(got, vec![("Rope Notes", "Rope Notes.md"), ("rope journal", "Rope Notes.md")]);
    }

    #[test]
    fn finds_unlinked_mentions() {
        let ix = ix();
        let m = unlinked_mentions(&ix, &NotePath::new("Rope Notes.md"));
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(|x| x.from.as_str() == "Draft.md"));
    }
}
