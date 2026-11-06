// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A note as the web app shows it: the publish pipeline's HTML with links
//! pointing into the app, notes and files the reader may not see left as
//! plain text, and (full edition) math and Mermaid diagrams as SVG images.

use quire_app::index::Index;
use quire_app::publish::{Links, render_note};
use quire_app::vault::NotePath;

use crate::paths::Guard;
use crate::{Web, url_path};

pub(crate) fn note_href(p: &NotePath) -> String {
    format!("/n/{}", url_path(p.as_str()))
}

pub(crate) fn file_href(p: &NotePath) -> String {
    format!("/f/{}", url_path(p.as_str()))
}

/// The note at `path` (text `src`) as an HTML fragment.
pub(crate) fn note(web: &Web, ix: &Index, hidden: &[String], path: &NotePath, src: &str) -> String {
    let vault = web.guard.vault();
    let library = quire_app::citations::vault_library(vault);
    let note = |p: &NotePath| Guard::listed(hidden, p).then(|| note_href(p));
    let file = |p: &NotePath| Guard::listed(hidden, p).then(|| file_href(p));
    let tag = |t: &str| Some(format!("/tags/{}", url_path(t)));
    let r = render_note(vault, ix, &library, path, src, &Links { note: &note, file: &file, tag: &tag });
    let html = relative_links(hidden, path, &r.html);
    typeset(&html)
}

/// `<img src="pics/a.png">` and `<a href="Other.md">` (plain Markdown
/// links and images) resolved against the note's folder, so they reach
/// the file through the app; links to other sites stay as they are.
fn relative_links(hidden: &[String], from: &NotePath, html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some((start, attr_len)) = next_attr(rest) {
        let value_start = start + attr_len;
        let Some(end) = rest[value_start..].find('"') else { break };
        let value = &rest[value_start..value_start + end];
        out.push_str(&rest[..value_start]);
        out.push_str(&rewrite(hidden, from, value));
        rest = &rest[value_start + end..];
    }
    out.push_str(rest);
    out
}

/// The next ` src="` or ` href="`: (where it starts, its length).
fn next_attr(s: &str) -> Option<(usize, usize)> {
    let a = s.find(" src=\"").map(|i| (i, 6));
    let b = s.find(" href=\"").map(|i| (i, 7));
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.0 < y.0 { x } else { y }),
        (x, y) => x.or(y),
    }
}

fn rewrite(hidden: &[String], from: &NotePath, value: &str) -> String {
    let raw = value.replace("&amp;", "&");
    let is_relative = !raw.is_empty()
        && !raw.starts_with('/')
        && !raw.starts_with('#')
        && !raw.contains("://")
        && !raw.starts_with("mailto:")
        && !raw.starts_with("data:")
        && !raw.contains(':');
    if !is_relative {
        return value.to_string();
    }
    let (target, frag) = raw.split_once('#').map_or((raw.as_str(), ""), |(a, b)| (a, b));
    let decoded = crate::percent_decode(target).unwrap_or_else(|| target.to_string());
    let mut parts: Vec<&str> = from.folder().split('/').filter(|s| !s.is_empty()).collect();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return "#".into();
                }
            }
            s => parts.push(s),
        }
    }
    let p = NotePath::new(parts.join("/"));
    if !Guard::listed(hidden, &p) {
        return "#".into();
    }
    let href = if p.is_markdown() { note_href(&p) } else { file_href(&p) };
    if frag.is_empty() { crate::html::esc(&href) } else { crate::html::esc(&format!("{href}#{frag}")) }
}

/// Math and Mermaid diagrams as SVG images (full edition); Quire Lite
/// shows them as their source.
fn typeset(html: &str) -> String {
    if cfg!(feature = "typeset") { quire_app::publish::typeset(html) } else { html.to_string() }
}
