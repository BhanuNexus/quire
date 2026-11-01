// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTML rendering for export and preview.
//!
//! Obsidian extensions are rewritten into plain CommonMark/inline HTML before
//! the `CommonMark` renderer runs, so the renderer itself stays standard.

use pulldown_cmark::{Parser, html};
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
    let mut out = String::with_capacity(prepared.len() * 3 / 2);
    html::push_html(&mut out, Parser::new_ext(&prepared, crate::options()));
    out
}

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
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>{}</title>\n<style>{EXPORT_CSS}</style>\n</head>\n<body>\n<article>\n{}</article>\n</body>\n</html>\n",
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
    fn preview_links_round_trip() {
        let md = for_preview("go to [[My Note#Part 2]]", &|_| None);
        assert_eq!(md, "go to [My Note](quire://note/My%20Note#Part%202)");
        let url = md.split('(').nth(1).unwrap().trim_end_matches(')');
        assert_eq!(parse_note_url(url), Some(("My Note".into(), Some("Part 2".into()))));
    }
}
