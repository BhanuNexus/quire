// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Section-level search for agents and the panel (design §C.1).
//!
//! [`IndexService::search_sections`] returns hits with the heading and line
//! range they came from, so a caller can read just that part of a note.
//! Today each hit is the note-level hybrid hit mapped to the section of its
//! best passage; WS-C replaces this with chunk-level fusion and the full
//! filter set behind the same signature.

use quire_vault::NotePath;

use crate::actor::IndexService;

/// Which documents to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKind {
    #[default]
    Notes,
    Attachments,
    All,
}

/// Narrow a search. Empty fields don't filter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchFilters {
    /// Folder prefix, e.g. `Projects/` (case-insensitive).
    pub folder: Option<String>,
    /// Every tag must be present (nested tags match their parent).
    pub tags: Vec<String>,
    /// A `quire-view` filter expression. Accepted, not applied yet (WS-C).
    pub where_: Option<String>,
    /// Unix ms bounds on the note's modified time. Accepted, not applied yet (WS-C).
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub kind: SearchKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionHit {
    pub path: NotePath,
    /// The section's heading text, `None` before the first heading.
    pub heading: Option<String>,
    /// 1-based inclusive line range of the section.
    pub lines: (u32, u32),
    pub snippet: String,
    pub score: f32,
    pub by_meaning: bool,
}

impl IndexService {
    /// Search, returning the matching section of each hit.
    pub fn search_sections(&self, query: &str, filters: &SearchFilters, limit: usize) -> Vec<SectionHit> {
        if filters.kind == SearchKind::Attachments {
            return Vec::new();
        }
        let folder = filters.folder.as_deref().map(|f| f.trim_matches('/').to_lowercase()).filter(|f| !f.is_empty());
        let hits = self.hybrid_search(query, limit.saturating_mul(4).max(limit));
        let ix = self.read();
        hits.into_iter()
            .filter(|h| {
                folder.as_ref().is_none_or(|f| {
                    let p = h.path.as_str().to_lowercase();
                    p.strip_prefix(f.as_str()).is_some_and(|rest| rest.starts_with('/'))
                })
            })
            .filter(|h| ix.get(&h.path).is_some_and(|r| filters.tags.iter().all(|t| r.has_tag(t))))
            .take(limit)
            .map(|h| {
                let src = ix.source(&h.path).unwrap_or_default();
                let (heading, lines) = section_at(src, h.offset);
                SectionHit {
                    heading: h.heading.clone().or(heading),
                    lines,
                    snippet: h.snippet,
                    score: h.score,
                    by_meaning: h.by_meaning,
                    path: h.path,
                }
            })
            .collect()
    }
}

/// The section containing byte `offset`: from its heading line to the line
/// before the next heading of the same or a higher level. Without an
/// offset, the whole note.
fn section_at(src: &str, offset: Option<usize>) -> (Option<String>, (u32, u32)) {
    let total = u32::try_from(src.lines().count().max(1)).unwrap_or(u32::MAX);
    let Some(offset) = offset else { return (None, (1, total)) };
    let line_of = |byte: usize| u32::try_from(src[..byte.min(src.len())].matches('\n').count() + 1).unwrap_or(u32::MAX);
    let headings = quire_markdown::outline(src);
    let Some(i) = headings.iter().rposition(|h| h.offset <= offset) else {
        let end = headings.first().map_or(total, |h| line_of(h.offset).saturating_sub(1).max(1));
        return (None, (1, end));
    };
    let h = &headings[i];
    let end = headings[i + 1..].iter().find(|n| n.level <= h.level).map_or(total, |n| line_of(n.offset).saturating_sub(1));
    let start = line_of(h.offset);
    (Some(h.text.clone()), (start, end.max(start)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_map_to_lines() {
        let src = "intro\n# A\none\n## A1\ntwo\n# B\nthree\n";
        assert_eq!(section_at(src, None), (None, (1, 7)));
        assert_eq!(section_at(src, Some(1)), (None, (1, 1)));
        assert_eq!(section_at(src, Some(src.find("one").unwrap())), (Some("A".into()), (2, 5)));
        assert_eq!(section_at(src, Some(src.find("two").unwrap())), (Some("A1".into()), (4, 5)));
        assert_eq!(section_at(src, Some(src.find("three").unwrap())), (Some("B".into()), (6, 7)));
    }

    #[test]
    fn filters_by_folder_and_tag() {
        let v = quire_vault::Vault::in_memory(&[
            ("Projects/Rope.md", "# Rope\nropes make undo cheap #cs\n"),
            ("Other/Rope two.md", "# Rope two\nropes everywhere\n"),
        ]);
        let svc = IndexService::spawn();
        svc.load(v);
        svc.flush();
        let all = svc.search_sections("ropes", &SearchFilters::default(), 10);
        assert_eq!(all.len(), 2);
        let f = SearchFilters { folder: Some("projects/".into()), ..Default::default() };
        let hits = svc.search_sections("ropes", &f, 10);
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["Projects/Rope.md"]);
        assert_eq!(hits[0].lines, (1, 2));
        let f = SearchFilters { tags: vec!["cs".into()], ..Default::default() };
        assert_eq!(svc.search_sections("ropes", &f, 10).len(), 1);
        let f = SearchFilters { kind: SearchKind::Attachments, ..Default::default() };
        assert!(svc.search_sections("ropes", &f, 10).is_empty());
    }
}
