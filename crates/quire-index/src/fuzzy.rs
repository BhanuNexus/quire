// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quick-open: fzf-class fuzzy matching over note names, paths and aliases.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use quire_vault::NotePath;

use crate::record::NoteRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickOpenHit {
    pub path: NotePath,
    pub title: String,
    pub score: u32,
    /// Char indices into `path` that matched, for highlighting.
    pub indices: Vec<u32>,
}

pub(crate) fn quick_open<'a>(records: impl Iterator<Item = &'a NoteRecord>, query: &str, limit: usize) -> Vec<QuickOpenHit> {
    let mut config = Config::DEFAULT.match_paths();
    config.normalize = true;
    let mut matcher = Matcher::new(config);
    // Folding the query too makes accents optional both ways (§C.2): "lodz"
    // and "łódź" both find "Łódź".
    let pattern = Pattern::parse(&crate::tokenize::fold_str(query), CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut hits: Vec<QuickOpenHit> = Vec::new();
    for rec in records {
        if query.trim().is_empty() {
            hits.push(QuickOpenHit { path: rec.path.clone(), title: rec.title.clone(), score: 0, indices: vec![] });
            continue;
        }
        let mut indices = Vec::new();
        let mut path_score = pattern.indices(Utf32Str::new(rec.path.without_ext(), &mut buf), &mut matcher, &mut indices);
        // Letters nucleo doesn't normalize (Ł, ß, ø…): match the folded path
        // and map the matched chars back.
        if path_score.is_none() && !rec.path.without_ext().is_ascii() {
            let (folded, map) = crate::tokenize::fold(rec.path.without_ext());
            let mut fi = Vec::new();
            path_score = pattern.indices(Utf32Str::new(&folded, &mut buf), &mut matcher, &mut fi);
            indices = fi.into_iter().filter_map(|i| map.get(i as usize).copied()).collect();
        }
        // Name matches rank above folder matches; aliases count as names.
        let name_score = std::iter::once(rec.name.as_str())
            .chain(rec.title.as_str().split('\n').take(1))
            .chain(rec.aliases.iter().map(String::as_str))
            .filter_map(|s| {
                pattern.score(Utf32Str::new(s, &mut buf), &mut matcher).or_else(|| {
                    (!s.is_ascii())
                        .then(|| crate::tokenize::fold_str(s))
                        .and_then(|f| pattern.score(Utf32Str::new(&f, &mut buf), &mut matcher))
                })
            })
            .max()
            .map(|s| s + s / 2);
        if let Some(score) = path_score.into_iter().chain(name_score).max() {
            indices.sort_unstable();
            indices.dedup();
            hits.push(QuickOpenHit { path: rec.path.clone(), title: rec.title.clone(), score, indices });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.as_str().len().cmp(&b.path.as_str().len())));
    hits.truncate(limit);
    hits
}
