// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Flashcards written in notes (design §E.6).
//!
//! Three forms, in notes tagged `#flashcards` or with `srs: true`:
//!
//! * `Question :: Answer` on one line;
//! * a question paragraph, a line with only `?`, then the answer, ending at
//!   a blank line;
//! * clozes: each `==highlight==` in a paragraph is a card that hides it.
//!
//! A card's id is its `^block-id` (clozes add `:1`, `:2`…), or else
//! `k_` + 12 hex digits of a hash of its folded question. [`add_ids`] writes
//! `^k-…` ids so edits to the question keep the card's history.

use std::ops::Range;

use crate::scan::{Inline, scan_with, verbatim_ranges};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// `Question :: Answer`
    Basic,
    /// Question, `?`, answer.
    MultiLine,
    /// `==hidden==` in a paragraph.
    Cloze,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub kind: CardKind,
    /// Shown first. For a cloze: the paragraph with the hidden part as `[…]`.
    pub question: String,
    /// Shown after Space. For a cloze: the hidden text.
    pub answer: String,
    /// For a cloze: the whole paragraph without highlight marks.
    pub context: String,
    /// 0-based lines the card spans.
    pub lines: Range<usize>,
    /// The card's `^id`, if written.
    pub block_id: Option<String>,
    /// Byte offset where [`add_ids`] puts ` ^id` (end of the id line).
    pub id_at: usize,
}

/// Lower case, single spaces, no markup characters that don't change meaning.
pub fn fold(s: &str) -> String {
    s.split_whitespace().map(|w| w.trim_matches(['*', '_', '`']).to_lowercase()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 12 hex digits of the FNV-1a hash of the folded text.
pub fn hash12(text: &str) -> String {
    format!("{:016x}", fnv64(&fold(text)))[..12].to_string()
}

/// The card id for a `^block-id` (`^k-3fa1…` is `k_3fa1…`).
fn id_from_block(block: &str) -> String {
    match block.strip_prefix("k-") {
        Some(hex) if hex.len() == 12 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => format!("k_{hex}"),
        _ => block.to_string(),
    }
}

/// A `^id` at the end of a line: (start of ` ^id`, the id).
fn trailing_block_id(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_end();
    let caret = t.rfind('^')?;
    let id = &t[caret + 1..];
    let ok =
        (caret == 0 || t[..caret].ends_with([' ', '\t'])) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| (t[..caret].trim_end().len(), id))
}

fn strip_bullet(s: &str) -> &str {
    let t = s.trim_start();
    for b in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(b) {
            return r.trim_start();
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(r) = t[digits..].strip_prefix(". ").or_else(|| t[digits..].strip_prefix(") "))
    {
        return r.trim_start();
    }
    t
}

/// The `::` that splits a one-line card, outside `[…]` and `(…)`.
fn basic_split(line: &str) -> Option<usize> {
    let mut depth = 0i32;
    let b = line.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i + 1 < b.len() {
        match b[i] {
            b'[' | b'(' => depth += 1,
            b']' | b')' => depth -= 1,
            b':' if b[i + 1] == b':' && depth == 0 => {
                if found.is_some() || b.get(i + 2) == Some(&b':') {
                    return None; // `:::` or two splits: not a card
                }
                found = Some(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    found
}

struct LineInfo<'a> {
    start: usize,
    /// Without the newline.
    text: &'a str,
}

/// Every card in a note, in order.
pub fn cards(src: &str) -> Vec<Card> {
    let verbatim = verbatim_ranges(src);
    let in_verbatim = |a: usize, b: usize| verbatim.iter().any(|r| r.start < b.max(a + 1) && a < r.end);
    let mut lines = Vec::new();
    let mut pos = 0;
    for raw in src.split_inclusive('\n') {
        lines.push(LineInfo { start: pos, text: raw.trim_end_matches(['\n', '\r']) });
        pos += raw.len();
    }
    let highlights: Vec<Range<usize>> = scan_with(src, &verbatim)
        .into_iter()
        .filter_map(|i| match i {
            Inline::Highlight { range } => Some(range),
            _ => None,
        })
        .collect();
    let mut out: Vec<Card> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // A paragraph: consecutive non-blank lines outside code and front matter.
        let usable = |l: &LineInfo<'_>| !l.text.trim().is_empty() && !in_verbatim(l.start, l.start + l.text.len());
        if !usable(&lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && usable(&lines[i]) {
            i += 1;
        }
        let para = &lines[start..i];
        if let Some(q) = para.iter().position(|l| l.text.trim() == "?").filter(|q| *q > 0 && *q + 1 < para.len()) {
            let last = &para[para.len() - 1];
            let (id_at, block) = match trailing_block_id(last.text) {
                Some((at, id)) => (last.start + at, Some(id.to_string())),
                None => (last.start + last.text.trim_end().len(), None),
            };
            let question = para[..q].iter().map(|l| l.text.trim()).collect::<Vec<_>>().join("\n");
            let mut answer: Vec<&str> = para[q + 1..].iter().map(|l| l.text.trim()).collect();
            if let (Some(b), Some(a)) = (&block, answer.last_mut()) {
                *a = a.strip_suffix(&format!("^{b}")).unwrap_or(a).trim_end();
            }
            out.push(Card {
                id: block.as_deref().map_or_else(|| format!("k_{}", hash12(&question)), id_from_block),
                kind: CardKind::MultiLine,
                question,
                answer: answer.join("\n"),
                context: String::new(),
                lines: start..i,
                block_id: block,
                id_at,
            });
            continue;
        }
        let mut cloze_lines = Vec::new();
        for (k, l) in para.iter().enumerate() {
            let t = l.text.trim_start();
            if t.starts_with('#') && t.trim_start_matches('#').starts_with(' ') {
                continue; // headings aren't cards
            }
            let is_task = crate::tasks::marker_in_line(l.text).is_some();
            if !is_task && let Some(split) = basic_split(l.text) {
                let q = strip_bullet(&l.text[..split]).trim();
                let rest = &l.text[split + 2..];
                let (id_at, block) = match trailing_block_id(l.text) {
                    Some((at, id)) if at > split => (l.start + at, Some(id.to_string())),
                    _ => (l.start + l.text.trim_end().len(), None),
                };
                let a = match &block {
                    Some(b) => rest.trim().strip_suffix(&format!("^{b}")).unwrap_or(rest).trim(),
                    None => rest.trim(),
                };
                if !q.is_empty() && !a.is_empty() {
                    out.push(Card {
                        id: block.as_deref().map_or_else(|| format!("k_{}", hash12(q)), id_from_block),
                        kind: CardKind::Basic,
                        question: q.to_string(),
                        answer: a.to_string(),
                        context: String::new(),
                        lines: start + k..start + k + 1,
                        block_id: block,
                        id_at,
                    });
                    continue;
                }
            }
            cloze_lines.push(k);
        }
        // Clozes in the rest of the paragraph.
        let (pa, pb) = (para[0].start, para[para.len() - 1].start + para[para.len() - 1].text.len());
        let hl: Vec<&Range<usize>> = highlights
            .iter()
            .filter(|r| r.start >= pa && r.end <= pb)
            .filter(|r| cloze_lines.iter().any(|k| r.start >= para[*k].start && r.start < para[*k].start + para[*k].text.len()))
            .collect();
        if hl.is_empty() {
            continue;
        }
        let last = &para[para.len() - 1];
        let (id_at, block) = match trailing_block_id(last.text) {
            Some((at, id)) => (last.start + at, Some(id.to_string())),
            None => (last.start + last.text.trim_end().len(), None),
        };
        let body_end = if block.is_some() { id_at } else { pb };
        let render = |hide: Option<&Range<usize>>| {
            let mut s = String::new();
            let mut at = pa;
            for r in &hl {
                s.push_str(&src[at..r.start]);
                let inner = &src[r.start + 2..r.end - 2];
                if hide == Some(*r) {
                    s.push_str("[…]");
                } else {
                    s.push_str(inner);
                }
                at = r.end;
            }
            s.push_str(&src[at..body_end]);
            s.lines().map(|l| strip_bullet(l).trim_end()).collect::<Vec<_>>().join("\n")
        };
        let context = render(None);
        for (n, r) in hl.iter().enumerate() {
            let question = render(Some(r));
            out.push(Card {
                id: block.as_deref().map_or_else(|| format!("k_{}", hash12(&question)), |b| format!("{}:{}", id_from_block(b), n + 1)),
                kind: CardKind::Cloze,
                answer: src[r.start + 2..r.end - 2].to_string(),
                question,
                context: context.clone(),
                lines: start..i,
                block_id: block.clone(),
                id_at,
            });
        }
    }
    // The same question twice in a note: later copies get `-2`, `-3`…
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for c in &mut out {
        let n = seen.entry(c.id.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            c.id = format!("{}-{n}", c.id);
        }
    }
    out
}

/// What [`add_ids`] does: insertions into the note (offset, ` ^id`) and the
/// cards whose id changes (old, new), for the review log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddIds {
    pub inserts: Vec<(usize, String)>,
    pub renames: Vec<(String, String)>,
}

impl AddIds {
    /// The note with the ids written.
    pub fn apply(&self, src: &str) -> String {
        let mut s = src.to_string();
        for (at, text) in self.inserts.iter().rev() {
            s.insert_str(*at, text);
        }
        s
    }
}

/// Give every card without a `^id` one, so editing its question later
/// keeps its history.
///
/// Basic and multi-line cards get `^k-` + their current hash, so their id
/// doesn't change; a paragraph of clozes gets one id.
pub fn add_ids(src: &str) -> AddIds {
    let all = cards(src);
    let mut out = AddIds::default();
    let mut done_at = std::collections::HashSet::new();
    for c in all.iter().filter(|c| c.block_id.is_none()) {
        let block = match c.kind {
            CardKind::Basic | CardKind::MultiLine => format!("k-{}", hash12(&c.question)),
            CardKind::Cloze => format!("k-{}", hash12(&c.context)),
        };
        if done_at.insert(c.id_at) {
            out.inserts.push((c.id_at, format!(" ^{block}")));
        }
    }
    out.inserts.sort_by_key(|i| i.0);
    if out.inserts.is_empty() {
        return out;
    }
    let after = cards(&out.apply(src));
    for (old, new) in all.iter().zip(&after) {
        if old.id != new.id {
            out.renames.push((old.id.clone(), new.id.clone()));
        }
    }
    out
}

/// How alike two questions are, 0–1 (Dice coefficient of letter pairs of the folded text).
pub fn similarity(a: &str, b: &str) -> f64 {
    let pairs = |s: &str| -> Vec<(char, char)> {
        let c: Vec<char> = fold(s).chars().collect();
        c.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (mut x, y) = (pairs(a), pairs(b));
    if x.is_empty() && y.is_empty() {
        return if fold(a) == fold(b) { 1.0 } else { 0.0 };
    }
    let total = x.len() + y.len();
    let mut common = 0u32;
    for p in &y {
        if let Some(i) = x.iter().position(|q| q == p) {
            x.swap_remove(i);
            common += 1;
        }
    }
    2.0 * f64::from(common) / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\nsrs: true\nq: a :: b\n---\n# Biology :: notes\n\nWhat is ATP? :: The cell's energy currency\n- Mitosis :: Cell division ^mito\n\nName the powerhouse\nof the cell\n?\nThe mitochondrion\n\nThe ==nucleus== holds the ==DNA==.\n\n```\ncode :: not a card\n```\n\n- [ ] task [due:: 2026-12-01]\nkey:: value with (a::b)\nA ::: reversed\n";

    #[test]
    fn finds_all_three_forms() {
        let c = cards(NOTE);
        let kinds: Vec<(CardKind, &str, &str)> = c.iter().map(|c| (c.kind, c.question.as_str(), c.answer.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (CardKind::Basic, "What is ATP?", "The cell's energy currency"),
                (CardKind::Basic, "Mitosis", "Cell division"),
                (CardKind::MultiLine, "Name the powerhouse\nof the cell", "The mitochondrion"),
                (CardKind::Cloze, "The […] holds the DNA.", "nucleus"),
                (CardKind::Cloze, "The nucleus holds the […].", "DNA"),
                (CardKind::Basic, "key", "value with (a::b)"),
            ]
        );
        assert_eq!(c[1].id, "mito");
        assert!(c[0].id.starts_with("k_") && c[0].id.len() == 14);
        assert_eq!(c[3].context, "The nucleus holds the DNA.");
        assert_eq!(c[2].lines, 9..13);
        // Ids follow the folded question.
        assert_eq!(hash12("What  is **ATP?**"), hash12("what is atp?"));
    }

    #[test]
    fn adding_ids_keeps_basic_ids_and_renames_clozes() {
        let add = add_ids(NOTE);
        let out = add.apply(NOTE);
        assert!(out.contains("The cell's energy currency ^k-"), "{out}");
        assert!(out.contains("Cell division ^mito\n"), "existing ids stay");
        assert!(out.contains("holds the ==DNA==. ^k-"));
        let before = cards(NOTE);
        let after = cards(&out);
        assert_eq!(before.len(), after.len());
        assert_eq!(before[0].id, after[0].id, "basic card keeps its id");
        assert_eq!(before[2].id, after[2].id, "multi-line card keeps its id");
        assert_eq!(add.renames.len(), 2, "two clozes get paragraph ids");
        assert!(after[3].id.ends_with(":1") && after[4].id.ends_with(":2"));
        assert_eq!(add_ids(&out), AddIds::default(), "nothing left to add");
    }

    #[test]
    fn duplicates_and_similarity() {
        let c = cards("A :: 1\nA :: 1\n");
        assert_eq!(c[1].id, format!("{}-2", c[0].id));
        assert!(similarity("What is the capital of France?", "What's the capital of France?") > 0.8);
        assert!(similarity("Capital of France", "Largest ocean") < 0.3);
        assert!((similarity("x", "x") - 1.0).abs() < 1e-9);
    }
}
