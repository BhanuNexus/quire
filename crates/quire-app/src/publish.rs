// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Publish notes as a static website: plain HTML files in a folder, ready
//! for GitHub Pages, Netlify, Cloudflare Pages or any web server.
//!
//! Only notes marked `publish: true` in their properties go out, unless
//! everything is asked for (then `publish: false` still keeps a note out).
//! Links to notes that aren't published become plain text, so a site never
//! points at, or names the file of, a private note.
//!
//! The site has a page per note with its backlinks and tags, a home page
//! (the note called `index` or `Home`, if published, then a list of pages),
//! a tags page, and search that runs in the browser.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quire_index::Index;
use quire_markdown::Value;
use quire_markdown::render::{self, escape_html};
use quire_vault::{NotePath, Vault};

/// Which notes go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selection {
    /// Notes with `publish: true`.
    #[default]
    Marked,
    /// Every note, except ones with `publish: false` and templates.
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishReport {
    pub pages: usize,
    /// Images and other files the pages embed, copied alongside.
    pub files: usize,
    /// Links left as plain text because their note isn't published.
    pub private_links: usize,
}

fn is_marked(v: Option<&Value>) -> Option<bool> {
    match v?.as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

fn excluded_folder(p: &NotePath) -> bool {
    let top = p.as_str().split('/').next().unwrap_or_default();
    p.as_str().contains('/') && ["Templates", "Exports"].iter().any(|f| top.eq_ignore_ascii_case(f))
}

/// A URL-safe file name for a note, unique within the site.
fn slug(p: &NotePath, taken: &mut BTreeSet<String>) -> String {
    let mut s = String::new();
    for c in p.without_ext().chars() {
        if c.is_alphanumeric() {
            s.extend(c.to_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let base = s.trim_matches('-').to_string();
    let base = if base.is_empty() || base == "index" || base == "tags" || base == "search" { format!("note-{base}") } else { base };
    let mut out = base.clone();
    let mut n = 2;
    while !taken.insert(out.clone()) {
        out = format!("{base}-{n}");
        n += 1;
    }
    out
}

/// A heading's `id`: lowercase words joined by `-`.
fn anchor(text: &str) -> String {
    let mut out = String::new();
    for w in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.extend(w.chars().flat_map(char::to_lowercase));
    }
    out
}

/// Give `<h1>`…`<h6>` an `id`, so `[[Note#Heading]]` links land on them.
fn add_heading_ids(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(i) = rest.find("<h") {
        let bytes = rest.as_bytes();
        let level = bytes.get(i + 2).copied().filter(|b| (b'1'..=b'6').contains(b));
        let after = bytes.get(i + 3).copied();
        let (Some(level), Some(b'>')) = (level, after) else {
            out.push_str(&rest[..i + 2]);
            rest = &rest[i + 2..];
            continue;
        };
        let close = format!("</h{}>", level as char);
        let Some(end) = rest[i..].find(&close) else { break };
        let inner = &rest[i + 4..i + end];
        // The text without tags, for the id.
        let mut text = String::new();
        let mut in_tag = false;
        for c in inner.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let _ = write!(out, "{}<h{} id=\"{}\">", &rest[..i], level as char, anchor(&text.replace("&amp;", "&")));
        rest = &rest[i + 4..];
    }
    out.push_str(rest);
    out
}

/// Replace each `quire-view` code block with the view it describes, listing
/// only published notes (a view never reveals a private note's title).
fn render_views(html: &str, src: &str, ix: &Index, from: &NotePath, url: &HashMap<NotePath, String>) -> String {
    use quire_index::views::{self, Layout};
    let open = "<pre><code class=\"language-quire-view\">";
    let blocks = views::find_blocks(src);
    if blocks.is_empty() || !html.contains(open) {
        return html.to_string();
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    for b in &blocks {
        let Some(i) = rest.find(open) else { break };
        let Some(len) = rest[i..].find("</code></pre>") else { break };
        out.push_str(&rest[..i]);
        rest = &rest[i + len + "</code></pre>".len()..];
        let Ok(spec) = views::parse(&b.spec) else {
            continue;
        };
        let mut r = views::run(&spec, ix, &today, Some(from));
        r.rows.retain(|row| url.contains_key(&row.path));
        let link = |row: &views::ViewRow| format!("<a href=\"{}\">{}</a>", url[&row.path], escape_html(&row.title));
        out.push_str("<div class=\"view\">");
        if r.rows.is_empty() {
            out.push_str("<p class=\"muted\">Nothing here yet.</p>");
        }
        match spec.layout {
            Layout::Table if !r.rows.is_empty() => {
                out.push_str("<table><thead><tr><th>Note</th>");
                let fields: Vec<&String> = spec.fields.iter().filter(|f| f.as_str() != "title").collect();
                for f in &fields {
                    let _ = write!(out, "<th>{}</th>", escape_html(f));
                }
                out.push_str("</tr></thead><tbody>");
                for row in &r.rows {
                    let _ = write!(out, "<tr><td>{}</td>", link(row));
                    for (f, c) in spec.fields.iter().zip(&row.cells) {
                        if f != "title" {
                            let _ = write!(out, "<td>{}</td>", escape_html(c));
                        }
                    }
                    out.push_str("</tr>");
                }
                out.push_str("</tbody></table>");
            }
            Layout::Kanban if !r.rows.is_empty() => {
                out.push_str("<div class=\"board\">");
                for g in &r.groups {
                    let cards: Vec<_> = r.rows.iter().filter(|row| &row.group == g).collect();
                    let name = if g.is_empty() { "(none)" } else { g };
                    let _ = write!(out, "<section><h4>{} <span class=\"muted\">{}</span></h4>", escape_html(name), cards.len());
                    for row in cards {
                        let details: Vec<String> = spec
                            .fields
                            .iter()
                            .zip(&row.cells)
                            .filter(|(f, c)| f.as_str() != "title" && !c.is_empty())
                            .map(|(_, c)| escape_html(c))
                            .collect();
                        let _ = write!(out, "<div class=\"card\">{}<small>{}</small></div>", link(row), details.join(" · "));
                    }
                    out.push_str("</section>");
                }
                out.push_str("</div>");
            }
            Layout::Calendar if !r.rows.is_empty() => {
                let mut rows: Vec<_> = r.rows.iter().filter(|row| row.date.is_some()).collect();
                rows.sort_by(|a, b| a.date.cmp(&b.date));
                out.push_str("<ul class=\"dates\">");
                for row in rows {
                    let _ = write!(out, "<li><time>{}</time> {}</li>", escape_html(row.date.as_deref().unwrap_or_default()), link(row));
                }
                out.push_str("</ul>");
            }
            _ => {
                out.push_str("<ul>");
                for row in &r.rows {
                    let _ = write!(out, "<li>{}</li>", link(row));
                }
                out.push_str("</ul>");
            }
        }
        out.push_str("</div>");
    }
    out.push_str(rest);
    out
}

fn tag_anchor(t: &str) -> String {
    t.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

const CSS: &str = r#"
:root{--fg:#1f2328;--bg:#fff;--muted:#656d76;--accent:#0969da;--code:#f6f8fa;--mark:#fff8c5;--border:#d0d7de}
@media (prefers-color-scheme:dark){:root{--fg:#e6edf3;--bg:#0d1117;--muted:#8d96a0;--accent:#4493f8;--code:#161b22;--mark:#bb800926;--border:#30363d}}
*{box-sizing:border-box}
body{font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Inter,sans-serif;color:var(--fg);background:var(--bg);margin:0}
header{border-bottom:1px solid var(--border);padding:.6rem 1.25rem;display:flex;gap:1rem;align-items:center;flex-wrap:wrap}
header .site{font-weight:600;color:var(--fg);text-decoration:none}
header nav a{color:var(--muted);text-decoration:none;margin-right:.8rem}
#q{margin-left:auto;padding:.35rem .6rem;border:1px solid var(--border);border-radius:6px;background:var(--bg);color:var(--fg);min-width:12rem}
#results{position:absolute;right:1.25rem;top:3rem;background:var(--bg);border:1px solid var(--border);border-radius:8px;max-width:28rem;width:calc(100% - 2.5rem);display:none;z-index:2}
#results a{display:block;padding:.5rem .8rem;color:var(--fg);text-decoration:none;border-bottom:1px solid var(--border)}
#results small{display:block;color:var(--muted)}
main{max-width:46rem;margin:2rem auto;padding:0 1.25rem}
h1,h2,h3{line-height:1.25}a{color:var(--accent)}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--code);border-radius:6px}
code{padding:.15em .35em;font-size:.9em}pre{padding:1rem;overflow:auto}pre code{padding:0}
blockquote{margin:0;padding-left:1rem;border-left:3px solid var(--border);color:var(--muted)}
table{border-collapse:collapse}td,th{border:1px solid var(--border);padding:.4rem .7rem}
mark{background:var(--mark);color:inherit}.tag{color:var(--accent);font-size:.9em;text-decoration:none}
.wikilink.unresolved{color:inherit}
img{max-width:100%}
aside{margin-top:3rem;border-top:1px solid var(--border);padding-top:1rem;color:var(--muted);font-size:.95em}
aside ul{padding-left:1.2rem}aside li{margin:.3rem 0}aside .ctx{display:block;font-size:.9em}
.pages li{margin:.25rem 0}.folder{color:var(--muted);font-size:.9em;margin-top:1.5rem}
.view{margin:1rem 0}.view table{width:100%}.muted{color:var(--muted);font-weight:400}
.board{display:flex;gap:.75rem;overflow-x:auto}.board section{flex:1;min-width:11rem;background:var(--code);border-radius:8px;padding:.5rem}
.board h4{margin:.2rem .3rem .5rem;font-size:.9rem}.card{background:var(--bg);border:1px solid var(--border);border-radius:6px;padding:.45rem .6rem;margin-bottom:.4rem}
.card small{display:block;color:var(--muted)}.dates{list-style:none;padding:0}.dates time{color:var(--muted);font-variant-numeric:tabular-nums;margin-right:.6rem}
footer{max-width:46rem;margin:3rem auto 2rem;padding:0 1.25rem;color:var(--muted);font-size:.85em}
"#;

const SEARCH_JS: &str = r#"(function(){var q=document.getElementById('q'),r=document.getElementById('results');if(!q)return;
function esc(s){return s.replace(/[&<>"]/g,function(c){return{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]})}
q.addEventListener('input',function(){var t=q.value.trim().toLowerCase();if(!t){r.style.display='none';return}
var words=t.split(/\s+/),hits=[];for(var i=0;i<PAGES.length;i++){var p=PAGES[i],h=(p.t+' '+p.x).toLowerCase(),ok=true,score=0;
for(var j=0;j<words.length;j++){var k=h.indexOf(words[j]);if(k<0){ok=false;break}score+=p.t.toLowerCase().indexOf(words[j])>=0?0:1}
if(ok)hits.push([score,p])}hits.sort(function(a,b){return a[0]-b[0]});
r.innerHTML=hits.slice(0,12).map(function(h){var p=h[1],k=p.x.toLowerCase().indexOf(words[0]);var s=k<0?p.x.slice(0,90):p.x.slice(Math.max(0,k-40),k+60);
return '<a href="'+p.u+'">'+esc(p.t)+'<small>'+esc(s)+'</small></a>'}).join('')||'<a>No matches</a>';r.style.display='block'});
document.addEventListener('click',function(e){if(e.target!==q)r.style.display='none'})})();"#;

fn page(site: &str, title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>{title} · {site}</title>\n<link rel=\"stylesheet\" href=\"style.css\">\n</head>\n<body>\n\
         <header><a class=\"site\" href=\"index.html\">{site}</a><nav><a href=\"tags.html\">Tags</a></nav>\
         <input id=\"q\" type=\"search\" placeholder=\"Search\" aria-label=\"Search\"><div id=\"results\"></div></header>\n\
         <main>\n{body}</main>\n<footer>Made with Quire</footer>\n<script src=\"search.js\"></script>\n</body>\n</html>\n",
        title = escape_html(title),
        site = escape_html(site),
    )
}

/// Write the site for `vault` into `out` (created if needed; files Quire
/// writes there are replaced, others are left alone).
pub fn publish(vault: &Vault, ix: &Index, out: &Path, site_title: &str, which: Selection) -> Result<PublishReport, String> {
    let chosen: Vec<&quire_index::NoteRecord> = ix
        .records()
        .filter(|r| !excluded_folder(&r.path))
        .filter(|r| match (is_marked(r.properties.get("publish")), which) {
            (Some(m), _) => m,
            (None, Selection::All) => true,
            (None, Selection::Marked) => false,
        })
        .collect();
    let mut chosen = chosen;
    chosen.sort_by_key(|a| a.path.as_str().to_lowercase());
    if chosen.is_empty() {
        return Err(match which {
            Selection::Marked => {
                "No notes are marked for publishing. Add `publish: true` to a note's properties, or publish all notes.".into()
            }
            Selection::All => "There are no notes to publish.".into(),
        });
    }
    std::fs::create_dir_all(out).map_err(|e| format!("can't create {}: {e}", out.display()))?;
    let write = |name: &str, text: &str| std::fs::write(out.join(name), text).map_err(|e| format!("can't write {name}: {e}"));

    let mut taken = BTreeSet::new();
    let url: HashMap<NotePath, String> = chosen.iter().map(|r| (r.path.clone(), format!("{}.html", slug(&r.path, &mut taken)))).collect();
    let mut report = PublishReport::default();
    let mut files: BTreeSet<NotePath> = BTreeSet::new();
    let mut tags: BTreeMap<String, Vec<&quire_index::NoteRecord>> = BTreeMap::new();
    let mut search = Vec::new();

    for rec in &chosen {
        let src = ix.source(&rec.path).unwrap_or_default();
        let private = std::cell::Cell::new(0);
        let embedded = std::cell::RefCell::new(Vec::new());
        let html = render::to_html(src, &|l| {
            if l.embed
                && let Some(f) = vault.resolve_file(&l.target).filter(|f| !f.is_markdown())
            {
                let href = format!("files/{}", render::encode_url(f.as_str()));
                embedded.borrow_mut().push(f);
                return Some(href);
            }
            let target = ix.resolve(&l.target, Some(&rec.path));
            if let Some(u) = target.as_ref().and_then(|t| url.get(t)) {
                Some(match &l.heading {
                    Some(h) => format!("{u}#{}", anchor(h)),
                    None => u.clone(),
                })
            } else {
                private.set(private.get() + usize::from(target.is_some()));
                None
            }
        });
        report.private_links += private.get();
        files.extend(embedded.into_inner());
        let html = render_views(&html, src, ix, &rec.path, &url);
        // Tags link to the tags page.
        let mut body = add_heading_ids(&html);
        for t in &rec.tags {
            let from = format!("<span class=\"tag\">#{}</span>", escape_html(t));
            let to = format!("<a class=\"tag\" href=\"tags.html#{}\">#{}</a>", tag_anchor(t), escape_html(t));
            body = body.replace(&from, &to);
            tags.entry(t.to_lowercase()).or_default().push(rec);
        }
        // Published notes that link here.
        let mut seen = BTreeSet::new();
        let back: Vec<String> = ix
            .backlinks(&rec.path)
            .iter()
            .filter(|b| b.from != rec.path && seen.insert(b.from.clone()))
            .filter_map(|b| {
                let u = url.get(&b.from)?;
                let title = ix.get(&b.from).map_or_else(|| b.from.stem().to_string(), |r| r.title.clone());
                Some(format!(
                    "<li><a href=\"{u}\">{}</a><span class=\"ctx\">{}</span></li>",
                    escape_html(&title),
                    escape_html(b.context.trim().trim_start_matches(['-', '*', '>', ' ']))
                ))
            })
            .collect();
        if !back.is_empty() {
            let _ = writeln!(body, "<aside><strong>Linked from</strong><ul>{}</ul></aside>", back.concat());
        }
        write(&url[&rec.path], &page(site_title, &rec.title, &body))?;
        search.push(format!(
            "{{\"t\":{},\"u\":{},\"x\":{}}}",
            serde_json::to_string(&rec.title).unwrap_or_default(),
            serde_json::to_string(&url[&rec.path]).unwrap_or_default(),
            serde_json::to_string(&rec.plain.chars().take(4000).collect::<String>()).unwrap_or_default()
        ));
        report.pages += 1;
    }

    // Home: the note called index or Home first, then every page by folder.
    let home = chosen.iter().find(|r| ["index", "home"].contains(&r.path.stem().to_lowercase().as_str()));
    let mut body = String::new();
    if let Some(h) = home {
        let _ = write!(body, "<p><a href=\"{}\">{}</a></p>", url[&h.path], escape_html(&h.title));
    } else {
        let _ = write!(body, "<h1>{}</h1>", escape_html(site_title));
    }
    let mut folder = None;
    body.push_str("<ul class=\"pages\">");
    for r in &chosen {
        let f = r.path.folder();
        if folder != Some(f) {
            if !f.is_empty() {
                let _ = write!(body, "</ul><div class=\"folder\">{}</div><ul class=\"pages\">", escape_html(f));
            }
            folder = Some(f);
        }
        let _ = write!(body, "<li><a href=\"{}\">{}</a></li>", url[&r.path], escape_html(&r.title));
    }
    body.push_str("</ul>\n");
    write("index.html", &page(site_title, site_title, &body))?;

    let mut body = String::from("<h1>Tags</h1>\n");
    for (t, notes) in &tags {
        let _ = write!(body, "<h2 id=\"{}\">#{}</h2><ul class=\"pages\">", tag_anchor(t), escape_html(t));
        for r in notes {
            let _ = write!(body, "<li><a href=\"{}\">{}</a></li>", url[&r.path], escape_html(&r.title));
        }
        body.push_str("</ul>\n");
    }
    if tags.is_empty() {
        body.push_str("<p>No tags yet.</p>");
    }
    write("tags.html", &page(site_title, "Tags", &body))?;
    write("style.css", CSS)?;
    write("search.js", &format!("var PAGES=[{}];\n{SEARCH_JS}\n", search.join(",\n")))?;

    for f in &files {
        let Ok(bytes) = vault.storage().read_bytes(f) else { continue };
        let dest: PathBuf = out.join("files").join(f.as_str());
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&dest, bytes).map_err(|e| format!("can't copy {f}: {e}"))?;
        report.files += 1;
    }
    Ok(report)
}

impl crate::workspace::Workspace {
    /// Save open notes, then publish the vault's site to `out`.
    pub fn publish_site(&mut self, out: &Path, site_title: &str, which: Selection) -> Result<PublishReport, String> {
        self.save_all();
        self.index().flush();
        let vault = self.vault().ok_or("Open a vault first")?.clone();
        let ix = self.index().read();
        publish(&vault, &ix, out, site_title, which)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_index::NoteRecord;

    fn index(files: &[(&str, &str)]) -> Index {
        let mut ix = Index::new();
        for (p, s) in files {
            ix.insert_record(NoteRecord::from_source(NotePath::new(*p), s), (*s).to_string());
        }
        ix.refresh();
        ix
    }

    #[test]
    fn publishes_marked_notes_and_keeps_private_ones_out() {
        let files = [
            ("Home.md", "---\npublish: true\n---\n# Welcome\nStart with [[Garden]]. Not [[Diary]]. #site\n"),
            ("Notes/Garden.md", "---\npublish: yes\n---\n# Garden\nTomatoes. See [[Home]] and ![[pic.png]]. #plants\n"),
            ("Diary.md", "# Diary\nSecret plans. [[Garden]]\n"),
            ("Templates/Page.md", "---\npublish: true\n---\n# {{title}}\n"),
        ];
        let vault = Vault::in_memory(&files);
        vault.storage().write_bytes_atomic(&NotePath::new("attachments/pic.png"), b"PNG").unwrap();
        let ix = index(&files);
        let dir = tempfile::tempdir().unwrap();
        let r = publish(&vault, &ix, dir.path(), "My garden", Selection::Marked).unwrap();
        assert_eq!(r, PublishReport { pages: 2, files: 1, private_links: 1 });

        let read = |n: &str| std::fs::read_to_string(dir.path().join(n)).unwrap();
        let home = read("home.html");
        assert!(home.contains("<a class=\"wikilink\" href=\"notes-garden.html\">Garden</a>"), "{home}");
        assert!(home.contains("<span class=\"wikilink unresolved\">Diary</span>"), "private links become text");
        assert!(home.contains("href=\"tags.html#site\""));
        let garden = read("notes-garden.html");
        assert!(garden.contains("files/attachments/pic.png"));
        assert!(garden.contains("Linked from") && garden.contains("home.html"), "backlinks from published notes");
        assert!(!garden.contains("diary.html") && !garden.contains("Secret"), "backlinks from private notes stay out");
        assert_eq!(std::fs::read(dir.path().join("files/attachments/pic.png")).unwrap(), b"PNG");
        // Nothing private anywhere on the site.
        for f in ["index.html", "tags.html", "search.js", "home.html", "notes-garden.html"] {
            assert!(!read(f).contains("Secret plans"), "{f} leaks the diary");
        }
        assert!(!dir.path().join("diary.html").exists());
        assert!(read("index.html").contains("home.html"));
        assert!(read("search.js").contains("Tomatoes"));

        // Everything, except notes that opt out and templates.
        let dir2 = tempfile::tempdir().unwrap();
        let r = publish(&vault, &ix, dir2.path(), "All", Selection::All).unwrap();
        assert_eq!(r.pages, 3);
        assert!(!dir2.path().join("templates-page.html").exists());
    }

    #[test]
    fn views_render_as_html_and_never_list_private_notes() {
        let files = [
            (
                "Board.md",
                "---\npublish: true\n---\n# Board\n```quire-view\nfrom: \"#project\"\nlayout: kanban\ngroup: status\nfields: [title, owner]\n```\n",
            ),
            ("A.md", "---\npublish: true\nstatus: active\nowner: Asha\n---\n# Alpha #project\n"),
            ("Secret.md", "---\nstatus: active\n---\n# Secret launch #project\n"),
        ];
        let vault = Vault::in_memory(&files);
        let ix = index(&files);
        let dir = tempfile::tempdir().unwrap();
        publish(&vault, &ix, dir.path(), "Site", Selection::Marked).unwrap();
        let board = std::fs::read_to_string(dir.path().join("board.html")).unwrap();
        assert!(board.contains("class=\"board\"") && board.contains("href=\"a.html\">Alpha") && board.contains("Asha"), "{board}");
        assert!(!board.contains("quire-view") && !board.contains("Secret"), "{board}");
    }

    #[test]
    fn nothing_marked_is_an_error_not_an_empty_site() {
        let files = [("A.md", "# A\n")];
        let dir = tempfile::tempdir().unwrap();
        assert!(publish(&Vault::in_memory(&files), &index(&files), dir.path(), "x", Selection::Marked).is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn headings_get_ids_that_links_use() {
        assert_eq!(
            add_heading_ids("<h2>Next <code>steps</code> &amp; more</h2><p>x</p>"),
            "<h2 id=\"next-steps-more\">Next <code>steps</code> &amp; more</h2><p>x</p>"
        );
        assert_eq!(add_heading_ids("<hr><h1>A</h1>"), "<hr><h1 id=\"a\">A</h1>");
        assert_eq!(anchor("Next steps & more"), "next-steps-more");
    }

    #[test]
    fn slugs_are_unique_and_safe() {
        let mut taken = BTreeSet::new();
        assert_eq!(slug(&NotePath::new("Projects/Big Idea!.md"), &mut taken), "projects-big-idea");
        assert_eq!(slug(&NotePath::new("Projects/Big Idea?.md"), &mut taken), "projects-big-idea-2");
        assert_eq!(slug(&NotePath::new("index.md"), &mut taken), "note-index");
    }
}
