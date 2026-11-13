// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Typed links as HTML (0.13): the breadcrumbs trail, "Links by type" and
//! a hierarchy tree, for the web app of `quire-cli serve` and for
//! published sites. Server-rendered, no script, every text escaped.
//!
//! One rule above all: **the reader sees only what `href` gives a URL**. A
//! note `href` answers `None` for (a hidden folder, a locked folder, an
//! unpublished note) appears nowhere: not as a link, not as a title, not in
//! a trail (which stops below it), not as a group or a count. The list of
//! types in a group comes only from visible notes too.

use std::fmt::Write as _;

use quire_index::{Hier, Index, Joined, TrailMode};
use quire_markdown::render::escape_html;
use quire_vault::NotePath;

use crate::links_state::{RelGroup, relations_where};

/// A note's URL for this reader; `None` when they may not see the note.
pub type Href<'a> = &'a dyn Fn(&NotePath) -> Option<String>;

/// Whether some note the reader may see has a hierarchy edge to another.
///
/// Asked only of the notes `href` accepts, so hidden notes
/// can't turn anything on (not even a link to the Hierarchy page).
pub fn has_visible_hierarchy(ix: &Index, href: Href<'_>) -> bool {
    let rel = ix.relations();
    if !rel.has_hierarchy() {
        return false;
    }
    let keep = |q: &NotePath| href(q).is_some();
    ix.records().any(|r| keep(&r.path) && rel.of(&r.path).iter().any(|e| e.hier.is_some() && e.visible(&keep)))
}

/// Whether to show the trail and typed links when nothing says otherwise:
/// the vault's `[links] trail` (`auto` means when a note the reader may see
/// has a hierarchy edge).
pub fn auto_on(ix: &Index, href: Href<'_>) -> bool {
    match ix.link_config().trail {
        TrailMode::On => true,
        TrailMode::Off => false,
        TrailMode::Auto => has_visible_hierarchy(ix, href),
    }
}

fn link(href: &str, title: &str) -> String {
    format!("<a href=\"{}\">{}</a>", escape_html(href), escape_html(title))
}

/// "Maths › Calculus › Limits", one line per path (at most four), through
/// visible notes only. Empty when the note has no visible parent.
pub fn trail_html(ix: &Index, p: &NotePath, href: Href<'_>) -> String {
    let keep = |q: &NotePath| href(q).is_some();
    let rel = relations_where(&Joined::main_only(ix), p, &keep);
    if rel.trails.is_empty() {
        return String::new();
    }
    let mut out = String::from("<nav class=\"trail\" aria-label=\"Breadcrumbs\">");
    for t in &rel.trails {
        out.push_str("<ol>");
        for (i, (q, title)) in t.iter().enumerate() {
            let last = i + 1 == t.len();
            match href(q) {
                Some(h) if !last => {
                    let _ = write!(out, "<li>{}</li>", link(&h, title));
                }
                _ => {
                    let _ = write!(out, "<li aria-current=\"page\">{}</li>", escape_html(title));
                }
            }
        }
        out.push_str("</ol>");
    }
    out.push_str("</nav>");
    out
}

/// A group's heading for the page: "Parents", "Supports".
fn heading(g: &RelGroup) -> String {
    g.hier.map_or_else(|| g.label.clone(), |h| h.label().to_string())
}

/// "Links by type": the note's typed links, grouped (Parents, Children,
/// Siblings, Previous, Next, then free-form types), to visible notes only.
pub fn typed_html(ix: &Index, p: &NotePath, href: Href<'_>) -> String {
    let keep = |q: &NotePath| href(q).is_some();
    let rel = relations_where(&Joined::main_only(ix), p, &keep);
    let mut out = String::new();
    for g in &rel.groups {
        let items: Vec<String> = g
            .items
            .iter()
            .filter_map(|i| {
                let h = href(&i.path)?;
                let mut text = String::new();
                if i.incoming {
                    text.push_str("<span aria-hidden=\"true\">← </span><span class=\"sr-only\">from </span>");
                }
                text.push_str(&link(&h, &i.title));
                if i.derived {
                    text.push_str(" <small class=\"muted\">(from a tag, folder, list or name)</small>");
                }
                Some(format!("<li>{text}</li>"))
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        let _ = write!(out, "<dt>{}</dt><dd><ul>{}</ul></dd>", escape_html(&heading(g)), items.concat());
    }
    if out.is_empty() {
        return String::new();
    }
    format!("<section class=\"typed\"><h2>Links by type</h2><dl>{out}</dl></section>")
}

/// Most notes one hierarchy page lists.
const TREE_CAP: usize = 3000;

fn walk(
    q: &NotePath,
    path: &mut Vec<NotePath>,
    count: &mut usize,
    out: &mut String,
    href: Href<'_>,
    title: &dyn Fn(&NotePath) -> String,
    kids: &dyn Fn(&NotePath) -> Vec<NotePath>,
) {
    *count += 1;
    let Some(h) = href(q) else { return };
    let again = path.contains(q);
    let sub = if again || *count > TREE_CAP { Vec::new() } else { kids(q) };
    let _ = write!(out, "<li>{}{}", link(&h, &title(q)), if again { " <small class=\"muted\">(again)</small>" } else { "" });
    if !sub.is_empty() {
        path.push(q.clone());
        out.push_str("<ul>");
        for k in &sub {
            walk(k, path, count, out, href, title, kids);
        }
        out.push_str("</ul>");
        path.pop();
    }
    out.push_str("</li>");
}

/// The hierarchy as nested lists.
///
/// Top notes are visible notes with visible children and no visible parent.
/// A note already above it in its branch is shown once more without its
/// children ("again").
pub fn hierarchy_html(ix: &Index, href: Href<'_>) -> String {
    let rel = ix.relations();
    let keep = |q: &NotePath| href(q).is_some();
    let title = |q: &NotePath| ix.get(q).map_or_else(|| q.stem().to_string(), |r| r.title.clone());
    let kids = |q: &NotePath| -> Vec<NotePath> {
        let mut v: Vec<NotePath> = rel.related_where(q, Hier::Down, &keep).into_iter().cloned().collect();
        v.sort_by_cached_key(|k| (title(k).to_lowercase(), k.clone()));
        v
    };
    let mut roots: Vec<NotePath> = ix
        .records()
        .map(|r| &r.path)
        .filter(|q| href(q).is_some() && !kids(q).is_empty() && rel.related_where(q, Hier::Up, &keep).is_empty())
        .cloned()
        .collect();
    roots.sort_by_cached_key(|k| (title(k).to_lowercase(), k.clone()));
    if roots.is_empty() {
        return String::new();
    }
    let mut count = 0;
    let mut out = String::new();
    out.push_str("<ul class=\"tree-h\">");
    for r in &roots {
        walk(r, &mut Vec::new(), &mut count, &mut out, href, &title, &kids);
    }
    out.push_str("</ul>");
    if count > TREE_CAP {
        out.push_str("<p class=\"muted\">The list stops here; open a note to see what is below it.</p>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::{Selection, TypedLinks, publish_with};
    use quire_index::NoteRecord;
    use quire_vault::Vault;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % n as u64).expect("small")
        }
    }

    fn stem(n: usize) -> String {
        format!("note-zz{n}zz")
    }

    fn title(n: usize) -> String {
        format!("Title zz{n}zz")
    }

    /// Notes with random typed links (properties, so they aren't text in a page), tag notes,
    /// folder notes and list notes. `publish:` is random. Returns files and which are published.
    fn random_vault(seed: u64, n: usize) -> (Vec<(String, String)>, Vec<bool>) {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut published = Vec::new();
        let mut files = Vec::new();
        let keys = ["up", "down", "same", "next", "prev", "parent", "related", "supports"];
        for i in 0..n {
            let publish = rng.below(3) != 0;
            published.push(publish);
            let mut fm = format!("publish: {publish}\n");
            for _ in 0..rng.below(4) {
                let k = keys[rng.below(keys.len())];
                let _ = writeln!(fm, "{k}: \"[[{}]]\"", stem(rng.below(n)));
            }
            match rng.below(8) {
                0 => {
                    let _ = write!(fm, "BC-tag-note-tag: shared\nBC-tag-note-field: lbl-zz{i}zz\n");
                }
                1 => {
                    let _ = write!(fm, "BC-folder-note-field: lbl-zz{i}zz\nBC-folder-note-recurse: true\n");
                }
                2 => {
                    let _ = write!(fm, "BC-list-note-field: down\nBC-list-note-neighbour-field: nbr-zz{i}zz\n");
                }
                _ => {}
            }
            let mut body = format!("# {}\n\n", title(i));
            if rng.below(2) == 0 {
                body.push_str("#shared\n");
            }
            if fm.contains("BC-list-note-field") {
                for _ in 0..3 {
                    let _ = writeln!(body, "- [[{}|item]]", stem(rng.below(n)));
                }
            }
            let folder = ["", "A/", "A/B/"][rng.below(3)];
            files.push((format!("{folder}{}.md", stem(i)), format!("---\n{fm}---\n{body}")));
        }
        (files, published)
    }

    fn index(files: &[(String, String)]) -> Index {
        let mut ix = Index::new();
        for (p, s) in files {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p.as_str()), s), s.clone());
        }
        ix.refresh();
        ix
    }

    fn read_all(dir: &std::path::Path) -> Vec<(String, String)> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap_or_default()))
            .collect()
    }

    /// Whatever the links, a published site names no unpublished note, in its trails or its typed links or
    /// anywhere else, and a trail never passes through one.
    #[test]
    fn published_sites_never_name_unpublished_notes() {
        let mut shown_links = 0;
        for seed in 1..=40u64 {
            let n = 6 + usize::try_from(seed % 9).unwrap();
            let (files, published) = random_vault(seed, n);
            let pairs: Vec<(&str, &str)> = files.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();
            let vault = Vault::in_memory(&pairs);
            let ix = index(&files);
            for typed in [TypedLinks::On, TypedLinks::Auto] {
                for which in [Selection::Marked, Selection::All] {
                    let dir = tempfile::tempdir().unwrap();
                    if publish_with(&vault, &ix, dir.path(), "Site", which, typed).is_err() {
                        continue;
                    }
                    for (name, text) in read_all(dir.path()) {
                        shown_links += text.matches("Links by type").count() + text.matches("class=\"trail\"").count();
                        for (i, p) in published.iter().enumerate() {
                            if !p {
                                assert!(
                                    !text.contains(&stem(i)) && !text.contains(&title(i)) && !text.contains(&format!("zz{i}zz")),
                                    "seed {seed}: {name} names unpublished note {i}:\n{text}"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(shown_links > 50, "the test must actually show typed links ({shown_links})");
    }

    /// A hidden list note's neighbour field and the order of its items stay off visible pages.
    #[test]
    fn a_hidden_list_notes_neighbour_edges_are_not_shown() {
        let files = [
            ("Private/Plan.md", "---\nBC-list-note-field: down\nBC-list-note-neighbour-field: layoff-order\n---\n- [[A]]\n- [[B]]\n"),
            ("A.md", "# A\n"),
            ("B.md", "# B\n"),
        ];
        let ix = index(&files.iter().map(|(p, s)| ((*p).to_string(), (*s).to_string())).collect::<Vec<_>>());
        let seen = |p: &NotePath| (!p.as_str().starts_with("Private/")).then(|| format!("/n/{}", p.as_str()));
        for note in ["A.md", "B.md"] {
            let t = typed_html(&ix, &NotePath::new(note), &seen);
            assert!(!t.contains("ayoff") && !t.contains("Next") && !t.contains("Previous"), "{note}: {t}");
        }
        let all = |p: &NotePath| Some(format!("/n/{}", p.as_str()));
        assert!(typed_html(&ix, &NotePath::new("A.md"), &all).contains("Layoff order"), "visible to those who may see the list note");
        let tree = hierarchy_html(&ix, &seen);
        assert!(!tree.contains("B</a>"), "no tree edge from a hidden list: {tree}");
    }

    #[test]
    fn trails_stop_below_a_hidden_note_and_text_is_escaped() {
        let files = [
            ("Top.md", "---\ntitle: Top\n---\n"),
            ("Mid.md", "---\nup: \"[[Top]]\"\ntitle: Hidden <mid>\n---\n"),
            ("Leaf.md", "---\nup: \"[[Mid]]\"\ntitle: Leaf <b>bold</b>\n---\n"),
        ];
        let ix = index(&files.iter().map(|(p, s)| ((*p).to_string(), (*s).to_string())).collect::<Vec<_>>());
        let seen = |p: &NotePath| (p.as_str() != "Mid.md").then(|| format!("/n/{}", p.as_str()));
        assert_eq!(trail_html(&ix, &NotePath::new("Leaf.md"), &seen), "", "no visible parent, no trail");
        let all = |p: &NotePath| Some(format!("/n/{}", p.as_str()));
        let t = trail_html(&ix, &NotePath::new("Leaf.md"), &all);
        assert!(t.contains("Hidden &lt;mid&gt;") && t.contains("Leaf &lt;b&gt;bold&lt;/b&gt;") && !t.contains("<b>"), "{t}");
        let typed = typed_html(&ix, &NotePath::new("Top.md"), &seen);
        assert_eq!(typed, "", "the only child is hidden");
        let tree = hierarchy_html(&ix, &all);
        assert!(tree.contains("Top") && tree.contains("Hidden &lt;mid&gt;"), "{tree}");
        assert!(!hierarchy_html(&ix, &seen).contains("Mid"));
    }
}
