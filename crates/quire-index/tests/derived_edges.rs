// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13: edges derived from Breadcrumbs' tag, folder, list and Dendron
//! notes, on a vault written the way Breadcrumbs users write theirs
//! (`tests/fixtures/breadcrumbs/`).

#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::BTreeSet;

use quire_index::{Dendron, Hier, Index, LinkConfig, NoteRecord};
use quire_vault::NotePath;

fn load(dir: &std::path::Path, root: &std::path::Path, ix: &mut Index) {
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        if path.is_dir() {
            load(&path, root, ix);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let src = std::fs::read_to_string(&path).unwrap();
            ix.insert_record(NoteRecord::from_source(NotePath::new(rel), &src), src);
        }
    }
}

fn vault(cfg: LinkConfig) -> Index {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/breadcrumbs");
    let mut ix = Index::new();
    load(&root, &root, &mut ix);
    ix.set_link_config(cfg);
    ix.refresh();
    ix
}

fn p(s: &str) -> NotePath {
    NotePath::new(s)
}

fn edges(ix: &Index, from: &str, field: &str) -> BTreeSet<String> {
    ix.relations().of(&p(from)).iter().filter(|e| e.field == field && !e.implied).map(|e| e.to.to_string()).collect()
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(ToString::to_string).collect()
}

#[test]
fn tag_notes() {
    let ix = vault(LinkConfig::default());
    assert_eq!(edges(&ix, "Recipes.md", "down"), set(&["Soup.md", "Cake.md"]), "a nested tag counts");
    assert_eq!(edges(&ix, "Exact Recipes.md", "sibling"), set(&["Soup.md"]), "exact skips #recipe/dessert; the field can be a sibling");
    assert!(ix.relations().of(&p("Recipes.md")).iter().all(|e| e.derived));
    // Implied reverse edges as for written links.
    assert_eq!(ix.relations().parents(&p("Soup.md")), [&p("Recipes.md")]);
    assert!(ix.relations().of(&p("Other.md")).is_empty());
}

#[test]
fn folder_notes() {
    let ix = vault(LinkConfig::default());
    assert_eq!(edges(&ix, "Topics/Topics.md", "down"), set(&["Topics/Alpha.md", "Topics/Sub/Beta.md"]), "recurse reaches subfolders");
    assert_eq!(ix.relations().trails(&p("Topics/Sub/Beta.md"), 4, 10), [vec![p("Topics/Topics.md"), p("Topics/Sub/Beta.md")]]);
}

#[test]
fn list_notes() {
    let ix = vault(LinkConfig::default());
    assert_eq!(edges(&ix, "Reading List.md", "down"), set(&["Book One.md", "Book Two.md"]));
    assert_eq!(edges(&ix, "Book One.md", "down"), set(&["Chapter 1.md"]));
    assert_eq!(edges(&ix, "Book One.md", "child"), set(&["Chapter 2.md"]), "a field before the link names that edge");
    assert_eq!(edges(&ix, "Book One.md", "next"), set(&["Book Two.md"]), "neighbour field");
    assert_eq!(edges(&ix, "Chapter 1.md", "next"), set(&["Chapter 2.md"]));
    assert!(ix.relations().incoming(&p("Not A Link In Code.md")).is_empty(), "code blocks are skipped");
    assert_eq!(ix.relations().step(&p("Book Two.md"), false), Some(p("Book One.md")));
}

#[test]
fn dendron_names_need_the_setting() {
    assert!(vault(LinkConfig::default()).relations().parents(&p("proj.alpha.md")).is_empty());
    let on = vault(LinkConfig { dendron: Dendron { enabled: true, ..Dendron::default() }, ..LinkConfig::default() });
    assert_eq!(edges(&on, "proj.alpha.beta.md", "up"), set(&["proj.alpha.md"]));
    assert_eq!(on.relations().trails(&p("proj.alpha.beta.md"), 4, 10), [vec![p("proj.md"), p("proj.alpha.md"), p("proj.alpha.beta.md")]]);
    assert_eq!(on.relations().children(&p("proj.md")), [&p("proj.alpha.md")]);
}

#[test]
fn derived_edges_are_stable_and_never_double_a_written_link() {
    let a = vault(LinkConfig::default());
    let b = vault(LinkConfig::default());
    let list = |ix: &Index| ix.relations().derived().map(|(f, k, t, o)| format!("{f} {k} {t} by {o}")).collect::<Vec<_>>();
    assert_eq!(list(&a), list(&b), "the order doesn't depend on how the index was filled");
    // A note that also writes the link by hand has one edge, and it is the written one.
    let mut ix = vault(LinkConfig::default());
    let src = "---\nBC-tag-note-tag: recipe\nBC-tag-note-field: down\n---\ndown:: [[Soup]]\n".to_string();
    ix.insert_record(NoteRecord::from_source(p("Recipes.md"), &src), src);
    ix.refresh();
    let soup: Vec<_> = ix.relations().of(&p("Recipes.md")).iter().filter(|e| e.to == p("Soup.md") && e.hier == Some(Hier::Down)).collect();
    assert_eq!(soup.len(), 1);
    assert!(!soup[0].derived);
}

#[test]
fn derived_edges_and_their_reverses_name_the_note_that_made_them() {
    let ix = vault(LinkConfig::default());
    let rel = ix.relations();
    // Book One next Book Two comes from the list note, and so does the implied Book Two prev Book One.
    let next = rel.of(&p("Book One.md")).iter().find(|e| e.field == "next" && e.to == p("Book Two.md")).unwrap();
    assert_eq!(next.origin, Some(p("Reading List.md")));
    let prev = rel.of(&p("Book Two.md")).iter().find(|e| e.hier == Some(Hier::Prev) && e.to == p("Book One.md")).unwrap();
    assert!(prev.implied && prev.origin == Some(p("Reading List.md")));
    assert!(rel.incoming(&p("Book Two.md")).iter().any(|e| e.origin == Some(p("Reading List.md"))));
    // Hide the list note: the neighbour edges, the implied ones and the trails through them go with it.
    let keep = |q: &NotePath| q.as_str() != "Reading List.md";
    assert!(!next.visible(&keep) && !prev.visible(&keep));
    assert!(rel.related_where(&p("Book One.md"), Hier::Next, &keep).is_empty());
    assert!(rel.related_where(&p("Book Two.md"), Hier::Prev, &keep).is_empty());
    assert_eq!(rel.related(&p("Book One.md"), Hier::Next), [&p("Book Two.md")]);
    // A written link has no origin.
    assert!(rel.of(&p("Recipes.md")).iter().all(|e| e.origin == Some(p("Recipes.md"))));
}
