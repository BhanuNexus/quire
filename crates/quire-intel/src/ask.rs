// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ask your vault: retrieve passages, then answer with citations.
//!
//! Retrieval fuses meaning (semantic index) with keywords (BM25 per term).
//! With a language model the passages become numbered sources it must cite;
//! offline, an extractive answer quotes the best-matching sentences and cites
//! where each came from.

use std::collections::{HashMap, HashSet};

use quire_index::Index;
use quire_index::chunk::chunk_note;
use quire_index::semantic::SemanticIndex;
use quire_index::tokenize::content_tokens;
use quire_vault::NotePath;
use unicode_segmentation::UnicodeSegmentation;

use crate::llm::LlmRequest;

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// 1-based number used in citations.
    pub n: usize,
    pub path: NotePath,
    pub heading: Option<String>,
    /// Byte offset of the passage in its note.
    pub offset: usize,
    pub text: String,
    pub score: f32,
}

impl Source {
    /// `[[Note#Heading]]`, or `[[Note]]` when the heading is the title.
    pub fn citation(&self) -> String {
        let stem = self.path.stem();
        match &self.heading {
            Some(h) if h != stem => format!("[[{stem}#{h}]]"),
            _ => format!("[[{stem}]]"),
        }
    }

    fn body(&self) -> &str {
        self.text.split_once('\n').map_or(self.text.as_str(), |(_, b)| b)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub markdown: String,
    pub sources: Vec<Source>,
    /// "extractive" or the model name.
    pub by: String,
}

/// Find the passages most likely to answer `question`.
pub fn retrieve(ix: &Index, semantic: Option<&SemanticIndex>, question: &str, k: usize) -> Vec<Source> {
    let q_terms: Vec<String> = {
        let mut seen = HashSet::new();
        content_tokens(question).filter(|t| seen.insert(t.clone())).take(10).collect()
    };
    // (path, offset) → (heading, text, score)
    let mut cand: HashMap<(NotePath, usize), (Option<String>, String, f32)> = HashMap::new();

    if let Some(sem) = semantic {
        for (ci, sim) in sem.search(question, 24) {
            let c = &sem.chunks[ci];
            cand.insert((c.path.clone(), c.range.start), (c.heading.clone(), c.text.clone(), sim));
        }
    }

    // Keywords, OR-combined: notes matching more question terms rank higher.
    let mut note_hits: HashMap<NotePath, usize> = HashMap::new();
    for t in &q_terms {
        for h in ix.search(&format!("{t} "), 12) {
            *note_hits.entry(h.path).or_default() += 1;
        }
    }
    let mut notes: Vec<(NotePath, usize)> = note_hits.into_iter().collect();
    notes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (path, _) in notes.into_iter().take(6) {
        let Some(src) = ix.source(&path) else { continue };
        for c in chunk_note(&path, src) {
            let toks: HashSet<String> = content_tokens(&c.text).collect();
            let overlap = q_terms.iter().filter(|t| toks.contains(*t)).count();
            if overlap == 0 {
                continue;
            }
            let kw = 0.8 * overlap as f32 / q_terms.len().max(1) as f32;
            cand.entry((c.path.clone(), c.range.start)).and_modify(|e| e.2 += kw).or_insert((c.heading, c.text, kw));
        }
    }

    let mut list: Vec<_> = cand.into_iter().collect();
    list.sort_by(|a, b| b.1.2.total_cmp(&a.1.2).then_with(|| a.0.cmp(&b.0)));
    list.into_iter()
        .take(k)
        .enumerate()
        .map(|(i, ((path, offset), (heading, text, score)))| Source { n: i + 1, path, heading, offset, text, score })
        .collect()
}

/// Sentences worth quoting: complete statements, not headings, tag lines or link lists.
fn answer_like(sent: &str) -> bool {
    let words: Vec<&str> = sent.split_whitespace().collect();
    if words.len() < 5 {
        return false;
    }
    let tags = words.iter().filter(|w| w.starts_with('#')).count();
    if tags * 4 > words.len() {
        return false;
    }
    let punctuated = sent.ends_with(['.', '!', '?', '…', ':', ';']);
    punctuated || words.len() >= 8
}

/// Offline answer: the sentences that best match the question, each cited.
pub fn extractive_answer(question: &str, sources: &[Source], semantic: Option<&SemanticIndex>) -> String {
    let q: HashSet<String> = content_tokens(question).collect();
    if q.is_empty() || sources.is_empty() {
        return "I couldn't find anything about that in your notes.".into();
    }
    let qv = semantic.map(|s| s.embed_query(question));
    let mut scored: Vec<(f32, String, String)> = Vec::new();
    for s in sources {
        for sent in s.body().split('\n').flat_map(|p| p.unicode_sentences()).map(str::trim) {
            if !answer_like(sent) {
                continue;
            }
            let toks: Vec<String> = content_tokens(sent).collect();
            if toks.len() < 3 {
                continue;
            }
            let hits = toks.iter().filter(|t| q.contains(*t)).collect::<HashSet<_>>().len() as f32;
            let lexical = hits / (toks.len() as f32).sqrt();
            let meaning = match (&qv, semantic) {
                (Some(qv), Some(sem)) => {
                    let mut v = sem.embed_query(sent);
                    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                    v.iter_mut().for_each(|x| *x /= n);
                    v.iter().zip(qv).map(|(a, b)| a * b).sum::<f32>().max(0.0)
                }
                _ => 0.0,
            };
            let score = lexical + 0.6 * meaning + 0.05 * s.score;
            if hits > 0.0 || meaning > 0.35 {
                scored.push((score, sent.to_string(), s.citation()));
            }
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    let lines: Vec<String> =
        scored.into_iter().filter(|(_, s, _)| seen.insert(s.clone())).take(3).map(|(_, s, c)| format!("- {s} {c}")).collect();
    if lines.is_empty() {
        return "I couldn't find anything about that in your notes.".into();
    }
    format!("From your notes:\n\n{}", lines.join("\n"))
}

/// Prompt a language model to answer only from the sources, with citations.
pub fn build_ask_request(question: &str, sources: &[Source]) -> LlmRequest {
    let mut ctx = String::new();
    for s in sources {
        ctx.push_str(&format!("[{}] {}\n{}\n\n", s.n, s.citation(), s.body().chars().take(3000).collect::<String>()));
    }
    LlmRequest {
        system: "You answer questions using only the user's notes provided as sources. After each claim, cite the source \
                 with its wikilink exactly as given, e.g. [[Note#Heading]]. If the sources don't contain the answer, say so \
                 plainly. Reply in concise Markdown."
            .into(),
        prompt: format!("Question: {question}\n\nSources:\n\n{ctx}"),
        max_tokens: 900,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_index::semantic::build_lsa_index;

    fn ix() -> Index {
        let mut ix = Index::new();
        ix.upsert(
            NotePath::new("Rope Notes.md"),
            "# Rope Notes\n## Undo\nRopes make undo cheap because unchanged subtrees are shared between versions.\n\n## Lines\nEach node stores line counts so jumping to a line is fast.",
        );
        ix.upsert(NotePath::new("Bread.md"), "# Bread\nSourdough needs a slow rise and a hot oven.");
        ix.upsert(NotePath::new("Editors.md"), "# Editors\nZed and Helix store text in ropes; VS Code uses a piece table.");
        ix
    }

    #[test]
    fn retrieves_and_answers_with_citations() {
        let ix = ix();
        let chunks = ix.paths().iter().flat_map(|p| chunk_note(p, ix.source(p).unwrap())).collect();
        let sem = build_lsa_index(chunks);
        let sources = retrieve(&ix, Some(&sem), "why is undo cheap with ropes?", 4);
        assert_eq!(sources[0].citation(), "[[Rope Notes#Undo]]");
        assert!(sources.iter().all(|s| s.path.as_str() != "Bread.md"));
        let ans = extractive_answer("why is undo cheap with ropes?", &sources, Some(&sem));
        assert!(ans.contains("subtrees are shared") && ans.contains("[[Rope Notes#Undo]]"), "{ans}");
        assert!(extractive_answer("quantum chromodynamics", &[], None).contains("couldn't find"));
        assert!(!answer_like("How Editors Store Text"));
        assert!(!answer_like("Related: Rope Notes · #reading/cs #editors"));
        assert!(answer_like("Edits touch only the path from the root to one leaf"));
        let req = build_ask_request("why?", &sources);
        assert!(req.prompt.contains("[1] [[Rope Notes#Undo]]"));
    }
}
