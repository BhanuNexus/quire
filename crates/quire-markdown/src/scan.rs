// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scanner for Obsidian-style extensions that `CommonMark` does not know about:
//! `[[wikilinks]]`, `![[embeds]]`, `#nested/tags` and `==highlights==`.
//!
//! The scanner runs over the raw source but skips "verbatim" regions (code,
//! math, HTML, front matter) reported by the `CommonMark` parser.

use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Note name (without heading/alias), trimmed.
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
}

impl WikiLink {
    /// Text shown to readers.
    pub fn display(&self) -> &str {
        self.alias.as_deref().unwrap_or(match &self.heading {
            Some(h) if self.target.is_empty() => h,
            _ => &self.target,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    WikiLink { range: Range<usize>, link: WikiLink },
    Tag { range: Range<usize>, name: String },
    Highlight { range: Range<usize> },
}

impl Inline {
    pub fn range(&self) -> Range<usize> {
        match self {
            Self::WikiLink { range, .. } | Self::Tag { range, .. } | Self::Highlight { range } => range.clone(),
        }
    }
}

/// Byte ranges whose content must be treated literally (code, math, HTML,
/// front matter, link destinations), sorted and merged.
pub fn verbatim_ranges(src: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut open: Option<usize> = None;
    let parsed = crate::parse_source(src);
    for (ev, range) in Parser::new_ext(&parsed, crate::options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::CodeBlock(_) | Tag::MetadataBlock(_) | Tag::HtmlBlock) => open = Some(range.start),
            Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_) | TagEnd::HtmlBlock) => {
                if let Some(s) = open.take() {
                    out.push(s..range.end);
                }
            }
            Event::Code(_) | Event::InlineMath(_) | Event::DisplayMath(_) | Event::InlineHtml(_) | Event::Html(_) => out.push(range),
            Event::Start(Tag::Link { .. }) => {
                // The destination part `(...)` of an inline link is verbatim too.
                if let Some(off) = src[range.clone()].rfind("](") {
                    out.push(range.start + off + 1..range.end);
                }
            }
            _ => {}
        }
    }
    merge(out)
}

fn merge(mut v: Vec<Range<usize>>) -> Vec<Range<usize>> {
    v.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Index of the first verbatim range that ends after `pos`, if `pos` is inside it.
fn covering(ranges: &[Range<usize>], pos: usize) -> Option<&Range<usize>> {
    let i = ranges.partition_point(|r| r.end <= pos);
    ranges.get(i).filter(|r| r.start <= pos)
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// Scan `src` for extension inlines, skipping verbatim regions.
pub fn scan_inlines(src: &str) -> Vec<Inline> {
    let verbatim = verbatim_ranges(src);
    scan_with(src, &verbatim)
}

pub(crate) fn scan_with(src: &str, verbatim: &[Range<usize>]) -> Vec<Inline> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(r) = covering(verbatim, i) {
            i = r.end;
            continue;
        }
        match bytes[i] {
            b'[' if bytes.get(i + 1) == Some(&b'[') => {
                if let Some((inline, end)) = wikilink(src, i) {
                    out.push(inline);
                    i = end;
                    continue;
                }
            }
            b'!' if src[i..].starts_with("![[") => {
                if let Some((inline, end)) = wikilink(src, i + 1) {
                    if let Inline::WikiLink { range, mut link } = inline {
                        link.embed = true;
                        out.push(Inline::WikiLink { range: i..range.end, link });
                    }
                    i = end;
                    continue;
                }
            }
            b'#' => {
                let prev = src[..i].chars().next_back();
                if prev.is_none_or(|c| c.is_whitespace() || c == '(' || c == ',') {
                    let rest = &src[i + 1..];
                    let len: usize = rest.chars().take_while(|&c| is_tag_char(c)).map(char::len_utf8).sum();
                    let name = rest[..len].trim_end_matches('/');
                    if !name.is_empty() && !name.chars().all(|c| c.is_ascii_digit()) {
                        let end = i + 1 + name.len();
                        out.push(Inline::Tag { range: i..end, name: name.to_string() });
                        i = end;
                        continue;
                    }
                }
            }
            b'=' if bytes.get(i + 1) == Some(&b'=') => {
                let body_start = i + 2;
                let line_end = src[body_start..].find('\n').map_or(src.len(), |n| body_start + n);
                if let Some(off) = src[body_start..line_end].find("==") {
                    let body = &src[body_start..body_start + off];
                    let ok = !body.is_empty() && !body.starts_with(char::is_whitespace) && !body.ends_with(char::is_whitespace);
                    if ok {
                        let end = body_start + off + 2;
                        out.push(Inline::Highlight { range: i..end });
                        i = end;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += src[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn wikilink(src: &str, start: usize) -> Option<(Inline, usize)> {
    let body_start = start + 2;
    let rest = &src[body_start..];
    let close = rest.find("]]")?;
    let inner = &rest[..close];
    if inner.is_empty() || inner.contains('\n') || inner.contains("[[") {
        return None;
    }
    let (target_part, alias) = match inner.split_once('|') {
        Some((t, a)) => (t, Some(a.trim().to_string()).filter(|a| !a.is_empty())),
        None => (inner, None),
    };
    let (target, heading) = match target_part.split_once('#') {
        Some((t, h)) => (t.trim(), Some(h.trim().to_string()).filter(|h| !h.is_empty())),
        None => (target_part.trim(), None),
    };
    if target.is_empty() && heading.is_none() {
        return None;
    }
    let end = body_start + close + 2;
    let link = WikiLink { target: target.to_string(), heading, alias, embed: false };
    Some((Inline::WikiLink { range: start..end, link }, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<String> {
        scan_inlines(src)
            .into_iter()
            .map(|i| match i {
                Inline::WikiLink { link, .. } => format!("link:{}", link.target),
                Inline::Tag { name, .. } => format!("tag:{name}"),
                Inline::Highlight { range } => format!("hl:{}", &src[range]),
            })
            .collect()
    }

    #[test]
    fn finds_extensions_and_skips_code() {
        let src = "See [[Rust Notes#Ropes|ropes]] and #rust/perf ==key== idea.\n\n`[[not a link]] #nottag`\n\n```\n#nope [[nope]]\n```\n# Heading not tag\nissue #42 is not a tag\n";
        assert_eq!(names(src), vec!["link:Rust Notes", "tag:rust/perf", "hl:==key=="]);
    }

    #[test]
    fn parses_wikilink_parts() {
        let v = scan_inlines("![[Diagram.png]] [[#Local heading]]");
        let Inline::WikiLink { link, range } = &v[0] else { panic!() };
        assert!(link.embed);
        assert_eq!(*range, 0..16);
        let Inline::WikiLink { link, .. } = &v[1] else { panic!() };
        assert_eq!(link.target, "");
        assert_eq!(link.display(), "Local heading");
    }

    #[test]
    fn tag_in_url_is_ignored() {
        assert!(names("visit https://x.dev/#anchor now").is_empty());
        assert_eq!(names("(#idea), #todo"), vec!["tag:idea", "tag:todo"]);
    }
}
