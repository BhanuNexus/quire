// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian Bases: a `.base` file is YAML with `filters`, `properties` and
//! `views` (`table` or `cards`).
//!
//! Each view becomes a `quire-view` spec. Filters it can say (`and`/`or`
//! with `file.inFolder("…")`, `file.hasTag("…")` and property comparisons)
//! go into the spec; when a filter needs more (`not`, functions, dates),
//! the whole filter becomes an expression ([`super::expr`]) the app checks
//! for each note. `formulas` add computed columns and `summaries` a row of
//! totals under a table (or a line under cards). Not supported (and said
//! so): `groupBy`, view types other than table and cards, and functions
//! [`super::expr`] doesn't know.

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
    /// The columns, in order, as the base names them (`file.name`,
    /// `status`, `formula.total`, `file.mtime`).
    pub columns: Vec<String>,
    /// Column → summary (a built-in name like `Sum`, or one of the base's own).
    pub summaries: Vec<(String, String)>,
    /// The filter as an expression, when the spec can't say it: the spec
    /// then lists every note and this decides.
    pub filter: Option<String>,
    /// Sort by a column the spec can't sort by (a formula, `file.mtime`), descending?
    pub sort: Option<(String, bool)>,
    /// Applied after `filter` when there is one.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Base {
    pub views: Vec<BaseView>,
    /// `formulas:` by name, as written.
    pub formulas: Vec<(String, String)>,
    /// `summaries:` (the base's own summaries) by name, as written.
    pub custom_summaries: Vec<(String, String)>,
    /// Display names from `properties:`, by column as the base names it.
    pub display: Vec<(String, String)>,
}

impl Base {
    /// The heading of a column: its display name, or the name without `note.`/`formula.`.
    pub fn label(&self, column: &str) -> String {
        if let Some((_, l)) = self.display.iter().find(|(k, _)| k == column || k.strip_prefix("note.") == Some(column)) {
            return l.clone();
        }
        match column {
            "file.name" | "file.basename" => "Name".into(),
            _ => column.strip_prefix("note.").or_else(|| column.strip_prefix("formula.")).unwrap_or(column).replace('_', " "),
        }
    }
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
#[derive(Default, Clone)]
struct Filters {
    sources: Vec<String>,
    conds: Vec<String>,
    unsupported: Vec<String>,
}

/// A filter tree (`and`/`or`/`not` lists of expressions) as one expression.
fn filter_expr(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(e) => Some(format!("({e})")),
        Yaml::Hash(h) => {
            let mut parts = Vec::new();
            for (k, v) in h {
                let items: Vec<String> = v.as_vec().map(|l| l.iter().filter_map(filter_expr).collect()).unwrap_or_default();
                if items.is_empty() {
                    continue;
                }
                parts.push(match k.as_str()? {
                    "and" => format!("({})", items.join(" && ")),
                    "or" => format!("({})", items.join(" || ")),
                    "not" => format!("!({})", items.join(" || ")),
                    _ => return None,
                });
            }
            (!parts.is_empty()).then(|| parts.join(" && "))
        }
        Yaml::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(filter_expr).collect();
            (!parts.is_empty()).then(|| format!("({})", parts.join(" && ")))
        }
        _ => None,
    }
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

/// `name: expression` pairs of a YAML map (`formulas:`, `summaries:`).
fn named(y: &Yaml) -> Vec<(String, String)> {
    y.as_hash().map(|h| h.iter().filter_map(|(k, v)| Some((s(k)?, s(v)?))).collect()).unwrap_or_default()
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
    let top_expr = if doc["filters"].is_badvalue() { None } else { filter_expr(&doc["filters"]) };
    let formulas = named(&doc["formulas"]);
    let custom_summaries = named(&doc["summaries"]);
    let display: Vec<(String, String)> = doc["properties"]
        .as_hash()
        .map(|h| h.iter().filter_map(|(k, v)| Some((s(k)?, s(&v["displayName"])?))).collect())
        .unwrap_or_default();
    let labels: Vec<(String, String)> = display.iter().filter_map(|(k, l)| Some((field(k).ok()?, l.clone()))).collect();
    let mut views = Vec::new();
    let empty = vec![Yaml::Hash(yaml_rust2::yaml::Hash::new())];
    let list = doc["views"].as_vec().filter(|v| !v.is_empty()).unwrap_or(&empty);
    for (n, v) in list.iter().enumerate() {
        let kind = match v["type"].as_str() {
            Some("cards") => ViewKind::Cards,
            _ => ViewKind::Table,
        };
        let mut f = top.clone();
        let mut notes = Vec::new();
        if let Some(t) = v["type"].as_str().filter(|t| !matches!(*t, "table" | "cards")) {
            notes.push(format!("the “{t}” view type (shown as a table)"));
        }
        if !v["filters"].is_badvalue() {
            f.node(&v["filters"], false);
        }
        if !v["groupBy"].is_badvalue() {
            notes.push("grouping".into());
        }
        // A filter the spec can't say: the whole filter is checked note by note.
        let view_expr = if v["filters"].is_badvalue() { None } else { filter_expr(&v["filters"]) };
        let filter = (!f.unsupported.is_empty())
            .then(|| [top_expr.clone(), view_expr].into_iter().flatten().collect::<Vec<_>>().join(" && "))
            .filter(|e| !e.is_empty());
        if filter.is_some() {
            f = Filters::default();
        }
        let mut columns = Vec::new();
        let mut fields = vec!["title".to_string()];
        for o in v["order"].as_vec().map(Vec::as_slice).unwrap_or_default() {
            let Some(name) = s(o) else { continue };
            if let Ok(x) = field(&name)
                && x != "name"
            {
                fields.push(x);
            }
            columns.push(name);
        }
        if !columns.iter().any(|c| matches!(c.as_str(), "file.name" | "file.basename")) {
            columns.insert(0, "file.name".into());
        }
        let summaries = named(&v["summaries"]);
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
        let mut sort = None;
        if let Some(first) = v["sort"].as_vec().and_then(|s| s.first()) {
            let desc = first["direction"].as_str().is_some_and(|d| d.eq_ignore_ascii_case("desc"));
            if let Some(p) = first["property"].as_str() {
                match field(p) {
                    Ok(x) => push("sort", &format!("{x}{}", if desc { " desc" } else { "" })),
                    Err(_) => sort = Some((p.to_string(), desc)),
                }
            }
        }
        let limit = v["limit"].as_i64().filter(|l| *l > 0).map(|l| l as usize);
        if let Some(l) = limit.filter(|_| filter.is_none() && sort.is_none()) {
            push("limit", &l.to_string());
        }
        push("fields", &format!("[{}]", fields.join(", ")));
        push("layout", if kind == ViewKind::Cards { "list" } else { "table" });
        let name = s(&v["name"]).unwrap_or_else(|| format!("View {}", n + 1));
        // What still can't be shown: filters with nothing to check them by, and the view notes.
        let mut unsupported = if filter.is_none() { f.unsupported } else { Vec::new() };
        unsupported.extend(notes);
        views.push(BaseView { name, kind, spec, labels: labels.clone(), unsupported, columns, summaries, filter, sort, limit });
    }
    Ok(Base { views, formulas, custom_summaries, display })
}

/// What a note made by [`to_note`] can't carry over from this view.
fn lost_in_note(v: &BaseView) -> Vec<String> {
    let mut out = v.unsupported.clone();
    if v.filter.is_some() {
        out.push("filters that need expressions".into());
    }
    if v.columns.iter().any(|c| c.starts_with("formula.")) {
        out.push("formulas".into());
    }
    if !v.summaries.is_empty() {
        out.push("summaries".into());
    }
    out
}

/// "Convert to note with quire-view": a note with one view per base view,
/// and what didn't convert in a callout.
pub fn to_note(base: &Base, title: &str) -> String {
    let mut out = format!("# {title}\n\n");
    let mut skipped: Vec<String> = base.views.iter().flat_map(lost_in_note).collect();
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
        // Projects AND urgent: more than a spec's sources can say, so the filter is an expression.
        assert_eq!(v.filter.as_deref(), Some(r#"((file.inFolder("Projects")) && (status != "done")) && ((file.hasTag("urgent")))"#));
        assert_eq!(v.spec, "sort: due desc\nfields: [title, status, owner.name]\nlayout: table\n");
        assert_eq!(v.limit, Some(20));
        assert_eq!(v.columns, ["file.name", "status", "note.owner.name", "formula.age"]);
        assert!(v.unsupported.is_empty(), "{:?}", v.unsupported);
        assert_eq!(v.labels, vec![("status".to_string(), "Status".to_string())]);
        assert_eq!(b.formulas, vec![("age".to_string(), "now() - file.ctime".to_string())]);
        assert_eq!(b.label("status"), "Status");
        assert_eq!(b.label("formula.age"), "age");
        let cards = &b.views[1];
        assert_eq!(cards.filter.as_deref(), Some(r#"((file.inFolder("Projects")) && (status != "done")) && !((file.hasTag("archived")))"#));
        assert_eq!(cards.spec, "fields: [title]\nlayout: list\n");
        // Only a folder and a comparison: the spec says it all.
        let map = &b.views[2];
        assert_eq!(map.spec, "from: Projects/\nwhere: status != done\nfields: [title]\nlayout: table\n");
        assert_eq!(map.filter, None);
        assert!(map.unsupported.iter().any(|u| u.contains("map")));
        let note = to_note(&b, "Projects");
        assert!(note.starts_with("# Projects\n\n> [!note] Converted from an Obsidian base\n> Not converted: "), "{note}");
        assert!(note.contains("formulas") && note.contains("filters that need expressions"));
        assert_eq!(note.matches("```quire-view").count(), 3);
        assert!(parse("views: [").is_err());
        assert_eq!(parse("").unwrap().views.len(), 1, "an empty base is one table of every note");
    }

    /// Formulas and summaries as Obsidian's Bases syntax documents them.
    const BUDGET: &str = r#"
filters:
  and:
    - file.inFolder("Bills")
formulas:
  total: "price * qty"
  label: 'if(paid, "Paid", "Due " + due.format("D MMM"))'
  overdue: "!paid && due < today()"
properties:
  formula.total:
    displayName: Total
summaries:
  mean3: "values.mean().round(3)"
views:
  - type: table
    name: Bills
    order:
      - file.name
      - price
      - qty
      - formula.total
      - formula.label
      - due
    sort:
      - property: formula.total
        direction: DESC
    summaries:
      formula.total: Sum
      price: mean3
      due: Earliest
  - type: cards
    name: Overdue
    filters:
      and:
        - formula.overdue
"#;

    #[test]
    fn formulas_and_summaries() {
        let b = parse(BUDGET).unwrap();
        assert_eq!(b.formulas.len(), 3);
        assert_eq!(b.custom_summaries, vec![("mean3".to_string(), "values.mean().round(3)".to_string())]);
        assert_eq!(b.label("formula.total"), "Total");
        let v = &b.views[0];
        assert_eq!(v.columns, ["file.name", "price", "qty", "formula.total", "formula.label", "due"]);
        assert_eq!(v.sort, Some(("formula.total".to_string(), true)), "sorted by a formula: done by the app");
        assert_eq!(
            v.summaries,
            vec![
                ("formula.total".to_string(), "Sum".to_string()),
                ("price".to_string(), "mean3".to_string()),
                ("due".to_string(), "Earliest".to_string())
            ]
        );
        assert_eq!(v.spec, "from: Bills/\nfields: [title, price, qty, due]\nlayout: table\n");
        assert!(v.unsupported.is_empty());
        for (_, f) in &b.formulas {
            assert!(super::super::expr::parse(f).is_ok(), "{f}");
        }
        let overdue = &b.views[1];
        assert_eq!(overdue.filter.as_deref(), Some(r#"((file.inFolder("Bills"))) && ((formula.overdue))"#));
    }
}
