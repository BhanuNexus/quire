// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian Bases expressions: a `.base` file can come from another device, and
//! its expressions run for every row on the UI thread. Parsing and evaluating any
//! text must return (an error is fine) within the limits in `expr::limits`.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use quire_import::obsidian::expr::{Clock, Eval, Row, Val, infer, parse};

struct Note;

impl Row for Note {
    fn property(&self, name: &str) -> Option<Val> {
        match name {
            "price" => Some(Val::Num(12.5)),
            "qty" => Some(infer("3")),
            "due" => Some(infer("2026-11-20")),
            "tags" => Some(Val::List(vec![Val::Str("a".into()), Val::Str("b".into())])),
            _ => None,
        }
    }
    fn file(&self, field: &str) -> Option<Val> {
        match field {
            "name" | "basename" | "path" | "folder" | "ext" => Some(Val::Str("Rent".into())),
            "size" => Some(Val::Num(1200.0)),
            "ctime" | "mtime" => Some(infer("2026-11-01 08:30")),
            "tags" | "links" => Some(Val::List(vec![Val::Str("a".into())])),
            _ => None,
        }
    }
    fn has_tag(&self, tag: &str) -> bool {
        tag == "a"
    }
    fn has_link(&self, target: &str) -> bool {
        target == "Landlord"
    }
}

fuzz_target!(|data: &str| {
    let Val::Date(now, _) = infer("2026-11-05 10:00") else { return };
    let mut formulas = BTreeMap::new();
    formulas.insert("total".to_string(), parse("price * qty"));
    formulas.insert("f".to_string(), parse(data));
    let ev = Eval { formulas: &formulas, clock: Clock { now } };
    if let Ok(e) = parse(data) {
        if let Ok(v) = ev.eval(&e, &Note) {
            let _ = v.show();
            let _ = v.truthy();
        }
        let _ = ev.summary(&e, vec![Val::Num(1.0), Val::Str("x".into())], &Note);
    }
    let _ = ev.formula("f", &Note);
});
