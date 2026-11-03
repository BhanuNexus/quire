// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Finding text in a PDF's pages, making highlight anchors from a
//! selection, and finding highlights again after the PDF changed.
//!
//! Matching ignores case and treats any run of white space (including line
//! breaks) as one space, so a quote that wrapped differently still matches.

use crate::annot::PdfAnchor;
#[cfg(feature = "text")]
use crate::text::PageText;

/// Characters kept around a highlight for re-anchoring.
pub const CONTEXT: usize = 32;

/// A match: page (0-based) and a char range of that page's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub page: usize,
    pub start: usize,
    pub end: usize,
}

/// `s` folded for matching, with each folded char's index in `s`.
pub fn fold(s: &str) -> (Vec<char>, Vec<usize>) {
    let mut out = Vec::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    let mut space = false;
    for (i, c) in s.chars().enumerate() {
        if c.is_whitespace() {
            if !space && !out.is_empty() {
                out.push(' ');
                map.push(i);
            }
            space = true;
            continue;
        }
        space = false;
        // Ligatures and other one-to-many folds keep one char per source char.
        out.push(c.to_lowercase().next().unwrap_or(c));
        map.push(i);
    }
    if out.last() == Some(&' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// Every occurrence of `query` in `text`, as char ranges of `text`.
pub fn find_in(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let (q, _) = fold(query);
    if q.is_empty() {
        return Vec::new();
    }
    let (t, map) = fold(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i + q.len() <= t.len() {
        if t[i..i + q.len()] == q[..] {
            out.push(map[i]..map[i + q.len() - 1] + 1);
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Find `query` on every page, in order.
#[cfg(feature = "text")]
pub fn find(pages: &[PageText], query: &str) -> Vec<Hit> {
    pages
        .iter()
        .enumerate()
        .flat_map(|(page, p)| find_in(&p.text(), query).into_iter().map(move |r| Hit { page, start: r.start, end: r.end }))
        .collect()
}

fn slice(chars: &[char], r: std::ops::Range<usize>) -> String {
    chars.get(r).unwrap_or_default().iter().collect()
}

/// An anchor for chars `range` of page `page` (0-based), with its boxes
/// and the text around it.
#[cfg(feature = "text")]
pub fn anchor_for(text: &PageText, page: usize, range: std::ops::Range<usize>, color: &str) -> PdfAnchor {
    let chars: Vec<char> = text.text().chars().collect();
    let end = range.end.min(chars.len());
    let start = range.start.min(end);
    let t = slice(&chars, start..end).split_whitespace().collect::<Vec<_>>().join(" ");
    PdfAnchor {
        p: u32::try_from(page + 1).unwrap_or(1),
        q: text.quads(start..end),
        t,
        pre: slice(&chars, start.saturating_sub(CONTEXT)..start).replace('\n', " "),
        suf: slice(&chars, end..(end + CONTEXT).min(chars.len())).replace('\n', " "),
        c: color.to_string(),
        other: serde_json::Map::new(),
    }
}

/// Where a highlight is after the PDF changed.
///
/// Its text is searched for near its old page (± 2 pages), and the
/// occurrence whose surroundings match best wins. `None` when the text is
/// gone (the highlight is "detached"). Area highlights (no text) stay where
/// they were.
#[cfg(feature = "text")]
pub fn reanchor(a: &PdfAnchor, pages: &[PageText]) -> Option<PdfAnchor> {
    if a.t.trim().is_empty() {
        return (a.p as usize <= pages.len()).then(|| a.clone());
    }
    let old = (a.p as usize).saturating_sub(1);
    let lo = old.saturating_sub(2);
    let hi = (old + 2).min(pages.len().saturating_sub(1));
    let mut best: Option<(f64, usize, std::ops::Range<usize>)> = None;
    for (page, text) in pages.iter().enumerate().take(hi + 1).skip(lo) {
        let body = text.text();
        let chars: Vec<char> = body.chars().collect();
        for r in find_in(&body, &a.t) {
            let pre = slice(&chars, r.start.saturating_sub(CONTEXT)..r.start);
            let suf = slice(&chars, r.end..(r.end + CONTEXT).min(chars.len()));
            let sim = |x: &str, y: &str| {
                let (x, y) = (fold(x).0.into_iter().collect::<String>(), fold(y).0.into_iter().collect::<String>());
                if x.is_empty() && y.is_empty() { 1.0 } else { strsim::normalized_levenshtein(&x, &y) }
            };
            let score = sim(&pre, &a.pre) + sim(&suf, &a.suf) - 0.1 * page.abs_diff(old) as f64;
            if best.as_ref().is_none_or(|b| score > b.0) {
                best = Some((score, page, r));
            }
        }
    }
    let (_, page, r) = best?;
    let mut n = anchor_for(&pages[page], page, r, &a.c);
    n.other.clone_from(&a.other);
    Some(n)
}

#[cfg(all(test, feature = "text"))]
mod tests {
    use super::*;
    use crate::text::TextLine;

    fn page(lines: &[&str]) -> PageText {
        let mut p = PageText::default();
        for (i, l) in lines.iter().enumerate() {
            let y = 700.0 - 14.0 * i as f32;
            if i > 0 {
                p.chars.push(('\n', [72.0, y, 72.0, y]));
            }
            for (k, c) in l.chars().enumerate() {
                let x = 72.0 + 6.0 * k as f32;
                p.chars.push((c, [x, y, x + 6.0, y + 10.0]));
            }
            p.lines.push(TextLine { text: (*l).to_string(), quad: [72.0, y, 72.0 + 6.0 * l.chars().count() as f32, y + 10.0] });
        }
        p
    }

    #[test]
    fn finds_across_line_breaks_ignoring_case() {
        let pages = vec![page(&["Ropes are balanced", "trees of strings."]), page(&["Nothing here"])];
        let hits = find(&pages, "balanced  TREES");
        assert_eq!(hits, vec![Hit { page: 0, start: 10, end: 24 }]);
        assert!(find(&pages, "   ").is_empty());
        assert_eq!(find_in("aaaa", "aa").len(), 2, "matches don't overlap");
    }

    #[test]
    fn anchors_re_find_their_text_after_the_pdf_changes() {
        let v1 = [page(&["Intro text.", "Ropes are balanced trees of strings. Ropes are fun."])];
        let r = find_in(&v1[0].text(), "Ropes are")[1].clone();
        let a = anchor_for(&v1[0], 0, r, "yellow");
        assert_eq!(a.t, "Ropes are");
        assert_eq!(a.suf, " fun.");
        assert_eq!(a.q.len(), 1);
        // A page was added in front and the text rewrapped.
        let v2 = vec![page(&["Cover"]), page(&["Intro text. Ropes are balanced trees", "of strings. Ropes are fun."])];
        let b = reanchor(&a, &v2).expect("found again");
        assert_eq!(b.p, 2);
        assert_eq!(b.t, "Ropes are");
        assert_eq!(b.suf, " fun.", "the second occurrence, by its surroundings");
        let v3 = vec![page(&["Completely different."])];
        assert_eq!(reanchor(&a, &v3), None, "detached");
        let far = vec![page(&["a"]), page(&["b"]), page(&["c"]), page(&["d"]), page(&["Ropes are fun."])];
        assert_eq!(reanchor(&a, &far), None, "only ± 2 pages are searched");
    }
}
