// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Related notes" by TF-IDF cosine similarity over content terms.

use std::collections::HashMap;

use quire_index::semantic::SemanticIndex;
use quire_index::{Index, NoteRecord};
use quire_vault::NotePath;

#[derive(Debug, Clone, PartialEq)]
pub struct RelatedNote {
    pub path: NotePath,
    pub title: String,
    pub score: f32,
    /// Already linked in either direction.
    pub linked: bool,
    /// The strongest shared terms — the "why".
    pub because: Vec<String>,
}

fn doc_freq(index: &Index) -> HashMap<&str, u32> {
    let mut df: HashMap<&str, u32> = HashMap::new();
    for r in index.records() {
        for t in r.terms.keys() {
            *df.entry(t.as_str()).or_default() += 1;
        }
    }
    df
}

pub(crate) fn tfidf<'a>(rec: &'a NoteRecord, df: &HashMap<&str, u32>, n: f32) -> HashMap<&'a str, f32> {
    let mut v: HashMap<&str, f32> = rec
        .terms
        .iter()
        .map(|(t, &tf)| {
            let d = df.get(t.as_str()).copied().unwrap_or(1) as f32;
            (t.as_str(), (1.0 + (tf as f32).ln()) * ((n + 1.0) / (d + 0.5)).ln())
        })
        .collect();
    let norm = v.values().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.values_mut() {
            *x /= norm;
        }
    }
    v
}

pub fn related_notes(index: &Index, path: &NotePath, limit: usize) -> Vec<RelatedNote> {
    related_notes_with(index, None, path, limit)
}

/// Related notes blending word overlap (TF-IDF) with meaning (semantic index)
/// when one is available. `because` always lists the shared words.
pub fn related_notes_with(index: &Index, semantic: Option<&SemanticIndex>, path: &NotePath, limit: usize) -> Vec<RelatedNote> {
    let sem: HashMap<NotePath, f32> = semantic.map(|s| s.related(path, 30).into_iter().collect()).unwrap_or_default();
    let Some(me) = index.get(path) else {
        return Vec::new();
    };
    let df = doc_freq(index);
    let n = index.len() as f32;
    let mine = tfidf(me, &df, n);
    if mine.is_empty() {
        return Vec::new();
    }
    let graph = index.graph();
    let linked: std::collections::HashSet<&NotePath> =
        graph.outgoing(path).iter().chain(graph.backlinks(path).iter().map(|b| &b.from)).collect();

    let mut out: Vec<RelatedNote> = index
        .records()
        .filter(|r| &r.path != path)
        .filter_map(|r| {
            let theirs = tfidf(r, &df, n);
            let mut shared: Vec<(&str, f32)> = mine.iter().filter_map(|(t, a)| theirs.get(t).map(|b| (*t, a * b))).collect();
            let lexical: f32 = shared.iter().map(|(_, s)| s).sum();
            let score = match sem.get(&r.path) {
                Some(m) => 0.5 * lexical + 0.5 * m,
                None if semantic.is_some() => 0.5 * lexical,
                None => lexical,
            };
            if score < 0.05 {
                return None;
            }
            shared.sort_by(|a, b| b.1.total_cmp(&a.1));
            Some(RelatedNote {
                path: r.path.clone(),
                title: r.title.clone(),
                score,
                linked: linked.contains(&r.path),
                because: shared.into_iter().take(3).map(|(t, _)| t.to_string()).collect(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_topical_neighbours() {
        let mut ix = Index::new();
        ix.upsert(NotePath::new("Ropes.md"), "Ropes are balanced trees for text editors. Rope insert is logarithmic.");
        ix.upsert(NotePath::new("Piece Table.md"), "A piece table is another text editor buffer. Editors like VS Code use it.");
        ix.upsert(NotePath::new("Bread.md"), "Sourdough bread needs flour, water, salt and patience.");
        ix.upsert(NotePath::new("Gap Buffer.md"), "Gap buffers power Emacs. A text buffer with a gap. See [[Ropes]].");
        let rel = related_notes(&ix, &NotePath::new("Ropes.md"), 5);
        assert!(!rel.is_empty());
        assert!(rel.iter().all(|r| r.path.as_str() != "Bread.md"));
        assert!(rel.iter().any(|r| r.path.as_str() == "Gap Buffer.md" && r.linked));
        assert!(rel[0].because.iter().any(|t| t.starts_with("text") || t.starts_with("editor")));
    }
}
