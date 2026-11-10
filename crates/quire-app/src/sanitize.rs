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

/// `data:` only as a picture or sound.
///
/// A style with a value that fetches, runs or binds
/// something (`url(…)`, `expression(…)`, `javascript:`, `@import`, `behavior`, `-moz-binding`) is dropped;
/// nothing else changes.
fn filter<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    if attribute == "style" {
        let lower = value.to_ascii_lowercase().replace(['\\', '\u{0}', ' ', '\t', '\n'], "");
        if ["url(", "expression(", "javascript:", "vbscript:", "@import", "behavior", "-moz-binding"].iter().any(|bad| lower.contains(bad))
        {
            return None;
        }
    }
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

    // ------------------------------------------------ 0.12: no code from notes ever runs

    /// A note written to run something, in every way Quire knows of: raw
    /// HTML, Markdown links, Dataview and Tasks blocks, Templater tags.
    const HOSTILE: &str = "---\ntitle: <script>alert('title')</script>\ntags: [\"<img src=x onerror=alert(1)>\"]\n---\n\
# <script>alert('h1')</script> Heading <img src=x onerror=alert('h')>\n\n\
<script>alert('raw')</script> <SCRIPT SRC=//evil.example/x.js></SCRIPT> <scr<script>ipt>alert(1)</scr</script>ipt>\n\n\
<img src=\"x\" onerror=\"alert('img')\"> <img src=x onerror=alert(1)//\n\n\
<a href=\"javascript:alert('a')\">js</a> <a href=\" JaVaScRiPt:alert(1)\">js2</a> <a href=\"&#106;avascript:alert(1)\">js3</a> <a href=\"java&#9;script:alert(1)\">js4</a> <a href=\"vbscript:msgbox(1)\">vb</a>\n\n\
[md](javascript:alert('md')) [md2](JAVASCRIPT:alert(1)) <javascript:alert(1)> ![img](javascript:alert(1)) [ref][1] [data](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)\n\n\
[1]: javascript:alert('ref')\n\n\
<iframe src=\"https://evil.example\"></iframe> <iframe srcdoc=\"<script>alert(1)</script>\"></iframe> <object data=\"javascript:alert(1)\"></object> <embed src=\"x.swf\">\n\n\
<svg onload=alert(1)><script>alert(1)</script></svg> <svg/onload=alert(1)> <math><mtext><script>alert(1)</script></mtext></math>\n\n\
<div onclick=\"alert(1)\" onmouseover=\"alert(2)\" style=\"background:url(javascript:alert(1));width:expression(alert(1))\">div</div>\n\n\
<details open ontoggle=alert(1)><summary>s</summary></details> <input autofocus onfocus=alert(1)> <button formaction=\"javascript:alert(1)\">b</button> <body onload=alert(1)>\n\n\
<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\"> <base href=\"https://evil.example/\"> <link rel=\"stylesheet\" href=\"https://evil.example/x.css\">\n\n\
<form action=\"https://evil.example\"><input name=pw></form> <style>body{display:none}</style> <noscript><img src=x onerror=alert(1)></noscript>\n\n\
`$= dv.pages().length` and `= this.file.name`\n\n\
```dataviewjs\ndv.paragraph(eval(\"1+1\")); require('child_process').exec('id')\n```\n\n\
```tasks\nfilter by function task.description.includes(String(eval('1')))\n```\n\n\
<% tp.system.prompt(\"x\") %> <%* tR += require('fs').readFileSync('/etc/passwd') %> {{ eval }}\n";

    const FORBIDDEN: &[&str] =
        &["script", "iframe", "frame", "object", "embed", "svg", "math", "meta", "base", "link", "form", "style", "button", "body", "html"];

    /// Every real tag in `html` (escaped text such as `&lt;img onerror=…&gt;` is only text): none
    /// of the elements in `FORBIDDEN`, no `on…=` attribute, no `javascript:` URL or `srcdoc`.
    fn assert_inert(what: &str, html: &str) {
        let mut rest = html;
        while let Some(i) = rest.find('<') {
            let Some(len) = rest[i..].find('>') else { break };
            let tag = &rest[i..=i + len];
            let lower = tag.to_ascii_lowercase();
            let name: String = lower.trim_start_matches(['<', '/']).chars().take_while(char::is_ascii_alphanumeric).collect();
            assert!(!FORBIDDEN.contains(&name.as_str()), "{what}: <{name}> in the page: {tag}\n{html}");
            for bad in ["javascript:", "vbscript:", "srcdoc", "formaction", "expression(", "data:text/html"] {
                assert!(!lower.contains(bad), "{what}: {bad} in {tag}\n{html}");
            }
            let on_handler = lower.split([' ', '/', '\t', '\n']).skip(1).any(|a| a.starts_with("on") && a.contains('='));
            assert!(!on_handler, "{what}: an event handler in {tag}\n{html}");
            rest = &rest[i + len + 1..];
        }
    }

    /// Every way a note becomes HTML a browser opens: the published site and
    /// `quire-cli serve` (`render_note`), "Copy as HTML", and `quire-cli export`.
    #[test]
    fn hostile_notes_are_inert_in_every_html_output() {
        use quire_index::{Index, NoteRecord};
        use quire_vault::{NotePath, Vault};
        let mut ix = Index::new();
        let path = NotePath::new("Evil.md");
        ix.insert_record(NoteRecord::from_source(path.clone(), HOSTILE), HOSTILE.to_string());
        ix.refresh();
        let vault = Vault::in_memory(&[("Evil.md", HOSTILE)]);
        let library = crate::citations::vault_library(&vault);
        let note = |_: &NotePath| Some("/n/x".to_string());
        let tag = |t: &str| Some(format!("/tags/{t}"));
        let links = crate::publish::Links { note: &note, file: &note, tag: &tag };
        let rendered = crate::publish::render_note(&vault, &ix, &library, &path, HOSTILE, &links);
        assert_inert("render_note (publish and serve)", &rendered.html);
        // The words stay as text, so the note still reads as what it is.
        assert!(rendered.html.contains("dv.pages().length") && rendered.html.contains("&lt;% tp.system.prompt"), "{}", rendered.html);

        assert_inert("copy as HTML", &crate::copy_html::render_without_vault(HOSTILE));
        let copied = crate::copy_html::render(&vault, &ix, &library, &path, HOSTILE);
        assert_inert("copy as HTML (vault)", &copied);

        // `quire-cli export` cleans the HTML the same way before wrapping it in a page.
        let body = note_html(&quire_markdown::render::to_html(HOSTILE, &|_| None));
        let page = quire_markdown::render::html_document("Evil", &body);
        let page_lower = page.to_ascii_lowercase();
        assert_eq!(page_lower.matches("<script").count(), 0, "{page}");
        assert_inert("export", &body);
        // The plain renderer does not clean (so it must never be served as it is).
        assert!(quire_markdown::render::to_html(HOSTILE, &|_| None).contains("<script>alert('raw')"));
    }

    /// A published site has one script, its own search box, and its search data is a data file.
    #[test]
    fn a_published_site_runs_only_its_own_search_script() {
        use quire_index::{Index, NoteRecord};
        use quire_vault::{NotePath, Vault};
        let files = [("Evil.md", HOSTILE.to_string()), ("Home.md", "# Home </script><script>alert(1)</script>\n".to_string())];
        let mut ix = Index::new();
        for (p, s) in &files {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p.to_string()), s), s.clone());
        }
        ix.refresh();
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        let vault = Vault::in_memory(&refs);
        let dir = tempfile::tempdir().unwrap();
        crate::publish::publish(&vault, &ix, dir.path(), "Site </title><script>alert(1)</script>", crate::publish::Selection::All).unwrap();
        let mut pages = 0;
        for e in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !std::path::Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("html")) {
                continue;
            }
            pages += 1;
            let html = std::fs::read_to_string(e.path()).unwrap();
            assert_eq!(html.to_ascii_lowercase().matches("<script").count(), 1, "{name}: {html}");
            assert!(html.contains("<script src=\"search.js\"></script>"), "{name}");
            // What a note can put on the page is the text of `<main>`; the head and the header are Quire's.
            if let (Some(a), Some(b)) = (html.find("<main>"), html.find("</main>")) {
                assert_inert(&name, &html[a..b]);
            }
            assert!(!html.contains("alert(1)</script>") && !html.contains("<title>Site </title>"), "{name}");
        }
        assert!(pages >= 4, "{pages} pages");
        let js = std::fs::read_to_string(dir.path().join("search.js")).unwrap();
        assert!(js.starts_with("var PAGES=["), "{js}");
    }

    /// Dataview JS, Tasks JS filters and Templater tags are never evaluated: they are
    /// reported as not translated, or left as written.
    #[test]
    fn dataviewjs_tasks_functions_and_templater_tags_never_run() {
        use quire_index::views::{find_compat_blocks, tasks_query};
        let blocks = find_compat_blocks(HOSTILE);
        let langs: Vec<&str> = blocks.iter().map(|b| b.lang.as_str()).collect();
        assert_eq!(langs, ["dataviewjs", "tasks"]);
        let js = blocks[0].translate();
        assert!(js.spec.is_empty() && js.unsupported[0].contains("never"), "{js:?}");
        let tasks = blocks[1].translate();
        assert!(!tasks.spec.contains("eval") && tasks.unsupported.iter().any(|u| u.contains("JavaScript")), "{tasks:?}");
        for line in ["filter by function task.urgency > 5", "sort by function task.due.moment", "group by function task.file.folder"] {
            let t = tasks_query::translate(line);
            assert!(!t.spec.contains("function") && t.unsupported.len() == 1, "{line}: {t:?}");
        }
        // Templater tags and Dataview inline queries are text in a new note.
        let vars = quire_vault::TemplateVars { title: "T".into(), now: chrono::Local::now() };
        let tpl = "<% tp.date.now() %> <%* tR += 1 %> `$= 1+1` {{title}} {{eval:1+1}}";
        let (text, _) = quire_vault::expand_template(tpl, &vars);
        assert_eq!(text, "<% tp.date.now() %> <%* tR += 1 %> `$= 1+1` T {{eval:1+1}}");
    }

    /// No script engine is part of the build: the whole dependency list has none.
    #[test]
    fn no_script_engine_is_in_the_dependency_list() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
        let names: Vec<&str> = lock.lines().filter_map(|l| l.strip_prefix("name = \"")?.strip_suffix('"')).collect();
        assert!(names.len() > 100, "read the lock file");
        let engines = [
            "boa_engine",
            "rquickjs",
            "quick-js",
            "quickjs",
            "deno_core",
            "v8",
            "mozjs",
            "mlua",
            "rlua",
            "rhai",
            "wasmtime",
            "wasmer",
            "rustpython",
            "rustpython-vm",
            "pyo3",
            "neon",
            "wry",
            "webkit2gtk",
            "tauri",
            "cef",
            "headless_chrome",
            "piccolo",
            "steel-core",
            "starlark",
            "koto",
            "dyon",
            "duktape",
            "javascriptcore",
        ];
        for e in engines {
            assert!(!names.contains(&e), "{e} is in Cargo.lock: notes would be able to run code");
        }
        // Typst (PDF export) carries a WebAssembly interpreter for its own plugins. Nothing can
        // reach it from a note: note text is escaped into string literals (quire-typeset's tests)
        // and the typesetter's world serves only files it was given. Nothing else may use it.
        let mut users = Vec::new();
        for block in lock.split("[[package]]").skip(1) {
            let name = block.lines().find_map(|l| l.strip_prefix("name = \"")?.strip_suffix('"')).unwrap_or_default();
            if block.lines().any(|l| l.trim().trim_matches(|c| c == '"' || c == ',').split(' ').next() == Some("wasmi")) {
                users.push(name);
            }
        }
        assert!(users.iter().all(|n| n.starts_with("typst")), "something besides Typst runs WebAssembly: {users:?}");
    }
}
