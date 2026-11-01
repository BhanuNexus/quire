// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tag suggestions: reuse the vault's own vocabulary instead of inventing tags.

use std::collections::HashMap;

use quire_index::Index;
use quire_index::tokenize::tokens;
use quire_vault::NotePath;

use crate::related::related_notes;

/// Existing vault tags that fit this note, best first, with a 0–1 confidence.
pub fn suggest_tags(index: &Index, path: &NotePath, limit: usize) -> Vec<(String, f32)> {
    let Some(rec) = index.get(path) else {
        return Vec::new();
    };
    let have: std::collections::HashSet<String> = rec.tags.iter().map(|t| t.to_lowercase()).collect();
    let total_terms: f32 = rec.terms.values().sum::<u32>().max(1) as f32;
    let mut score: HashMap<String, f32> = HashMap::new();

    // 1. The tag's words appear in the note.
    for (tag, count) in index.tags() {
        if have.contains(&tag) || count == 0 {
            continue;
        }
        let leaf = tag.rsplit('/').next().unwrap_or(&tag);
        let hits: u32 = tokens(leaf).map(|t| rec.terms.get(&t).copied().unwrap_or(0)).sum();
        if hits > 0 {
            *score.entry(tag).or_default() += (hits as f32 / total_terms * 20.0).min(0.6);
        }
    }
    // 2. Similar notes carry the tag.
    for rel in related_notes(index, path, 8) {
        if let Some(other) = index.get(&rel.path) {
            for t in &other.tags {
                let t = t.to_lowercase();
                if !have.contains(&t) {
                    *score.entry(t).or_default() += rel.score * 0.8;
                }
            }
        }
    }
    let mut v: Vec<(String, f32)> = score.into_iter().map(|(t, s)| (t, s.min(1.0))).filter(|(_, s)| *s > 0.08).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_existing_tags() {
        let mut ix = Index::new();
        ix.upsert(NotePath::new("A.md"), "Rope data structures for editors. #editors #cs/datastructures");
        ix.upsert(NotePath::new("B.md"), "Cooking pasta tonight. #food");
        ix.upsert(NotePath::new("New.md"), "Building editors: ropes versus piece tables for editors.");
        let s = suggest_tags(&ix, &NotePath::new("New.md"), 3);
        assert_eq!(s.first().map(|x| x.0.as_str()), Some("editors"));
        assert!(!s.iter().any(|(t, _)| t == "food"));
    }
}
