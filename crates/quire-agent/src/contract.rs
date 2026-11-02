// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Write contracts: what agent-written notes in a folder (or of a `type`)
//! must look like (design §A.7, docs/formats/policy.md).
//!
//! ```toml
//! [[contract]]
//! name = "concept pages"
//! applies = { folder = "wiki/concepts/" }   # or { type = "concept" }
//! required = ["type", "sources"]
//! fields.type = { allowed = ["concept", "entity"] }
//! fields.sources = { kind = "links", min = 1, must_resolve = true }
//! fields.updated = { kind = "date" }
//! body = { max_bytes = 20000, must_link_to_source = true }
//! ```
//!
//! `submit` checks every agent write against the contracts that apply to
//! the resulting text and refuses it with `contract_violation` (each broken
//! rule named, so the agent can fix and retry). Lint reports the person's
//! notes that break them, without refusing anything.

use std::collections::BTreeMap;

use quire_markdown::{Value, frontmatter};
use serde::Deserialize;

use crate::change::Violation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Date,
    Number,
    Bool,
    List,
    /// Wikilinks (`[[Note]]`), one or a list.
    Links,
    /// One of `allowed`.
    Enum,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FieldRule {
    pub kind: Option<FieldKind>,
    pub allowed: Vec<String>,
    /// For lists and links: the fewest items; for numbers: the smallest value.
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Links must point at notes that exist.
    pub must_resolve: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Applies {
    /// A folder prefix or glob (like a zone path).
    pub folder: Option<String>,
    /// The front-matter `type`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BodyRule {
    pub max_bytes: Option<u64>,
    /// The body links to at least one of the notes in `sources`.
    pub must_link_to_source: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Contract {
    pub name: String,
    #[serde(default)]
    pub applies: Applies,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldRule>,
    #[serde(default)]
    pub body: BodyRule,
}

impl Contract {
    /// A contract from its `[[contract]]` table.
    pub fn parse(t: &toml::Table) -> Result<Self, String> {
        let c: Self = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        if c.name.trim().is_empty() {
            return Err("a contract needs a name".into());
        }
        Ok(c)
    }

    /// Whether the contract covers the note at `path` with front-matter
    /// `type` `kind`. A zone's `index.md` and `log.md` are structure, not
    /// pages, and never covered.
    pub fn applies_to(&self, path: &str, kind: Option<&str>) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
        !matches!(name.as_str(), "index.md" | "log.md")
            && self.applies.folder.as_deref().is_none_or(|f| crate::policy::path_in_zone(f, path))
            && self.applies.kind.as_deref().is_none_or(|k| kind.is_some_and(|t| t.trim().eq_ignore_ascii_case(k)))
    }

    /// What `text` (the note at `path`) breaks. `resolves(target)` says
    /// whether a link target names an existing note.
    pub fn check(&self, text: &str, resolves: &dyn Fn(&str) -> bool) -> Vec<Violation> {
        let fm = frontmatter::parse(text).unwrap_or_default();
        let mut out = Vec::new();
        let v = |field: &str, rule: &str, message: String| Violation { field: field.into(), rule: rule.into(), message };
        for f in &self.required {
            if fm.get(f).is_none_or(is_empty) {
                out.push(v(f, "required", format!("add the `{f}` property")));
            }
        }
        for (name, rule) in &self.fields {
            let Some(value) = fm.get(name).filter(|x| !is_empty(x)) else { continue };
            out.extend(check_field(name, rule, value, resolves));
        }
        let body = body_of(text);
        if let Some(max) = self.body.max_bytes
            && body.len() as u64 > max
        {
            out.push(v("body", "max_bytes", format!("the text is {} bytes; keep it under {max}", body.len())));
        }
        if self.body.must_link_to_source {
            let sources: Vec<String> =
                fm.get("sources").map(|s| links_in(s).into_iter().map(|l| l.to_lowercase()).collect()).unwrap_or_default();
            let linked = quire_markdown::extract(body).links.iter().filter(|l| l.is_internal()).any(|l| {
                sources.iter().any(|s| *s == l.target.to_lowercase() || s.rsplit('/').next() == l.target.to_lowercase().rsplit('/').next())
            });
            if !linked {
                out.push(v("body", "must_link_to_source", "link to at least one of the notes in `sources` in the text".into()));
            }
        }
        out
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Str(s) => s.trim().is_empty(),
        Value::List(l) => l.is_empty(),
        Value::Map(m) => m.is_empty(),
    }
}

/// The text after the front matter.
fn body_of(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else { return text };
    rest.find("\n---").map_or(text, |i| {
        let after = &rest[i + 4..];
        after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")).unwrap_or(after)
    })
}

/// The `[[targets]]` in a property value (without `|alias` or `#heading`).
pub fn links_in(v: &Value) -> Vec<String> {
    let items: Vec<String> = match v {
        Value::Str(s) => vec![s.clone()],
        Value::List(l) => l.clone(),
        Value::Map(_) => Vec::new(),
    };
    let mut out = Vec::new();
    for item in items {
        let mut rest = item.as_str();
        while let Some(i) = rest.find("[[") {
            let after = &rest[i + 2..];
            let Some(j) = after.find("]]") else { break };
            let inner = &after[..j];
            let target = inner.split(['|', '#']).next().unwrap_or("").trim();
            if !target.is_empty() {
                out.push(target.to_string());
            }
            rest = &after[j + 2..];
        }
    }
    out
}

fn check_field(name: &str, rule: &FieldRule, value: &Value, resolves: &dyn Fn(&str) -> bool) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut v = |rule_name: &str, message: String| out.push(Violation { field: name.into(), rule: rule_name.into(), message });
    let scalars = value.as_list();
    let kind = rule.kind.or_else(|| (!rule.allowed.is_empty()).then_some(FieldKind::Enum));
    if !rule.allowed.is_empty() {
        for s in &scalars {
            if !rule.allowed.iter().any(|a| a.eq_ignore_ascii_case(s)) {
                v("allowed", format!("`{name}` is “{s}”; use one of: {}", rule.allowed.join(", ")));
            }
        }
    }
    let count = |n: usize, what: &str, v: &mut dyn FnMut(&str, String)| {
        let n = n as f64;
        if let Some(min) = rule.min.filter(|m| n < *m) {
            v("min", format!("`{name}` needs at least {min} {what}"));
        }
        if let Some(max) = rule.max.filter(|m| n > *m) {
            v("max", format!("`{name}` may have at most {max} {what}"));
        }
    };
    match kind {
        Some(FieldKind::Text) if !matches!(value, Value::Str(_)) => v("kind", format!("`{name}` should be text")),
        Some(FieldKind::Date) => {
            for s in &scalars {
                if !frontmatter::is_date(s) {
                    v("kind", format!("`{name}` should be a date like 2026-11-02, not “{s}”"));
                }
            }
        }
        Some(FieldKind::Number) => match value.as_str().map(str::trim).and_then(|s| s.parse::<f64>().ok()) {
            None => v("kind", format!("`{name}` should be a number")),
            Some(n) => {
                if rule.min.is_some_and(|m| n < m) || rule.max.is_some_and(|m| n > m) {
                    v("range", format!("`{name}` is {n}, outside the allowed range"));
                }
            }
        },
        Some(FieldKind::Bool) => {
            if !value.as_str().is_some_and(|s| matches!(s.trim().to_lowercase().as_str(), "true" | "false" | "yes" | "no")) {
                v("kind", format!("`{name}` should be true or false"));
            }
        }
        Some(FieldKind::List) => count(scalars.len(), "items", &mut v),
        Some(FieldKind::Links) => {
            let links = links_in(value);
            if links.len() < scalars.len() {
                v("kind", format!("`{name}` should be links like [[Note]]"));
            }
            count(links.len(), "links", &mut v);
            if rule.must_resolve {
                for l in links.iter().filter(|l| !resolves(l)) {
                    v("must_resolve", format!("`{name}` links to [[{l}]], which doesn't exist"));
                }
            }
        }
        Some(FieldKind::Text | FieldKind::Enum) | None => {}
    }
    out
}

/// The contracts in `tables`, and the ones that can't be read (name or
/// position, and why).
pub fn parse_all(tables: &[toml::Table]) -> (Vec<Contract>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        match Contract::parse(t) {
            Ok(c) => ok.push(c),
            Err(e) => {
                let name = t.get("name").and_then(toml::Value::as_str).map_or_else(|| format!("contract {}", i + 1), str::to_string);
                bad.push(format!("{name}: {e}"));
            }
        }
    }
    (ok, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept() -> Contract {
        let t: toml::Table = toml::from_str(
            r#"
name = "concept pages"
applies = { folder = "wiki/concepts/" }
required = ["type", "sources"]
fields.type = { allowed = ["concept", "entity"] }
fields.sources = { kind = "links", min = 1, must_resolve = true }
fields.updated = { kind = "date" }
body = { max_bytes = 200, must_link_to_source = true }
"#,
        )
        .unwrap();
        Contract::parse(&t).unwrap()
    }

    fn rules(vs: &[Violation]) -> Vec<String> {
        vs.iter().map(|v| format!("{}:{}", v.field, v.rule)).collect()
    }

    #[test]
    fn checks_the_documented_contract() {
        let c = concept();
        assert!(c.applies_to("wiki/concepts/Rope.md", None));
        assert!(c.applies_to("Wiki/Concepts/Rope.md", None));
        assert!(!c.applies_to("wiki/Rope.md", None));
        let resolves = |t: &str| t == "Paper";
        let good = "---\ntype: concept\nsources: [\"[[Paper]]\"]\nupdated: 2026-11-02\n---\nRopes, from [[Paper]].\n";
        assert!(c.check(good, &resolves).is_empty(), "{:?}", c.check(good, &resolves));
        let bad = "---\ntype: idea\nsources: [\"[[Missing]]\"]\nupdated: soon\n---\nNo links here.\n";
        assert_eq!(rules(&c.check(bad, &resolves)), ["sources:must_resolve", "type:allowed", "updated:kind", "body:must_link_to_source"]);
        let none = "Just text.\n";
        assert_eq!(rules(&c.check(none, &resolves)), ["type:required", "sources:required", "body:must_link_to_source"]);
        let long = format!("---\ntype: concept\nsources: \"[[Paper]]\"\n---\n[[Paper]] {}\n", "x".repeat(300));
        assert_eq!(rules(&c.check(&long, &resolves)), ["body:max_bytes"]);
        let msg = &c.check(bad, &resolves)[1].message;
        assert!(msg.contains("concept, entity"), "{msg}");
    }

    #[test]
    fn by_type_and_bad_tables() {
        let t: toml::Table = toml::from_str("name = \"people\"\napplies = { type = \"person\" }\nrequired = [\"born\"]\n").unwrap();
        let c = Contract::parse(&t).unwrap();
        assert!(c.applies_to("anywhere/Ada.md", Some("Person")));
        assert!(!c.applies_to("anywhere/Ada.md", None));
        let bad: toml::Table = toml::from_str("name = \"x\"\nfields.a = { kind = \"color\" }\n").unwrap();
        let (ok, errs) = parse_all(&[t, bad, toml::Table::new()]);
        assert_eq!(ok.len(), 1);
        assert_eq!(errs.len(), 2);
        assert!(errs[0].starts_with("x: "), "{errs:?}");
        assert_eq!(links_in(&Value::List(vec!["[[A|a]]".into(), "[[B#h]] and [[C]]".into()])), ["A", "B", "C"]);
    }
}
