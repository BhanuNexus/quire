// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bibliography library (design §G.3). Owned by WS-G; kept in
//! `ws.features.citations`.
//!
//! * The library is one or more BibTeX files in the vault, named in
//!   `.quire/shared/vault.toml` under `[citations] library` (default
//!   `references.bib` when it exists). They are read off the UI thread when
//!   the vault opens and again when one changes.
//! * Import takes a `.bib` file or CSL-JSON (Zotero's export) and merges it
//!   into the first library file by key, after a dry run. A file outside
//!   the vault (`[citations] watch`, e.g. a Better BibTeX auto-export) is
//!   checked every few seconds and merged when it changes.
//! * `[@key]` citations render author-date in preview, HTML export and
//!   publish ([`render_source`]); the PDF export gives the library to
//!   Typst's `bibliography()` with an embedded style.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::Receiver;
use quire_markdown::cite::{self, Formatter};
use quire_vault::{NotePath, Vault, VaultPrefs};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::workspace::{NoticeLevel, Workspace};

/// A person's name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Name {
    pub family: String,
    pub given: String,
}

impl Name {
    /// `Lovelace, Ada` (`Lovelace` without a given name).
    pub fn inverted(&self) -> String {
        if self.given.is_empty() { self.family.clone() } else { format!("{}, {}", self.family, self.given) }
    }

    /// `Ada Lovelace`.
    pub fn natural(&self) -> String {
        if self.given.is_empty() { self.family.clone() } else { format!("{} {}", self.given, self.family) }
    }
}

/// One work in the library.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reference {
    pub key: String,
    /// BibTeX entry type (`article`, `book`, …).
    pub kind: String,
    pub authors: Vec<Name>,
    pub editors: Vec<Name>,
    pub title: String,
    /// Journal, book or proceedings it appeared in.
    pub container: String,
    /// `1843`, or empty.
    pub year: String,
    pub volume: String,
    pub issue: String,
    pub pages: String,
    pub publisher: String,
    pub doi: String,
    pub url: String,
    /// The `file` field (Zotero, Better BibTeX): a path to the work's PDF.
    pub file: String,
    /// Which library file it came from.
    pub source: Option<NotePath>,
}

impl Reference {
    /// The names to cite by: authors, else editors.
    fn names(&self) -> &[Name] {
        if self.authors.is_empty() { &self.editors } else { &self.authors }
    }

    /// `Lovelace`, `Lovelace and Babbage`, `Lovelace et al.` (the title when nobody is named).
    pub fn short_authors(&self) -> String {
        match self.names() {
            [] => self.title.chars().take(40).collect(),
            [a] => a.family.clone(),
            [a, b] => format!("{} and {}", a.family, b.family),
            [a, ..] => format!("{} et al.", a.family),
        }
    }

    /// The file name of the work's PDF, from `file` (Zotero writes
    /// `title:path/to/paper.pdf:application/pdf`).
    pub fn pdf_name(&self) -> Option<String> {
        self.file
            .split([';', ':'])
            .map(str::trim)
            .find(|p| p.to_lowercase().ends_with(".pdf"))
            .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_string())
    }
}

/// The library: every reference, by key, and the year labels that tell
/// works of the same author and year apart (1843a, 1843b).
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub refs: Vec<Reference>,
    by_key: HashMap<String, usize>,
    year_labels: HashMap<String, String>,
    /// Files it was read from, and problems reading them.
    pub files: Vec<NotePath>,
    pub problems: Vec<String>,
}

impl Library {
    pub fn new(mut refs: Vec<Reference>, files: Vec<NotePath>, problems: Vec<String>) -> Self {
        // A key defined twice: the first file wins.
        let mut seen = std::collections::HashSet::new();
        refs.retain(|r| seen.insert(r.key.clone()));
        let by_key = refs.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, r) in refs.iter().enumerate() {
            groups.entry((r.short_authors(), r.year.clone())).or_default().push(i);
        }
        let mut year_labels = HashMap::new();
        for idx in groups.into_values().filter(|v| v.len() > 1) {
            let mut idx = idx;
            idx.sort_by(|a, b| refs[*a].title.to_lowercase().cmp(&refs[*b].title.to_lowercase()));
            for (n, i) in idx.into_iter().enumerate() {
                let suffix = char::from(b'a' + u8::try_from(n.min(25)).unwrap_or(25));
                let year = if refs[i].year.is_empty() { "n.d.".to_string() } else { refs[i].year.clone() };
                year_labels.insert(refs[i].key.clone(), format!("{year}{suffix}"));
            }
        }
        Self { refs, by_key, year_labels, files, problems }
    }

    pub fn get(&self, key: &str) -> Option<&Reference> {
        self.by_key.get(key).map(|i| &self.refs[*i])
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// References matching `query` (key, authors, title, year), best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Reference> {
        let q: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut hits: Vec<(usize, &Reference)> = self
            .refs
            .iter()
            .filter_map(|r| {
                let hay = format!("{} {} {} {}", r.key, r.names().iter().map(Name::natural).collect::<Vec<_>>().join(" "), r.title, r.year)
                    .to_lowercase();
                let mut score = 0;
                for t in &q {
                    let at = hay.find(t.as_str())?;
                    score += usize::from(!r.key.to_lowercase().starts_with(t.as_str())) + usize::from(at > 0);
                }
                Some((score, r))
            })
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));
        hits.into_iter().take(limit).map(|(_, r)| r).collect()
    }
}

fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '_' | '[' | ']' | '`' | '<' | '\\' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Join names: `A`, `A and B`, `A, B, and C`.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a}, and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

impl Formatter for Library {
    fn has(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    fn authors(&self, key: &str) -> String {
        self.get(key).map(Reference::short_authors).unwrap_or_default()
    }

    fn year(&self, key: &str) -> String {
        if let Some(y) = self.year_labels.get(key) {
            return y.clone();
        }
        match self.get(key) {
            Some(r) if !r.year.is_empty() => r.year.clone(),
            _ => "n.d.".into(),
        }
    }

    /// Chicago author-date, in Markdown: `Lovelace, Ada. 1843. *Notes*. …`.
    fn reference(&self, key: &str) -> String {
        let Some(r) = self.get(key) else { return String::new() };
        let mut names: Vec<String> = r.names().iter().enumerate().map(|(i, n)| if i == 0 { n.inverted() } else { n.natural() }).collect();
        if names.len() > 10 {
            names.truncate(7);
            names.push("et al".into());
        }
        let mut s = md_escape(&join_names(&names));
        if r.authors.is_empty() && !r.editors.is_empty() {
            s.push_str(if r.editors.len() == 1 { ", ed" } else { ", eds" });
        }
        if !s.is_empty() && !s.ends_with('.') {
            s.push('.');
        }
        s.push(' ');
        s.push_str(&self.year(key));
        s.push_str(". ");
        let in_container = !r.container.is_empty();
        let title = md_escape(r.title.trim_end_matches('.'));
        if in_container {
            let _ = write!(s, "“{title}.” *{}*", md_escape(&r.container));
            if !r.volume.is_empty() {
                s.push(' ');
                s.push_str(&md_escape(&r.volume));
            }
            if !r.issue.is_empty() {
                let _ = write!(s, " ({})", md_escape(&r.issue));
            }
            if !r.pages.is_empty() {
                let _ = write!(s, ": {}", md_escape(&r.pages.replace("--", "–")));
            }
            s.push('.');
        } else if !title.is_empty() {
            let _ = write!(s, "*{title}*.");
        }
        if !r.publisher.is_empty() {
            s.push(' ');
            s.push_str(&md_escape(&r.publisher));
            s.push('.');
        }
        if !r.doi.is_empty() {
            let doi = r.doi.trim_start_matches("https://doi.org/");
            let _ = write!(s, " <https://doi.org/{doi}>.");
        } else if !r.url.is_empty() && (r.url.starts_with("http://") || r.url.starts_with("https://")) {
            let _ = write!(s, " <{}>.", r.url);
        }
        s
    }

    fn sort_key(&self, key: &str) -> String {
        self.get(key)
            .map(|r| {
                format!(
                    "{} {} {}",
                    r.names().first().map(|n| n.family.to_lowercase()).unwrap_or_default(),
                    self.year(key),
                    r.title.to_lowercase()
                )
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------- parsing

fn person(p: &biblatex::Person) -> Name {
    let family = if p.prefix.is_empty() { p.name.clone() } else { format!("{} {}", p.prefix, p.name) };
    let family = if p.suffix.is_empty() { family } else { format!("{family}, {}", p.suffix) };
    Name { family, given: p.given_name.clone() }
}

/// The references in BibTeX (or BibLaTeX) text.
pub fn parse_bibtex(text: &str) -> Result<Vec<Reference>, String> {
    use biblatex::ChunksExt as _;
    let bib = std::panic::catch_unwind(|| biblatex::Bibliography::parse(text))
        .map_err(|_| "the BibTeX reader failed on this file".to_string())?
        .map_err(|e| {
            let line = text[..e.span.start.min(text.len())].matches('\n').count() + 1;
            format!("line {line}: {:?}", e.kind)
        })?;
    let field = |e: &biblatex::Entry, k: &str| {
        e.get(k).map(|c| c.format_verbatim().split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default()
    };
    Ok(bib
        .iter()
        .map(|e| {
            let year_src = [field(e, "year"), field(e, "date")].into_iter().find(|y| !y.is_empty()).unwrap_or_default();
            let year: String = year_src.chars().filter(char::is_ascii_digit).take(4).collect();
            let container = ["journaltitle", "journal", "booktitle", "maintitle", "series"]
                .iter()
                .map(|k| field(e, k))
                .find(|v| !v.is_empty())
                .unwrap_or_default();
            Reference {
                key: e.key.clone(),
                kind: e.entry_type.to_string(),
                authors: e.author().map(|v| v.iter().map(person).collect()).unwrap_or_default(),
                editors: e.editors().map(|v| v.iter().flat_map(|(ps, _)| ps.iter().map(person)).collect()).unwrap_or_default(),
                title: field(e, "title"),
                container,
                year: if year.len() == 4 { year } else { String::new() },
                volume: field(e, "volume"),
                issue: [field(e, "number"), field(e, "issue")].into_iter().find(|v| !v.is_empty()).unwrap_or_default(),
                pages: field(e, "pages"),
                publisher: [field(e, "publisher"), field(e, "institution"), field(e, "school"), field(e, "organization")]
                    .into_iter()
                    .find(|v| !v.is_empty())
                    .unwrap_or_default(),
                doi: field(e, "doi"),
                url: field(e, "url"),
                file: field(e, "file"),
                source: None,
            }
        })
        .collect())
}

fn csl_names(v: Option<&Json>) -> Vec<Name> {
    v.and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|n| {
                    let s = |k: &str| n.get(k).and_then(Json::as_str).unwrap_or_default().trim().to_string();
                    let literal = s("literal");
                    if !literal.is_empty() {
                        return Some(Name { family: literal, given: String::new() });
                    }
                    let particle = s("non-dropping-particle");
                    let family = if particle.is_empty() { s("family") } else { format!("{particle} {}", s("family")) };
                    (!family.is_empty()).then(|| Name { family, given: s("given") })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The references in CSL-JSON (an array, as Zotero exports it).
pub fn parse_csl_json(text: &str) -> Result<Vec<Reference>, String> {
    let v: Json = serde_json::from_str(text).map_err(|e| format!("not CSL-JSON: {e}"))?;
    let items = match &v {
        Json::Array(a) => a.clone(),
        Json::Object(o) if o.contains_key("items") => o.get("items").and_then(Json::as_array).cloned().unwrap_or_default(),
        Json::Object(_) => vec![v.clone()],
        _ => return Err("not CSL-JSON: expected a list of items".into()),
    };
    let mut out = Vec::new();
    for (n, it) in items.iter().enumerate() {
        let s = |k: &str| match it.get(k) {
            Some(Json::String(s)) => s.trim().to_string(),
            Some(Json::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let key = [s("citation-key"), s("citationKey"), s("id")]
            .into_iter()
            .find(|k| !k.is_empty())
            .ok_or_else(|| format!("item {} has no id", n + 1))?;
        let issued = it.get("issued");
        let year = issued
            .and_then(|d| d.get("date-parts"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get(0))
            .and_then(|y| y.as_i64().map(|y| y.to_string()).or_else(|| y.as_str().map(str::to_string)))
            .or_else(|| {
                issued
                    .and_then(|d| d.get("raw").or_else(|| d.get("literal")))
                    .and_then(Json::as_str)
                    .map(|r| r.chars().filter(char::is_ascii_digit).take(4).collect())
            })
            .unwrap_or_default();
        let kind = match s("type").as_str() {
            "article-journal" | "article-magazine" | "article-newspaper" | "article" => "article",
            "book" => "book",
            "chapter" => "incollection",
            "paper-conference" => "inproceedings",
            "thesis" => "phdthesis",
            "report" => "techreport",
            "webpage" | "post-weblog" | "post" => "online",
            _ => "misc",
        };
        out.push(Reference {
            key: key.replace([' ', ',', '{', '}'], "_"),
            kind: kind.into(),
            authors: csl_names(it.get("author")),
            editors: csl_names(it.get("editor")),
            title: s("title"),
            container: s("container-title"),
            year: if year.len() == 4 { year } else { String::new() },
            volume: s("volume"),
            issue: s("issue"),
            pages: s("page"),
            publisher: s("publisher"),
            doi: s("DOI"),
            url: s("URL"),
            file: String::new(),
            source: None,
        });
    }
    Ok(out)
}

/// Read a file to import by its look: CSL-JSON starts with `[` or `{`.
pub fn parse_any(text: &str) -> Result<Vec<Reference>, String> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('[') || t.starts_with('{') { parse_csl_json(t) } else { parse_bibtex(t) }
}

fn bib_value(s: &str) -> String {
    // Braces must balance inside `{…}`; drop stray ones.
    s.replace(['{', '}'], "")
}

/// One BibTeX entry for `r`.
pub fn to_bibtex(r: &Reference) -> String {
    let names = |v: &[Name]| v.iter().map(Name::inverted).collect::<Vec<_>>().join(" and ");
    let journal = r.kind == "article";
    let fields: [(&str, String); 13] = [
        ("author", names(&r.authors)),
        ("editor", names(&r.editors)),
        ("title", r.title.clone()),
        (if journal { "journal" } else { "booktitle" }, r.container.clone()),
        ("year", r.year.clone()),
        ("volume", r.volume.clone()),
        ("number", r.issue.clone()),
        ("pages", r.pages.clone()),
        ("publisher", r.publisher.clone()),
        ("doi", r.doi.clone()),
        ("url", r.url.clone()),
        ("file", r.file.clone()),
        ("", String::new()),
    ];
    let mut s = format!("@{}{{{},\n", if r.kind.is_empty() { "misc" } else { &r.kind }, r.key);
    for (k, v) in fields {
        if !k.is_empty() && !v.trim().is_empty() {
            let _ = writeln!(s, "  {k} = {{{}}},", bib_value(v.trim()));
        }
    }
    s.push_str("}\n");
    s
}

/// Where each entry of a BibTeX file is: key and byte range (from `@` to
/// the closing brace's line end). `@string`, `@comment` and `@preamble` are skipped.
pub fn bib_spans(text: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text[i..].find('@').map(|k| i + k) {
        let rest = &text[at + 1..];
        let Some(open) = rest.find(['{', '(']) else { break };
        let kind = rest[..open].trim().to_lowercase();
        let close_ch = if b[at + 1 + open] == b'{' { b'}' } else { b')' };
        let body = at + 1 + open + 1;
        let mut depth = 1;
        let mut j = body;
        while j < b.len() && depth > 0 {
            if b[j] == b[at + 1 + open] && close_ch == b'}' {
                depth += 1;
            } else if b[j] == close_ch {
                depth -= 1;
            }
            j += 1;
        }
        let end = text[j..].find('\n').map_or(text.len(), |n| j + n + 1);
        if !kind.is_empty() && kind.chars().all(char::is_alphanumeric) && !matches!(kind.as_str(), "string" | "comment" | "preamble") {
            let key = text[body..j.saturating_sub(1)].split(',').next().unwrap_or_default().trim().to_string();
            if !key.is_empty() {
                out.push((key, at..end));
            }
        }
        i = j.max(at + 1);
    }
    out
}

/// What an import would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// New keys, in the order they came.
    pub added: Vec<Reference>,
    /// Keys already in the library whose details differ (they are replaced).
    pub updated: Vec<Reference>,
    /// Keys already in the library, unchanged.
    pub unchanged: usize,
    /// Fields Quire doesn't read (abstract, keywords, isbn, …) of the
    /// incoming BibTeX entries, by key, written into the library as they are.
    pub extras: HashMap<String, Vec<(String, String)>>,
}

impl ImportPlan {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty()
    }

    /// "12 new, 2 updated, 30 already there".
    pub fn summary(&self) -> String {
        format!("{} new, {} updated, {} already there", self.added.len(), self.updated.len(), self.unchanged)
    }
}

/// Compare what is being imported with the library file's text.
pub fn plan_import(library_text: &str, incoming: Vec<Reference>) -> ImportPlan {
    let existing: HashMap<String, Reference> =
        parse_bibtex(library_text).unwrap_or_default().into_iter().map(|r| (r.key.clone(), r)).collect();
    let mut plan = ImportPlan::default();
    let mut seen = std::collections::HashSet::new();
    for r in incoming {
        if !seen.insert(r.key.clone()) {
            continue;
        }
        match existing.get(&r.key) {
            None => plan.added.push(r),
            Some(old) => {
                let merged = fill_from(r, old);
                if same(old, &merged) {
                    plan.unchanged += 1;
                } else {
                    plan.updated.push(merged);
                }
            }
        }
    }
    plan
}

fn same(a: &Reference, b: &Reference) -> bool {
    let mut b2 = b.clone();
    b2.source.clone_from(&a.source);
    *a == b2
}

/// `new` with what it leaves empty taken from `old`: an export without a
/// DOI or the `file` link doesn't remove the library's.
fn fill_from(mut new: Reference, old: &Reference) -> Reference {
    for (n, o) in [
        (&mut new.kind, &old.kind),
        (&mut new.title, &old.title),
        (&mut new.container, &old.container),
        (&mut new.year, &old.year),
        (&mut new.volume, &old.volume),
        (&mut new.issue, &old.issue),
        (&mut new.pages, &old.pages),
        (&mut new.publisher, &old.publisher),
        (&mut new.doi, &old.doi),
        (&mut new.url, &old.url),
        (&mut new.file, &old.file),
    ] {
        if n.trim().is_empty() {
            n.clone_from(o);
        }
    }
    if new.authors.is_empty() {
        new.authors.clone_from(&old.authors);
    }
    if new.editors.is_empty() {
        new.editors.clone_from(&old.editors);
    }
    new
}

/// BibTeX fields Quire reads; the others are kept as written.
const READ_FIELDS: [&str; 20] = [
    "author",
    "editor",
    "title",
    "journaltitle",
    "journal",
    "booktitle",
    "maintitle",
    "series",
    "year",
    "date",
    "volume",
    "number",
    "issue",
    "pages",
    "publisher",
    "institution",
    "school",
    "organization",
    "doi",
    "url",
];

/// For each entry of a BibTeX text, the fields Quire doesn't read, as
/// BibTeX (`{…}` values), in file order. Empty when the text isn't BibTeX.
pub fn other_fields(text: &str) -> HashMap<String, Vec<(String, String)>> {
    use biblatex::ChunksExt as _;
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('[') || t.starts_with('{') {
        return HashMap::new();
    }
    let Ok(Ok(bib)) = std::panic::catch_unwind(|| biblatex::Bibliography::parse(t)) else { return HashMap::new() };
    bib.iter()
        .map(|e| {
            let mut v: Vec<(String, String)> = e
                .fields
                .iter()
                .filter(|(k, _)| {
                    let k = k.to_ascii_lowercase();
                    k != "file" && !READ_FIELDS.contains(&k.as_str())
                })
                .map(|(k, c)| (k.clone(), c.to_biblatex_string(matches!(k.as_str(), "url" | "doi" | "file"))))
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            (e.key.clone(), v)
        })
        .collect()
}

/// [`to_bibtex`] with `extra` fields (already BibTeX values) before the end.
fn to_bibtex_with(r: &Reference, extra: &[(String, String)]) -> String {
    let mut s = to_bibtex(r);
    if extra.is_empty() {
        return s;
    }
    s.truncate(s.len() - "}\n".len());
    for (k, v) in extra {
        let _ = writeln!(s, "  {k} = {v},");
    }
    s.push_str("}\n");
    s
}

/// The library file's text after an import: updated entries replaced in
/// place, new ones appended. Everything else stays as written.
pub fn apply_import(library_text: &str, plan: &ImportPlan) -> String {
    let spans = bib_spans(library_text);
    // Read the whole file once (an entry alone may use the file's @string macros).
    // Fields Quire doesn't read stay: the library's, then the import's over them.
    let kept = other_fields(library_text);
    let extra_for = |key: &str| {
        let mut v: Vec<(String, String)> = kept.get(key).cloned().unwrap_or_default();
        for (k, val) in plan.extras.get(key).into_iter().flatten() {
            match v.iter_mut().find(|(k2, _)| k2 == k) {
                Some(slot) => slot.1.clone_from(val),
                None => v.push((k.clone(), val.clone())),
            }
        }
        v
    };
    let mut out = library_text.to_string();
    let mut repl: Vec<(Range<usize>, String)> = plan
        .updated
        .iter()
        .filter_map(|r| {
            let (_, range) = spans.iter().find(|(k, _)| *k == r.key)?;
            Some((range.clone(), to_bibtex_with(r, &extra_for(&r.key))))
        })
        .collect();
    repl.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    for (r, text) in repl {
        out.replace_range(r, &text);
    }
    if !plan.added.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        for r in &plan.added {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&to_bibtex_with(r, plan.extras.get(&r.key).map_or(&[], Vec::as_slice)));
        }
    }
    out
}

// ---------------------------------------------------------------- vault prefs

/// `[citations]` in `.quire/shared/vault.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CitationPrefs {
    /// Library files, vault paths (`references.bib`); a string or a list.
    pub library: Files,
    /// The PDF export's style: `apa`, `chicago-author-date` or `ieee`.
    pub style: String,
    /// A BibTeX or CSL-JSON file outside the vault to merge in when it
    /// changes (`~` is the home folder).
    pub watch: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Files {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl Files {
    pub fn list(&self) -> Vec<String> {
        match self {
            Self::None => Vec::new(),
            Self::One(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

/// The styles the PDF export knows (Typst's built-in CSL styles), with labels.
pub const STYLES: [(&str, &str); 3] = [("apa", "APA"), ("chicago-author-date", "Chicago (author-date)"), ("ieee", "IEEE")];

/// The default library file.
pub const DEFAULT_LIBRARY: &str = "references.bib";

impl CitationPrefs {
    pub fn load(vault: &Vault) -> Self {
        VaultPrefs::load(vault).section("citations")
    }

    /// The library files that exist (the default one when none is named).
    pub fn library_files(&self, vault: &Vault) -> Vec<NotePath> {
        let named = self.library.list();
        let names = if named.is_empty() { vec![DEFAULT_LIBRARY.to_string()] } else { named };
        names.iter().map(|n| NotePath::new(n.trim().trim_start_matches("./"))).filter(|p| vault.exists(p)).collect()
    }

    /// Where imports go: the first named library file (or the default).
    pub fn import_target(&self) -> NotePath {
        NotePath::new(self.library.list().first().map_or(DEFAULT_LIBRARY, String::as_str).trim().trim_start_matches("./"))
    }

    pub fn style(&self) -> &str {
        STYLES.iter().find(|(id, _)| *id == self.style).map_or("apa", |(id, _)| id)
    }

    /// The watched file as a path on this computer.
    pub fn watch_path(&self) -> Option<PathBuf> {
        let w = self.watch.trim();
        if w.is_empty() {
            return None;
        }
        if let Some(rest) = w.strip_prefix("~/") {
            return directories::BaseDirs::new().map(|d| d.home_dir().join(rest));
        }
        Some(PathBuf::from(w))
    }
}

/// Change `[citations]` (style, library, watch) in the vault's prefs.
pub fn save_prefs(ws: &mut Workspace, f: impl FnOnce(&mut CitationPrefs)) -> Result<(), String> {
    let vault = ws.vault.clone().ok_or("No vault is open")?;
    let mut prefs = VaultPrefs::load(&vault);
    if prefs.is_newer() {
        return Err("The vault's settings were written by a newer Quire; they weren't changed.".into());
    }
    let mut c: CitationPrefs = prefs.section("citations");
    f(&mut c);
    prefs.set_section("citations", &c).map_err(|e| e.to_string())?;
    prefs.save(&vault).map_err(|e| e.to_string())?;
    ws.features.citations.prefs = c;
    reload(ws);
    Ok(())
}

// ---------------------------------------------------------------- state

/// The library being read off the UI thread.
type Loading = Receiver<Library>;

/// State for this feature.
#[derive(Default)]
pub struct CitationsState {
    pub library: Arc<Library>,
    pub prefs: CitationPrefs,
    loading: Option<Loading>,
    /// The watched file's last modification time we merged.
    watched: Option<SystemTime>,
    next_watch_check: Option<Instant>,
    /// A pending import (dry run done, waiting for the person).
    pub import: Option<PendingImport>,
    /// Read the library on the next tick (the vault is set after `vault_opened`).
    needs_load: bool,
}

impl std::fmt::Debug for CitationsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CitationsState").field("refs", &self.library.refs.len()).finish_non_exhaustive()
    }
}

/// An import waiting for confirmation.
#[derive(Debug, Clone)]
pub struct PendingImport {
    /// What was chosen (a file name, for the dialog).
    pub from: String,
    pub target: NotePath,
    pub plan: ImportPlan,
}

/// How often the watched file is checked.
const WATCH_EVERY: Duration = Duration::from_secs(5);

fn read_library(vault: &Vault, prefs: &CitationPrefs) -> Library {
    let files = prefs.library_files(vault);
    let mut refs = Vec::new();
    let mut problems = Vec::new();
    for f in &files {
        match vault.read(f).map_err(|e| e.to_string()).and_then(|t| parse_bibtex(&t)) {
            Ok(rs) => refs.extend(rs.into_iter().map(|mut r| {
                r.source = Some(f.clone());
                r
            })),
            Err(e) => problems.push(format!("{f}: {e}")),
        }
    }
    Library::new(refs, files, problems)
}

/// Read the library again (off the UI thread).
pub fn reload(ws: &mut Workspace) {
    let Some(vault) = ws.vault.clone() else { return };
    let prefs = CitationPrefs::load(&vault);
    ws.features.citations.prefs = prefs.clone();
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(read_library(&vault, &prefs));
    });
    ws.features.citations.loading = Some(rx);
}

pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.citations = CitationsState { needs_load: true, ..CitationsState::default() };
    ws.features.citations.next_watch_check = Some(Instant::now() + WATCH_EVERY);
}

/// A file changed: re-read the library when it is one of its files (or the prefs).
pub(crate) fn file_changed(ws: &mut Workspace, path: &NotePath) {
    let st = &ws.features.citations;
    let ours = st.library.files.contains(path)
        || st.prefs.library_files_names().iter().any(|n| n == path.as_str())
        || (st.prefs.library.list().is_empty() && path.as_str() == DEFAULT_LIBRARY);
    if ours {
        reload(ws);
    }
}

impl CitationPrefs {
    fn library_files_names(&self) -> Vec<String> {
        self.library.list().into_iter().map(|n| n.trim().trim_start_matches("./").to_string()).collect()
    }
}

/// Per-frame work: a finished load, the watched file.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    if std::mem::take(&mut ws.features.citations.needs_load) {
        reload(ws);
    }
    let loaded = ws.features.citations.loading.as_ref().and_then(|rx| rx.try_recv().ok());
    let mut changed = loaded.is_some();
    if let Some(lib) = loaded {
        ws.features.citations.loading = None;
        for p in &lib.problems {
            ws.notify(NoticeLevel::Warn, format!("Couldn't read the bibliography {p}"));
        }
        ws.features.citations.library = Arc::new(lib);
    }
    if ws.features.citations.next_watch_check.is_some_and(|t| now >= t) {
        ws.features.citations.next_watch_check = Some(now + WATCH_EVERY);
        changed |= check_watched(ws);
    }
    changed
}

pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    if ws.features.citations.prefs.watch.trim().is_empty() { None } else { ws.features.citations.next_watch_check }
}

/// Merge the watched file into the library when it changed since last time.
fn check_watched(ws: &mut Workspace) -> bool {
    let Some(path) = ws.features.citations.prefs.watch_path() else { return false };
    let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return false };
    let last = ws.features.citations.watched;
    ws.features.citations.watched = Some(modified);
    // The first look only records the time, unless the library is empty.
    if last == Some(modified) || (last.is_none() && !ws.features.citations.library.is_empty()) {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let incoming = match parse_any(&text) {
        Ok(r) => r,
        Err(e) => {
            ws.notify(NoticeLevel::Warn, format!("Couldn't read the watched bibliography {}: {e}", path.display()));
            return true;
        }
    };
    let target = ws.features.citations.prefs.import_target();
    match import_now(ws, &target, incoming, other_fields(&text)) {
        Ok(plan) if !plan.is_empty() => {
            ws.notify(NoticeLevel::Info, format!("Bibliography updated from {}: {}", path.display(), plan.summary()));
        }
        Ok(_) => {}
        Err(e) => ws.notify(NoticeLevel::Warn, e),
    }
    true
}

/// Merge `incoming` into `target` now. Returns what was done.
/// `extras` are the incoming entries' other fields ([`other_fields`]).
pub(crate) fn import_now(
    ws: &mut Workspace,
    target: &NotePath,
    incoming: Vec<Reference>,
    extras: HashMap<String, Vec<(String, String)>>,
) -> Result<ImportPlan, String> {
    let vault = ws.vault.clone().ok_or("No vault is open")?;
    let current = if vault.exists(target) { vault.read(target).map_err(|e| e.to_string())? } else { String::new() };
    let mut plan = plan_import(&current, incoming);
    plan.extras = extras;
    if plan.is_empty() {
        return Ok(plan);
    }
    let text = apply_import(&current, &plan);
    vault.storage().write_atomic(target, &text).map_err(|e| format!("Couldn't write {target}: {e}"))?;
    reload(ws);
    Ok(plan)
}

/// Read a file to import and work out the dry run (the dialog shows it).
pub fn prepare_import(ws: &mut Workspace, path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let incoming = parse_any(&text)?;
    if incoming.is_empty() {
        return Err(format!("{} has no references", path.display()));
    }
    let vault = ws.vault.clone().ok_or("No vault is open")?;
    let target = ws.features.citations.prefs.import_target();
    let current = if vault.exists(&target) { vault.read(&target).unwrap_or_default() } else { String::new() };
    let mut plan = plan_import(&current, incoming);
    plan.extras = other_fields(&text);
    let from = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    ws.features.citations.import = Some(PendingImport { from, target, plan });
    Ok(())
}

/// Do the pending import.
pub fn confirm_import(ws: &mut Workspace) -> Result<String, String> {
    let p = ws.features.citations.import.take().ok_or("Nothing to import")?;
    let mut incoming = p.plan.added.clone();
    incoming.extend(p.plan.updated.iter().cloned());
    let mut done = import_now(ws, &p.target, incoming, p.plan.extras)?;
    // Only the new and changed works were merged; count the rest as the dry run did.
    done.unchanged += p.plan.unchanged;
    if ws.features.citations.prefs.library.list().is_empty() && p.target.as_str() != DEFAULT_LIBRARY {
        let t = p.target.as_str().to_string();
        save_prefs(ws, |c| c.library = Files::One(t))?;
    }
    Ok(format!("Imported into {}: {}", p.target, done.summary()))
}

// ---------------------------------------------------------------- rendering

/// `src` with known `[@key]` citations rendered author-date and a reference
/// list, for preview, HTML export and publish. Unchanged without a library.
pub fn render_source(ws: &Workspace, src: &str) -> String {
    let lib = &ws.features.citations.library;
    if lib.is_empty() || !src.contains('@') {
        return src.to_string();
    }
    cite::render(src, lib.as_ref())
}

/// The library of a vault (read now; for publish and exports).
pub fn vault_library(vault: &Vault) -> Library {
    read_library(vault, &CitationPrefs::load(vault))
}

/// `src` rendered with `lib` (see [`render_source`]).
pub fn render_with<'a>(lib: &Library, src: &'a str) -> std::borrow::Cow<'a, str> {
    if lib.is_empty() || !src.contains('@') { std::borrow::Cow::Borrowed(src) } else { std::borrow::Cow::Owned(cite::render(src, lib)) }
}

/// The same for a vault without a workspace (CLI export, publish jobs).
pub fn render_with_vault(vault: &Vault, src: &str) -> String {
    if !src.contains('@') {
        return src.to_string();
    }
    let lib = read_library(vault, &CitationPrefs::load(vault));
    if lib.is_empty() { src.to_string() } else { cite::render(src, &lib) }
}

/// The note's Markdown for the PDF export, with each known citation
/// replaced by a placeholder, and the bibliography Typst gets (only the
/// works cited, the style from `[citations] style`).
#[cfg(feature = "typeset")]
pub fn for_pdf(vault: &Vault, src: &str) -> (String, Option<quire_typeset::Bibliography>) {
    use quire_typeset::{BibCite, CiteForm};
    if !src.contains('@') {
        return (src.to_string(), None);
    }
    let prefs = CitationPrefs::load(vault);
    let lib = read_library(vault, &prefs);
    if lib.is_empty() {
        return (src.to_string(), None);
    }
    let mut out = String::with_capacity(src.len());
    let mut pos = 0;
    let mut cites = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for c in cite::scan(src) {
        if !c.items.iter().all(|i| lib.has(&i.key)) {
            continue;
        }
        let works = c
            .items
            .iter()
            .map(|i| BibCite {
                key: i.key.clone(),
                prefix: i.prefix.clone(),
                supplement: i.locator.clone(),
                form: if c.in_text {
                    CiteForm::Prose
                } else if i.suppress_author {
                    CiteForm::Year
                } else {
                    CiteForm::Normal
                },
            })
            .collect();
        for i in &c.items {
            if !keys.contains(&i.key) {
                keys.push(i.key.clone());
            }
        }
        out.push_str(&src[pos..c.range.start]);
        out.push_str(&quire_typeset::cite_placeholder(cites.len()));
        cites.push((works, cite::format_citation(&lib, &c)));
        pos = c.range.end;
    }
    if cites.is_empty() {
        return (src.to_string(), None);
    }
    out.push_str(&src[pos..]);
    let bibtex = keys.iter().filter_map(|k| lib.get(k)).map(to_bibtex).collect::<Vec<_>>().join("\n");
    (out, Some(quire_typeset::Bibliography { bibtex, style: prefs.style().to_string(), cites }))
}

// ---------------------------------------------------------------- in the editor

/// The citation under the caret of the active note, and the reference.
pub fn at_caret(ws: &Workspace) -> Option<(cite::Citation, Option<Reference>)> {
    let doc = ws.active_doc()?;
    let byte = doc.buffer().char_to_byte(doc.selection().head);
    let text = doc.text();
    // Look only at the caret's line (fast on big notes).
    let start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    let end = text[byte..].find('\n').map_or(text.len(), |i| byte + i);
    let c = cite::at(&text[start..end], byte - start)?;
    let key = c.items.first().map(|i| i.key.clone())?;
    let r = ws.features.citations.library.get(&key).cloned();
    Some((c, r))
}

/// The full references of the citation at `byte` of `line` (a line of a
/// note), one per line, for a hover tooltip. `None` when there is no
/// citation there or none of its works is in the library.
pub fn hover_text(ws: &Workspace, line: &str, byte: usize) -> Option<String> {
    let c = cite::at(line, byte)?;
    let lib = &ws.features.citations.library;
    let refs: Vec<String> =
        c.items.iter().filter(|i| lib.get(&i.key).is_some()).map(|i| lib.reference(&i.key).replace(['*', '\\'], "")).collect();
    (!refs.is_empty()).then(|| refs.join("\n"))
}

/// Open what a citation points at: the work's PDF in the vault when its
/// `file` field names one, else its entry in the library file. Returns
/// false when there is no known citation at the caret.
pub fn follow_at_caret(ws: &mut Workspace) -> bool {
    let Some((_, Some(r))) = at_caret(ws) else { return false };
    open_reference(ws, &r);
    true
}

/// Open a reference's PDF or library entry.
pub fn open_reference(ws: &mut Workspace, r: &Reference) {
    if let Some(name) = r.pdf_name()
        && let Some(p) = ws.vault.as_ref().and_then(|v| v.resolve_file(&name))
    {
        ws.request_ui(crate::workspace::UiRequest::OpenFile(p));
        return;
    }
    let Some(src) = r.source.clone() else { return };
    if ws.open_note(&src).is_some()
        && let Some(doc) = ws.active_doc_mut()
    {
        let text = doc.text();
        if let Some((_, range)) = bib_spans(&text).into_iter().find(|(k, _)| *k == r.key) {
            let c = doc.byte_to_char(range.start);
            doc.set_selection(quire_text::Selection::cursor(c));
            ws.request_ui(crate::workspace::UiRequest::RevealCaret);
        }
    }
}

/// `[@` completion: references matching what's typed after `@`.
pub fn suggestions(ws: &Workspace, before: &str) -> Option<(usize, String, Vec<crate::suggest::SuggestItem>)> {
    let lib = &ws.features.citations.library;
    if lib.is_empty() {
        return None;
    }
    // The `@` that starts the key being typed, inside `[` … (no `]` yet).
    let open = before.rfind('[')?;
    let inside = &before[open + 1..];
    if inside.contains(']') || inside.contains("[[") {
        return None;
    }
    let at = inside.rfind('@')?;
    let query = &inside[at + 1..];
    if query.contains(|c: char| c.is_whitespace() || c == ',' || c == ';') || query.chars().count() > 60 {
        return None;
    }
    let col = before[..=open + 1 + at].chars().count();
    let items = lib
        .search(query, 8)
        .into_iter()
        .map(|r| crate::suggest::SuggestItem {
            label: format!("{} {}", r.short_authors(), if r.year.is_empty() { "n.d." } else { &r.year }),
            detail: r.title.chars().take(70).collect(),
            insert: r.key.clone(),
        })
        .collect::<Vec<_>>();
    (!items.is_empty()).then(|| (col, query.to_string(), items))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIB: &str = r"% My library
@string{sm = {Scientific Memoirs}}

@article{ada,
  author = {Lovelace, Ada and Babbage, Charles},
  title = {Notes on the {Analytical Engine}},
  journal = sm,
  year = {1843},
  volume = {3},
  pages = {666--731},
  file = {Full Text:papers/notes.pdf:application/pdf},
  abstract = {On the {Engine}.},
}

@book{knuth1,
  author = {Donald E. Knuth},
  title = {The Art of Computer Programming},
  publisher = {Addison-Wesley},
  date = {1968-01-01},
}

@book{knuth2,
  author = {Knuth, Donald E.},
  title = {Literate Programming},
  year = 1968,
}
";

    #[test]
    fn reads_bibtex_and_formats_author_date() {
        let refs = parse_bibtex(BIB).unwrap();
        assert_eq!(refs.len(), 3);
        let lib = Library::new(refs, Vec::new(), Vec::new());
        let ada = lib.get("ada").unwrap();
        assert_eq!(ada.container, "Scientific Memoirs", "@string macros resolve");
        assert_eq!(ada.title, "Notes on the Analytical Engine");
        assert_eq!(ada.pdf_name().as_deref(), Some("notes.pdf"));
        assert_eq!(lib.authors("ada"), "Lovelace and Babbage");
        assert_eq!((lib.year("knuth1"), lib.year("knuth2")), ("1968b".into(), "1968a".into()), "same author and year: a, b by title");
        assert_eq!(
            lib.reference("ada"),
            "Lovelace, Ada, and Charles Babbage. 1843. “Notes on the Analytical Engine.” *Scientific Memoirs* 3: 666–731."
        );
        assert_eq!(lib.reference("knuth1"), "Knuth, Donald E. 1968b. *The Art of Computer Programming*. Addison-Wesley.");
        let out = cite::render("See [@ada, p. 670] and @knuth1.\n", &lib);
        assert!(
            out.starts_with("See (Lovelace and Babbage 1843, p. 670) and Knuth (1968b).\n\n## References\n\nKnuth, Donald E. 1968b."),
            "{out}"
        );
        assert_eq!(lib.search("knuth literate", 5)[0].key, "knuth2");
        assert_eq!(lib.search("ad", 5)[0].key, "ada");
        assert!(parse_bibtex("@article{broken, title = {x").is_err());
    }

    #[test]
    fn reads_csl_json() {
        let json = r#"[{"id":"turing1936","type":"article-journal","title":"On Computable Numbers","author":[{"family":"Turing","given":"Alan"}],
            "container-title":"Proceedings of the London Mathematical Society","issued":{"date-parts":[[1936,11]]},"volume":"s2-42","issue":"1","page":"230-265","DOI":"10.1112/plms/s2-42.1.230"},
            {"id":"w3c","type":"webpage","title":"HTML","author":[{"literal":"W3C"}],"URL":"https://html.spec.whatwg.org"}]"#;
        let refs = parse_any(json).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!((refs[0].kind.as_str(), refs[0].year.as_str(), refs[0].issue.as_str()), ("article", "1936", "1"));
        assert_eq!(refs[1].authors[0].family, "W3C");
        let lib = Library::new(refs, Vec::new(), Vec::new());
        assert!(lib.reference("turing1936").ends_with("<https://doi.org/10.1112/plms/s2-42.1.230>."), "{}", lib.reference("turing1936"));
        assert_eq!(lib.year("w3c"), "n.d.");
        assert!(parse_any("[{\"title\":\"no id\"}]").is_err());
    }

    #[test]
    fn imports_merge_by_key_after_a_dry_run() {
        let incoming = parse_any(
            r#"[{"id":"ada","type":"article-journal","title":"Notes on the Analytical Engine, revised","author":[{"family":"Lovelace","given":"Ada"}],"issued":{"date-parts":[[1843]]}},
                {"id":"new1","type":"book","title":"New Book","author":[{"family":"Hopper","given":"Grace"}],"issued":{"date-parts":[[1952]]}}]"#,
        )
        .unwrap();
        let plan = plan_import(BIB, incoming);
        assert_eq!(plan.summary(), "1 new, 1 updated, 0 already there");
        let out = apply_import(BIB, &plan);
        assert!(out.starts_with("% My library\n@string{sm = {Scientific Memoirs}}\n"), "everything else stays");
        let back = parse_bibtex(&out).unwrap();
        assert_eq!(back.len(), 4);
        let ada = back.iter().find(|r| r.key == "ada").unwrap();
        assert_eq!(ada.title, "Notes on the Analytical Engine, revised");
        assert_eq!(ada.pdf_name().as_deref(), Some("notes.pdf"), "the file link is kept");
        assert!(out.contains("@book{knuth1,\n  author = {Donald E. Knuth}"), "untouched entries keep their text");
        assert!(out.contains("  abstract = {On the {Engine}.},\n}"), "fields Quire doesn't read stay: {out}");
        assert_eq!(ada.volume, "3", "what the import leaves out stays");
        // BibTeX brings its other fields along.
        let bib = "@book{new2, author = {Liskov, Barbara}, title = {Abstraction}, year = {1986}, isbn = {0-262-12112-3}}";
        let mut plan = plan_import(&out, parse_any(bib).unwrap());
        plan.extras = other_fields(bib);
        let out2 = apply_import(&out, &plan);
        assert!(out2.contains("  isbn = {0-262-12112-3},\n}"), "{out2}");
        let again = plan_import(&out, parse_bibtex(&out).unwrap());
        assert!(again.is_empty(), "importing the same again changes nothing: {}", again.summary());
        assert_eq!(bib_spans(BIB).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["ada", "knuth1", "knuth2"]);
    }

    #[test]
    fn prefs_name_files_and_styles() {
        let p: CitationPrefs =
            toml::from_str("library = [\"refs/a.bib\", \"b.bib\"]\nstyle = \"ieee\"\nwatch = \"~/Zotero/lib.bib\"\nkeep = 1").unwrap();
        assert_eq!(p.library.list(), ["refs/a.bib", "b.bib"]);
        assert_eq!(p.style(), "ieee");
        assert_eq!(p.import_target().as_str(), "refs/a.bib");
        assert!(p.other.contains_key("keep"));
        let d = CitationPrefs::default();
        assert_eq!((d.style(), d.import_target().as_str()), ("apa", "references.bib"));
        let v = Vault::in_memory(&[("references.bib", BIB)]);
        assert_eq!(d.library_files(&v).len(), 1);
        assert_eq!(read_library(&v, &d).refs.len(), 3);
        assert!(render_with_vault(&v, "[@ada]").contains("## References"));
    }

    #[cfg(feature = "typeset")]
    #[test]
    fn the_pdf_export_has_citations_and_a_reference_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("references.bib"), BIB).unwrap();
        std::fs::create_dir_all(dir.path().join(".quire/shared")).unwrap();
        std::fs::write(dir.path().join(".quire/shared/vault.toml"), "format = 1\n[citations]\nstyle = \"chicago-author-date\"\n").unwrap();
        let v = Vault::open_local(dir.path()).unwrap();
        let note = "# Engines\n\nAs [@ada, p. 670] showed, and @knuth2 later. Unknown [@nobody] stays. `[@ada]` in code too.\n";
        let (md, bib) = for_pdf(&v, note);
        let bib = bib.unwrap();
        assert_eq!(bib.cites.len(), 2);
        assert_eq!(bib.style, "chicago-author-date");
        assert!(bib.bibtex.starts_with("@article{ada,") && bib.bibtex.contains("@book{knuth2,"));
        assert!(md.contains("[@nobody]") && md.contains("`[@ada]`"));
        let out = NotePath::new("Exports/Engines.pdf");
        let path = crate::export_pdf::export_pdf(&v, "Engines", note, &out).unwrap();
        let pages = quire_pdf::text::extract(&std::fs::read(path).unwrap()).unwrap();
        let text: String = pages.iter().map(quire_pdf::PageText::text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("References"), "{text}");
        assert!(text.contains("Lovelace") && text.contains("1843"), "{text}");
        assert!(text.contains("Literate Programming"), "{text}");
        // A bibliography Typst can't read still gives a PDF, with the citations as text.
        std::fs::write(dir.path().join("references.bib"), "@article{ada, title = {Notes}, author = {Lovelace, Ada}, year = {18x3}}\n")
            .unwrap();
        let path = crate::export_pdf::export_pdf(&v, "Engines", "Cited [@ada].", &out).unwrap();
        assert!(std::fs::read(path).unwrap().starts_with(b"%PDF-"));
    }
}
