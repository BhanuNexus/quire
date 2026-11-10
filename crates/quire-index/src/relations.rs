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

use std::collections::{HashMap, HashSet, VecDeque};

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
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkConfig {
    pub up: Vec<String>,
    pub down: Vec<String>,
    pub same: Vec<String>,
    pub next: Vec<String>,
    pub prev: Vec<String>,
    pub implied: bool,
    pub trail: TrailMode,
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
    /// Edges from `(from, field, to)` triples of resolved typed links.
    pub(crate) fn build(edges: Vec<(NotePath, String, NotePath)>, cfg: &LinkConfig) -> Self {
        let table = cfg.table();
        let mut r = Self::default();
        let mut seen: HashSet<(NotePath, String, NotePath, bool)> = HashSet::new();
        let mut implied = Vec::new();
        for (from, field, to) in edges {
            *r.fields.entry(field.clone()).or_default() += 1;
            if from == to {
                continue;
            }
            let hier = table.get(&field).copied();
            r.has_hierarchy |= hier.is_some();
            if !seen.insert((from.clone(), field.clone(), to.clone(), false)) {
                continue;
            }
            r.into.entry(to.clone()).or_default().push(Relation { field: field.clone(), hier, to: from.clone(), implied: false });
            if let Some(h) = hier.filter(|_| cfg.implied) {
                implied.push((to.clone(), h.reverse(), from.clone()));
            }
            r.out.entry(from).or_default().push(Relation { field, hier, to, implied: false });
        }
        for (from, h, to) in implied {
            let field = cfg.field_for(h);
            let list = r.out.entry(from.clone()).or_default();
            // Written both ways already: one edge.
            if list.iter().any(|e| e.hier == Some(h) && e.to == to) || !seen.insert((from, field.clone(), to.clone(), true)) {
                continue;
            }
            list.push(Relation { field, hier: Some(h), to, implied: true });
        }
        r
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
            let parents: Vec<&NotePath> = self.parents(last).into_iter().filter(|q| !path.contains(q)).collect();
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
        Relations::build(edges.iter().map(|(a, f, b)| (p(a), (*f).to_string(), p(b))).collect(), cfg)
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
}
