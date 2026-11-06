// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Note HTML cleaned to an allow-list before a browser sees it (0.10.1
//! security review #4).
//!
//! Markdown lets a note carry raw HTML. In the web app of `quire-cli serve`
//! and on a published site, a note can come from somewhere else (a
//! captured page, an agent, another device), and CSP doesn't control
//! where the page itself goes: `<meta http-equiv="refresh">`, `<base>`, a
//! form or a frame could take the reader to a look-alike sign-in page.
//!
//! [`note_html`] keeps what notes use (formatting, headings, lists, tables,
//! callouts, `details`/`summary`, footnotes, task boxes, images, audio,
//! links) and drops everything else: `meta`, `base`, `link`, `form`,
//! `input` other than a disabled check box, `button`, `iframe`, `frame`,
//! `object`, `embed`, `svg`, `math`, `script` and `style` with their
//! contents, every `on…` attribute, `formaction`, `srcdoc`, and links to
//! anything but `http(s)`, `mailto`, `tel` or a relative path. A `data:`
//! URL is kept only as the source of a picture or a sound.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::OnceLock;

/// The cleaner, built once.
fn cleaner() -> &'static ammonia::Builder<'static> {
    static B: OnceLock<ammonia::Builder<'static>> = OnceLock::new();
    B.get_or_init(|| {
        let mut b = ammonia::Builder::default();
        b.add_tags([
            "audio", "video", "source", "track", "section", "article", "aside", "header", "footer", "nav", "main", "input", "label",
        ])
        .rm_tags(["area", "map"])
        .add_clean_content_tags([
            "noscript", "template", "textarea", "select", "option", "title", "iframe", "object", "embed", "svg", "math",
        ])
        // `class` and `id` for callouts, heading anchors, footnotes and
        // paragraph ids; `style` only with the properties below.
        .add_generic_attributes(["class", "id", "style", "dir", "aria-label", "aria-hidden", "role"])
        .add_tag_attributes("div", ["data-callout", "data-callout-fold"])
        .add_tag_attributes("a", ["title"])
        .add_tag_attributes("img", ["title", "loading"])
        .add_tag_attributes("audio", ["src", "controls", "preload", "loop", "muted"])
        .add_tag_attributes("video", ["src", "controls", "preload", "loop", "muted", "poster", "width", "height"])
        .add_tag_attributes("source", ["src", "type"])
        .add_tag_attributes("track", ["src", "kind", "label", "srclang"])
        .add_tag_attributes("details", ["open"])
        .add_tag_attributes("time", ["datetime"])
        .add_tag_attributes("li", ["value"])
        .add_tag_attributes("ol", ["start", "type", "reversed"])
        .add_tag_attributes("th", ["style"])
        .add_tag_attributes("td", ["style"])
        .add_tag_attribute_values("input", "type", ["checkbox"])
        .add_tag_attributes("input", ["checked", "disabled"])
        // A task box shows a state; it never takes input.
        .set_tag_attribute_value("input", "disabled", "")
        .filter_style_properties(HashSet::from([
            "color",
            "background-color",
            "font-weight",
            "font-style",
            "font-variant",
            "font-size",
            "text-align",
            "text-decoration",
            "text-decoration-line",
            "text-transform",
            "vertical-align",
            "white-space",
            "width",
            "height",
            "max-width",
        ]))
        .url_schemes(HashSet::from(["http", "https", "mailto", "tel", "data"]))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(None)
        .attribute_filter(filter);
        b
    })
}

/// `data:` only as a picture or sound; nothing else changes.
fn filter<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    let v = value.trim_start();
    if v.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("data:")) {
        let media = matches!((element, attribute), ("img" | "audio" | "video" | "source", "src") | ("video", "poster"));
        let kind = v[5..].get(..6).unwrap_or_default().to_ascii_lowercase();
        let ok = media && (kind.starts_with("image/") || kind.starts_with("audio/") || kind.starts_with("video/"));
        return ok.then_some(Cow::Borrowed(value));
    }
    Some(Cow::Borrowed(value))
}

/// Note HTML as the web app and published sites show it: see the module.
pub fn note_html(html: &str) -> String {
    cleaner().clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_and_active_content_are_dropped() {
        for bad in [
            "<meta http-equiv=\"refresh\" content=\"0; url=https://evil.example/quire-login\">",
            "<base href=\"https://evil.example/\">",
            "<form action=\"https://evil.example/\"><input name=\"pass\"><button>Sign in</button></form>",
            "<iframe src=\"https://evil.example/\"></iframe>",
            "<object data=\"https://evil.example/x\"></object>",
            "<embed src=\"https://evil.example/x\">",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<a href=\"data:text/html,<script>alert(1)</script>\">x</a>",
            "<img src=\"x\" onerror=\"alert(1)\">",
            "<script>location='https://evil.example/'</script>",
            "<svg><a href=\"https://evil.example/\"><text>x</text></a></svg>",
            "<link rel=\"stylesheet\" href=\"https://evil.example/x.css\">",
            "<details data-dir=\"..\" open><summary>x</summary></details>",
            "<button formaction=\"https://evil.example/\">x</button>",
            "<p style=\"background:url(https://evil.example/x.png)\">x</p>",
        ] {
            let out = note_html(bad);
            let low = out.to_lowercase();
            for word in [
                "<meta",
                "<base",
                "<form",
                "<iframe",
                "<object",
                "<embed",
                "javascript:",
                "data:text",
                "onerror",
                "<script",
                "<svg",
                "<link",
                "data-dir",
                "formaction",
                "evil.example/x.png",
                "evil.example/\"",
            ] {
                assert!(!low.contains(word), "{bad} → {out}");
            }
            assert!(!low.contains("<input name"), "{bad} → {out}");
        }
    }

    #[test]
    fn note_features_survive() {
        let keep = [
            "<div class=\"callout\" data-callout=\"warning\" data-callout-fold=\"-\">",
            "<div class=\"callout-title\"><span class=\"callout-icon\">!</span>Careful</div>",
            "<table><thead><tr><th style=\"text-align:center\">A</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>",
            "<details open=\"\"><summary>More</summary><p>Hidden</p></details>",
            "<p><b>b</b> <i>i</i> <u>u</u> <mark>m</mark> <kbd>Ctrl</kbd> <sub>2</sub><sup>3</sup> <span style=\"color:red\">red</span></p>",
            "<img src=\"/f/pics/a.png\" alt=\"A\">",
            "<img src=\"data:image/svg+xml;base64,PHN2Zy8+\" alt=\"drawing\">",
            "<audio controls=\"\" preload=\"metadata\" src=\"/f/Voice%20note.wav\"><a href=\"/f/Voice%20note.wav\">Voice note.wav</a></audio>",
            "<a class=\"wikilink\" href=\"/n/Other.md#part\">Other</a>",
            "<a href=\"https://example.com/\">site</a>",
            "<a class=\"block-id\" id=\"^idea-1\"></a>",
            "<h2 id=\"a-heading\">A heading</h2>",
            "<sup class=\"footnote-reference\"><a href=\"#1\">1</a></sup>",
            "<input disabled=\"\" type=\"checkbox\" checked=\"\">",
            "<a href=\"mailto:me@example.com\">mail</a>",
        ];
        for k in keep {
            let out = note_html(k);
            let squash = |s: &str| s.replace(['\n', ' '], "");
            let mut parts: Vec<&str> = k.split(['<', '>', ' ']).filter(|p| !p.is_empty()).collect();
            parts.sort_unstable();
            for p in parts {
                assert!(squash(&out).contains(&squash(p)), "{k} lost {p}: {out}");
            }
        }
        // A task box is always disabled, whatever the note says.
        assert!(note_html("<input type=\"checkbox\">").contains("disabled"));
        assert!(!note_html("<input type=\"text\" value=\"x\">").contains("text"));
    }
}
