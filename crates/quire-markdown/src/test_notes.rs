// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text for the tests that compare a scanner with the code it replaced: real notes,
//! and generated notes that put every construct the scanners look at in every order.

use proptest::prelude::*;

/// How many cases a property test runs: `default`, or more when `PROPTEST_CASES` asks
/// for more (the weekly job sets 50000), so the environment can only raise it.
pub(crate) fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse::<u32>().ok()).map_or(default, |n| n.max(default))
}

/// The notes of the sample vault and a design note (a copy, in `tests/fixtures/`, so
/// moving the note in `docs/` breaks nothing).
pub(crate) fn fixture_notes() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "md") {
                out.push(std::fs::read_to_string(&p).unwrap());
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    walk(&root.join("sample-vault"), &mut out);
    out.push(std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/design-note.md")).unwrap());
    assert!(out.len() > 5, "the fixture notes were not found");
    out
}

/// A note made of lines with code fences, front matter, math, indented code,
/// callouts, block ids, flash cards, highlights, citations and links, in any order.
pub(crate) fn note_lines() -> impl Strategy<Value = String> {
    let line = prop::sample::select(vec![
        "",
        "",
        "---",
        "title: x",
        "text",
        "text ^id1",
        "^id2",
        "x ^id-3 ",
        "- item ^id4",
        "```",
        "```rust",
        "~~~",
        "$$",
        "x ^2 $$",
        "    indented ^id5",
        "\tcode",
        "> [!note] Title",
        "> [!warning]- Folded ^id6",
        "> body ^id7",
        "Question",
        "?",
        "Answer",
        "Q :: A",
        "a {{c1::cloze}} b",
        "==mark== and `code ==x==`",
        "@key and [@key, p. 1] and [[link]]",
        "[[open",
        "see [[a]] and [[b",
        "[x](u) `a` <b>h</b> ![[e]]",
        "a\r",
    ]);
    prop::collection::vec(line, 0..40).prop_map(|v| v.join("\n"))
}
