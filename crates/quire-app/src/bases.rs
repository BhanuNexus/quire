// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian Bases with formulas and summaries: one view of a `.base` file
//! worked out against the index (design §E.5, 0.10).
//!
//! The view's `quire-view` spec picks and sorts the notes; then each note
//! is checked against the base's filter expression (when the spec can't
//! say it), its formula columns are worked out, and the summaries are
//! taken over the rows. Errors stay in the cell they belong to.

use std::collections::BTreeMap;

use chrono::{Local, NaiveDateTime, TimeZone};
use quire_import::obsidian::base::Base;
use quire_import::obsidian::expr::{self, Clock, Eval, Expr, Row, Val};
use quire_index::NoteRecord;
use quire_markdown::Value;
use quire_vault::NotePath;

use crate::workspace::Workspace;

/// One cell of a base table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// The note's name: a link.
    Name(String),
    /// A property: the table edits it in place.
    Property { field: String, value: String },
    /// Worked out (a formula, `file.size`).
    Computed(String),
    /// A formula that can't be worked out, and why.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub path: NotePath,
    pub cells: Vec<Cell>,
    /// The `groupBy` value ("" when missing or not grouped).
    pub group: String,
}

/// A base view, worked out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    /// Column headings.
    pub headings: Vec<String>,
    pub rows: Vec<TableRow>,
    /// Matches before the limit.
    pub total: usize,
    /// Per column: the summary's name and value (or why it failed).
    pub summaries: Vec<Option<(String, Result<String, String>)>>,
    /// The filter can't be used (a syntax error): nothing is shown.
    pub error: Option<String>,
    /// `groupBy`: the groups in order (rows are sorted into them).
    pub groups: Vec<String>,
}

/// A note as an expression sees it.
struct RecordRow<'a> {
    r: &'a NoteRecord,
    modified: Option<i64>,
}

/// A front-matter value as an expression value (`3` is a number, `2026-11-05` a date).
pub fn value(v: &Value) -> Val {
    match v {
        Value::Str(s) => expr::infer(s),
        Value::List(l) => Val::List(l.iter().map(|s| expr::infer(s)).collect()),
        Value::Map(_) => Val::Str(v.display()),
    }
}

fn local(ms: i64) -> Option<NaiveDateTime> {
    Local.timestamp_millis_opt(ms).single().map(|d| d.naive_local())
}

impl Row for RecordRow<'_> {
    fn property(&self, name: &str) -> Option<Val> {
        quire_index::views::property(self.r, name).map(value).or_else(|| {
            // 0.12: an inline field holding links (`up:: [[X]]`) is a property too.
            let v = quire_index::views::typed_targets(self.r, name);
            (!v.is_empty()).then(|| Val::List(v.into_iter().map(|t| Val::Str(format!("[[{t}]]"))).collect()))
        })
    }

    fn file(&self, field: &str) -> Option<Val> {
        let p = &self.r.path;
        Some(match field {
            "name" | "basename" => Val::Str(self.r.name.clone()),
            "path" => Val::Str(p.as_str().to_string()),
            "folder" => Val::Str(p.folder().to_string()),
            "ext" => Val::Str(p.as_str().rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default()),
            "size" => Val::Num(self.r.stats.chars as f64),
            // No creation time is kept: both are the last change.
            "ctime" | "mtime" => Val::Date(local(self.modified?)?, true),
            "tags" => Val::List(self.r.tags.iter().map(|t| Val::Str(format!("#{t}"))).collect()),
            "links" => Val::List(self.r.links.iter().filter(|l| l.is_internal()).map(|l| Val::Str(l.target.clone())).collect()),
            _ => return None,
        })
    }

    fn has_tag(&self, tag: &str) -> bool {
        self.r.has_tag(tag)
    }

    fn has_link(&self, target: &str) -> bool {
        let t = target.trim_start_matches("[[").trim_end_matches("]]").trim_end_matches(".md");
        self.r.links.iter().any(|l| l.is_internal() && l.target.eq_ignore_ascii_case(t))
    }
}

/// A row with nothing in it (custom summaries have no note).
struct NoRow;

impl Row for NoRow {
    fn property(&self, _: &str) -> Option<Val> {
        None
    }
    fn file(&self, _: &str) -> Option<Val> {
        None
    }
    fn has_tag(&self, _: &str) -> bool {
        false
    }
    fn has_link(&self, _: &str) -> bool {
        false
    }
}

/// The value a column has for a note.
fn column_value(ev: &Eval<'_>, column: &str, row: &RecordRow<'_>) -> Result<Val, String> {
    if let Some(f) = column.strip_prefix("formula.") {
        return ev.formula(f, row);
    }
    let e = expr::parse(column)?;
    ev.eval(&e, row)
}

/// Work out view `view` of `base` against the vault's index, at `now`.
pub fn table(ws: &Workspace, base: &Base, view: usize, now: NaiveDateTime) -> Table {
    let Some(v) = base.views.get(view) else { return Table::default() };
    let formulas: BTreeMap<String, Result<Expr, String>> =
        base.formulas.iter().map(|(k, src)| (k.clone(), expr::parse(src).map_err(|e| format!("formula “{k}”: {e}")))).collect();
    let ev = Eval { formulas: &formulas, clock: Clock { now } };
    let filter = match v.filter.as_deref().map(expr::parse) {
        Some(Err(e)) => return Table { error: Some(format!("The filter can't be read: {e}")), ..Table::default() },
        Some(Ok(f)) => Some(f),
        None => None,
    };
    let result = ws.run_view(&v.spec, None);
    let Ok(found) = result.as_ref() else {
        return Table { error: result.as_ref().as_ref().err().cloned(), ..Table::default() };
    };
    let group_of: std::collections::HashMap<NotePath, String> = found.rows.iter().map(|r| (r.path.clone(), r.group.clone())).collect();
    let ix = ws.index().read();
    let mut rows: Vec<(&NoteRecord, Option<i64>)> =
        found.rows.iter().filter_map(|r| ix.get(&r.path).map(|rec| (rec, ix.modified(&r.path)))).collect();
    if let Some(f) = &filter {
        rows.retain(|(r, m)| ev.eval(f, &RecordRow { r, modified: *m }).is_ok_and(|v| v.truthy()));
    }
    if let Some((key, desc)) = &v.sort {
        let mut keyed: Vec<(Val, (&NoteRecord, Option<i64>))> =
            rows.into_iter().map(|(r, m)| (column_value(&ev, key, &RecordRow { r, modified: m }).unwrap_or(Val::Null), (r, m))).collect();
        keyed.sort_by(|a, b| {
            let o = expr::compare(&a.0, &b.0);
            if *desc { o.reverse() } else { o }
        });
        rows = keyed.into_iter().map(|(_, r)| r).collect();
    }
    let total = rows.len();
    if let Some(l) = v.limit.filter(|_| v.filter.is_some() || v.sort.is_some()) {
        rows.truncate(l);
    }
    let mut columns_vals: Vec<Vec<Val>> = vec![Vec::new(); v.columns.len()];
    let out_rows: Vec<TableRow> = rows
        .iter()
        .map(|(r, m)| {
            let row = RecordRow { r, modified: *m };
            let cells = v
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let val = column_value(&ev, c, &row);
                    columns_vals[i].push(val.clone().unwrap_or(Val::Null));
                    match (c.as_str(), val) {
                        ("file.name" | "file.basename", _) => Cell::Name(r.title.clone()),
                        (_, Err(e)) => Cell::Error(e),
                        (c, Ok(val)) if c.starts_with("formula.") || c.starts_with("file.") => Cell::Computed(val.show()),
                        (c, Ok(_)) => {
                            let field = c.strip_prefix("note.").unwrap_or(c).to_string();
                            let value = quire_index::views::field(r, &field).unwrap_or_default();
                            Cell::Property { field, value }
                        }
                    }
                })
                .collect();
            let group = group_of.get(&r.path).cloned().unwrap_or_default();
            TableRow { path: r.path.clone(), cells, group }
        })
        .collect();
    // `groupBy`: rows by group (in the view's order of groups), keeping their order within.
    let mut out_rows = out_rows;
    let mut groups = Vec::new();
    if let Some((_, desc)) = &v.group {
        groups.clone_from(&found.groups);
        if *desc {
            groups.reverse();
        }
        let pos = |g: &str| groups.iter().position(|x| x.eq_ignore_ascii_case(g)).unwrap_or(usize::MAX);
        out_rows.sort_by_key(|r| pos(&r.group));
    }
    let summaries = v
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = v.summaries.iter().find(|(k, _)| k == c || k.strip_prefix("note.") == Some(c.as_str()))?.1.clone();
            let vals = &columns_vals[i];
            let res = match expr::summarize(&name, vals) {
                Some(r) => r.map(|v| v.show()),
                None => match base.custom_summaries.iter().find(|(k, _)| *k == name) {
                    Some((_, src)) => expr::parse(src).and_then(|e| ev.summary(&e, vals.clone(), &NoRow)).map(|v| v.show()),
                    None => Err(format!("“{name}” isn't a summary")),
                },
            };
            Some((name, res))
        })
        .collect();
    let headings = v.columns.iter().map(|c| base.label(c)).collect();
    Table { headings, rows: out_rows, total, summaries, error: None, groups }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formulas_filters_and_summaries_over_the_vault() {
        let mut ws = Workspace::new(crate::Settings::default());
        ws.attach_vault(quire_vault::Vault::in_memory(&[
            ("Bills/Rent.md", "---\nprice: 900\nqty: 1\npaid: false\ndue: 2026-11-01\n---\n# Rent\n"),
            ("Bills/Power.md", "---\nprice: 40.5\nqty: 2\npaid: true\ndue: 2026-11-20\n---\n# Power\n#home\n"),
            ("Bills/Water.md", "---\nprice: 12\nqty: 3\npaid: false\ndue: 2026-12-01\n---\n# Water\n"),
            ("Other.md", "---\nprice: 1\n---\n"),
        ]));
        ws.index().flush();
        let base = quire_import::obsidian::base::parse(
            r#"
filters:
  and:
    - file.inFolder("Bills")
formulas:
  total: "price * qty"
  state: 'if(paid, "Paid", if(due < today(), "Late", "Due " + due.format("D MMM")))'
  bad: "price.sparkle()"
summaries:
  mean2: "values.mean().round(2)"
views:
  - type: table
    name: Bills
    order: [file.name, price, formula.total, formula.state, formula.bad]
    sort:
      - property: formula.total
        direction: DESC
    summaries:
      formula.total: Sum
      price: mean2
  - type: table
    name: Unpaid
    order: [file.name, due]
    filters:
      not:
        - paid
    summaries:
      due: Earliest
"#,
        )
        .unwrap();
        let now = expr::infer("2026-11-05 10:00");
        let Val::Date(now, _) = now else { panic!("a date") };
        let t = table(&ws, &base, 0, now);
        assert_eq!(t.error, None);
        assert_eq!(t.headings, ["Name", "price", "total", "state", "bad"]);
        let names: Vec<String> = t.rows.iter().map(|r| r.path.stem().to_string()).collect();
        assert_eq!(names, ["Rent", "Power", "Water"], "sorted by the formula, largest first");
        assert_eq!(t.rows[0].cells[2], Cell::Computed("900".into()));
        assert_eq!(t.rows[0].cells[3], Cell::Computed("Late".into()));
        assert_eq!(t.rows[1].cells[3], Cell::Computed("Paid".into()));
        assert_eq!(t.rows[2].cells[3], Cell::Computed("Due 1 Dec".into()));
        assert_eq!(t.rows[0].cells[1], Cell::Property { field: "price".into(), value: "900".into() });
        assert_eq!(t.rows[0].cells[4], Cell::Error("“sparkle()” isn't supported".into()));
        assert_eq!(t.summaries[2], Some(("Sum".into(), Ok("1017".into()))));
        assert_eq!(t.summaries[1], Some(("mean2".into(), Ok("317.5".into()))));
        assert_eq!(t.summaries[0], None);
        let unpaid = table(&ws, &base, 1, now);
        let names: Vec<&str> = unpaid.rows.iter().map(|r| r.path.stem()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Rent") && names.contains(&"Water"), "{names:?}");
        assert_eq!(unpaid.summaries[1], Some(("Earliest".into(), Ok("2026-11-01".into()))));
    }
}
