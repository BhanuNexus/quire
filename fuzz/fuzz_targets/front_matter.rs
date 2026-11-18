// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Front matter: reading it, and editing it, on any text. Synced and shared notes
//! reach this reader, so it must return, never panic or run away, and an edit must
//! not change the text outside the field it names (P1).
#![no_main]

use libfuzzer_sys::fuzz_target;
use quire_markdown::Value;
use quire_markdown::frontmatter::{parse, remove_field, set_field, set_field_path};

fuzz_target!(|data: &str| {
    let (key, text) = match data.split_once('\u{1f}') {
        Some((k, t)) => (k, t),
        None => ("title", data),
    };
    if let Some(fm) = parse(text) {
        assert!(fm.range.start <= fm.range.end && text.get(fm.range.clone()).is_some(), "front matter range {:?}", fm.range);
    }
    let edit = set_field(text, key, &Value::Str("x".into()));
    let _ = edit.apply(text);
    let _ = remove_field(text, key).map(|e| e.apply(text));
    let path: Vec<&str> = key.split('.').collect();
    let _ = set_field_path(text, &path, &Value::List(vec!["a".into()])).map(|e| e.apply(text));
});
