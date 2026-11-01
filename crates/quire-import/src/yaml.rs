// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writing front matter for imported notes.

pub use quire_markdown::frontmatter::yaml_scalar as scalar;
use std::fmt::Write as _;

pub enum Field {
    One(String),
    Many(Vec<String>),
}

/// A `---` block (with trailing newline), or "" when there are no fields.
pub fn front_matter(fields: &[(String, Field)]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let mut out = String::from("---\n");
    for (k, v) in fields {
        let key = k.trim().to_lowercase().replace(' ', "_");
        match v {
            Field::One(s) => {
                let _ = writeln!(out, "{key}: {}", scalar(s));
            }
            Field::Many(items) => {
                let list: Vec<String> = items.iter().filter(|i| !i.trim().is_empty()).map(|i| scalar(i)).collect();
                let _ = writeln!(out, "{key}: [{}]", list.join(", "));
            }
        }
    }
    out.push_str("---\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed_and_round_trips() {
        let fm = front_matter(&[
            ("Title".into(), Field::One("Plan: v2".into())),
            ("tags".into(), Field::Many(vec!["a".into(), "b c".into()])),
            ("Due Date".into(), Field::One("2026-11-01".into())),
            ("done".into(), Field::One("yes".into())),
        ]);
        assert_eq!(fm, "---\ntitle: \"Plan: v2\"\ntags: [a, b c]\ndue_date: 2026-11-01\ndone: \"yes\"\n---\n");
        let parsed = quire_markdown::frontmatter::parse(&format!("{fm}body")).unwrap();
        assert_eq!(parsed.title(), Some("Plan: v2"));
        assert_eq!(parsed.tags(), vec!["a", "b c"]);
    }
}
