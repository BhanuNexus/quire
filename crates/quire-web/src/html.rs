// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The page frame and small HTML helpers. Everything a note or a path puts
//! on a page goes through [`esc`].

use std::fmt::Write as _;
use std::sync::OnceLock;

pub(crate) use quire_app::markdown::render::escape_html as esc;

const CSS: &str = include_str!("../assets/app.css");
pub(crate) const JS: &str = include_str!("../assets/app.js");
pub(crate) const ICON: &str = include_str!("../assets/icon.svg");

/// The app's stylesheet, with the callout styles exports and sites use.
pub(crate) fn css() -> &'static str {
    static C: OnceLock<String> = OnceLock::new();
    C.get_or_init(|| format!("{CSS}\n{}\n", quire_app::markdown::render::CALLOUT_CSS))
}

/// A short fingerprint of the assets, so a new build's are fetched at once.
pub(crate) fn asset_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        use sha2::Digest as _;
        let d = sha2::Sha256::digest(format!("{}{JS}{ICON}", css()).as_bytes());
        d.iter().take(5).fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    })
}

/// What the frame shows around a page.
pub(crate) struct Frame<'a> {
    pub title: &'a str,
    /// The session's CSRF token (sign-out form); empty when signed out.
    pub csrf: &'a str,
    /// The sidebar's file tree, already rendered; empty for none.
    pub tree: &'a str,
    pub read_only: bool,
    /// The search box's text.
    pub query: &'a str,
    /// The folder "New note" starts in.
    pub folder: &'a str,
}

fn head(title: &str) -> String {
    let v = asset_version();
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
<meta name=\"color-scheme\" content=\"light dark\">\n<meta name=\"referrer\" content=\"same-origin\">\n\
<title>{} · Quire</title>\n<link rel=\"icon\" href=\"/assets/icon.svg?v={v}\" type=\"image/svg+xml\">\n\
<link rel=\"stylesheet\" href=\"/assets/app.css?v={v}\">\n<script src=\"/assets/app.js?v={v}\"></script>\n</head>\n",
        esc(title)
    )
}

pub(crate) fn page(f: &Frame<'_>, main: &str) -> String {
    let mut out = head(f.title);
    out.push_str("<body>\n<a class=\"skip\" href=\"#main\">Skip to the note</a>\n<header class=\"top\">\n");
    if !f.tree.is_empty() {
        out.push_str(
            "<button type=\"button\" id=\"menu\" class=\"icon-btn\" aria-controls=\"side\" aria-expanded=\"false\" aria-label=\"Files\">\
<svg viewBox=\"0 0 20 20\" aria-hidden=\"true\"><path d=\"M3 5h14M3 10h14M3 15h14\"/></svg></button>\n",
        );
    }
    out.push_str("<a class=\"brand\" href=\"/\"><img src=\"/assets/icon.svg?v=");
    out.push_str(asset_version());
    out.push_str(
        "\" alt=\"\" width=\"22\" height=\"22\"><span class=\"brand-name\">Quire</span></a>\n\
<span class=\"beta\" title=\"quire-cli serve is in Beta, for self-hosters\">Beta</span>\n",
    );
    if !f.csrf.is_empty() {
        let _ = write!(
            out,
            "<form class=\"search\" action=\"/search\" role=\"search\"><input type=\"search\" name=\"q\" value=\"{}\" \
placeholder=\"Search notes\" aria-label=\"Search notes\" autocomplete=\"off\"></form>\n\
<button type=\"button\" id=\"quick\" class=\"btn ghost hide-narrow\" title=\"Open a note by name (Ctrl+K)\">Open… <kbd>Ctrl K</kbd></button>\n",
            esc(f.query)
        );
        if !f.read_only {
            let _ = writeln!(out, "<a class=\"btn ghost hide-narrow\" href=\"/new?folder={}\">New note</a>", crate::encode_query(f.folder));
        }
    }
    out.push_str(
        "<button type=\"button\" id=\"theme\" class=\"icon-btn\" aria-label=\"Switch light or dark\" title=\"Light or dark\">\
<svg viewBox=\"0 0 20 20\" aria-hidden=\"true\"><circle cx=\"10\" cy=\"10\" r=\"6\"/><path d=\"M10 4a6 6 0 0 1 0 12z\" class=\"fill\"/></svg></button>\n",
    );
    if !f.csrf.is_empty() {
        let _ = writeln!(
            out,
            "<form method=\"post\" action=\"/logout\" class=\"logout\"><input type=\"hidden\" name=\"_csrf\" value=\"{}\">\
<button class=\"btn ghost\">Sign out</button></form>",
            esc(f.csrf)
        );
    }
    out.push_str("</header>\n<div class=\"wrap\">\n");
    if !f.tree.is_empty() {
        out.push_str("<nav id=\"side\" aria-label=\"Files\">\n");
        if !f.read_only {
            let _ = writeln!(out, "<a class=\"btn new-side\" href=\"/new?folder={}\">New note</a>", crate::encode_query(f.folder));
        }
        out.push_str(f.tree);
        out.push_str("<p class=\"side-links\"><a href=\"/tags\">Tags</a></p>\n</nav>\n");
    }
    out.push_str("<main id=\"main\" tabindex=\"-1\">\n");
    out.push_str(main);
    out.push_str(FOOTER);
    out.push_str("\n</main>\n</div>\n");
    if !f.csrf.is_empty() {
        out.push_str(
            "<dialog id=\"palette\" aria-label=\"Open a note\"><form method=\"dialog\" class=\"palette\">\
<input id=\"palette-q\" type=\"search\" placeholder=\"Type a note's name\" aria-label=\"Note name\" autocomplete=\"off\" \
role=\"combobox\" aria-controls=\"palette-list\" aria-expanded=\"true\">\
<ul id=\"palette-list\" role=\"listbox\"></ul></form></dialog>\n",
        );
    }
    out.push_str("</body>\n</html>\n");
    out
}

/// Under every page: what this server is (Beta, self-hosted, not end to
/// end encrypted).
pub(crate) const FOOTER: &str = "\n<footer class=\"foot\"><span class=\"beta\">Beta</span> \
Quire serve, for self-hosters: your agents and your browser, on a server you own. \
This server holds a decrypted copy of your notes. \
<a href=\"https://github.com/BhanuNexus/quire/blob/main/docs/serve.md\" rel=\"noreferrer\">About serve</a></footer>";

/// A bare page (sign-in, errors) without the app's frame.
pub(crate) fn plain(title: &str, main: &str) -> String {
    page(&Frame { title, csrf: "", tree: "", read_only: true, query: "", folder: "" }, &format!("<div class=\"narrow\">{main}</div>"))
}

pub(crate) fn error_page(message: &str) -> String {
    plain("Quire", &format!("<h1>Quire</h1><p>{}</p><p><a href=\"/\">Back to your notes</a></p>", esc(message)))
}

/// A hidden CSRF field.
pub(crate) fn csrf_field(token: &str) -> String {
    format!("<input type=\"hidden\" name=\"_csrf\" value=\"{}\">", esc(token))
}
