// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    /// Byte offset of the heading's first character.
    pub offset: usize,
    /// URL-style anchor (`#my-heading`), unique within the document.
    pub anchor: String,
}

/// GitHub-style slug: lowercase, alphanumerics kept, spaces → `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    out
}

pub fn outline(src: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut cur: Option<Heading> = None;
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for (ev, range) in Parser::new_ext(src, crate::options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some(Heading { level: level as u8, text: String::new(), offset: range.start, anchor: String::new() });
            }
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) => {
                if let Some(h) = cur.as_mut() {
                    h.text.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut h) = cur.take() {
                    h.text = h.text.trim().to_string();
                    let base = slugify(&h.text);
                    let n = seen.entry(base.clone()).or_insert(0);
                    h.anchor = if *n == 0 { base } else { format!("{base}-{n}") };
                    *n += 1;
                    out.push(h);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_outline_with_unique_anchors() {
        let o = outline("# Intro\ntext\n## The `Rope` type\n## Intro\nSetext\n---\n");
        let anchors: Vec<_> = o.iter().map(|h| (h.level, h.anchor.as_str())).collect();
        assert_eq!(anchors, vec![(1, "intro"), (2, "the-rope-type"), (2, "intro-1"), (2, "setext")]);
        assert_eq!(o[1].text, "The Rope type");
    }
}
