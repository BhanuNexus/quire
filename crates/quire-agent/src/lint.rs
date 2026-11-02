// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Vault lint: deterministic checks over every note (design §A.8).
//!
//! | Check | Finds | Quick fix |
//! |---|---|---|
//! | `broken-link` | links and embeds that don't resolve (case and accents ignored) | point at the closest name, or create the note |
//! | `broken-block` | `[[Note#^id]]` where the block is missing | — |
//! | `orphan` | notes with no links in or out (not daily notes, templates, sources) | — |
//! | `missing-concept` | a missing note linked from 3 or more notes | create it |
//! | `missing-property` | a note breaking a write contract's `required` | add the property |
//! | `duplicate-title` | two or more notes with the same title | — |
//! | `empty-note` | no words besides front matter | move to the trash |
//! | `stale-index` | a zone's `index.md` lists missing pages or misses some | update the index |
//!
//! Each check is `on` (inbox and CLI), `warn` (CLI and the inbox's Checks
//! list, not the badge) or `off`, in `vault.toml`'s `[lint]` table, which
//! also takes folders to skip:
//!
//! ```toml
//! [lint]
//! exclude = ["Archive/"]
//! orphan = "off"
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use quire_markdown::{LinkKind, Value, extract};
use quire_vault::NotePath;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::change::{Change, ChangeSet};
use crate::contract::Contract;
use crate::inbox::{InboxItem, LintCheck};
use crate::policy::{Policy, ZoneRole};

pub const ALL_CHECKS: [LintCheck; 8] = [
    LintCheck::BrokenLink,
    LintCheck::BrokenBlock,
    LintCheck::Orphan,
    LintCheck::MissingConcept,
    LintCheck::MissingProperty,
    LintCheck::DuplicateTitle,
    LintCheck::EmptyNote,
    LintCheck::StaleIndex,
];

impl LintCheck {
    /// The name used in `vault.toml` and the CLI.
    pub const fn name(self) -> &'static str {
        match self {
            Self::BrokenLink => "broken-link",
            Self::BrokenBlock => "broken-block",
            Self::Orphan => "orphan",
            Self::MissingConcept => "missing-concept",
            Self::MissingProperty => "missing-property",
            Self::DuplicateTitle => "duplicate-title",
            Self::EmptyNote => "empty-note",
            Self::StaleIndex => "stale-index",
        }
    }

    /// A heading for a group of findings.
    pub const fn title(self) -> &'static str {
        match self {
            Self::BrokenLink => "Broken links",
            Self::BrokenBlock => "Links to missing blocks",
            Self::Orphan => "Notes with no links",
            Self::MissingConcept => "Missing notes many link to",
            Self::MissingProperty => "Missing required properties",
            Self::DuplicateTitle => "Notes with the same title",
            Self::EmptyNote => "Empty notes",
            Self::StaleIndex => "Out-of-date index",
        }
    }

    const fn default_level(self) -> Level {
        match self {
            Self::Orphan | Self::DuplicateTitle | Self::EmptyNote => Level::Warn,
            _ => Level::On,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    On,
    Warn,
    Off,
}

/// `vault.toml` `[lint]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LintPrefs {
    /// Folders (or globs) not checked.
    pub exclude: Vec<String>,
    /// Check name → level.
    #[serde(flatten)]
    pub levels: BTreeMap<String, toml::Value>,
}

impl LintPrefs {
    pub fn level(&self, c: LintCheck) -> Level {
        self.levels
            .get(c.name())
            .and_then(toml::Value::as_str)
            .and_then(|s| match s {
                "on" => Some(Level::On),
                "warn" => Some(Level::Warn),
                "off" => Some(Level::Off),
                _ => None,
            })
            .unwrap_or_else(|| c.default_level())
    }

    fn excluded(&self, path: &NotePath) -> bool {
        self.exclude.iter().any(|f| crate::policy::path_in_zone(f, path.as_str()))
    }
}

/// Lower case, accents dropped, `ß` → `ss`: how link targets are compared.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkd() {
        if unicode_normalization::char::is_combining_mark(c) {
            continue;
        }
        match c {
            'ß' => out.push_str("ss"),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'ø' | 'Ø' => out.push('o'),
            'ł' | 'Ł' => out.push('l'),
            'đ' | 'Đ' => out.push('d'),
            'ı' => out.push('i'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// A link as lint sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintLink {
    pub target: String,
    pub heading: Option<String>,
    pub embed: bool,
    /// As written in the note (`[[Target]]`, `![[x.png]]`).
    pub source: String,
}

/// What lint needs from one note.
#[derive(Debug, Clone)]
pub struct LintNote {
    pub path: NotePath,
    pub title: String,
    pub aliases: Vec<String>,
    pub links: Vec<LintLink>,
    pub words: usize,
    /// Block ids (`^id` at the end of a paragraph).
    pub blocks: HashSet<String>,
    pub properties: BTreeMap<String, Value>,
    pub text: String,
}

impl LintNote {
    pub fn from_source(path: NotePath, text: String) -> Self {
        let ex = extract(&text);
        let title = ex.title().map_or_else(|| path.stem().to_string(), str::to_string);
        let aliases = ex.front_matter.as_ref().map(quire_markdown::FrontMatter::aliases).unwrap_or_default();
        let links = ex
            .links
            .iter()
            .filter(|l| l.is_internal())
            .map(|l| LintLink {
                target: l.target.clone(),
                heading: l.heading.clone(),
                embed: l.kind == LinkKind::Embed,
                source: text.get(l.range.clone()).unwrap_or_default().to_string(),
            })
            .collect();
        let blocks = text
            .lines()
            .filter_map(|l| {
                let w = l.trim_end().rsplit(char::is_whitespace).next()?;
                let id = w.strip_prefix('^')?;
                (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then(|| id.to_string())
            })
            .collect();
        let properties = ex.front_matter.map(|f| f.fields).unwrap_or_default();
        Self { path, title, aliases, links, words: ex.stats.words, blocks, properties, text }
    }
}

/// Everything one lint run looks at.
pub struct LintInput<'a> {
    pub notes: &'a [LintNote],
    /// Files that aren't notes (images, PDFs), for embeds.
    pub files: &'a [NotePath],
    pub policy: &'a Policy,
    pub contracts: &'a [Contract],
    pub prefs: &'a LintPrefs,
    pub daily_folder: &'a str,
    pub templates_folder: &'a str,
}

/// One problem found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: LintCheck,
    pub path: NotePath,
    pub detail: String,
    pub fix: Option<ChangeSet>,
    pub level: Level,
}

impl Finding {
    pub fn to_item(&self) -> InboxItem {
        InboxItem::Lint { check: self.check, path: self.path.clone(), detail: self.detail.clone(), fix: self.fix.clone() }
    }

    /// Plain words for what the quick fix does.
    pub fn fix_label(&self) -> Option<&'static str> {
        self.fix.as_ref()?;
        Some(match self.check {
            LintCheck::BrokenLink => "Fix links",
            LintCheck::MissingConcept => "Create the note",
            LintCheck::MissingProperty => "Add the properties",
            LintCheck::EmptyNote => "Move to trash",
            LintCheck::StaleIndex => "Update the index",
            LintCheck::BrokenBlock | LintCheck::Orphan | LintCheck::DuplicateTitle => "Fix",
        })
    }
}

/// What a link points at.
enum Target {
    Note(usize),
    /// A file that isn't a note (an image, a PDF).
    File,
    Missing,
}

struct Resolver {
    by_path: HashMap<String, usize>,
    by_name: HashMap<String, Vec<usize>>,
    files: HashSet<String>,
    file_names: HashSet<String>,
}

impl Resolver {
    fn new(notes: &[LintNote], files: &[NotePath]) -> Self {
        let mut by_path = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, n) in notes.iter().enumerate() {
            by_path.insert(fold(n.path.without_ext()), i);
            by_name.entry(fold(n.path.stem())).or_default().push(i);
            for a in &n.aliases {
                by_name.entry(fold(a)).or_default().push(i);
            }
        }
        let files: HashSet<String> = files.iter().map(|f| fold(f.as_str())).collect();
        let file_names = files.iter().map(|f| f.rsplit('/').next().unwrap_or(f).to_string()).collect();
        Self { by_path, by_name, files, file_names }
    }

    /// What a link target names.
    fn resolve(&self, target: &str, from: usize) -> Target {
        let t = fold(target.trim().trim_end_matches(".md"));
        if t.is_empty() {
            return Target::Note(from);
        }
        if let Some(i) = self.by_path.get(&t) {
            return Target::Note(*i);
        }
        let name = t.rsplit('/').next().unwrap_or(&t);
        if let Some(v) = self.by_name.get(name)
            && let Some(i) = v.first()
        {
            return Target::Note(*i);
        }
        let ft = fold(target.trim());
        if self.files.contains(&ft) || self.file_names.contains(ft.rsplit('/').next().unwrap_or(&ft)) {
            Target::File
        } else {
            Target::Missing
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// Run every check that isn't off.
pub fn lint(input: &LintInput<'_>) -> Vec<Finding> {
    let LintInput { notes, files, policy, contracts, prefs, daily_folder, templates_folder } = *input;
    let res = Resolver::new(notes, files);
    let mut out = Vec::new();
    let on = |c: LintCheck| prefs.level(c) != Level::Off;
    let mut push = |check: LintCheck, path: &NotePath, detail: String, fix: Option<ChangeSet>| {
        out.push(Finding { check, path: path.clone(), detail, fix, level: prefs.level(check) });
    };
    let in_folder = |p: &NotePath, f: &str| {
        let f = f.trim_matches('/');
        !f.is_empty() && crate::policy::path_in_zone(&format!("{f}/"), p.as_str())
    };
    let raw = |p: &NotePath| policy.role_of(p.as_str()) == Some(ZoneRole::Raw);
    let checked = |n: &LintNote| !prefs.excluded(&n.path) && !in_folder(&n.path, templates_folder);
    let names: Vec<(String, usize)> = notes.iter().enumerate().map(|(i, n)| (fold(n.path.stem()), i)).collect();

    let mut in_links = vec![0_usize; notes.len()];
    let mut out_links = vec![0_usize; notes.len()];
    // Missing targets → the notes that link to them.
    let mut missing: BTreeMap<String, (String, Vec<usize>)> = BTreeMap::new();
    for (i, n) in notes.iter().enumerate() {
        let mut broken: Vec<&LintLink> = Vec::new();
        let mut broken_blocks: Vec<&LintLink> = Vec::new();
        for l in &n.links {
            match res.resolve(&l.target, i) {
                Target::Note(j) => {
                    if j != i {
                        out_links[i] += 1;
                        in_links[j] += 1;
                    }
                    if let Some(id) = l.heading.as_deref().and_then(|h| h.strip_prefix('^'))
                        && !notes[j].blocks.contains(id)
                    {
                        broken_blocks.push(l);
                    }
                }
                Target::File => out_links[i] += 1,
                Target::Missing => {
                    broken.push(l);
                    let e = missing.entry(fold(&l.target)).or_insert_with(|| (l.target.clone(), Vec::new()));
                    if !e.1.contains(&i) {
                        e.1.push(i);
                    }
                }
            }
        }
        if !checked(n) {
            continue;
        }
        if on(LintCheck::BrokenLink) && !broken.is_empty() {
            let mut edited = n.text.clone();
            let mut fixed = 0;
            let mut parts = Vec::new();
            for l in &broken {
                let suggestion = suggest(&fold(&l.target), &names, notes);
                match &suggestion {
                    Some(s) => parts.push(format!("{} (did you mean [[{s}]]?)", l.source)),
                    None => parts.push(l.source.clone()),
                }
                if let Some(s) = suggestion
                    && let Some(at) = edited.find(&l.source)
                {
                    let replaced = l.source.replacen(&l.target, &s, 1);
                    edited.replace_range(at..at + l.source.len(), &replaced);
                    fixed += 1;
                }
            }
            parts.dedup();
            let detail = format!("{} doesn't resolve: {}", plural(broken.len(), "link", "links"), parts.join(", "));
            let fix = (fixed > 0).then(|| {
                ChangeSet::one(Change::Edit { path: n.path.clone(), new_text: edited }).titled(format!("Fix links in {}", n.path.stem()))
            });
            push(LintCheck::BrokenLink, &n.path, detail, fix);
        }
        if on(LintCheck::BrokenBlock) && !broken_blocks.is_empty() {
            let list: Vec<&str> = broken_blocks.iter().map(|l| l.source.as_str()).collect();
            push(LintCheck::BrokenBlock, &n.path, format!("The block isn't there any more: {}", list.join(", ")), None);
        }
        if on(LintCheck::EmptyNote) && n.words == 0 && !n.path.as_str().eq_ignore_ascii_case("log.md") {
            push(
                LintCheck::EmptyNote,
                &n.path,
                "Has no text".into(),
                Some(ChangeSet::one(Change::Delete { path: n.path.clone() }).titled(format!("Delete empty note {}", n.path.stem()))),
            );
        }
        if on(LintCheck::MissingProperty) {
            let kind = n.properties.get("type").and_then(Value::as_str);
            for c in contracts.iter().filter(|c| c.applies_to(n.path.as_str(), kind)) {
                let absent: Vec<&String> = c
                    .required
                    .iter()
                    .filter(|f| n.properties.get(*f).is_none_or(|v| v.as_list().is_empty() && v.as_map().is_none_or(BTreeMap::is_empty)))
                    .collect();
                if absent.is_empty() {
                    continue;
                }
                let names: Vec<String> = absent.iter().map(|f| format!("`{f}`")).collect();
                let set = absent.iter().map(|f| ((*f).clone(), Value::Str(String::new()))).collect();
                push(
                    LintCheck::MissingProperty,
                    &n.path,
                    format!("“{}” needs {}", c.name, names.join(", ")),
                    Some(
                        ChangeSet::one(Change::SetProperties { path: n.path.clone(), set, remove: Vec::new() })
                            .titled("Add required properties"),
                    ),
                );
            }
        }
    }
    if on(LintCheck::Orphan) {
        for (i, n) in notes.iter().enumerate() {
            let skip = !checked(n)
                || in_folder(&n.path, daily_folder)
                || raw(&n.path)
                || n.words == 0
                || crate::instructions::is_instructions_name(&n.path)
                || matches!(n.path.file_name().to_lowercase().as_str(), "index.md" | "log.md");
            if !skip && in_links[i] == 0 && out_links[i] == 0 {
                push(LintCheck::Orphan, &n.path, "No other note links here, and it links to none".into(), None);
            }
        }
    }
    if on(LintCheck::MissingConcept) {
        let home = policy.zones.iter().find(|z| z.role == ZoneRole::Agent && !z.path.contains(['*', '?', '['])).map(|z| z.path.clone());
        for (target, from) in missing.values() {
            let is_note = std::path::Path::new(target).extension().is_none_or(|e| e.eq_ignore_ascii_case("md"));
            if from.len() < 3 || !is_note {
                continue;
            }
            let name = target.rsplit('/').next().unwrap_or(target).trim();
            if name.is_empty() {
                continue;
            }
            let path = NotePath::new(format!("{}{}.md", home.as_deref().unwrap_or(""), name.trim_end_matches(".md")));
            let mut who: Vec<String> = from.iter().map(|i| format!("[[{}]]", notes[*i].path.stem())).collect();
            who.truncate(5);
            push(
                LintCheck::MissingConcept,
                &path,
                format!("[[{name}]] doesn't exist; {} link to it ({}…)", from.len(), who.join(", ")),
                Some(ChangeSet::one(Change::Create { path: path.clone(), text: format!("# {name}\n\n") }).titled(format!("Create {name}"))),
            );
        }
    }
    if on(LintCheck::DuplicateTitle) {
        let mut by_title: BTreeMap<String, Vec<&LintNote>> = BTreeMap::new();
        for n in notes.iter().filter(|n| checked(n) && !in_folder(&n.path, daily_folder)) {
            by_title.entry(fold(n.title.trim())).or_default().push(n);
        }
        for group in by_title.values().filter(|g| g.len() > 1) {
            let others: Vec<&str> = group[1..].iter().map(|n| n.path.as_str()).collect();
            push(
                LintCheck::DuplicateTitle,
                &group[0].path,
                format!("“{}” is also the title of {}", group[0].title, others.join(", ")),
                None,
            );
        }
    }
    if on(LintCheck::StaleIndex) {
        for z in policy.zones.iter().filter(|z| z.role == ZoneRole::Agent && !z.path.contains(['*', '?', '['])) {
            let folder = z.path.trim_end_matches('/');
            let index_path = NotePath::new(format!("{folder}/index.md"));
            let Some((ii, index)) = notes.iter().enumerate().find(|(_, n)| n.path.as_str().eq_ignore_ascii_case(index_path.as_str()))
            else {
                continue;
            };
            if prefs.excluded(&index.path) {
                continue;
            }
            let listed: HashSet<usize> =
                index.links.iter().filter_map(|l| if let Target::Note(j) = res.resolve(&l.target, ii) { Some(j) } else { None }).collect();
            let gone: Vec<&LintLink> = index.links.iter().filter(|l| matches!(res.resolve(&l.target, ii), Target::Missing)).collect();
            let unlisted: Vec<&LintNote> = notes
                .iter()
                .enumerate()
                .filter(|(j, n)| {
                    *j != ii
                        && crate::policy::path_in_zone(&z.path, n.path.as_str())
                        && !matches!(n.path.file_name().to_lowercase().as_str(), "index.md" | "log.md")
                        && !listed.contains(j)
                })
                .map(|(_, n)| n)
                .collect();
            if gone.is_empty() && unlisted.is_empty() {
                continue;
            }
            let mut parts = Vec::new();
            if !gone.is_empty() {
                parts.push(format!(
                    "lists {} that {} exist",
                    plural(gone.len(), "page", "pages"),
                    if gone.len() == 1 { "doesn't" } else { "don't" }
                ));
            }
            if !unlisted.is_empty() {
                parts.push(format!("misses {}", plural(unlisted.len(), "page", "pages")));
            }
            let mut text: String = index
                .text
                .split_inclusive('\n')
                .filter(|line| !gone.iter().any(|l| line.contains(&l.source) && line.trim_start().starts_with(['-', '*'])))
                .collect();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            for n in &unlisted {
                let _ = writeln!(text, "- [[{}]]", n.path.stem());
            }
            push(
                LintCheck::StaleIndex,
                &index.path,
                format!("{} {}", index.path, parts.join(" and ")),
                Some(ChangeSet::one(Change::Edit { path: index.path.clone(), new_text: text }).titled("Update the index")),
            );
        }
    }
    out.sort_by(|a, b| (a.check as u8, &a.path).cmp(&(b.check as u8, &b.path)));
    out
}

/// Findings as pretty JSON: `{"notes": n, "findings": [{check, level,
/// path, detail, fix}]}` (`quire-cli lint --json`).
pub fn to_json(findings: &[Finding], notes: usize) -> String {
    let list: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "check": f.check.name(),
                "level": if f.level == Level::Warn { "warn" } else { "on" },
                "path": f.path,
                "detail": f.detail,
                "fix": f.fix_label(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "notes": notes, "findings": list })).unwrap_or_default()
}

/// The closest note name to a missing target (similarity ≥ 0.85).
fn suggest(target: &str, names: &[(String, usize)], notes: &[LintNote]) -> Option<String> {
    let t = target.rsplit('/').next().unwrap_or(target);
    names
        .iter()
        .map(|(n, i)| (strsim::normalized_damerau_levenshtein(t, n), *i))
        .filter(|(s, _)| *s >= 0.85)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, i)| notes[i].path.stem().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(files: &[(&str, &str)]) -> Vec<LintNote> {
        files.iter().map(|(p, t)| LintNote::from_source(NotePath::new(*p), (*t).to_string())).collect()
    }

    fn run(files: &[(&str, &str)], prefs: &LintPrefs) -> Vec<Finding> {
        let ns = notes(files);
        let att = [NotePath::new("attachments/pic.png")];
        let contract: toml::Table =
            toml::from_str("name = \"concepts\"\napplies = { folder = \"wiki/\" }\nrequired = [\"type\"]\n").unwrap();
        let contracts = [Contract::parse(&contract).unwrap()];
        lint(&LintInput {
            notes: &ns,
            files: &att,
            policy: &Policy::default(),
            contracts: &contracts,
            prefs,
            daily_folder: "Daily",
            templates_folder: "Templates",
        })
    }

    fn summary(f: &[Finding]) -> Vec<String> {
        f.iter().map(|f| format!("{} {}", f.check.name(), f.path)).collect()
    }

    /// The fixture vault: each check fires exactly where expected.
    const VAULT: &[(&str, &str)] = &[
        ("Ropes.md", "# Ropes\nSee [[Rope Notes]] and [[Café]] and ![[pic.png]] and [[Rope Note]].\n"),
        ("Rope Notes.md", "# Rope Notes\nLinks to [[Ropes#^undo]] and [[Ropes]]. [[Gap]]\nUndo ^cheap\n"),
        ("cafe.md", "# Cafe\n[[Gap]] [[Ropes]]\n"),
        ("Third.md", "# Third\n[[Gap]] [[cafe]]\n"),
        ("Lonely.md", "# Lonely\nNothing links here.\n"),
        ("Empty.md", "---\ntags: x\n---\n"),
        ("Daily/2026-11-02.md", "# 2026-11-02\nNo links.\n"),
        ("Templates/T.md", "[[Nowhere]]\n"),
        ("Other/Ropes.md", "# Ropes\n[[Ropes]]\n"),
        ("wiki/index.md", "# Index\n- [[Concept A]]\n- [[Removed]]\n"),
        ("wiki/Concept A.md", "---\ntype: concept\n---\n# Concept A\n[[index]]\n"),
        ("wiki/Concept B.md", "# Concept B\n[[Concept A]]\n"),
    ];

    #[test]
    fn fixtures_give_the_expected_findings() {
        let f = run(VAULT, &LintPrefs::default());
        assert_eq!(
            summary(&f),
            [
                "broken-link Rope Notes.md",
                "broken-link Ropes.md",
                "broken-link Third.md",
                "broken-link cafe.md",
                "broken-link wiki/index.md",
                "broken-block Rope Notes.md",
                "orphan Lonely.md",
                "missing-concept wiki/Gap.md",
                "missing-property wiki/Concept B.md",
                "duplicate-title Ropes.md",
                "empty-note Empty.md",
                "stale-index wiki/index.md",
            ]
        );
        let ropes = f.iter().find(|x| x.path.as_str() == "Ropes.md").unwrap();
        assert!(ropes.detail.contains("[[Rope Note]] (did you mean [[Rope Notes]]?)"), "{}", ropes.detail);
        assert!(!ropes.detail.contains("Café"), "accents are ignored: [[Café]] is cafe.md");
        let Some(Change::Edit { new_text, .. }) = ropes.fix.as_ref().map(|c| c.changes[0].clone()) else { panic!() };
        assert!(new_text.contains("[[Rope Notes]]."), "{new_text}");
        let idx = f.iter().find(|x| x.check == LintCheck::StaleIndex).unwrap();
        let Some(Change::Edit { new_text, .. }) = idx.fix.as_ref().map(|c| c.changes[0].clone()) else { panic!() };
        assert_eq!(new_text, "# Index\n- [[Concept A]]\n- [[Concept B]]\n");
        let mc = f.iter().find(|x| x.check == LintCheck::MissingConcept).unwrap();
        assert!(mc.detail.starts_with("[[Gap]] doesn't exist; 3 link to it"), "{}", mc.detail);
        assert_eq!(f.iter().find(|x| x.check == LintCheck::Orphan).unwrap().level, Level::Warn);
    }

    #[test]
    fn levels_and_excludes() {
        let prefs: LintPrefs = toml::from_str("exclude = [\"wiki/\"]\norphan = \"off\"\nbroken-link = \"warn\"\n").unwrap();
        let f = run(VAULT, &prefs);
        assert!(!f.iter().any(|x| x.check == LintCheck::Orphan));
        assert!(!f.iter().any(|x| x.path.as_str().starts_with("wiki/") && x.check != LintCheck::MissingConcept));
        assert!(f.iter().filter(|x| x.check == LintCheck::BrokenLink).all(|x| x.level == Level::Warn));
        assert_eq!(fold("Ångström Straße Łódź"), "angstrom strasse lodz");
    }
}
