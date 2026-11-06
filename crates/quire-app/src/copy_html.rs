// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Copy as HTML" and `quire-cli html` (issue #17).
//!
//! A note, or the selected part of one, is rendered by the same pipeline as
//! published sites and the web app ([`crate::publish::render_note`]:
//! callouts, wikilinks and embeds, citations, views, Excalidraw drawings),
//! with math and Mermaid diagrams as SVG pictures in the full edition
//! (Quire Lite keeps their source, as everywhere).
//!
//! The HTML is meant to be pasted into an email or a document, so:
//!
//! * **Images, recordings and other files** in the vault get absolute
//!   `file://` URLs. A relative path means nothing once pasted, and a
//!   `file://` URL lets a desktop mail app or word processor on this
//!   computer pick the picture up. Links to other sites, `data:` pictures
//!   (drawings, math, diagrams) and in-page `#anchors` stay as they are.
//! * **Links to other notes** become their text: a note has no address a
//!   reader elsewhere could open. Tags stay text too.
//! * **Callouts** get inline styles (a coloured bar and title), because
//!   mail apps and online editors drop `<style>` sheets on paste.
//!
//! The plain-text form that goes with it on the clipboard is the Markdown
//! itself, without the properties block.

use std::fmt::Write as _;
use std::path::Path;

use quire_index::Index;
use quire_markdown::render::{self, encode_url};
use quire_vault::{NotePath, Vault};

use crate::citations::Library;
use crate::publish::{Links, render_note};

/// What "Copy as HTML" puts on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopiedHtml {
    /// The HTML fragment.
    pub html: String,
    /// The Markdown, for apps that take plain text only.
    pub text: String,
    /// The selection was copied, not the whole note.
    pub selection: bool,
}

/// `src` (the note at `path`, or part of it) as an HTML fragment for the
/// clipboard or `quire-cli html`: see the module.
pub fn render(vault: &Vault, ix: &Index, library: &Library, path: &NotePath, src: &str) -> String {
    // Files are linked relative to the note's folder, the way a Markdown
    // image is written, so both kinds become `file://` URLs the same way below.
    let up = "../".repeat(path.folder().split('/').filter(|s| !s.is_empty()).count());
    let relative = |p: &NotePath| Some(format!("{up}{}", encode_url(p.as_str())));
    // A drawing note (`.excalidraw.md`) is drawn in place; other notes are text.
    let note = |p: &NotePath| quire_canvas::excalidraw::is_drawing(p.as_str()).then(|| relative(p)).flatten();
    let tag = |_: &str| None;
    let r = render_note(vault, ix, library, path, src, &Links { note: &note, file: &relative, tag: &tag });
    let html = crate::publish::typeset(&r.html);
    let html = absolute_file_urls(&html, path, &|p| vault.storage().absolute(p));
    inline_callout_styles(&html)
}

/// The same for text with no vault around it (a scratchpad): no links to
/// resolve, nothing to embed.
pub fn render_without_vault(src: &str) -> String {
    let html = crate::sanitize::note_html(&render::to_html(src, &|_| None));
    inline_callout_styles(&crate::publish::typeset(&html))
}

/// The Markdown that goes with the HTML as plain text: `src` without its
/// properties block.
pub fn plain_text(src: &str) -> String {
    let body = quire_markdown::frontmatter::parse(src).map_or(src, |fm| &src[fm.range.end..]);
    body.trim_start_matches(['\r', '\n']).to_string()
}

/// Relative `src` and `href` values (files next to the note, or reached
/// with `../`) as absolute `file://` URLs. Values that leave the vault,
/// and everything that isn't a relative path, are left as they are.
fn absolute_file_urls(html: &str, from: &NotePath, abs: &dyn Fn(&NotePath) -> Option<std::path::PathBuf>) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some((start, attr_len)) = next_attr(rest) {
        let value_start = start + attr_len;
        let Some(end) = rest[value_start..].find('"') else { break };
        let value = &rest[value_start..value_start + end];
        out.push_str(&rest[..value_start]);
        match resolve(from, value).and_then(|(p, frag)| Some((abs(&p)?, frag))) {
            Some((path, frag)) => {
                out.push_str(&file_url(&path));
                if !frag.is_empty() {
                    out.push('#');
                    out.push_str(&render::escape_html(&frag));
                }
            }
            None => out.push_str(value),
        }
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

/// A relative URL (as written in the HTML) as the vault file it points at,
/// with its `#fragment`.
fn resolve(from: &NotePath, value: &str) -> Option<(NotePath, String)> {
    let raw = value.replace("&amp;", "&");
    if raw.is_empty() || raw.starts_with(['/', '#', '\\']) || raw.contains(':') {
        return None;
    }
    let (target, frag) = raw.split_once('#').unwrap_or((raw.as_str(), ""));
    let decoded = percent_decode(target)?;
    let mut parts: Vec<&str> = from.folder().split('/').filter(|s| !s.is_empty()).collect();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| (NotePath::new(parts.join("/")), frag.to_string()))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `file:///home/me/Notes/pics/a%20b.png`, `file:///C:/Users/me/…` on Windows.
pub fn file_url(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut s = path.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// A callout's colour (`r,g,b`), as in [`render::CALLOUT_CSS`].
fn callout_rgb(kind: &str) -> &'static str {
    match kind {
        "abstract" => "0,176,200",
        "tip" => "0,168,150",
        "success" => "46,160,67",
        "question" => "224,142,11",
        "warning" => "232,122,0",
        "failure" | "danger" | "bug" => "229,72,77",
        "example" => "139,92,246",
        "quote" => "138,143,152",
        _ => "68,138,255",
    }
}

/// Callouts styled inline: pasted HTML keeps `style` attributes, not sheets.
fn inline_callout_styles(html: &str) -> String {
    const OPEN: &str = "<div class=\"callout\" data-callout=\"";
    const TITLE: &str = "<div class=\"callout-title\">";
    const ICON: &str = "<span class=\"callout-icon\">";
    if !html.contains(OPEN) {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;
    while let Some(i) = rest.find(OPEN) {
        let after = &rest[i + OPEN.len()..];
        let Some(q) = after.find('"') else { break };
        let rgb = callout_rgb(&after[..q]);
        out.push_str(&rest[..i]);
        let _ = write!(
            out,
            "<div style=\"margin:1em 0;padding:.6em 1em;border-left:3px solid rgb({rgb});background-color:rgba({rgb},.08);border-radius:4px\" class=\"callout\" data-callout=\""
        );
        rest = after;
        // Its title comes right after the opening tag.
        if let Some(t) = rest.find(TITLE).filter(|t| !rest[..*t].contains(OPEN)) {
            out.push_str(&rest[..t]);
            let _ = write!(out, "<div style=\"font-weight:600;color:rgb({rgb})\" class=\"callout-title\">");
            rest = &rest[t + TITLE.len()..];
            if let Some(rest_after_icon) = rest.strip_prefix(ICON) {
                let _ = write!(out, "<span style=\"margin-right:.4em\" class=\"callout-icon\">");
                rest = rest_after_icon;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, Vault, quire_index::IndexService) {
        let dir = tempfile::tempdir().unwrap();
        for (rel, text) in [
            ("Other.md", "# Other\n"),
            ("Projects/Plan.md", "x"),
            ("pics/a b.png", "PNG"),
            ("Projects/voice.wav", "RIFF"),
            ("Sketch.excalidraw.md", include_str!("../../quire-canvas/testdata/excalidraw/Plain.excalidraw.md")),
        ] {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let v = Vault::open_local(dir.path()).unwrap();
        let svc = quire_index::IndexService::spawn();
        svc.load(v.clone());
        svc.flush();
        (dir, v, svc)
    }

    #[test]
    fn a_note_renders_for_pasting() {
        let (dir, v, svc) = vault();
        let src = "---\ngoal: 10\n---\n# Plan\n\n> [!warning] Mind the gap\n> Body\n\nSee [[Other]] and #todo ==now==.\n\n\
                   ![[a b.png]]\n\n![[voice.wav]]\n\n![local](../pics/a%20b.png) [site](https://example.com/x?a=1&b=2) [top](#plan)\n";
        let path = NotePath::new("Projects/Plan.md");
        let html = render(&v, &svc.read(), &Library::default(), &path, src);
        let root = file_url(&std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf()));
        let root_plain = file_url(dir.path());
        let has = |s: &str| html.contains(&s.replace("ROOT", &root)) || html.contains(&s.replace("ROOT", &root_plain));
        // Callouts, styled inline.
        assert!(html.contains("class=\"callout\" data-callout=\"warning\""), "{html}");
        assert!(html.contains("border-left:3px solid rgb(232,122,0)"), "{html}");
        assert!(html.contains("color:rgb(232,122,0)\" class=\"callout-title\""), "{html}");
        // Links to notes and tags are text.
        assert!(html.contains("Other</span>") && !html.contains("Other.md"), "{html}");
        assert!(html.contains("<span class=\"tag\">#todo</span>"), "{html}");
        assert!(html.contains("<mark>now</mark>"), "{html}");
        // Files are absolute `file://` URLs, wherever they're embedded from.
        assert!(has("<img src=\"ROOT/pics/a%20b.png\""), "{html}");
        assert!(has("src=\"ROOT/Projects/voice.wav\""), "{html}");
        assert_eq!(html.matches("/pics/a%20b.png\"").count(), 2, "{html}");
        // Other sites and anchors stay; the properties don't show.
        assert!(html.contains("href=\"https://example.com/x?a=1&amp;b=2\""), "{html}");
        assert!(html.contains("href=\"#plan\""), "{html}");
        assert!(!html.contains("goal: 10"), "{html}");
        assert!(!html.contains("../"), "{html}");
    }

    #[test]
    fn a_selection_renders_on_its_own() {
        let (_dir, v, svc) = vault();
        let html = render(&v, &svc.read(), &Library::default(), &NotePath::new("Other.md"), "Just **this** part");
        assert_eq!(html.trim(), "<p>Just <strong>this</strong> part</p>");
        assert_eq!(render_without_vault("> [!tip]\n> Hi\n").matches("rgb(0,168,150)").count(), 2);
    }

    #[test]
    fn excalidraw_drawings_are_pictures() {
        let (_dir, v, svc) = vault();
        let html = render(&v, &svc.read(), &Library::default(), &NotePath::new("Other.md"), "![[Sketch.excalidraw]]\n");
        assert!(html.contains("<img src=\"data:image/svg+xml;base64,"), "{html}");
    }

    #[test]
    fn math_and_diagrams() {
        let (_dir, v, svc) = vault();
        let html = render(&v, &svc.read(), &Library::default(), &NotePath::new("Other.md"), "$x^2$\n\n```mermaid\ngraph TD; A-->B\n```\n");
        if cfg!(feature = "typeset") {
            assert!(html.contains("<img class=\"math\" src=\"data:image/svg+xml;base64,"), "{html}");
            assert!(html.contains("<figure class=\"diagram\"><img src=\"data:image/svg+xml;base64,"), "{html}");
        } else {
            // Quire Lite: the source, as everywhere.
            assert!(html.contains("x^2"), "{html}");
            assert!(html.contains("A--&gt;B"), "{html}");
        }
    }

    #[test]
    fn the_command_copies_the_selection_or_the_whole_note() {
        let src = "---\ngoal: 5\n---\n# Note\n\nFirst **one**.\n\nSecond.\n";
        let mut ws = crate::Workspace::new(crate::Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Note.md", src), ("Other.md", "x")]));
        ws.index().flush();
        ws.open_note(&NotePath::new("Note.md"));
        let Some(crate::workspace::UiRequest::CopyHtml(all)) = ws.run(crate::Command::CopyAsHtml) else { panic!("asks to copy") };
        assert!(!all.selection, "{all:?}");
        assert!(all.html.contains("Note</h1>") && all.html.contains("<p>Second.</p>"), "{}", all.html);
        assert_eq!(all.text, "# Note\n\nFirst **one**.\n\nSecond.\n", "the Markdown, without the properties");
        let start = src.find("First").unwrap();
        ws.active_doc_mut().unwrap().set_selection(quire_text::Selection::new(start, start + "First **one**.".len()));
        let part = ws.copy_as_html(None).unwrap();
        assert!(part.selection);
        assert_eq!(part.html.trim(), "<p>First <strong>one</strong>.</p>");
        assert_eq!(part.text, "First **one**.");
        // From a note's menu: that note, whatever is selected in the open one.
        let other = ws.copy_as_html(Some(&NotePath::new("Other.md"))).unwrap();
        assert_eq!((other.html.trim(), other.selection), ("<p>x</p>", false));
        assert!(ws.copy_as_html(Some(&NotePath::new("Missing.md"))).is_none());
    }

    #[test]
    fn urls() {
        let from = NotePath::new("a/b/Note.md");
        assert_eq!(resolve(&from, "../../x/y%20z.png#p"), Some((NotePath::new("x/y z.png"), "p".into())));
        assert_eq!(resolve(&from, "c.png"), Some((NotePath::new("a/b/c.png"), String::new())));
        for left in ["../../../out.png", "/etc/passwd", "https://x.org/a.png", "data:image/png;base64,AA", "#top", "mailto:a@b.c", ""] {
            assert_eq!(resolve(&from, left), None, "{left}");
        }
        #[cfg(unix)]
        assert_eq!(file_url(Path::new("/home/me/My Notes/ü.png")), "file:///home/me/My%20Notes/%C3%BC.png");
        #[cfg(windows)]
        assert_eq!(file_url(Path::new("C:\\Users\\me\\My Notes\\ü.png")), "file:///C:/Users/me/My%20Notes/%C3%BC.png");
        assert_eq!(plain_text("---\ngoal: 3\n---\n\n# Hi\n"), "# Hi\n");
        assert_eq!(plain_text("No properties"), "No properties");
    }
}
