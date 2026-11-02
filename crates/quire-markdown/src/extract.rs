// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! One pass that pulls out everything the index and UI need to know about a note.

use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

use crate::frontmatter::{self, FrontMatter};
use crate::outline::{Heading, outline};
use crate::scan::{Inline, scan_with, verbatim_ranges};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// `[[Note]]`
    Wiki,
    /// `![[Note]]`
    Embed,
    /// `[text](Note.md)` — relative link to a note or file.
    Relative,
    /// `[text](https://…)`
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    /// Note name for internal links (no `.md`), URL for external ones.
    pub target: String,
    pub heading: Option<String>,
    pub display: String,
    pub range: Range<usize>,
}

impl Link {
    pub fn is_internal(&self) -> bool {
        !matches!(self.kind, LinkKind::External)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub done: bool,
    pub text: String,
    /// Byte offset of the `[ ]` marker.
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub words: usize,
    pub chars: usize,
    pub lines: usize,
    pub reading_minutes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub front_matter: Option<FrontMatter>,
    pub headings: Vec<Heading>,
    pub links: Vec<Link>,
    /// Unique tags, inline and front matter, without `#`, in first-seen order.
    pub tags: Vec<String>,
    pub tasks: Vec<Task>,
    pub stats: Stats,
    /// Plain prose (no markup), for search and intelligence.
    pub plain: String,
}

impl Extracted {
    /// Best title: front-matter `title`, else first H1.
    pub fn title(&self) -> Option<&str> {
        self.front_matter
            .as_ref()
            .and_then(FrontMatter::title)
            .or_else(|| self.headings.iter().find(|h| h.level == 1).map(|h| h.text.as_str()))
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_external(url: &str) -> bool {
    url.contains("://") || url.starts_with("mailto:") || url.starts_with("tel:")
}

/// Turn `folder/My%20Note.md#part` into (`folder/My Note`, Some("part")).
pub fn internal_target(dest: &str) -> (String, Option<String>) {
    let (path, heading) = match dest.split_once('#') {
        Some((p, h)) => (p, Some(percent_decode(h))),
        None => (dest, None),
    };
    let path = percent_decode(path);
    let path = path.strip_suffix(".md").unwrap_or(&path).trim_start_matches("./").to_string();
    (path, heading.filter(|h| !h.is_empty()))
}

/// Replace `[[Target#h|Alias]]` with its display text (`Alias`, else `Target`).
fn strip_wikilinks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("[[") {
        let Some(close) = rest[open + 2..].find("]]") else {
            break;
        };
        out.push_str(rest[..open].trim_end_matches('!'));
        let inner = &rest[open + 2..open + 2 + close];
        let shown = match inner.split_once('|') {
            Some((_, alias)) => alias,
            None => inner.split('#').find(|p| !p.is_empty()).unwrap_or(inner),
        };
        out.push_str(shown.trim());
        rest = &rest[open + 2 + close + 2..];
    }
    out.push_str(rest);
    out
}

pub fn extract(src: &str) -> Extracted {
    let mut out = Extracted { front_matter: frontmatter::parse(src), headings: outline(src), ..Default::default() };

    let mut link_stack: Vec<(Range<usize>, String, String)> = Vec::new();
    let mut in_task: Option<(bool, usize, String)> = None;
    let mut in_code_block = false;
    let mut in_meta = false;

    let parsed = crate::parse_source(src);
    for (ev, range) in Parser::new_ext(&parsed, crate::options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Link { link_type, dest_url, .. }) => {
                let dest = if link_type == LinkType::Email { format!("mailto:{dest_url}") } else { dest_url.to_string() };
                link_stack.push((range, dest, String::new()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((range, dest, display)) = link_stack.pop() {
                    let link = if is_external(&dest) {
                        Link { kind: LinkKind::External, target: dest, heading: None, display, range }
                    } else {
                        let (target, heading) = internal_target(&dest);
                        Link { kind: LinkKind::Relative, target, heading, display, range }
                    };
                    out.links.push(link);
                }
            }
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::MetadataBlock(_)) => in_meta = true,
            Event::End(TagEnd::MetadataBlock(_)) => in_meta = false,
            Event::TaskListMarker(done) => in_task = Some((done, range.start, String::new())),
            Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                if let Some((done, offset, text)) = in_task.take() {
                    out.tasks.push(Task { done, text: text.trim().to_string(), offset });
                }
                out.plain.push('\n');
            }
            Event::Text(t) | Event::Code(t) => {
                if in_meta {
                    continue;
                }
                if let Some((_, _, d)) = link_stack.last_mut() {
                    d.push_str(&t);
                }
                if let Some((_, _, text)) = in_task.as_mut() {
                    text.push_str(&t);
                }
                if !in_code_block {
                    out.plain.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, text)) = in_task.as_mut() {
                    text.push(' ');
                }
                out.plain.push(' ');
            }
            Event::End(TagEnd::Heading(_) | TagEnd::TableCell) => out.plain.push('\n'),
            _ => {}
        }
    }

    // Callout markers (`[!tip]-`) and paragraph ids (`^id`) aren't prose.
    if out.plain.contains("[!") || out.plain.contains('^') {
        out.plain = out
            .plain
            .split('\n')
            .map(|line| {
                let line = crate::callout::parse_header(&format!("> {line}")).map_or(line, |h| &line[h.marker.end - 2..]);
                crate::callout::block_id(line).map_or_else(|| line.to_string(), |(r, _)| line[..r.start].to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let verbatim = verbatim_ranges(src);
    let mut tags: Vec<String> = out.front_matter.as_ref().map(FrontMatter::tags).unwrap_or_default();
    for inline in scan_with(src, &verbatim) {
        match inline {
            Inline::WikiLink { range, link } => out.links.push(Link {
                kind: if link.embed { LinkKind::Embed } else { LinkKind::Wiki },
                display: link.display().to_string(),
                target: link.target,
                heading: link.heading,
                range,
            }),
            Inline::Tag { name, .. } => tags.push(name),
            Inline::Highlight { .. } => {}
        }
    }
    out.links.sort_by_key(|l| l.range.start);
    let mut seen = std::collections::HashSet::new();
    out.tags = tags.into_iter().filter(|t| seen.insert(t.to_lowercase())).collect();

    // Wikilink syntax leaks into plain text as `[[x|y]]`; keep only what readers see.
    out.plain = strip_wikilinks(&out.plain);
    let words = out.plain.unicode_words().count();
    out.stats = Stats { words, chars: src.chars().count(), lines: src.lines().count(), reading_minutes: words.div_ceil(230) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntags: [project]\n---\n# Quire Plan\n\nSee [[Rope Notes#Undo|undo]], [spec](docs/My%20Spec.md#api) and [site](https://example.com).\n\n- [ ] write parser #todo\n- [x] pick rope\n\n```\n[[ignored]] #ignored\n```\n";

    #[test]
    fn extracts_everything() {
        let e = extract(NOTE);
        assert_eq!(e.title(), Some("Quire Plan"));
        let kinds: Vec<_> = e.links.iter().map(|l| (l.kind, l.target.as_str())).collect();
        assert_eq!(
            kinds,
            vec![(LinkKind::Wiki, "Rope Notes"), (LinkKind::Relative, "docs/My Spec"), (LinkKind::External, "https://example.com"),]
        );
        assert_eq!(e.links[0].display, "undo");
        assert_eq!(e.links[1].heading.as_deref(), Some("api"));
        assert_eq!(e.tags, vec!["project", "todo"]);
        assert_eq!(e.tasks.len(), 2);
        assert_eq!(e.tasks[0].text, "write parser #todo");
        assert!(e.tasks[1].done);
        assert!(e.stats.words >= 10);
        assert!(!e.plain.contains("ignored"));
        assert!(e.plain.contains("See undo,") && !e.plain.contains('|'));
    }
}
