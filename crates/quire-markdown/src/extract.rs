// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! One pass that pulls out everything the index and UI need to know about a note.

use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

use crate::frontmatter::{self, FrontMatter};
use crate::outline::Heading;
use crate::scan::{Inline, scan_with};

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
    /// 0.12 typed links: the normalized name of the inline field
    /// (`parent:: [[X]]`, `[supports:: [[Y]]]`) or front-matter property
    /// (`up: "[[X]]"`) the link is in. See [`crate::fields`].
    pub field: Option<String>,
}

impl Link {
    pub fn is_internal(&self) -> bool {
        !matches!(self.kind, LinkKind::External)
    }
}

/// A checklist item. The metadata fields come from the text after the task
/// (`📅 2026-12-01`, `[due:: …]`); see [`crate::tasks`]. Dates are kept as
/// written (`YYYY-MM-DD`); `quire-plan` interprets them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Task {
    pub done: bool,
    pub text: String,
    /// Byte offset of the `[ ]` marker.
    pub offset: usize,
    /// 📅 / `[due:: …]`
    pub due: Option<String>,
    /// ⏳ / `[scheduled:: …]`
    pub scheduled: Option<String>,
    /// 🛫 / `[start:: …]`
    pub start: Option<String>,
    /// ✅ / `[completion:: …]`
    pub done_on: Option<String>,
    /// ➕ / `[created:: …]`
    pub created: Option<String>,
    /// 🔁 rule as written (`every month on the 1st`).
    pub recurrence: Option<String>,
    /// As in Obsidian Tasks: 0 = highest (🔺), 1 = high (⏫), 2 = medium
    /// (🔼), 3 = low (🔽), 4 = lowest (⏬); `None` = normal.
    pub priority: Option<u8>,
    /// ⏰ date and time as written.
    pub reminder: Option<String>,
    /// Day-planner block `14:00 - 15:30` (start, end).
    pub time_block: Option<(String, String)>,
    /// Byte range (in the source) of the trailing metadata fields.
    pub meta_range: Option<Range<usize>>,
    /// Time spent, `[time:: 1h25m]` (0.11; a task note keeps time entries instead).
    pub time_spent: Option<String>,
    /// Time estimate, `[estimate:: 2h]`.
    pub estimate: Option<String>,
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
    let mut out = Extracted { front_matter: frontmatter::parse(src), ..Default::default() };
    let mut outliner = crate::outline::Outliner::default();
    let mut verbatim = crate::scan::Verbatim::default();

    let mut link_stack: Vec<(Range<usize>, String, String)> = Vec::new();
    let mut in_task: Option<(bool, usize, String)> = None;
    let mut in_code_block = false;
    let mut in_meta = false;

    let parsed = crate::parse_source(src);
    for (ev, range) in Parser::new_ext(&parsed, crate::options()).into_offset_iter() {
        outliner.event(&ev, &range);
        verbatim.event(src, &ev, &range);
        match ev {
            Event::Start(Tag::Link { link_type, dest_url, .. }) => {
                let dest = if link_type == LinkType::Email { format!("mailto:{dest_url}") } else { dest_url.to_string() };
                link_stack.push((range, dest, String::new()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((range, dest, display)) = link_stack.pop() {
                    let link = if is_external(&dest) {
                        Link { kind: LinkKind::External, target: dest, heading: None, display, range, field: None }
                    } else {
                        let (target, heading) = internal_target(&dest);
                        Link { kind: LinkKind::Relative, target, heading, display, range, field: None }
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
                    let mut task = Task { done, text: text.trim().to_string(), offset, ..Task::default() };
                    let line_start = src[..offset.min(src.len())].rfind('\n').map_or(0, |i| i + 1);
                    let line_end = src[offset.min(src.len())..].find('\n').map_or(src.len(), |i| offset + i);
                    crate::tasks::parse_meta(&src[line_start..line_end], offset - line_start, &mut task);
                    out.tasks.push(task);
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

    out.headings = outliner.out;

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
    let verbatim = verbatim.finish();
    let mut tags: Vec<String> = out.front_matter.as_ref().map(FrontMatter::tags).unwrap_or_default();
    for inline in scan_with(src, &verbatim) {
        match inline {
            Inline::WikiLink { range, link } => out.links.push(Link {
                kind: if link.embed { LinkKind::Embed } else { LinkKind::Wiki },
                display: link.display().to_string(),
                target: link.target,
                heading: link.heading,
                range,
                field: None,
            }),
            Inline::Tag { name, .. } => tags.push(name),
            Inline::Highlight { .. } => {}
        }
    }
    // 0.12: links in front-matter properties, and the type of links in inline fields.
    if let Some(fm) = &out.front_matter {
        out.links.extend(crate::fields::front_matter_links(src, fm.range.end));
    }
    out.links.sort_by_key(|l| l.range.start);
    crate::fields::type_links(&mut out.links, &crate::fields::inline_fields_with(src, &verbatim));
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
