// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Extractive summary: pick the most central sentences, keep original order.
//!
//! Sentences are scored by the document frequency of their content words
//! (Luhn-style), normalised by sqrt(length) so long sentences don't win by
//! size alone, with a small bonus for early sentences.

use std::collections::HashMap;

use quire_index::tokenize::content_tokens;
use unicode_segmentation::UnicodeSegmentation;

pub fn summarize(plain: &str, max_sentences: usize) -> Vec<String> {
    let sentences: Vec<&str> =
        plain.split('\n').flat_map(|para| para.unicode_sentences()).map(str::trim).filter(|s| s.split_whitespace().count() >= 4).collect();
    if sentences.len() <= max_sentences {
        return sentences.into_iter().map(str::to_string).collect();
    }
    let mut freq: HashMap<String, f32> = HashMap::new();
    for s in &sentences {
        for t in content_tokens(s) {
            *freq.entry(t).or_default() += 1.0;
        }
    }
    let n = sentences.len() as f32;
    let mut scored: Vec<(usize, f32)> = sentences
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let toks: Vec<String> = content_tokens(s).collect();
            let sum: f32 = toks.iter().map(|t| freq[t] - 1.0).sum();
            let position = 1.0 + 0.3 * (1.0 - i as f32 / n);
            // Headings and fragments rarely make good summary sentences.
            let complete = if s.ends_with(['.', '!', '?', '…']) { 1.0 } else { 0.5 };
            (i, sum / (toks.len().max(1) as f32).sqrt() * position * complete)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut keep: Vec<usize> = scored.into_iter().take(max_sentences).map(|(i, _)| i).collect();
    keep.sort_unstable();
    keep.into_iter().map(|i| sentences[i].to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_central_sentences_in_order() {
        let text = "Ropes make text editors fast. My cat likes the sun today. \
                    A rope stores text in a balanced tree of chunks. \
                    Editors use ropes because inserting text into a rope is logarithmic. \
                    Lunch was a sandwich with cheese and tomato.";
        let s = summarize(text, 2);
        assert_eq!(s.len(), 2);
        assert!(s.iter().all(|x| x.to_lowercase().contains("rope")));
        assert!(summarize("Too short.", 3).is_empty());
    }
}
