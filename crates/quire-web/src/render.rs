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
    let tag = |t: &str| format!("/tags/{}", url_path(t));
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

#[cfg(feature = "typeset")]
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// Math and Mermaid as SVG images (full edition). Errors leave the source.
#[cfg(feature = "typeset")]
fn typeset(html: &str) -> String {
    use base64::Engine as _;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Option<(String, f32, f32)>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let render = |key: String, f: &dyn Fn() -> Option<(String, f32, f32)>| {
        if let Some(v) = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&key) {
            return v.clone();
        }
        let v = f();
        let mut c = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if c.len() > 2000 {
            c.clear();
        }
        c.insert(key, v.clone());
        v
    };
    let img = |class: &str, svg: &str, w: f32, h: f32, alt: &str| {
        let data = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
        format!(
            "<img class=\"{class}\" src=\"data:image/svg+xml;base64,{data}\" width=\"{}\" height=\"{}\" alt=\"{}\">",
            (w * 4.0 / 3.0).round(),
            (h * 4.0 / 3.0).round(),
            crate::html::esc(alt)
        )
    };
    let mut out = html.to_string();
    for (open, display) in [("<span class=\"math math-display\">", true), ("<span class=\"math math-inline\">", false)] {
        let mut done = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(open) {
            let Some(len) = rest[i + open.len()..].find("</span>") else { break };
            let latex = unescape(&rest[i + open.len()..i + open.len() + len]);
            done.push_str(&rest[..i]);
            let key = format!("{display}{latex}");
            let style = quire_typeset::MathStyle::default();
            match render(key, &|| quire_typeset::math_svg(&latex, display, &style).ok().map(|r| (r.svg, r.width_pt, r.height_pt))) {
                Some((svg, w, h)) => done.push_str(&img(if display { "math display" } else { "math" }, &svg, w, h, &latex)),
                None => done.push_str(&rest[i..i + open.len() + len + 7]),
            }
            rest = &rest[i + open.len() + len + 7..];
        }
        done.push_str(rest);
        out = done;
    }
    let open = "<pre><code class=\"language-mermaid\">";
    let mut done = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find(open) {
        let Some(len) = rest[i..].find("</code></pre>") else { break };
        let source = unescape(&rest[i + open.len()..i + len]);
        done.push_str(&rest[..i]);
        let svg = render(format!("mermaid{source}"), &|| quire_typeset::diagram_svg(&source, false).ok().map(|s| (s, 0.0, 0.0)));
        match svg {
            Some((svg, ..)) => {
                let data = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
                done.push_str(&format!(
                    "<figure class=\"diagram\"><img src=\"data:image/svg+xml;base64,{data}\" alt=\"Diagram\"></figure>"
                ));
            }
            None => done.push_str(&rest[i..i + len + 13]),
        }
        rest = &rest[i + len + 13..];
    }
    done.push_str(rest);
    done
}

/// Quire Lite shows math and diagrams as their source.
#[cfg(not(feature = "typeset"))]
fn typeset(html: &str) -> String {
    html.to_string()
}
