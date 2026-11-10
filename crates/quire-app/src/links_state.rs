// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 typed links and hierarchy: the entry module.
//!
//! What the desktop shows for typed links, worked out once per index
//! version and note and then shared (`Rc`): the Insights groups
//! ("Parents", "Children", "Supports"…), the breadcrumbs trail, the
//! hierarchy tree's rows. Commands: "Add typed link…" (a dialog in the
//! desktop), "Go to parent", "Go to next/previous sibling" and "Show
//! hierarchy". The edges themselves are in the index
//! (`quire_index::relations`), built with the backlinks.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use quire_index::{Hier, Index, LinkConfig, TrailMode};
use quire_markdown::fields::{self, FieldStyle};
use quire_text::{EditKind, Selection, Transaction};
use quire_vault::{NotePath, VaultEvent, VaultPrefs};

use crate::commands::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// One related note in a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelItem {
    pub path: NotePath,
    pub title: String,
    /// The field it comes through (`parent`, `supports`).
    pub field: String,
    /// Implied by the other note's link (A `up` B shows on B as a child).
    pub implied: bool,
    /// The other note links here (`←`), rather than this note to it.
    pub incoming: bool,
}

/// Notes related one way: "Parents", "Supports"…
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelGroup {
    pub label: String,
    /// The hierarchy kind, for hierarchy groups.
    pub hier: Option<Hier>,
    pub items: Vec<RelItem>,
}

/// Everything typed about the active note.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteRelations {
    /// Hierarchy groups first (Parents, Children, Siblings, Previous,
    /// Next), then Supports, Contradicts, then other types by name.
    pub groups: Vec<RelGroup>,
    /// Paths from a top note down to this one, as (path, title).
    pub trails: Vec<Vec<(NotePath, String)>>,
    /// The trail is turned on for this vault (`[links] trail`, or `auto`
    /// with hierarchy fields in the vault).
    pub trail_on: bool,
}

impl NoteRelations {
    /// Whether to draw the trail above the note.
    pub fn show_trail(&self) -> bool {
        self.trail_on && !self.trails.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// A row of the hierarchy tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: NotePath,
    pub title: String,
    pub depth: usize,
    pub has_children: bool,
    /// Already shown above it in this branch (a cycle): not expandable.
    pub repeat: bool,
}

type RelCache = RefCell<Option<((u64, Option<NotePath>), Rc<NoteRelations>)>>;

#[derive(Default)]
pub struct LinksState {
    /// vault.toml was read once since the vault opened (the index read `[links]` on load).
    prefs_seen: bool,
    cache: RelCache,
}

/// Most trails shown above a note, and their longest length.
const MAX_TRAILS: usize = 4;
const MAX_TRAIL_DEPTH: usize = 32;

fn title_of(ix: &Index, p: &NotePath) -> String {
    ix.get(p).map_or_else(|| p.stem().to_string(), |r| r.title.clone())
}

/// `supports` → "Supports", `see-also` → "See also".
pub fn type_label(field: &str) -> String {
    let words = field.replace(['-', '_'], " ");
    let mut c = words.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Sort key of a free-form group: Supports, Contradicts, then by name.
fn free_rank(field: &str) -> (u8, String) {
    match field {
        "supports" => (0, String::new()),
        "contradicts" => (1, String::new()),
        f => (2, f.to_string()),
    }
}

/// The groups and trails of `p`.
pub fn relations_of(ix: &Index, p: &NotePath) -> NoteRelations {
    let rel = ix.relations();
    let cfg = ix.link_config();
    let mut hier: Vec<RelGroup> = Vec::new();
    let mut free: Vec<(String, RelGroup)> = Vec::new();
    let mut push = |label: String, h: Option<Hier>, key: String, item: RelItem| {
        let group = if let Some(h) = h {
            if let Some(g) = hier.iter_mut().find(|g| g.hier == Some(h)) {
                g
            } else {
                hier.push(RelGroup { label, hier: Some(h), items: Vec::new() });
                hier.last_mut().expect("just pushed")
            }
        } else if let Some((_, g)) = free.iter_mut().find(|(k, _)| *k == key) {
            g
        } else {
            free.push((key, RelGroup { label, hier: None, items: Vec::new() }));
            &mut free.last_mut().expect("just pushed").1
        };
        if !group.items.iter().any(|i| i.path == item.path && i.incoming == item.incoming) {
            group.items.push(item);
        }
    };
    for e in rel.of(p) {
        let item = RelItem { path: e.to.clone(), title: title_of(ix, &e.to), field: e.field.clone(), implied: e.implied, incoming: false };
        match e.hier {
            Some(h) => push(h.label().to_string(), Some(h), String::new(), item),
            None => push(type_label(&e.field), None, e.field.clone(), item),
        }
    }
    for e in rel.incoming(p) {
        // Hierarchy links into here already show as implied edges.
        if e.hier.is_some() && cfg.implied {
            continue;
        }
        let item = RelItem { path: e.to.clone(), title: title_of(ix, &e.to), field: e.field.clone(), implied: false, incoming: true };
        push(type_label(&e.field), None, e.field.clone(), item);
    }
    hier.sort_by_key(|g| match g.hier {
        Some(Hier::Up) => 0,
        Some(Hier::Down) => 1,
        Some(Hier::Same) => 2,
        Some(Hier::Prev) => 3,
        _ => 4,
    });
    free.sort_by_key(|(k, _)| free_rank(k));
    // A note's own links keep their written order; implied and incoming ones
    // follow by title, so the order doesn't depend on how the index was built.
    for g in hier.iter_mut().chain(free.iter_mut().map(|(_, g)| g)) {
        g.items.sort_by_cached_key(|i| {
            let derived = i.implied || i.incoming;
            (derived, if derived { i.title.to_lowercase() } else { String::new() })
        });
    }
    let groups = hier.into_iter().chain(free.into_iter().map(|(_, g)| g)).collect();
    let trails = rel
        .trails(p, MAX_TRAILS, MAX_TRAIL_DEPTH)
        .into_iter()
        .map(|t| t.into_iter().map(|q| (q.clone(), title_of(ix, &q))).collect())
        .collect();
    let trail_on = match cfg.trail {
        TrailMode::On => true,
        TrailMode::Off => false,
        TrailMode::Auto => rel.has_hierarchy(),
    };
    NoteRelations { groups, trails, trail_on }
}

/// The top of each hierarchy: notes with children and no parents, by title.
pub fn tree_roots(ix: &Index) -> Vec<(NotePath, String)> {
    let rel = ix.relations();
    let mut v: Vec<(NotePath, String)> = ix
        .paths()
        .into_iter()
        .filter(|p| rel.parents(p).is_empty() && !rel.children(p).is_empty())
        .map(|p| {
            let t = title_of(ix, &p);
            (p, t)
        })
        .collect();
    v.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
    v
}

/// The tree under `roots` as rows, with the notes in `open` expanded.
///
/// Only expanded branches are walked, so the cost follows what is shown;
/// a note already on the branch above (a cycle) isn't expanded again.
pub fn tree_rows<S: std::hash::BuildHasher>(ix: &Index, roots: &[NotePath], open: &std::collections::HashSet<NotePath, S>) -> Vec<TreeRow> {
    fn walk<S: std::hash::BuildHasher>(
        ix: &Index,
        p: &NotePath,
        depth: usize,
        open: &std::collections::HashSet<NotePath, S>,
        path: &mut Vec<NotePath>,
        out: &mut Vec<TreeRow>,
    ) {
        let rel = ix.relations();
        let repeat = path.contains(p);
        let mut kids: Vec<&NotePath> = rel.children(p);
        out.push(TreeRow { path: p.clone(), title: title_of(ix, p), depth, has_children: !kids.is_empty() && !repeat, repeat });
        if repeat || !open.contains(p) || out.len() > 100_000 {
            return;
        }
        kids.sort_by_key(|k| title_of(ix, k).to_lowercase());
        path.push(p.clone());
        for k in kids {
            walk(ix, k, depth + 1, open, path, out);
        }
        path.pop();
    }
    let mut out = Vec::new();
    for r in roots {
        walk(ix, r, 0, open, &mut Vec::new(), &mut out);
    }
    out
}

/// Field names to offer: the hierarchy names in the settings, then every
/// other field that holds links in this vault, most used first.
pub fn field_names(ix: &Index) -> Vec<String> {
    let cfg = ix.link_config();
    let mut v: Vec<String> = Hier::ALL.iter().map(|h| cfg.field_for(*h)).collect();
    for (f, _) in ix.relations().fields() {
        if !v.iter().any(|x| x == f) {
            v.push(f.to_string());
        }
    }
    for f in ["supports", "contradicts", "source", "related"] {
        if !v.iter().any(|x| x == f) {
            v.push(f.to_string());
        }
    }
    v
}

/// The text typed into an inline field's value, when the caret is there.
///
/// `parent:: Ro` or `- [up:: [[A]], Ro` → (`parent`, char column of
/// `Ro`, `Ro`). `before` is the line up to the caret. `None` once the
/// value being typed has a bracket in it (`[[` has its own suggestions).
pub fn field_value_query(before: &str) -> Option<(String, usize, String)> {
    let sep = before.rfind("::")?;
    let head = &before[..sep];
    let value = &before[sep + 2..];
    let part_at = value.rfind(',').map_or(0, |i| i + 1);
    let part = &value[part_at..];
    let query = part.trim_start();
    if query.contains(['[', ']', '(', ')']) || query.chars().count() > 60 {
        return None;
    }
    // The key: after an open `[` or `(` (bracketed field), else the line's text after list or quote markers.
    let key = match head.rfind(['[', '(']) {
        Some(open) if fields::is_field_name(&head[open + 1..]) => &head[open + 1..],
        _ => head.trim_start().trim_start_matches(['>', ' ']).trim_start_matches(['-', '*', '+']).trim_start(),
    };
    if !fields::is_field_name(key) {
        return None;
    }
    let start = sep + 2 + part_at + (part.len() - query.len());
    Some((fields::normalize_key(key), before[..start].chars().count(), query.to_string()))
}

impl Workspace {
    /// Whether `field` holds links: a hierarchy field or a type used in the vault.
    pub fn is_link_field(&self, field: &str) -> bool {
        let ix = self.index.read();
        ix.link_config().kind(field).is_some() || field_names(&ix).iter().any(|f| f == field)
    }

    /// Typed links of the active note, cached per index version. Cheap to call every frame.
    pub fn note_relations(&self) -> Rc<NoteRelations> {
        let path = self.active_doc().and_then(|d| d.path.clone());
        let key = (self.index.version(), path);
        let st = &self.features.links;
        if let Some((k, v)) = st.cache.borrow().as_ref()
            && *k == key
        {
            return Rc::clone(v);
        }
        let Some(p) = key.1.as_ref() else { return Rc::default() };
        // Never wait for the index worker on a frame: keep the last result
        // (or nothing) until it has finished its batch.
        let Some(ix) = self.index.try_read() else {
            return st.cache.borrow().as_ref().filter(|(k, _)| k.1 == key.1).map(|(_, v)| Rc::clone(v)).unwrap_or_default();
        };
        let rc = Rc::new(relations_of(&ix, p));
        drop(ix);
        *st.cache.borrow_mut() = Some((key, Rc::clone(&rc)));
        rc
    }

    /// Add a typed link from the active note to `target`, in the style the
    /// note already uses (front matter or inline field). An undoable edit.
    pub fn add_typed_link(&mut self, field: &str, target: &NotePath) -> bool {
        if !fields::is_field_name(field) {
            self.notify(NoticeLevel::Warn, "A link type is a word or words: letters, digits, “-” and “_”");
            return false;
        }
        let Some(doc) = self.active_doc() else { return false };
        let text = doc.text();
        let style = fields::preferred_style(&text);
        let name = self.link_name(target, doc.path.as_ref());
        let edit = match fields::try_add_link(&text, field, &name, style) {
            Ok(Some(edit)) => edit,
            Err(why) => {
                self.notify(
                    NoticeLevel::Warn,
                    format!("Can't add a “{field}” link without rewriting what the note has there ({why}). Change that property by hand."),
                );
                return false;
            }
            Ok(None) => {
                self.notify(
                    NoticeLevel::Info,
                    format!("{} already links to {} as {}", doc.title(), target.stem(), fields::normalize_key(field)),
                );
                return false;
            }
        };
        let doc = self.active_doc_mut().expect("checked");
        apply_edit(doc, &edit)
    }

    /// How to name `target` in a link from `from`: its name, or its path
    /// when the name alone would find another note.
    fn link_name(&self, target: &NotePath, from: Option<&NotePath>) -> String {
        let ix = self.index.read();
        if ix.resolve(target.stem(), from).as_ref() == Some(target) { target.stem().to_string() } else { target.without_ext().to_string() }
    }

    /// Open the active note's first parent ("Go to parent").
    fn go_to_parent(&mut self) {
        let Some(p) = self.active_doc().and_then(|d| d.path.clone()) else { return };
        let parent = {
            let ix = self.index.read();
            let rel = ix.relations();
            // The parent on the first trail, so it matches what the trail shows.
            rel.trails(&p, 1, MAX_TRAIL_DEPTH)
                .first()
                .and_then(|t| t.iter().rev().nth(1).cloned())
                .or_else(|| rel.parents(&p).first().map(|q| (*q).clone()))
        };
        match parent {
            Some(q) => {
                self.open_note(&q);
            }
            None => self.notify(NoticeLevel::Info, "This note has no parent (no up or parent link)"),
        }
    }

    fn go_to_sibling(&mut self, forward: bool) {
        let Some(p) = self.active_doc().and_then(|d| d.path.clone()) else { return };
        let next = self.index.read().relations().step(&p, forward);
        match next {
            Some(q) => {
                self.open_note(&q);
            }
            None => self.notify(NoticeLevel::Info, if forward { "No next note here" } else { "No previous note here" }),
        }
    }
}

/// Apply a front-matter-style edit (byte range and text) to a document as
/// one undoable step, keeping the caret on the same text.
fn apply_edit(doc: &mut Document, edit: &quire_markdown::frontmatter::Edit) -> bool {
    let buf = doc.buffer();
    let (s, e) = (buf.byte_to_char(edit.range.start), buf.byte_to_char(edit.range.end));
    let head = doc.selection().head;
    let new_len = edit.text.chars().count();
    let caret = if head >= e { head - (e - s) + new_len } else { head.min(s + new_len) };
    doc.apply(Transaction::replace(s, e, edit.text.clone()).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok()
}

/// How a note writes its typed links (for the "Add typed link" dialog's hint).
pub fn style_of(ws: &Workspace) -> Option<FieldStyle> {
    ws.active_doc().map(|d| fields::preferred_style(&d.text()))
}

/// vault.toml changed (or was first read): use its `[links]` section. The
/// first read after a vault opens is skipped: the index read the section
/// itself when it loaded the vault, off the UI thread.
pub(crate) fn read_prefs(ws: &mut Workspace, prefs: &VaultPrefs) {
    if !std::mem::replace(&mut ws.features.links.prefs_seen, true) {
        return;
    }
    let cfg: LinkConfig = prefs.section(quire_index::relations::SECTION);
    if ws.index.read().link_config() != &cfg {
        ws.index.set_link_config(cfg);
    }
}

pub(crate) const fn tick(_ws: &mut Workspace, _now: Instant) -> bool {
    false
}

pub(crate) const fn next_deadline(_ws: &Workspace) -> Option<Instant> {
    None
}

pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::GoToParent => ws.go_to_parent(),
        Command::GoToNextSibling => ws.go_to_sibling(true),
        Command::GoToPrevSibling => ws.go_to_sibling(false),
        _ => return Some(UiRequest::Feature(cmd)),
    }
    None
}

pub(crate) fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        Command::ShowHierarchy => ws.vault().is_some(),
        _ => ws.active_doc().is_some_and(|d| d.path.is_some()),
    }
}

pub(crate) const fn decorations(_ws: &Workspace, _doc: &Document, _out: &mut Vec<Decoration>) {}

pub(crate) const fn decoration_event(_ws: &mut Workspace, _ev: &DecorationEvent) {}

pub(crate) const fn vault_event(_ws: &mut Workspace, _ev: &VaultEvent) {}

pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.links = LinksState::default();
}

pub(crate) const fn status(_ws: &Workspace, _out: &mut Vec<StatusItem>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(files));
        ws.index().flush();
        ws
    }

    const VAULT: &[(&str, &str)] = &[
        ("Root.md", "# Root\n"),
        ("A.md", "---\nup: \"[[Root]]\"\n---\n# A\n[supports:: [[Claim]]]\n"),
        ("B.md", "# B\nparent:: [[Root]]\ncontradicts:: [[Claim]]\n"),
        ("C.md", "# C\nup:: [[A]]\nsource:: [[Book]]\n"),
        ("Claim.md", "# Claim\n"),
        ("Book.md", "# Book\n"),
    ];

    fn labels(r: &NoteRelations) -> Vec<(&str, Vec<&str>)> {
        r.groups.iter().map(|g| (g.label.as_str(), g.items.iter().map(|i| i.title.as_str()).collect())).collect()
    }

    #[test]
    fn groups_by_type_in_order() {
        let mut ws = ws(VAULT);
        ws.open_note(&"A.md".into());
        let r = ws.note_relations();
        assert_eq!(labels(&r), [("Parents", vec!["Root"]), ("Children", vec!["C"]), ("Supports", vec!["Claim"])]);
        assert!(Rc::ptr_eq(&r, &ws.note_relations()), "cached per index version");
        assert_eq!(r.trails, [vec![("Root.md".into(), "Root".to_string()), ("A.md".into(), "A".to_string())]]);
        assert!(r.show_trail());
        ws.open_note(&"Claim.md".into());
        let r = ws.note_relations();
        assert_eq!(labels(&r), [("Supports", vec!["A"]), ("Contradicts", vec!["B"])]);
        assert!(r.groups.iter().all(|g| g.items.iter().all(|i| i.incoming)));
        assert!(!r.show_trail(), "a note without parents has no trail");
        ws.open_note(&"Root.md".into());
        assert_eq!(labels(&ws.note_relations()), [("Children", vec!["A", "B"])]);
    }

    #[test]
    fn tree_shows_expanded_branches_only() {
        let ws = ws(VAULT);
        let ix = ws.index().read();
        let roots: Vec<NotePath> = tree_roots(&ix).into_iter().map(|r| r.0).collect();
        assert_eq!(roots, [NotePath::from("Root.md")]);
        let rows = tree_rows(&ix, &roots, &std::collections::HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].has_children);
        let open = std::collections::HashSet::from([NotePath::from("Root.md"), NotePath::from("A.md")]);
        let rows = tree_rows(&ix, &roots, &open);
        assert_eq!(rows.iter().map(|r| (r.title.as_str(), r.depth)).collect::<Vec<_>>(), [("Root", 0), ("A", 1), ("C", 2), ("B", 1)]);
    }

    #[test]
    fn a_cycle_is_shown_once() {
        let ws = ws(&[("X.md", "up:: [[Y]]\n"), ("Y.md", "up:: [[X]]\n")]);
        let ix = ws.index().read();
        let open = std::collections::HashSet::from([NotePath::from("X.md"), NotePath::from("Y.md")]);
        let rows = tree_rows(&ix, &[NotePath::from("X.md")], &open);
        assert_eq!(rows.len(), 3);
        assert!(rows[2].repeat && !rows[2].has_children);
        assert!(relations_of(&ix, &NotePath::from("X.md")).trails.len() <= MAX_TRAILS);
    }

    #[test]
    fn add_typed_link_uses_the_note_s_style_and_undoes() {
        let mut ws = ws(VAULT);
        ws.open_note(&"B.md".into());
        assert!(ws.add_typed_link("parent", &"A.md".into()));
        assert_eq!(&*ws.active_doc().unwrap().text(), "# B\nparent:: [[Root]], [[A]]\ncontradicts:: [[Claim]]\n");
        assert!(!ws.add_typed_link("parent", &"A.md".into()), "already there");
        assert!(!ws.add_typed_link("bad:name", &"A.md".into()));
        ws.open_note(&"A.md".into());
        assert!(ws.add_typed_link("up", &"Book.md".into()));
        assert!(ws.active_doc().unwrap().text().starts_with("---\nup: [\"[[Root]]\", \"[[Book]]\"]\n---\n"));
        assert!(ws.active_doc_mut().unwrap().undo());
        assert_eq!(&*ws.active_doc().unwrap().text(), VAULT[1].1);
    }

    #[test]
    fn go_to_commands() {
        let mut ws =
            ws(&[("R.md", "# R\n"), ("One.md", "up:: [[R]]\n"), ("Two.md", "up:: [[R]]\n"), ("Three.md", "up:: [[R]]\nup:: [[One]]\n")]);
        ws.open_note(&"One.md".into());
        assert_eq!(ws.run(Command::GoToParent), None);
        assert_eq!(ws.active_doc().unwrap().path, Some("R.md".into()));
        ws.open_note(&"One.md".into());
        ws.run(Command::GoToNextSibling);
        assert_eq!(ws.active_doc().unwrap().path, Some("Three.md".into()));
        ws.run(Command::GoToPrevSibling);
        assert_eq!(ws.active_doc().unwrap().path, Some("One.md".into()));
        ws.open_note(&"R.md".into());
        ws.run(Command::GoToParent);
        assert!(ws.notices.iter().any(|n| n.text.contains("no parent")));
        assert!(matches!(ws.run(Command::AddTypedLink), Some(UiRequest::Feature(Command::AddTypedLink))));
    }

    #[test]
    fn field_names_offer_hierarchy_then_vault_types() {
        let ws = ws(VAULT);
        let names = field_names(&ws.index().read());
        assert_eq!(&names[..5], ["up", "down", "same", "next", "prev"]);
        assert!(names.contains(&"contradicts".to_string()) && names.contains(&"parent".to_string()));
    }

    #[test]
    fn field_value_queries() {
        assert_eq!(field_value_query("parent:: Ro"), Some(("parent".into(), 9, "Ro".into())));
        assert_eq!(field_value_query("- up:: "), Some(("up".into(), 7, String::new())));
        assert_eq!(field_value_query("child:: [[A]], Be"), Some(("child".into(), 15, "Be".into())));
        assert_eq!(field_value_query("Claim [supports:: Th"), Some(("supports".into(), 18, "Th".into())));
        assert_eq!(field_value_query("parent:: [[Ro"), None);
        assert_eq!(
            field_value_query("A sentence:: x").map(|q| q.0),
            Some("a-sentence".into()),
            "a field; whether it holds links is asked separately"
        );
        assert_eq!(field_value_query("no field here"), None);
    }

    #[test]
    fn type_labels() {
        assert_eq!(type_label("supports"), "Supports");
        assert_eq!(type_label("see-also"), "See also");
        assert_eq!(type_label("blocked_by"), "Blocked by");
    }
}
