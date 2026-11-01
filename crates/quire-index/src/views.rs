// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-view` blocks: saved queries over note properties.
//!
//! ````text
//! ```quire-view
//! from: "#project"          # tag, folder ("Projects/") or "all"
//! where: status != done and due < 2026-12-01
//! sort: due asc
//! group: status
//! layout: kanban            # table | list | kanban | calendar
//! fields: [title, status, due, tags]
//! date: due
//! ```
//! ````
//!
//! A spec is plain text inside a fenced block, so the note stays portable
//! Markdown. [`run`] evaluates it against the read model.

use std::cmp::Ordering;

use quire_markdown::frontmatter::{self, Value};
use quire_vault::NotePath;

use crate::{Index, NoteRecord};

/// The fence info string that marks a view block.
pub const FENCE: &str = "quire-view";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Table,
    List,
    Kanban,
    Calendar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    All,
    Tag(String),
    Folder(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Contains,
    Exists,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSpec {
    /// Any of these (an empty list means all notes).
    pub from: Vec<Source>,
    /// All of these.
    pub filters: Vec<Filter>,
    pub sort: Option<(String, bool)>,
    pub group: Option<String>,
    pub layout: Layout,
    pub fields: Vec<String>,
    pub date: Option<String>,
    /// Kanban column order; other values are appended.
    pub columns: Vec<String>,
    pub limit: Option<usize>,
}

impl Default for ViewSpec {
    fn default() -> Self {
        Self {
            from: Vec::new(),
            filters: Vec::new(),
            sort: None,
            group: None,
            layout: Layout::Table,
            fields: vec!["title".into()],
            date: None,
            columns: Vec::new(),
            limit: None,
        }
    }
}

fn strip_comment(v: &str) -> &str {
    // `# comment` after whitespace, outside quotes.
    let mut in_quote = None;
    for (i, c) in v.char_indices() {
        match c {
            '"' | '\'' if in_quote == Some(c) => in_quote = None,
            '"' | '\'' if in_quote.is_none() => in_quote = Some(c),
            '#' if in_quote.is_none() && i > 0 && v[..i].ends_with([' ', '\t']) => return v[..i].trim_end(),
            _ => {}
        }
    }
    v.trim_end()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn list(v: &str) -> Vec<String> {
    let v = v.trim();
    let inner = v.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(v);
    inner.split(',').map(unquote).filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect()
}

fn parse_filter(clause: &str) -> Result<Filter, String> {
    let c = clause.trim();
    for (word, op) in [(" exists", Op::Exists), (" missing", Op::Missing)] {
        if let Some(f) = c.strip_suffix(word) {
            return Ok(Filter { field: f.trim().to_lowercase(), op, value: String::new() });
        }
    }
    if let Some((f, v)) = c.split_once(" contains ") {
        return Ok(Filter { field: f.trim().to_lowercase(), op: Op::Contains, value: unquote(v) });
    }
    for (sym, op) in [("!=", Op::Ne), (">=", Op::Ge), ("<=", Op::Le), ("=", Op::Eq), (">", Op::Gt), ("<", Op::Lt)] {
        if let Some((f, v)) = c.split_once(sym) {
            let field = f.trim().to_lowercase();
            if field.is_empty() {
                break;
            }
            return Ok(Filter { field, op, value: unquote(v) });
        }
    }
    Err(format!("can't read the condition “{c}” (try: status != done)"))
}

/// Parse a view block's body.
pub fn parse(src: &str) -> Result<ViewSpec, String> {
    let mut spec = ViewSpec::default();
    for line in src.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            return Err(format!("expected “key: value”, found “{t}”"));
        };
        let v = strip_comment(v.trim());
        match k.trim().to_lowercase().as_str() {
            "from" => {
                spec.from = v
                    .split(" or ")
                    .flat_map(|part| part.split(','))
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| match s.as_str() {
                        "all" | "*" => None,
                        t if t.starts_with('#') => Some(Source::Tag(t[1..].to_string())),
                        "./" => Some(Source::Folder(".".to_string())),
                        f => Some(Source::Folder(f.trim_end_matches('/').to_string())),
                    })
                    .collect();
            }
            "where" | "filter" => {
                for clause in v.split(" and ") {
                    spec.filters.push(parse_filter(clause)?);
                }
            }
            "sort" => {
                let mut parts = v.split_whitespace();
                let field = parts.next().ok_or("sort needs a field")?.to_lowercase();
                let desc = matches!(parts.next(), Some(d) if d.eq_ignore_ascii_case("desc"));
                spec.sort = Some((field, desc));
            }
            "group" | "group_by" => spec.group = Some(unquote(v).to_lowercase()),
            "layout" | "view" => {
                spec.layout = match unquote(v).to_lowercase().as_str() {
                    "table" => Layout::Table,
                    "list" => Layout::List,
                    "kanban" | "board" => Layout::Kanban,
                    "calendar" => Layout::Calendar,
                    other => return Err(format!("unknown layout “{other}” (table, list, kanban, calendar)")),
                }
            }
            "fields" | "columns_shown" => {
                spec.fields = list(v);
                if spec.fields.is_empty() {
                    spec.fields.push("title".into());
                }
            }
            "date" => spec.date = Some(unquote(v).to_lowercase()),
            "columns" => spec.columns = list(v).into_iter().map(|c| if c == "(none)" { String::new() } else { c }).collect(),
            "limit" => spec.limit = Some(v.trim().parse().map_err(|_| format!("limit must be a number, found “{v}”"))?),
            other => return Err(format!("unknown key “{other}”")),
        }
    }
    if spec.layout == Layout::Kanban && spec.group.is_none() {
        spec.group = Some("status".into());
    }
    if spec.layout == Layout::Calendar && spec.date.is_none() {
        spec.date = Some(spec.sort.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| "date".into()));
    }
    Ok(spec)
}

/// A field's value for a note: built-ins first, then front matter.
pub fn field(r: &NoteRecord, name: &str) -> Option<String> {
    let v = match name {
        "title" => r.title.clone(),
        "name" => r.name.clone(),
        "path" | "file" => r.path.as_str().to_string(),
        "folder" => r.path.folder().to_string(),
        "tags" if !r.tags.is_empty() => r.tags.join(", "),
        "tasks" => r.tasks.len().to_string(),
        "open_tasks" => r.tasks.iter().filter(|t| !t.done).count().to_string(),
        "done_tasks" => r.tasks.iter().filter(|t| t.done).count().to_string(),
        "words" => r.stats.words.to_string(),
        "links" => r.links.len().to_string(),
        _ => {
            let key = r.properties.keys().find(|k| k.eq_ignore_ascii_case(name))?;
            return Some(r.properties[key].display()).filter(|s| !s.is_empty());
        }
    };
    Some(v)
}

fn list_values(r: &NoteRecord, name: &str) -> Vec<String> {
    if name == "tags" {
        return r.tags.clone();
    }
    match r.properties.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v) {
        Some(Value::List(v)) => v.clone(),
        Some(v) => vec![v.display()],
        None => field(r, name).into_iter().collect(),
    }
}

/// Compare as numbers, then dates/strings (ISO dates sort as strings).
pub fn compare(a: &str, b: &str) -> Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

fn matches(r: &NoteRecord, f: &Filter, today: &str) -> bool {
    let want = if f.value.eq_ignore_ascii_case("today") { today.to_string() } else { f.value.clone() };
    let have = field(r, &f.field);
    match f.op {
        Op::Exists => have.is_some(),
        Op::Missing => have.is_none(),
        Op::Contains => {
            let w = want.to_lowercase();
            let w = w.trim_start_matches('#');
            list_values(r, &f.field).iter().any(|v| v.to_lowercase() == w) || have.is_some_and(|h| h.to_lowercase().contains(w))
        }
        Op::Ne => {
            have.is_none_or(|h| !h.eq_ignore_ascii_case(&want) && !list_values(r, &f.field).iter().any(|v| v.eq_ignore_ascii_case(&want)))
        }
        Op::Eq => {
            have.is_some_and(|h| h.eq_ignore_ascii_case(&want) || list_values(r, &f.field).iter().any(|v| v.eq_ignore_ascii_case(&want)))
        }
        Op::Gt | Op::Lt | Op::Ge | Op::Le => {
            let Some(h) = have else { return false };
            // Dates compare by their day so `due < 2026-12-01` works with times.
            let h = if frontmatter::is_date(&h) && frontmatter::is_date(&want) { h[..10].to_string() } else { h };
            let o = compare(&h, &want);
            match f.op {
                Op::Gt => o == Ordering::Greater,
                Op::Lt => o == Ordering::Less,
                Op::Ge => o != Ordering::Less,
                _ => o != Ordering::Greater,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRow {
    pub path: NotePath,
    pub title: String,
    /// One per `spec.fields`, "" when missing.
    pub cells: Vec<String>,
    /// Group key (kanban column), "" when missing.
    pub group: String,
    /// `YYYY-MM-DD` for calendar layout.
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewResult {
    pub rows: Vec<ViewRow>,
    /// Kanban columns in display order ("" = no value).
    pub groups: Vec<String>,
    /// Matches before `limit` was applied.
    pub total: usize,
}

/// Resolve `./x` against the folder of the note holding the view.
fn resolve_folder(f: &str, current: Option<&NotePath>) -> String {
    match f.strip_prefix("./").or((f == ".").then_some("")) {
        Some(rest) => {
            let base = current.map_or("", |c| c.folder());
            let rest = rest.trim_end_matches('/');
            match (base.is_empty(), rest.is_empty()) {
                (true, _) => rest.to_string(),
                (false, true) => base.to_string(),
                (false, false) => format!("{base}/{rest}"),
            }
        }
        None => f.to_string(),
    }
}

fn in_folder(path: &str, folder: &str) -> bool {
    folder.is_empty()
        || (path.len() > folder.len() && path[..folder.len()].eq_ignore_ascii_case(folder) && path.as_bytes()[folder.len()] == b'/')
}

/// Evaluate a spec. `today` is `YYYY-MM-DD`, used by `where: due < today`.
/// `current` is the note holding the view: it's left out of the results,
/// and `from: ./Folder/` is relative to it.
pub fn run(spec: &ViewSpec, index: &Index, today: &str, current: Option<&NotePath>) -> ViewResult {
    let folders: Vec<Option<String>> =
        spec.from.iter().map(|s| if let Source::Folder(f) = s { Some(resolve_folder(f, current)) } else { None }).collect();
    let mut rows: Vec<(&NoteRecord, ViewRow)> = index
        .records()
        .filter(|r| Some(&r.path) != current)
        .filter(|r| {
            spec.from.is_empty()
                || spec.from.iter().zip(&folders).any(|(s, folder)| match s {
                    Source::All => true,
                    Source::Tag(t) => r.has_tag(t),
                    Source::Folder(_) => in_folder(r.path.as_str(), folder.as_deref().unwrap_or_default()),
                })
        })
        .filter(|r| spec.filters.iter().all(|f| matches(r, f, today)))
        .map(|r| {
            let cells = spec.fields.iter().map(|f| field(r, f).unwrap_or_default()).collect();
            let group = spec.group.as_deref().and_then(|g| list_values(r, g).into_iter().next()).unwrap_or_default();
            let date = spec.date.as_deref().and_then(|d| field(r, d)).filter(|d| frontmatter::is_date(d)).map(|d| d[..10].to_string());
            (r, ViewRow { path: r.path.clone(), title: r.title.clone(), cells, group, date })
        })
        .collect();
    match &spec.sort {
        Some((f, desc)) => rows.sort_by(|(a, _), (b, _)| {
            // Missing values sort last either way.
            let o = match (field(a, f), field(b, f)) {
                (Some(x), Some(y)) => {
                    let o = compare(&x, &y);
                    if *desc { o.reverse() } else { o }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            o.then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        }),
        None => rows.sort_by_key(|(a, _)| a.title.to_lowercase()),
    }
    let total = rows.len();
    let mut rows: Vec<ViewRow> = rows.into_iter().map(|(_, v)| v).collect();
    if let Some(l) = spec.limit {
        rows.truncate(l);
    }
    let mut groups = Vec::new();
    if spec.group.is_some() {
        groups.extend(spec.columns.iter().cloned());
        let mut seen: Vec<String> =
            rows.iter().map(|r| r.group.clone()).filter(|g| !groups.iter().any(|c| c.eq_ignore_ascii_case(g))).collect();
        seen.sort_by_key(|g| (g.is_empty(), g.to_lowercase()));
        seen.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        groups.extend(seen);
    }
    ViewResult { rows, groups, total }
}

/// A view block found in a note: the spec's text and where it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewBlock {
    pub spec: String,
    /// Line of the opening fence.
    pub open_line: usize,
    /// Line of the closing fence; the view renders below it.
    pub close_line: usize,
}

/// Find ```` ```quire-view ```` blocks in a note.
pub fn find_blocks(src: &str) -> Vec<ViewBlock> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String, String)> = None;
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        match &mut open {
            None => {
                let fence: String = t.chars().take_while(|&c| c == '`' || c == '~').collect();
                if fence.len() >= 3 && t[fence.len()..].trim() == FENCE {
                    open = Some((i, fence, String::new()));
                }
            }
            Some((start, fence, body)) => {
                if t.starts_with(fence.as_str()) && t.trim_end().chars().all(|c| c == fence.chars().next().unwrap_or('`')) {
                    out.push(ViewBlock { spec: std::mem::take(body), open_line: *start, close_line: i });
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut ix = Index::new();
        ix.upsert(
            NotePath::new("Projects/Alpha.md"),
            "---\nstatus: doing\ndue: 2026-11-20\npriority: 2\ntags: [project]\n---\n# Alpha\n- [ ] a\n- [x] b\n",
        );
        ix.upsert(
            NotePath::new("Projects/Beta.md"),
            "---\nstatus: done\ndue: 2026-11-03\npriority: 10\ntags: [project, rust]\n---\n# Beta\n",
        );
        ix.upsert(NotePath::new("Projects/Gamma.md"), "---\nstatus: todo\npriority: 1\n---\n# Gamma\nPart of #project\n");
        ix.upsert(NotePath::new("Notes/Other.md"), "---\nstatus: todo\n---\n# Other\n");
        ix.refresh();
        ix
    }

    fn titles(r: &ViewResult) -> Vec<&str> {
        r.rows.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn parses_specs() {
        let s = parse("from: \"#project\"   # tag\nwhere: status != done and due < 2026-12-01\nsort: due desc\nlayout: kanban\nfields: [Title, status, due]\n").unwrap();
        assert_eq!(s.from, vec![Source::Tag("project".into())]);
        assert_eq!(s.filters.len(), 2);
        assert_eq!(s.filters[0], Filter { field: "status".into(), op: Op::Ne, value: "done".into() });
        assert_eq!(s.sort, Some(("due".into(), true)));
        assert_eq!(s.group.as_deref(), Some("status"), "kanban groups by status by default");
        assert_eq!(s.fields, vec!["title", "status", "due"]);
        assert!(parse("layout: pie").is_err());
        assert!(parse("where: ???").is_err());
        assert!(parse("nonsense").is_err());
    }

    #[test]
    fn filters_sorts_and_groups() {
        let ix = index();
        let r =
            run(&parse("from: Projects/\nsort: priority desc\nfields: [title, priority, open_tasks]").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Beta", "Alpha", "Gamma"], "numeric sort, not string sort");
        assert_eq!(r.rows[1].cells, vec!["Alpha", "2", "1"]);
        let r = run(&parse("from: #project\nwhere: status != done").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Alpha", "Gamma"], "inline #tags count too");
        let r = run(&parse("where: due < 2026-11-10").unwrap(), &ix, "2026-11-01", None);
        assert_eq!(titles(&r), vec!["Beta"]);
        let r = run(&parse("where: due > today and tags contains project").unwrap(), &ix, "2026-11-05", None);
        assert_eq!(titles(&r), vec!["Alpha"]);
        let r = run(&parse("where: due missing").unwrap(), &ix, "", None);
        assert_eq!(titles(&r), vec!["Gamma", "Other"]);
        let r = run(&parse("from: Projects/\nlayout: kanban\ncolumns: [todo, doing, done, blocked]").unwrap(), &ix, "", None);
        assert_eq!(r.groups, vec!["todo", "doing", "done", "blocked"]);
        assert_eq!(r.rows.iter().find(|x| x.title == "Alpha").unwrap().group, "doing");
        let here = NotePath::new("Projects/Board.md");
        let r = run(&parse("from: ./").unwrap(), &ix, "", Some(&here));
        assert_eq!(titles(&r), vec!["Alpha", "Beta", "Gamma"], "relative to the view's note");
        let r = run(&parse("from: ./Sub/").unwrap(), &ix, "", Some(&here));
        assert!(r.rows.is_empty());
        let r = run(&parse("layout: calendar\ndate: due\nlimit: 1\nsort: due").unwrap(), &ix, "", None);
        assert_eq!((r.rows[0].date.as_deref(), r.total), (Some("2026-11-03"), 4));
    }

    #[test]
    fn finds_blocks() {
        let src = "# Board\n```quire-view\nfrom: Projects/\n```\ntext\n~~~~ quire-view\nlayout: list\n~~~~\n```rust\nx\n```\n";
        let b = find_blocks(src);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].open_line, b[0].close_line, b[0].spec.as_str()), (1, 3, "from: Projects/\n"));
        assert_eq!((b[1].open_line, b[1].close_line), (5, 7));
    }
}
