// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTML rendering for export and preview.
//!
//! Obsidian extensions are rewritten into plain CommonMark/inline HTML before
//! the `CommonMark` renderer runs, so the renderer itself stays standard.

use pulldown_cmark::{CowStr, Event, Parser, Tag, TagEnd, html};
use std::fmt::Write as _;

use crate::frontmatter;
use crate::scan::{Inline, WikiLink, scan_inlines};

/// Resolves a wikilink to an `href`. `None` marks it as unresolved.
pub type Resolver<'a> = &'a dyn Fn(&WikiLink) -> Option<String>;

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Minimal percent-encoding for path segments in URLs.
pub fn encode_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn is_image(target: &str) -> bool {
    let t = target.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp"].iter().any(|e| t.ends_with(e))
}

/// Rewrite extension inlines with `f`, dropping front matter.
fn rewrite(src: &str, mut f: impl FnMut(&Inline, &str) -> String) -> String {
    let body_start = frontmatter::parse(src).map_or(0, |fm| fm.range.end);
    let mut out = String::with_capacity(src.len() + 64);
    let mut pos = body_start;
    for inline in scan_inlines(src) {
        let r = inline.range();
        if r.start < body_start {
            continue;
        }
        out.push_str(&src[pos..r.start]);
        out.push_str(&f(&inline, &src[r.clone()]));
        pos = r.end;
    }
    out.push_str(&src[pos..]);
    out
}

/// Render a note body to an HTML fragment.
pub fn to_html(src: &str, resolve: Resolver<'_>) -> String {
    let src = &*crate::display_source(src);
    let prepared = rewrite(src, |inline, raw| match inline {
        Inline::WikiLink { link, .. } => {
            let label = escape_html(link.display());
            match resolve(link) {
                Some(href) if link.embed && is_image(&link.target) => {
                    format!("<img src=\"{}\" alt=\"{label}\">", escape_html(&href))
                }
                Some(href) => format!("<a class=\"wikilink\" href=\"{}\">{label}</a>", escape_html(&href)),
                None => format!("<span class=\"wikilink unresolved\">{label}</span>"),
            }
        }
        Inline::Tag { name, .. } => format!("<span class=\"tag\">#{}</span>", escape_html(name)),
        Inline::Highlight { .. } => format!("<mark>{}</mark>", &raw[2..raw.len() - 2]),
    });
    let prepared = anchor_block_ids(&prepared);
    let mut out = String::with_capacity(prepared.len() * 3 / 2);
    html::push_html(&mut out, with_callouts(Parser::new_ext(&prepared, crate::options())));
    out
}

/// ` ^id` at the end of a paragraph becomes an anchor that `[[Note#^id]]` links
/// to (`id="^id"`), instead of showing as text. Code and front matter are left alone.
fn anchor_block_ids(src: &str) -> String {
    if !src.contains('^') {
        return src.to_string();
    }
    let verbatim = crate::blocks::verbatim_blocks(src);
    let mut out = String::with_capacity(src.len() + 32);
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        match crate::callout::block_id(body).filter(|_| !verbatim.iter().any(|r| r.contains(&start))) {
            Some((range, id)) => {
                out.push_str(&body[..range.start]);
                let _ = write!(out, "<a class=\"block-id\" id=\"^{}\"></a>", escape_html(id));
                out.push_str(&line[body.len()..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Block quotes that start with `[!type]` become callout boxes:
/// `<div class="callout" data-callout="warning"><div class="callout-title">…</div><div class="callout-content">…</div></div>`.
fn with_callouts<'a>(events: impl Iterator<Item = Event<'a>>) -> impl Iterator<Item = Event<'a>> {
    let events: Vec<Event<'a>> = events.collect();
    let mut out = Vec::with_capacity(events.len());
    // For each open block quote: is it a callout?
    let mut quotes: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::BlockQuote(Some(gfm))) => {
                // GitHub's `> [!NOTE]` on its own line: the parser already took the marker off.
                let id = match gfm {
                    pulldown_cmark::BlockQuoteKind::Note => "note",
                    pulldown_cmark::BlockQuoteKind::Tip => "tip",
                    pulldown_cmark::BlockQuoteKind::Important => "important",
                    pulldown_cmark::BlockQuoteKind::Warning => "warning",
                    pulldown_cmark::BlockQuoteKind::Caution => "caution",
                };
                let kind = crate::callout::kind(id);
                let label = if id == "important" {
                    "Important"
                } else if id == "caution" {
                    "Caution"
                } else {
                    kind.label
                };
                quotes.push(true);
                out.push(Event::Html(CowStr::from(format!(
                    "<div class=\"callout\" data-callout=\"{}\">\n<div class=\"callout-title\"><span class=\"callout-icon\">{}</span>{label}</div>\n<div class=\"callout-content\">\n",
                    kind.id,
                    escape_html(&kind.glyph.to_string()),
                ))));
            }
            Event::Start(Tag::BlockQuote(None)) => {
                // The first line of the first paragraph: `[!type]± title`.
                let first_para = matches!(events.get(i + 1), Some(Event::Start(Tag::Paragraph)));
                let line_end = if first_para {
                    let mut j = i + 2;
                    while j < events.len() && !matches!(events[j], Event::SoftBreak | Event::HardBreak | Event::End(TagEnd::Paragraph)) {
                        j += 1;
                    }
                    j
                } else {
                    i + 1
                };
                let header: &[Event<'a>] = if first_para { &events[i + 2..line_end] } else { &[] };
                let plain: String = header
                    .iter()
                    .map(|e| match e {
                        Event::Text(t) | Event::Code(t) => t.as_ref(),
                        _ => "",
                    })
                    .collect();
                let line = format!("> {plain}");
                if let Some(h) = crate::callout::parse_header(&line) {
                    quotes.push(true);
                    let fold_attr = match h.fold {
                        Some(true) => " data-callout-fold=\"+\"",
                        Some(false) => " data-callout-fold=\"-\"",
                        None => "",
                    };
                    // The title keeps its formatting: drop the marker from the leading text.
                    let mut skip = h.marker.end - 2;
                    let mut title_events = Vec::new();
                    for e in header {
                        match e {
                            Event::Text(t) if skip > 0 => {
                                let n = skip.min(t.len());
                                skip -= n;
                                if n < t.len() {
                                    title_events.push(Event::Text(CowStr::from(t[n..].to_string())));
                                }
                            }
                            e => title_events.push(e.clone()),
                        }
                    }
                    let mut title = String::new();
                    html::push_html(&mut title, title_events.into_iter());
                    if title.trim().is_empty() {
                        title = escape_html(&h.display_title());
                    }
                    out.push(Event::Html(CowStr::from(format!(
                        "<div class=\"callout\" data-callout=\"{}\"{fold_attr}>\n<div class=\"callout-title\"><span class=\"callout-icon\">{}</span>{}</div>\n<div class=\"callout-content\">\n",
                        h.kind.id,
                        escape_html(&h.kind.glyph.to_string()),
                        title.trim()
                    ))));
                    // Keep the rest of that paragraph, if any.
                    let mut k = line_end;
                    if matches!(events.get(k), Some(Event::SoftBreak | Event::HardBreak)) {
                        k += 1;
                    }
                    if matches!(events.get(k), Some(Event::End(TagEnd::Paragraph))) {
                        i = k + 1;
                    } else {
                        out.push(Event::Start(Tag::Paragraph));
                        i = k;
                    }
                    continue;
                }
                quotes.push(false);
                out.push(events[i].clone());
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if quotes.pop() == Some(true) {
                    out.push(Event::Html(CowStr::from("</div>\n</div>\n")));
                } else {
                    out.push(events[i].clone());
                }
            }
            e => out.push(e.clone()),
        }
        i += 1;
    }
    out.into_iter()
}

/// CSS for callouts, shared by HTML export and published sites.
pub const CALLOUT_CSS: &str = concat!(
    ".callout{--c:68,138,255;margin:1rem 0;padding:.7rem 1rem;border-left:3px solid rgb(var(--c));background:rgba(var(--c),.08);border-radius:6px}",
    ".callout-title{display:flex;gap:.5rem;align-items:center;font-weight:600;color:rgb(var(--c))}",
    ".callout-icon{display:inline-flex;width:1.15rem;height:1.15rem;border-radius:50%;background:rgb(var(--c));color:#fff;font-size:.72rem;align-items:center;justify-content:center;flex:none}",
    ".callout-content>:first-child{margin-top:.4rem}.callout-content>:last-child{margin-bottom:0}",
    ".callout[data-callout=abstract]{--c:0,176,200}.callout[data-callout=tip]{--c:0,168,150}",
    ".callout[data-callout=success]{--c:46,160,67}.callout[data-callout=question]{--c:224,142,11}",
    ".callout[data-callout=warning]{--c:232,122,0}.callout[data-callout=failure],.callout[data-callout=danger],.callout[data-callout=bug]{--c:229,72,77}",
    ".callout[data-callout=example]{--c:139,92,246}.callout[data-callout=quote]{--c:138,143,152}",
    "a.block-id{scroll-margin-top:2rem}"
);

const EXPORT_CSS: &str = r#"
:root{--fg:#1f2328;--bg:#ffffff;--muted:#656d76;--accent:#0969da;--code:#f6f8fa;--mark:#fff8c5;--border:#d0d7de}
@media (prefers-color-scheme:dark){:root{--fg:#e6edf3;--bg:#0d1117;--muted:#8d96a0;--accent:#4493f8;--code:#161b22;--mark:#bb800926;--border:#30363d}}
body{font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Inter,sans-serif;color:var(--fg);background:var(--bg);max-width:46rem;margin:3rem auto;padding:0 1.25rem}
h1,h2,h3{line-height:1.25}h1{font-size:2rem}a{color:var(--accent)}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--code);border-radius:6px}
code{padding:.15em .35em;font-size:.9em}pre{padding:1rem;overflow:auto}pre code{padding:0}
blockquote{margin:0;padding-left:1rem;border-left:3px solid var(--border);color:var(--muted)}
table{border-collapse:collapse}td,th{border:1px solid var(--border);padding:.4rem .7rem}
mark{background:var(--mark);color:inherit}.tag{color:var(--accent);font-size:.9em}
.wikilink.unresolved{color:var(--muted);text-decoration:underline dotted}
img{max-width:100%}li.task{list-style:none}
"#;

/// A standalone HTML document for export.
pub fn to_html_document(title: &str, src: &str, resolve: Resolver<'_>) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>{}</title>\n<style>{EXPORT_CSS}{CALLOUT_CSS}</style>\n</head>\n<body>\n<article>\n{}</article>\n</body>\n</html>\n",
        escape_html(title),
        to_html(src, resolve)
    )
}

/// Scheme used for wikilinks in the in-app preview; the UI intercepts it.
pub const NOTE_SCHEME: &str = "quire://note/";

/// Markdown suitable for a plain `CommonMark` viewer: wikilinks become links
/// with the [`NOTE_SCHEME`], image embeds become images, highlights become bold.
pub fn for_preview(src: &str, resolve_embed: &dyn Fn(&str) -> Option<String>) -> String {
    let src = &*crate::display_source(src);
    let src = &*preview_callouts(src);
    rewrite(src, |inline, raw| match inline {
        Inline::WikiLink { link, .. } if link.embed && is_image(&link.target) => match resolve_embed(&link.target) {
            Some(uri) => format!("![{}]({})", link.display(), uri),
            None => format!("*[image: {}]*", link.target),
        },
        Inline::WikiLink { link, .. } => {
            let mut href = format!("{NOTE_SCHEME}{}", encode_url(&link.target));
            if let Some(h) = &link.heading {
                href.push('#');
                href.push_str(&encode_url(h));
            }
            format!("[{}]({href})", link.display().replace(['[', ']'], ""))
        }
        Inline::Tag { .. } => format!("`{raw}`"),
        Inline::Highlight { .. } => format!("**{}**", &raw[2..raw.len() - 2]),
    })
}

/// Callouts in the shape the preview's alert boxes expect: `> [!TYPE]` on its
/// own line (canonical type, no fold sign), the title in bold on the next.
/// Paragraph ids are dropped. Code blocks are left alone.
fn preview_callouts(src: &str) -> std::borrow::Cow<'_, str> {
    if !src.contains("[!") && !src.contains('^') {
        return std::borrow::Cow::Borrowed(src);
    }
    let verbatim = crate::blocks::verbatim_blocks(src);
    let mut out = String::with_capacity(src.len() + 64);
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        if verbatim.iter().any(|r| r.contains(&start)) {
            out.push_str(line);
            continue;
        }
        if let Some(h) = crate::callout::parse_header(body) {
            let quotes = &body[..body[..h.marker.end].find("[!").unwrap_or(0)];
            let _ = writeln!(out, "{quotes}[!{}]", h.kind.id.to_uppercase());
            if !h.title.is_empty() {
                // The title as its own bold paragraph, apart from the body.
                let _ = writeln!(out, "{quotes}**{}**", h.title);
                let _ = writeln!(out, "{}", quotes.trim_end());
            }
            continue;
        }
        match crate::callout::block_id(body) {
            Some((r, _)) => {
                out.push_str(&body[..r.start]);
                out.push_str(eol);
            }
            None => out.push_str(line),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Decode a [`NOTE_SCHEME`] URL back to (target, heading).
pub fn parse_note_url(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix(NOTE_SCHEME)?;
    Some(crate::extract::internal_target(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_extensions() {
        let src = "---\ntitle: x\n---\nHello [[World|planet]] and [[Missing]] #tag ==bright==\n\n`[[raw]]`\n";
        let html = to_html(src, &|l| (l.target == "World").then(|| "World.html".to_string()));
        assert!(html.contains("<a class=\"wikilink\" href=\"World.html\">planet</a>"));
        assert!(html.contains("<span class=\"wikilink unresolved\">Missing</span>"));
        assert!(html.contains("<span class=\"tag\">#tag</span>"));
        assert!(html.contains("<mark>bright</mark>"));
        assert!(html.contains("<code>[[raw]]</code>"));
        assert!(!html.contains("title: x"));
    }

    #[test]
    fn callouts_and_paragraph_ids_render() {
        let none = |_: &WikiLink| None;
        let html = to_html("> [!warning] Mind **the** gap\n> Body text\n\n> plain quote\n", &none);
        assert!(html.contains("<div class=\"callout\" data-callout=\"warning\">"), "{html}");
        assert!(
            html.contains("Mind **the** gap") || html.contains("Mind the gap") || html.contains("Mind <strong>the</strong> gap"),
            "{html}"
        );
        assert!(html.contains("<p>Body text</p>"), "{html}");
        assert!(html.contains("<blockquote>"), "plain quotes stay quotes: {html}");
        assert!(!html.contains("[!warning]"), "{html}");
        // GitHub-style alert, alias, fold sign and empty title.
        let html = to_html("> [!NOTE]\n> Hi\n\n> [!tldr]-\n> Short\n", &none);
        assert!(html.contains("data-callout=\"note\""), "{html}");
        assert!(html.contains("data-callout=\"abstract\" data-callout-fold=\"-\""), "{html}");
        assert!(html.contains(">Abstract</div>"), "{html}");
        // Nested callout closes properly.
        let html = to_html("> [!tip] Outer\n> > [!bug] Inner\n> > deep\n> back\n", &none);
        assert_eq!(html.matches("<div class=\"callout\"").count(), 2, "{html}");
        assert_eq!(html.matches("<div").count(), html.matches("</div>").count(), "{html}");
        // Paragraph ids become anchors, not text; code keeps them.
        let html = to_html("Some idea ^idea-1\n\n```\nx ^code\n```\n", &none);
        assert!(html.contains("<a class=\"block-id\" id=\"^idea-1\"></a>"), "{html}");
        assert!(!html.contains("^idea-1</p>") && !html.contains(" ^idea-1"), "{html}");
        assert!(html.contains("x ^code"), "{html}");
    }

    #[test]
    fn preview_links_round_trip() {
        let md = for_preview("go to [[My Note#Part 2]]", &|_| None);
        assert_eq!(md, "go to [My Note](quire://note/My%20Note#Part%202)");
        let url = md.split('(').nth(1).unwrap().trim_end_matches(')');
        assert_eq!(parse_note_url(url), Some(("My Note".into(), Some("Part 2".into()))));
    }
}
