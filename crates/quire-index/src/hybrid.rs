// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hybrid retrieval: BM25 keywords fused with semantic similarity by
//! reciprocal-rank fusion, so exact matches stay on top while passages that
//! only match by meaning still surface.

use std::collections::HashMap;

use quire_vault::NotePath;

use crate::actor::IndexService;
use crate::fulltext::{Query, snippet};

/// RRF constant; 60 is the value from the original paper and works well.
const RRF_K: f32 = 60.0;
/// Minimum cosine for a passage to appear without any keyword match.
const MEANING_THRESHOLD: f32 = 0.2;

#[derive(Debug, Clone, PartialEq)]
pub struct HybridHit {
    pub path: NotePath,
    pub title: String,
    pub score: f32,
    pub snippet: String,
    pub highlights: Vec<std::ops::Range<usize>>,
    /// Section of the best-matching passage.
    pub heading: Option<String>,
    /// Byte offset of the best passage (jump target).
    pub offset: Option<usize>,
    /// Found by meaning only, not by keywords.
    pub by_meaning: bool,
}

fn passage_snippet(text: &str, width: usize) -> String {
    // Drop the "Title — Heading" context line added for embedding.
    let body = text.split_once('\n').map_or(text, |(_, b)| b).replace('\n', " ");
    if body.chars().count() <= width {
        return body;
    }
    let cut: String = body.chars().take(width).collect();
    format!("{}…", cut.trim_end())
}

impl IndexService {
    /// Keyword + meaning search over notes. Falls back to keywords only
    /// while the semantic index is still building.
    pub fn hybrid_search(&self, query: &str, limit: usize) -> Vec<HybridHit> {
        let q = Query::parse(query);
        if q.is_empty() {
            return Vec::new();
        }
        let ix = self.read();
        let keyword = ix.search(query, 50);
        let mut fused: HashMap<NotePath, HybridHit> = HashMap::new();
        for (rank, h) in keyword.into_iter().enumerate() {
            let score = 1.0 / (RRF_K + rank as f32 + 1.0);
            fused.insert(
                h.path.clone(),
                HybridHit {
                    path: h.path,
                    title: h.title,
                    score,
                    snippet: h.snippet,
                    highlights: h.highlights,
                    heading: None,
                    offset: None,
                    by_meaning: false,
                },
            );
        }
        // Phrases are an explicit request for exact wording; skip meaning.
        if q.phrases.is_empty()
            && let Some(sem) = self.semantic()
        {
            let text_query: String = q.terms.iter().chain(q.prefix.iter()).cloned().collect::<Vec<_>>().join(" ");
            let mut seen: HashMap<NotePath, usize> = HashMap::new();
            for (ci, sim) in sem.search(&text_query, 80) {
                let c = &sem.chunks[ci];
                let Some(rec) = ix.get(&c.path) else { continue };
                if !q.tags.iter().all(|t| rec.has_tag(t)) {
                    continue;
                }
                let folded = crate::tokenize::fold_str(&c.text);
                if q.exclude.iter().any(|x| folded.contains(x.as_str())) {
                    continue;
                }
                if seen.contains_key(&c.path) {
                    continue; // best passage per note only
                }
                let rank = seen.len();
                seen.insert(c.path.clone(), rank);
                // Weak semantic matches shouldn't outrank solid keyword hits.
                let score = sim.clamp(0.0, 1.0) / (RRF_K + rank as f32 + 1.0);
                match fused.get_mut(&c.path) {
                    Some(hit) => {
                        hit.score += score;
                        hit.heading.clone_from(&c.heading);
                        hit.offset = Some(c.range.start);
                    }
                    None if sim >= MEANING_THRESHOLD => {
                        let (snip, hl) = snippet(&passage_snippet(&c.text, 400), &q.highlight_terms(), 140);
                        fused.insert(
                            c.path.clone(),
                            HybridHit {
                                path: c.path.clone(),
                                title: rec.title.clone(),
                                score,
                                snippet: snip,
                                highlights: hl,
                                heading: c.heading.clone(),
                                offset: Some(c.range.start),
                                by_meaning: true,
                            },
                        );
                    }
                    None => {}
                }
            }
        }
        let mut out: Vec<HybridHit> = fused.into_values().collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_vault::Vault;

    #[test]
    fn keyword_hits_lead_and_meaning_fills_in() {
        let vault = Vault::in_memory(&[
            ("Cars.md", "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road."),
            ("Autos.md", "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long."),
            ("Bread.md", "# Bread\nSourdough bread needs flour, water and a slow rise in a warm oven."),
            ("Baking.md", "# Baking\nBaking cakes needs flour, sugar and an oven at the right heat."),
            ("Trucks.md", "# Trucks\nA truck is a heavy vehicle; its diesel engine hauls loads by road."),
        ]);
        let svc = IndexService::spawn();
        svc.load(vault);
        svc.flush_semantic();
        let hits = svc.hybrid_search("automobile", 5);
        assert_eq!(hits[0].path.as_str(), "Autos.md");
        assert!(!hits[0].by_meaning);
        let meaning: Vec<&str> = hits.iter().filter(|h| h.by_meaning).map(|h| h.path.as_str()).collect();
        assert!(meaning.contains(&"Cars.md") || meaning.contains(&"Trucks.md"), "{hits:#?}");
        assert!(!meaning.contains(&"Bread.md"));
        assert!(svc.hybrid_search("\"slow rise\"", 5).iter().all(|h| !h.by_meaning));
    }
}
