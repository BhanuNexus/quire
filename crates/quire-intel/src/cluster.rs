// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Evidence clusters for ask-before-tell (design §C.4).
//!
//! The retrieved passages grouped by meaning, each group named by its most
//! distinctive words, so the person sees what the notes say before the
//! answer does.

use std::collections::{HashMap, HashSet};

use quire_index::Index;
use quire_index::semantic::SemanticIndex;
use quire_index::tokenize::content_tokens;

use crate::ask::Source;

/// Passages at least this similar (average cosine) end up together.
pub const MIN_SIM: f32 = 0.5;
/// Never more groups than this.
pub const MAX_CLUSTERS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    /// Up to three words that set this group apart.
    pub label: Vec<String>,
    /// Indices into the sources, best-ranked first.
    pub members: Vec<usize>,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-9 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

/// Average-linkage agglomerative clustering of unit vectors.
///
/// Merges the closest pair while it is at least `min_sim` similar or there
/// are more than `max` groups. Groups keep their members in input order and
/// come out ordered by their first member.
pub fn agglomerate(vectors: &[Vec<f32>], min_sim: f32, max: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = (0..vectors.len()).map(|i| vec![i]).collect();
    let link = |a: &[usize], b: &[usize]| -> f32 {
        let sum: f32 = a.iter().flat_map(|&i| b.iter().map(move |&j| (i, j))).map(|(i, j)| dot(&vectors[i], &vectors[j])).sum();
        sum / (a.len() * b.len()).max(1) as f32
    };
    while groups.len() > 1 {
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..groups.len() {
            for j in i + 1..groups.len() {
                let s = link(&groups[i], &groups[j]);
                if best.is_none_or(|b| s > b.2) {
                    best = Some((i, j, s));
                }
            }
        }
        let Some((i, j, s)) = best else { break };
        if s < min_sim && groups.len() <= max.max(1) {
            break;
        }
        let g = groups.remove(j);
        groups[i].extend(g);
        groups[i].sort_unstable();
    }
    groups.sort_by_key(|g| g[0]);
    groups
}

/// Group the sources of an answer and name each group.
pub fn evidence_clusters(question: &str, sources: &[Source], semantic: Option<&SemanticIndex>, ix: &Index) -> Vec<Cluster> {
    if sources.is_empty() {
        return Vec::new();
    }
    // Vectors: the semantic index's, or word counts when there is none.
    let vocab: Vec<String> = {
        let mut v: Vec<String> = sources.iter().flat_map(|s| content_tokens(s.body())).collect::<HashSet<_>>().into_iter().collect();
        v.sort();
        v
    };
    let vectors: Vec<Vec<f32>> = sources
        .iter()
        .map(|s| {
            if let Some(sem) = semantic {
                sem.vector_of(&s.path, s.offset).map_or_else(|| sem.embed_query(&s.text), <[f32]>::to_vec)
            } else {
                let toks: Vec<String> = content_tokens(s.body()).collect();
                unit(vocab.iter().map(|w| toks.iter().filter(|t| *t == w).count() as f32).collect())
            }
        })
        .map(unit)
        .collect();
    let groups = agglomerate(&vectors, MIN_SIM, MAX_CLUSTERS);
    let asked: HashSet<String> = content_tokens(question).collect();
    let counts: Vec<HashMap<String, f32>> = sources
        .iter()
        .map(|s| {
            let mut m = HashMap::new();
            for t in content_tokens(s.body()) {
                *m.entry(t).or_insert(0.0) += 1.0;
            }
            m
        })
        .collect();
    groups
        .iter()
        .map(|g| {
            let inside: HashSet<usize> = g.iter().copied().collect();
            let mut score: HashMap<&str, f32> = HashMap::new();
            for (i, c) in counts.iter().enumerate() {
                let (sign, n) = if inside.contains(&i) { (1.0, g.len()) } else { (-1.0, sources.len() - g.len()) };
                for (t, f) in c {
                    *score.entry(t.as_str()).or_insert(0.0) += sign * f / n.max(1) as f32;
                }
            }
            let mut terms: Vec<(&str, f32)> = score
                .into_iter()
                .filter(|(t, s)| *s > 0.0 && !asked.contains(*t) && t.chars().count() > 2)
                .map(|(t, s)| (t, s * ix.idf(t).unwrap_or(0.0).max(0.1)))
                .collect();
            terms.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
            Cluster { label: terms.into_iter().take(3).map(|(t, _)| t.to_string()).collect(), members: g.clone() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_index::chunk::chunk_note;
    use quire_index::semantic::build_lsa_index;
    use quire_vault::NotePath;

    #[test]
    fn merges_close_vectors_and_caps_groups() {
        let v = vec![vec![1.0, 0.0], vec![0.9, 0.1], vec![0.0, 1.0], vec![0.1, 0.9]].into_iter().map(unit).collect::<Vec<_>>();
        assert_eq!(agglomerate(&v, 0.5, 4), vec![vec![0, 1], vec![2, 3]]);
        assert_eq!(agglomerate(&v, 0.5, 1).len(), 1);
        let far: Vec<Vec<f32>> = (0..6).map(|i| (0..6).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect();
        assert_eq!(agglomerate(&far, 0.5, 4).len(), 4, "at most four groups");
        assert!(agglomerate(&[], 0.5, 4).is_empty());
    }

    #[test]
    fn groups_sources_by_topic_with_labels() {
        let mut ix = Index::new();
        for (p, s) in [
            ("Ropes.md", "# Ropes\nRopes balance trees of text for editors and share subtrees for undo."),
            ("Ropes 2.md", "# Ropes 2\nA rope keeps text in balanced trees; editors share subtrees for undo history."),
            ("Pricing.md", "# Pricing\nThe editor licence costs money; pricing changed this year with discounts for students."),
        ] {
            ix.upsert(NotePath::new(p), s);
        }
        let chunks = ix.paths().iter().flat_map(|p| chunk_note(p, ix.source(p).unwrap())).collect();
        let sem = build_lsa_index(chunks);
        let sources = crate::ask::retrieve(&ix, Some(&sem), "editor", 6);
        let clusters = evidence_clusters("editor", &sources, Some(&sem), &ix);
        assert!(clusters.len() >= 2, "{clusters:?}");
        let pricing = clusters.iter().find(|c| c.members.iter().any(|&m| sources[m].path.as_str() == "Pricing.md")).unwrap();
        assert!(pricing.members.iter().all(|&m| sources[m].path.as_str() == "Pricing.md"));
        assert!(!pricing.label.is_empty() && pricing.label.len() <= 3);
        assert!(!pricing.label.iter().any(|t| t == "editor"), "words of the question aren't labels");
        assert!(evidence_clusters("x", &[], None, &ix).is_empty());
    }
}
