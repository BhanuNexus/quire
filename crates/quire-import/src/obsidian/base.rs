// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian Bases: a `.base` file is YAML with `filters`, `properties` and
//! `views` (`table` or `cards`).
//!
//! Each view becomes a `quire-view` spec. Supported: `and`/`or` filters with `file.inFolder("…")`,
//! `file.hasTag("…")` and property comparisons; `order`, `sort`, `limit`.
//! Not supported (and said so): `formulas`, `summaries`, `not`, other
//! functions.

use yaml_rust2::{Yaml, YamlLoader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Table,
    Cards,
}

/// One view of a base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseView {
    pub name: String,
    pub kind: ViewKind,
    /// `quire-view` text, without fences.
    pub spec: String,
    /// Column names from `properties: … displayName`, by view field.
    pub labels: Vec<(String, String)>,
    /// What this view's settings use that Quire can't show.
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Base {
    pub views: Vec<BaseView>,
}

fn s(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A Bases property name as a view field (`note.author` → `author`).
fn field(p: &str) -> Result<String, String> {
    let p = p.trim();
    Ok(match p {
        "file.name" | "file.basename" => "name".into(),
        "file.path" => "path".into(),
        "file.folder" => "folder".into(),
        "file.tags" => "tags".into(),
        "file.links" => "links".into(),
        _ if p.starts_with("formula.") => return Err(format!("the formula “{}”", &p[8..])),
        _ if p.starts_with("file.") => return Err(format!("the property “{p}”")),
        _ => p.strip_prefix("note.").unwrap_or(p).to_string(),
    })
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// A filter's pieces: sources (OR'ed) and conditions (AND'ed).
#[derive(Default)]
struct Filters {
    sources: Vec<String>,
    conds: Vec<String>,
    unsupported: Vec<String>,
}

impl Filters {
    fn skip(&mut self, what: String) {
        if !self.unsupported.contains(&what) {
            self.unsupported.push(what);
        }
    }

    /// One expression: a source, a condition, or neither.
    fn expr(&mut self, e: &str, in_or: bool) {
        let e = e.trim();
        let call = |name: &str| e.strip_prefix(name).and_then(|r| r.strip_suffix(')')).map(unquote);
        if let Some(folder) = call("file.inFolder(") {
            self.sources.push(format!("{}/", folder.trim_end_matches('/')));
            return;
        }
        if let Some(tag) = call("file.hasTag(") {
            self.sources.push(format!("#{}", tag.trim_start_matches('#')));
            return;
        }
        if in_or {
            self.skip(format!("“{e}” inside “or”"));
            return;
        }
        for (sym, op) in [("==", "="), ("!=", "!="), (">=", ">="), ("<=", "<="), (">", ">"), ("<", "<")] {
            if let Some((f, v)) = e.split_once(sym) {
                match field(f) {
                    Ok(f) if !v.contains('(') => {
                        let v = unquote(v);
                        let v = if v.contains([' ', '=', '<', '>', '!']) { format!("\"{v}\"") } else { v };
                        self.conds.push(format!("{f} {op} {v}"));
                    }
                    Ok(_) => self.skip(format!("the function in “{e}”")),
                    Err(why) => self.skip(why),
                }
                return;
            }
        }
        self.skip(format!("the filter “{e}”"));
    }

    fn node(&mut self, y: &Yaml, in_or: bool) {
        match y {
            Yaml::String(e) => self.expr(e, in_or),
            Yaml::Hash(h) => {
                for (k, v) in h {
                    let items = v.as_vec().cloned().unwrap_or_default();
                    match k.as_str() {
                        Some("and") if !in_or => {
                            for i in &items {
                                self.node(i, false);
                            }
                        }
                        Some("or") => {
                            let before = self.sources.len();
                            for i in &items {
                                self.node(i, true);
                            }
                            if in_or || (!self.sources.is_empty() && before > 0) {
                                self.skip("nested “and”/“or” of folders and tags".into());
                            }
                        }
                        Some("not") => self.skip("“not” filters".into()),
                        Some(other) => self.skip(format!("“{other}” filters")),
                        None => {}
                    }
                }
            }
            Yaml::Array(items) => items.iter().for_each(|i| self.node(i, in_or)),
            _ => {}
        }
    }
}

/// Read a `.base` file.
pub fn parse(text: &str) -> Result<Base, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| format!("this base can't be read: {e}"))?;
    let doc = docs.into_iter().next().unwrap_or(Yaml::Null);
    let mut top = Filters::default();
    if !doc["filters"].is_badvalue() {
        top.node(&doc["filters"], false);
    }
    if top.sources.len() > 1 && !top.unsupported.is_empty() {
        top.skip("several folders or tags joined by “and”".into());
    }
    let mut file_notes = top.unsupported.clone();
    if !doc["formulas"].is_badvalue() {
        file_notes.push("formulas".into());
    }
    if !doc["summaries"].is_badvalue() {
        file_notes.push("summaries".into());
    }
    let labels: Vec<(String, String)> = doc["properties"]
        .as_hash()
        .map(|h| h.iter().filter_map(|(k, v)| Some((field(k.as_str()?).ok()?, s(&v["displayName"])?))).collect())
        .unwrap_or_default();
    let mut views = Vec::new();
    let empty = vec![Yaml::Hash(yaml_rust2::yaml::Hash::new())];
    let list = doc["views"].as_vec().filter(|v| !v.is_empty()).unwrap_or(&empty);
    for (n, v) in list.iter().enumerate() {
        let kind = match v["type"].as_str() {
            Some("cards") => ViewKind::Cards,
            _ => ViewKind::Table,
        };
        let mut f = Filters { sources: top.sources.clone(), conds: top.conds.clone(), unsupported: file_notes.clone() };
        if let Some(t) = v["type"].as_str().filter(|t| !matches!(*t, "table" | "cards")) {
            f.skip(format!("the “{t}” view type (shown as a table)"));
        }
        if !v["filters"].is_badvalue() {
            f.node(&v["filters"], false);
        }
        if !v["summaries"].is_badvalue() || !v["groupBy"].is_badvalue() {
            f.skip("summaries and grouping".into());
        }
        let mut fields = vec!["title".to_string()];
        for o in v["order"].as_vec().map(Vec::as_slice).unwrap_or_default() {
            match o.as_str().map(field) {
                Some(Ok(x)) if x == "name" => {}
                Some(Ok(x)) => fields.push(x),
                Some(Err(why)) => f.skip(why),
                None => {}
            }
        }
        let mut spec = String::new();
        let mut push = |k: &str, val: &str| {
            spec.push_str(k);
            spec.push_str(": ");
            spec.push_str(val);
            spec.push('\n');
        };
        if !f.sources.is_empty() {
            push("from", &f.sources.join(", "));
        }
        if !f.conds.is_empty() {
            push("where", &f.conds.join(" and "));
        }
        if let Some(first) = v["sort"].as_vec().and_then(|s| s.first()) {
            match first["property"].as_str().map(field) {
                Some(Ok(p)) => {
                    let desc = first["direction"].as_str().is_some_and(|d| d.eq_ignore_ascii_case("desc"));
                    push("sort", &format!("{p}{}", if desc { " desc" } else { "" }));
                }
                Some(Err(why)) => f.skip(why),
                None => {}
            }
        }
        if let Some(l) = v["limit"].as_i64().filter(|l| *l > 0) {
            push("limit", &l.to_string());
        }
        push("fields", &format!("[{}]", fields.join(", ")));
        push("layout", if kind == ViewKind::Cards { "list" } else { "table" });
        let name = s(&v["name"]).unwrap_or_else(|| format!("View {}", n + 1));
        views.push(BaseView { name, kind, spec, labels: labels.clone(), unsupported: f.unsupported });
    }
    Ok(Base { views })
}

/// "Convert to note with quire-view": a note with one view per base view,
/// and what didn't convert in a callout.
pub fn to_note(base: &Base, title: &str) -> String {
    let mut out = format!("# {title}\n\n");
    let mut skipped: Vec<&str> = base.views.iter().flat_map(|v| v.unsupported.iter().map(String::as_str)).collect();
    skipped.sort_unstable();
    skipped.dedup();
    if !skipped.is_empty() {
        out.push_str("> [!note] Converted from an Obsidian base\n> Not converted: ");
        out.push_str(&skipped.join("; "));
        out.push_str(".\n\n");
    }
    for v in &base.views {
        if base.views.len() > 1 {
            out.push_str("## ");
            out.push_str(&v.name);
            out.push_str("\n\n");
        }
        out.push_str("```quire-view\n");
        out.push_str(&v.spec);
        out.push_str("```\n\n");
    }
    out.trim_end().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
filters:
  and:
    - file.inFolder("Projects")
    - 'status != "done"'
formulas:
  age: "now() - file.ctime"
properties:
  note.status:
    displayName: Status
views:
  - type: table
    name: Active
    filters:
      or:
        - file.hasTag("urgent")
    order:
      - file.name
      - status
      - note.owner.name
      - formula.age
    sort:
      - property: due
        direction: DESC
    limit: 20
  - type: cards
    name: Gallery
    filters:
      not:
        - file.hasTag("archived")
  - type: map
"#;

    #[test]
    fn views_become_specs() {
        let b = parse(BASE).unwrap();
        assert_eq!(b.views.len(), 3);
        let v = &b.views[0];
        assert_eq!((v.name.as_str(), v.kind), ("Active", ViewKind::Table));
        assert_eq!(
            v.spec,
            "from: Projects/, #urgent\nwhere: status != done\nsort: due desc\nlimit: 20\nfields: [title, status, owner.name]\nlayout: table\n"
        );
        assert_eq!(v.labels, vec![("status".to_string(), "Status".to_string())]);
        assert!(v.unsupported.iter().any(|u| u.contains("formula")), "{:?}", v.unsupported);
        assert_eq!(b.views[1].spec, "from: Projects/\nwhere: status != done\nfields: [title]\nlayout: list\n");
        assert!(b.views[1].unsupported.iter().any(|u| u.contains("not")));
        assert!(b.views[2].unsupported.iter().any(|u| u.contains("map")));
        let note = to_note(&b, "Projects");
        assert!(note.starts_with("# Projects\n\n> [!note] Converted from an Obsidian base\n> Not converted: "));
        assert_eq!(note.matches("```quire-view").count(), 3);
        assert!(parse("views: [").is_err());
        assert_eq!(parse("").unwrap().views.len(), 1, "an empty base is one table of every note");
    }
}
