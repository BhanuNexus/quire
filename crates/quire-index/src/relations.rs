// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Typed links and the hierarchy they make (0.12).
//!
//! A link inside a field (`up:: [[X]]`, `up: "[[X]]"`, see
//! `quire_markdown::fields`) is a typed edge. The vault's `[links]`
//! section in `.quire/shared/vault.toml` says which field names are
//! hierarchy edges ([`LinkConfig`]); every other field is a free-form type
//! (`supports`, `contradicts`, `source`). Hierarchy edges imply their
//! reverse unless `implied = false`: A `up` B gives B `down` A.
//!
//! The edges are built with the backlinks, in the same pass over the same
//! records (`LinkGraph::build`), so they cost no extra walk of the vault.
//! Every query here is bounded: walks keep a visited set, so cycles end,
//! and stop after a fixed number of steps.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use quire_markdown::fields::normalize_key;
use quire_vault::NotePath;
use serde::{Deserialize, Serialize};

/// What a hierarchy field means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Hier {
    Up,
    Down,
    Same,
    Next,
    Prev,
}

impl Hier {
    pub const ALL: [Self; 5] = [Self::Up, Self::Down, Self::Same, Self::Next, Self::Prev];

    /// The edge the other note gets: `up` ↔ `down`, `next` ↔ `prev`, `same` ↔ `same`.
    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Self::Up => Self::Down,
            Self::Down => Self::Up,
            Self::Same => Self::Same,
            Self::Next => Self::Prev,
            Self::Prev => Self::Next,
        }
    }

    /// The group heading for notes related this way ("Parents").
    pub const fn label(self) -> &'static str {
        match self {
            Self::Up => "Parents",
            Self::Down => "Children",
            Self::Same => "Siblings",
            Self::Next => "Next",
            Self::Prev => "Previous",
        }
    }

    /// The name in `[links]` and in tool arguments (`up`).
    pub const fn key(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Same => "same",
            Self::Next => "next",
            Self::Prev => "prev",
        }
    }
}

/// When the breadcrumbs trail shows above a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrailMode {
    /// Only when some note in the vault has a hierarchy field.
    #[default]
    Auto,
    On,
    Off,
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// The `[links]` section of `.quire/shared/vault.toml`.
///
/// ```toml
/// [links]
/// up = ["up", "parent"]
/// down = ["down", "child"]
/// same = ["same", "sibling"]
/// next = ["next"]
/// prev = ["prev"]
/// implied = true      # A up B gives B down A
/// trail = "auto"      # auto | on | off
///
/// [links.types.supports]   # 0.13: a free-form type's colour in the graph
/// color = "#2e7d32"
///
/// [links.dendron]          # 0.13: `a.b.c` is a child of `a.b`
/// enabled = true
/// field = "up"
/// delimiter = "."
/// ```
///
/// Keys this version doesn't know are kept in `other`, so saving the
/// section from Settings never drops them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkConfig {
    pub up: Vec<String>,
    pub down: Vec<String>,
    pub same: Vec<String>,
    pub next: Vec<String>,
    pub prev: Vec<String>,
    pub implied: bool,
    pub trail: TrailMode,
    /// Free-form types with a style, by name as written (`supports`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<String, TypeStyle>,
    /// Dotted note names as a hierarchy (Breadcrumbs' Dendron notes).
    #[serde(skip_serializing_if = "Dendron::is_default")]
    pub dendron: Dendron,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// How a free-form type is drawn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TypeStyle {
    /// `#rrggbb`, the type's colour in the graph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// `#rrggbb` (or `#rgb`) as bytes.
#[must_use]
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().strip_prefix('#')?;
    if !h.is_ascii() {
        return None;
    }
    let digit = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
    match h.len() {
        6 => Some([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?]),
        3 => Some([digit(0, 1)? * 17, digit(1, 1)? * 17, digit(2, 1)? * 17]),
        _ => None,
    }
}

/// Dendron-style names: `a.b.c` is a child of `a.b` (0.13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dendron {
    pub enabled: bool,
    /// The field the child has for its parent (`up`).
    pub field: String,
    pub delimiter: String,
}

impl Default for Dendron {
    fn default() -> Self {
        Self { enabled: false, field: "up".into(), delimiter: ".".into() }
    }
}

impl Dendron {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            up: names(&["up", "parent"]),
            down: names(&["down", "child"]),
            same: names(&["same", "sibling"]),
            next: names(&["next"]),
            prev: names(&["prev"]),
            implied: true,
            trail: TrailMode::Auto,
            types: BTreeMap::new(),
            dendron: Dendron::default(),
            other: toml::Table::new(),
        }
    }
}

/// Where the settings live in `vault.toml`.
pub const SECTION: &str = "links";

impl LinkConfig {
    /// The vault's settings (defaults when the section is missing or wrong).
    pub fn load(vault: &quire_vault::Vault) -> Self {
        quire_vault::VaultPrefs::load(vault).section(SECTION)
    }

    pub fn fields(&self, h: Hier) -> &[String] {
        match h {
            Hier::Up => &self.up,
            Hier::Down => &self.down,
            Hier::Same => &self.same,
            Hier::Next => &self.next,
            Hier::Prev => &self.prev,
        }
    }

    /// What a (normalized) field name means, if it is a hierarchy field.
    pub fn kind(&self, field: &str) -> Option<Hier> {
        Hier::ALL.into_iter().find(|h| self.fields(*h).iter().any(|f| normalize_key(f) == field))
    }

    /// The name written for a new `h` edge, and shown for implied ones.
    pub fn field_for(&self, h: Hier) -> String {
        self.fields(h).first().map_or_else(|| h.key().to_string(), |f| normalize_key(f))
    }

    /// Write the settings into `prefs`' `[links]` section (the caller saves
    /// the file). A key that is still at its default and isn't in the file
    /// is not added, so saving from Settings doesn't fill a hand-written
    /// section with defaults; every other byte of the file stays.
    pub fn store(&self, prefs: &mut quire_vault::VaultPrefs) -> Result<(), quire_vault::PrefsError> {
        let existing: toml::Table = prefs.try_section(SECTION).ok().flatten().unwrap_or_default();
        let to_table = |c: &Self| match toml::Value::try_from(c) {
            Ok(toml::Value::Table(t)) => t,
            _ => toml::Table::new(),
        };
        let defaults = to_table(&Self::default());
        let mut t = to_table(self);
        t.retain(|k, v| existing.contains_key(k) || defaults.get(k) != Some(v));
        if t.is_empty() && existing.is_empty() {
            return Ok(());
        }
        prefs.set_section(SECTION, &t)
    }

    /// The graph colour set for a type (`field` is normalized), if valid.
    pub fn color_of(&self, field: &str) -> Option<[u8; 3]> {
        self.types.iter().find(|(k, _)| normalize_key(k) == field).and_then(|(_, t)| t.color.as_deref()).and_then(parse_hex_color)
    }

    fn table(&self) -> HashMap<String, Hier> {
        let mut t = HashMap::new();
        for h in Hier::ALL {
            for f in self.fields(h) {
                t.entry(normalize_key(f)).or_insert(h);
            }
        }
        t
    }
}

/// One typed edge from a note.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Relation {
    /// The field the link is in (`parent`, `supports`); for an implied
    /// edge, the first name configured for its kind (`down`).
    pub field: String,
    /// Its meaning when it is a hierarchy field.
    pub hier: Option<Hier>,
    /// The other note.
    pub to: NotePath,
    /// Implied by the other note's edge (A `up` B gives B this `down` A).
    pub implied: bool,
    /// Made from a naming scheme or a list (tag, folder, list, Dendron
    /// notes), not from a link written in the note.
    pub derived: bool,
}

/// The typed edges of a vault. Built by `LinkGraph::build`.
#[derive(Debug, Default, Clone)]
pub struct Relations {
    /// Each note's typed edges: its own, then the implied ones.
    out: HashMap<NotePath, Vec<Relation>>,
    /// Typed links into each note, as written in the note they come from
    /// (`to` is that note).
    into: HashMap<NotePath, Vec<Relation>>,
    /// How many links each field name holds, for suggestions and filters.
    fields: HashMap<String, usize>,
    /// Some note has a hierarchy field.
    has_hierarchy: bool,
}

/// Most steps a walk takes (each step looks at one note's edges).
const WALK_CAP: usize = 5000;

impl Relations {
    /// Edges from `(from, field, to)` triples of resolved typed links, and
    /// `derived` ones made from naming schemes (written edges win when both
    /// make the same edge).
    pub(crate) fn build(edges: Vec<(NotePath, String, NotePath)>, derived: Vec<(NotePath, String, NotePath)>, cfg: &LinkConfig) -> Self {
        let table = cfg.table();
        let mut r = Self::default();
        let mut seen: HashSet<(NotePath, String, NotePath, bool)> = HashSet::new();
        let mut implied = Vec::new();
        let all = edges.into_iter().map(|e| (e, false)).chain(derived.into_iter().map(|e| (e, true)));
        for ((from, field, to), is_derived) in all {
            if !is_derived {
                *r.fields.entry(field.clone()).or_default() += 1;
            }
            if from == to {
                continue;
            }
            let hier = table.get(&field).copied();
            r.has_hierarchy |= hier.is_some();
            if !seen.insert((from.clone(), field.clone(), to.clone(), false)) {
                continue;
            }
            let back = Relation { field: field.clone(), hier, to: from.clone(), implied: false, derived: is_derived };
            r.into.entry(to.clone()).or_default().push(back);
            if let Some(h) = hier.filter(|_| cfg.implied) {
                implied.push((to.clone(), h.reverse(), from.clone()));
            }
            r.out.entry(from).or_default().push(Relation { field, hier, to, implied: false, derived: is_derived });
        }
        for (from, h, to) in implied {
            let field = cfg.field_for(h);
            let list = r.out.entry(from.clone()).or_default();
            // Written both ways already: one edge.
            if list.iter().any(|e| e.hier == Some(h) && e.to == to) || !seen.insert((from, field.clone(), to.clone(), true)) {
                continue;
            }
            list.push(Relation { field, hier: Some(h), to, implied: true, derived: false });
        }
        r
    }

    fn sorted_out(&self) -> Vec<(&NotePath, &Vec<Relation>)> {
        // Notes in path order, each note's links in the order written: the same edges give the same result.
        let mut notes: Vec<(&NotePath, &Vec<Relation>)> = self.out.iter().collect();
        notes.sort_by(|a, b| a.0.cmp(b.0));
        notes
    }

    /// The typed links as written (not implied or derived edges): `(from, field, to)`.
    pub fn written(&self) -> impl Iterator<Item = (&NotePath, &str, &NotePath)> {
        self.sorted_out()
            .into_iter()
            .flat_map(|(from, list)| list.iter().filter(|e| !e.implied && !e.derived).map(move |e| (from, e.field.as_str(), &e.to)))
    }

    /// The edges derived from tag, folder, list and Dendron notes.
    pub fn derived(&self) -> impl Iterator<Item = (&NotePath, &str, &NotePath)> {
        self.sorted_out()
            .into_iter()
            .flat_map(|(from, list)| list.iter().filter(|e| e.derived).map(move |e| (from, e.field.as_str(), &e.to)))
    }

    /// The edges of several indexes read as one: what each wrote, and
    /// `cross`, the typed links between them. Implied edges are worked out
    /// again, so a link written on one side shows on the other.
    pub fn merged(parts: &[&Self], cross: Vec<(NotePath, String, NotePath)>, cfg: &LinkConfig) -> Self {
        let mut edges: Vec<(NotePath, String, NotePath)> =
            parts.iter().flat_map(|r| r.written().map(|(f, field, t)| (f.clone(), field.to_string(), t.clone()))).collect();
        edges.extend(cross);
        let derived = parts.iter().flat_map(|r| r.derived().map(|(f, field, t)| (f.clone(), field.to_string(), t.clone()))).collect();
        Self::build(edges, derived, cfg)
    }

    /// Every typed edge from `p`, its own first.
    pub fn of(&self, p: &NotePath) -> &[Relation] {
        self.out.get(p).map_or(&[], Vec::as_slice)
    }

    /// Typed links other notes make to `p` (`to` is the other note).
    pub fn incoming(&self, p: &NotePath) -> &[Relation] {
        self.into.get(p).map_or(&[], Vec::as_slice)
    }

    /// Notes `p` relates to as `h` (its own edges and implied ones), each once.
    pub fn related(&self, p: &NotePath, h: Hier) -> Vec<&NotePath> {
        let mut v: Vec<&NotePath> = Vec::new();
        for e in self.of(p).iter().filter(|e| e.hier == Some(h)) {
            if !v.contains(&&e.to) {
                v.push(&e.to);
            }
        }
        v
    }

    pub fn parents(&self, p: &NotePath) -> Vec<&NotePath> {
        self.related(p, Hier::Up)
    }

    pub fn children(&self, p: &NotePath) -> Vec<&NotePath> {
        self.related(p, Hier::Down)
    }

    /// Field names that hold links, with how many links each holds, most used first.
    pub fn fields(&self) -> Vec<(&str, usize)> {
        let mut v: Vec<(&str, usize)> = self.fields.iter().map(|(k, n)| (k.as_str(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v
    }

    /// Whether any note has a hierarchy field (the trail's `auto` mode).
    pub const fn has_hierarchy(&self) -> bool {
        self.has_hierarchy
    }

    /// Notes reached from `start` along `h` edges, breadth first, up to
    /// `depth` steps, with their distance. Ends at cycles; `start` isn't listed.
    pub fn walk(&self, start: &NotePath, h: Hier, depth: usize) -> Vec<(NotePath, usize)> {
        let mut seen: HashSet<&NotePath> = HashSet::from([start]);
        let mut out = Vec::new();
        let mut queue = VecDeque::from([(start, 0usize)]);
        let mut steps = 0;
        while let Some((p, d)) = queue.pop_front() {
            steps += 1;
            if d >= depth || steps > WALK_CAP {
                continue;
            }
            for n in self.related(p, h) {
                if seen.insert(n) {
                    out.push((n.clone(), d + 1));
                    queue.push_back((n, d + 1));
                }
            }
        }
        out
    }

    /// Siblings of `p`: its `same` edges, then the other children of its
    /// parents, each once, in that order.
    pub fn siblings(&self, p: &NotePath) -> Vec<NotePath> {
        let mut v: Vec<NotePath> = self.related(p, Hier::Same).into_iter().cloned().collect();
        for parent in self.parents(p) {
            let mut kids: Vec<&NotePath> = self.children(parent);
            kids.sort();
            for k in kids {
                if k != p && !v.contains(k) {
                    v.push(k.clone());
                }
            }
        }
        v
    }

    /// Paths from a top note down to `p` along parents (`[root, …, p]`),
    /// shortest first, at most `max_paths` of them and `max_depth` long. A
    /// note on a cycle ends its path there; a note with no parents gives
    /// no path.
    pub fn trails(&self, p: &NotePath, max_paths: usize, max_depth: usize) -> Vec<Vec<NotePath>> {
        self.trails_where(p, max_paths, max_depth, &|_| true)
    }

    /// [`Self::trails`] through the notes `keep` accepts only: a path never
    /// passes through, or names, a note `keep` refuses (a note hidden from
    /// the reader), so a trail stops below it.
    pub fn trails_where(&self, p: &NotePath, max_paths: usize, max_depth: usize, keep: &dyn Fn(&NotePath) -> bool) -> Vec<Vec<NotePath>> {
        let mut out: Vec<Vec<NotePath>> = Vec::new();
        let mut steps = 0;
        // Depth first from `p` up; `path` is p, parent, grandparent…
        let mut stack: Vec<Vec<&NotePath>> = vec![vec![p]];
        while let Some(path) = stack.pop() {
            steps += 1;
            if steps > WALK_CAP || out.len() >= max_paths * 4 {
                break;
            }
            let last = *path.last().expect("never empty");
            let parents: Vec<&NotePath> = self.parents(last).into_iter().filter(|q| !path.contains(q) && keep(q)).collect();
            if parents.is_empty() || path.len() > max_depth {
                if path.len() > 1 {
                    out.push(path.iter().rev().map(|q| (*q).clone()).collect());
                }
                continue;
            }
            for q in parents.into_iter().rev() {
                let mut next = path.clone();
                next.push(q);
                stack.push(next);
            }
        }
        out.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        out.dedup();
        out.truncate(max_paths);
        out
    }

    /// The note after (or before) `p` among its siblings: its `next`
    /// (`prev`) edge if it has one, else the next sibling under its first
    /// parent, by name.
    pub fn step(&self, p: &NotePath, forward: bool) -> Option<NotePath> {
        let h = if forward { Hier::Next } else { Hier::Prev };
        if let Some(n) = self.related(p, h).first() {
            return Some((*n).clone());
        }
        let parent = *self.parents(p).first()?;
        let mut kids: Vec<&NotePath> = self.children(parent);
        kids.sort_by_key(|k| k.stem().to_lowercase());
        let i = kids.iter().position(|k| *k == p)?;
        let j = if forward { i + 1 } else { i.checked_sub(1)? };
        kids.get(j).map(|k| (*k).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NotePath {
        NotePath::new(format!("{s}.md"))
    }

    fn rel(edges: &[(&str, &str, &str)], cfg: &LinkConfig) -> Relations {
        Relations::build(edges.iter().map(|(a, f, b)| (p(a), (*f).to_string(), p(b))).collect(), Vec::new(), cfg)
    }

    #[test]
    fn hierarchy_fields_imply_their_reverse() {
        let cfg = LinkConfig::default();
        let r = rel(&[("A", "up", "Root"), ("B", "parent", "Root"), ("A", "next", "B"), ("A", "supports", "Claim")], &cfg);
        assert_eq!(r.children(&p("Root")), [&p("A"), &p("B")]);
        assert!(r.of(&p("Root")).iter().all(|e| e.implied && e.field == "down"));
        assert_eq!(r.related(&p("B"), Hier::Prev), [&p("A")]);
        assert_eq!(r.siblings(&p("A")), [p("B")]);
        assert!(r.related(&p("Claim"), Hier::Down).is_empty(), "free-form types imply nothing");
        assert_eq!(r.incoming(&p("Claim"))[0].to, p("A"));
        assert!(r.has_hierarchy());
        let off = LinkConfig { implied: false, ..LinkConfig::default() };
        assert!(rel(&[("A", "up", "Root")], &off).children(&p("Root")).is_empty());
    }

    #[test]
    fn edges_written_both_ways_count_once() {
        let r = rel(&[("A", "up", "Root"), ("Root", "down", "A")], &LinkConfig::default());
        assert_eq!(r.of(&p("Root")).len(), 1);
        assert_eq!(r.of(&p("A")).len(), 1);
    }

    #[test]
    fn custom_field_names() {
        let cfg = LinkConfig { up: vec!["Broader".into()], down: vec!["narrower".into()], ..LinkConfig::default() };
        let r = rel(&[("A", "broader", "Root"), ("B", "up", "Root")], &cfg);
        assert_eq!(r.children(&p("Root")), [&p("A")]);
        assert_eq!(r.of(&p("Root"))[0].field, "narrower");
        assert_eq!(r.fields(), [("broader", 1), ("up", 1)]);
    }

    #[test]
    fn cycles_end_walks_and_trails() {
        let cfg = LinkConfig::default();
        let r = rel(&[("A", "up", "B"), ("B", "up", "C"), ("C", "up", "A"), ("A", "up", "Top")], &cfg);
        let up = r.walk(&p("A"), Hier::Up, 50);
        assert_eq!(up.len(), 3);
        let trails = r.trails(&p("A"), 5, 50);
        assert_eq!(trails[0], [p("Top"), p("A")]);
        assert!(trails.iter().all(|t| t.last() == Some(&p("A"))));
        assert!(r.trails(&p("Top"), 5, 50).is_empty());
    }

    #[test]
    fn several_parents_give_several_trails() {
        let r = rel(&[("Leaf", "up", "X"), ("Leaf", "up", "Y"), ("X", "up", "Root"), ("Y", "up", "Root")], &LinkConfig::default());
        let t = r.trails(&p("Leaf"), 5, 10);
        assert_eq!(t, [vec![p("Root"), p("X"), p("Leaf")], vec![p("Root"), p("Y"), p("Leaf")]]);
        assert_eq!(r.walk(&p("Root"), Hier::Down, 1), [(p("X"), 1), (p("Y"), 1)]);
        assert_eq!(r.walk(&p("Root"), Hier::Down, 5).len(), 3);
    }

    #[test]
    fn stepping_between_siblings() {
        let r = rel(&[("A", "up", "R"), ("B", "up", "R"), ("C", "up", "R"), ("C", "next", "A")], &LinkConfig::default());
        assert_eq!(r.step(&p("A"), true), Some(p("B")));
        assert_eq!(r.step(&p("B"), false), Some(p("A")));
        assert_eq!(r.step(&p("C"), true), Some(p("A")), "an explicit next wins");
        assert_eq!(r.step(&p("A"), false), Some(p("C")), "implied prev from C's next");
        assert_eq!(r.step(&p("R"), true), None);
    }

    #[test]
    fn config_reads_from_toml() {
        let c: LinkConfig = toml::from_str("up = [\"parent\"]\nimplied = false\ntrail = \"off\"\n").unwrap();
        assert_eq!(c.kind("parent"), Some(Hier::Up));
        assert_eq!(c.kind("up"), None);
        assert_eq!(c.kind("child"), Some(Hier::Down));
        assert!(!c.implied);
        assert_eq!(c.trail, TrailMode::Off);
    }

    const HAND: &str = "# Shared settings
format = 1

[web]
hide = [\"Journal/\"]   # not on the web

[links]
# what means \"parent\" here
up = [\"up\", \"parent\"]   # my parents
implied = true
future_key = \"kept\"

[links.types.supports]
color = \"#2e7d32\"
note = \"also kept\"

[tail]
x = 1
";

    #[test]
    fn settings_round_trip_keeps_every_other_byte() {
        let mut prefs = quire_vault::VaultPrefs::parse(HAND).unwrap();
        let cfg: LinkConfig = prefs.section(SECTION);
        assert_eq!(cfg.color_of("supports"), Some([0x2e, 0x7d, 0x32]));
        // Saving what was read changes nothing.
        cfg.store(&mut prefs).unwrap();
        assert_eq!(prefs.to_text(), HAND);
        // A change touches only its own line.
        let mut edited = cfg.clone();
        edited.implied = false;
        edited.down = vec!["child".into(), "kid".into()];
        edited.types.entry("contradicts".into()).or_default().color = Some("#c62828".into());
        edited.store(&mut prefs).unwrap();
        let out = prefs.to_text();
        assert!(out.starts_with("# Shared settings\nformat = 1\n\n[web]\nhide = [\"Journal/\"]   # not on the web\n\n[links]\n# what means \"parent\" here\nup = [\"up\", \"parent\"]   # my parents\nimplied = false\nfuture_key = \"kept\""), "{out}");
        assert!(
            out.contains("down = [\"child\", \"kid\"]")
                && out.contains("note = \"also kept\"")
                && out.contains("[links.types.contradicts]"),
            "{out}"
        );
        assert!(out.ends_with("[tail]\nx = 1\n"), "{out}");
        let again: LinkConfig = quire_vault::VaultPrefs::parse(&out).unwrap().section(SECTION);
        assert_eq!(again, edited);
    }

    #[test]
    fn default_settings_write_nothing_extra() {
        let mut prefs = quire_vault::VaultPrefs::parse("format = 1\n").unwrap();
        LinkConfig::default().store(&mut prefs).unwrap();
        let out = prefs.to_text();
        assert_eq!(out, "format = 1\n", "nothing is written for defaults");
    }

    #[test]
    fn colours_parse() {
        assert_eq!(parse_hex_color("#fff"), Some([255, 255, 255]));
        assert_eq!(parse_hex_color(" #0a0B0c "), Some([10, 11, 12]));
        assert_eq!(parse_hex_color("red"), None);
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("#é12345"), None);
    }
}
