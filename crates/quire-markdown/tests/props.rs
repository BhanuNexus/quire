// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Property tests (proptest) for the front-matter reader and writers, and
//! for tidying.
//!
//! Proptest shrinks a failure to a small input and saves its seed in
//! `props.proptest-regressions` beside this file; that file is committed, so
//! every case found once replays on every run. A run is 256 cases a property
//! (about a second); `PROPTEST_CASES=50000` makes it a cheap fuzzer.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use quire_markdown::Value;
use quire_markdown::frontmatter::{parse, remove_field, set_field};
use quire_markdown::linter::{Cx, Options, lint};

fn key() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,7}"
}

/// A one-line value; the characters YAML makes awkward are well
/// represented, and so are leading and trailing spaces (D5).
fn scalar() -> impl Strategy<Value = String> {
    "[ a-zA-Z0-9:#'\"\\[\\]{}&*!|>%@,.-]{0,26}"
}

/// A note: front matter built through the writer itself, then a body.
fn note() -> impl Strategy<Value = String> {
    (prop::collection::btree_map(key(), scalar(), 0..6), "(# T\n\n)?[a-z \n]{0,40}").prop_map(|(fields, body)| {
        let mut src = body;
        for (k, v) in fields.iter().rev() {
            src = set_field(&src, k, &Value::Str(v.clone())).apply(&src);
        }
        src
    })
}

fn fields_of(src: &str) -> BTreeMap<String, Value> {
    parse(src).map(|f| f.fields).unwrap_or_default()
}

fn body_of(src: &str) -> &str {
    &src[parse(src).map_or(0, |f| f.range.end)..]
}

proptest! {
    /// `set_field` changes the one field and nothing else, the value reads
    /// back exactly as given, and the body is byte-identical.
    #[test]
    fn set_field_changes_only_its_field(src in note(), k in key(), v in scalar()) {
        let before = fields_of(&src);
        let out = set_field(&src, &k, &Value::Str(v.clone())).apply(&src);
        let after = fields_of(&out);
        prop_assert_eq!(after.get(&k), Some(&Value::Str(v)), "{:?} -> {:?}", src, out);
        for (other, val) in &before {
            if other != &k {
                prop_assert_eq!(after.get(other), Some(val), "{} changed: {:?} -> {:?}", other, src, out);
            }
        }
        prop_assert_eq!(after.len(), before.len() + usize::from(!before.contains_key(&k)));
        prop_assert_eq!(body_of(&out), body_of(&src));
    }

    /// Adding a field and removing it again gives the fields back.
    #[test]
    fn set_then_remove_restores_the_fields(src in note(), k in key(), v in scalar()) {
        prop_assume!(!fields_of(&src).contains_key(&k));
        let with = set_field(&src, &k, &Value::Str(v)).apply(&src);
        let without = match remove_field(&with, &k) {
            Some(e) => e.apply(&with),
            None => with.clone(),
        };
        prop_assert_eq!(fields_of(&without), fields_of(&src), "{:?} -> {:?} -> {:?}", src, with, without);
    }

    /// Tidying is idempotent on any text. (Whether a rule needed the safety
    /// net is not asserted: `trailing-spaces` gives up on a code span that
    /// crosses a line, found by this test at 30,000 cases; see
    /// docs/design/0.14-health-fm.md.)
    #[test]
    fn tidy_is_idempotent(text in "(?s)[-a-z0-9 #*_`>\\[\\]()|:~=\r\n]{0,200}") {
        let (o, cx) = (Options::default(), Cx { stamp: "2026-11-10T09:30", edited: false });
        let once = lint(&text, &o, &cx);
        let twice = lint(&once.text, &o, &cx);
        prop_assert_eq!(&twice.text, &once.text);
    }

    /// Readers and writers return, never panic, on arbitrary text.
    #[test]
    fn front_matter_functions_are_total(body in "(?s).{0,300}", k in "(?s).{0,8}") {
        let src = format!("---\n{body}");
        let _ = parse(&src);
        let _ = parse(&body);
        let _ = set_field(&src, &k, &Value::Str("x".into())).apply(&src);
        let _ = remove_field(&src, &k).map(|e| e.apply(&src));
    }
}
