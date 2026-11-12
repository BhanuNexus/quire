// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Link graph: resolution, backlinks, unresolved links, orphans.

use std::collections::{HashMap, HashSet};

use quire_markdown::Link;
use quire_vault::NotePath;

use crate::record::NoteRecord;
use crate::relations::{LinkConfig, Relations};
use crate::tokenize::fold_str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    pub from: NotePath,
    pub link: Link,
    /// The line containing the link, for context.
    pub context: String,
}

/// Name → path resolution tables built from the current records: by
/// lowercase text, and by folded text (accents and case ignored, §C.2).
#[derive(Debug, Default, Clone)]
pub struct Resolver {
    exact: Tables,
    folded: Tables,
}

#[derive(Debug, Default, Clone)]
#[allow(clippy::struct_field_names, reason = "by_path / by_name / by_alias read as lookup tables")]
struct Tables {
    by_path: HashMap<String, NotePath>,
    by_name: HashMap<String, Vec<NotePath>>,
    by_alias: HashMap<String, NotePath>,
}

impl Tables {
    fn add(&mut self, rec: &NoteRecord, key: &dyn Fn(&str) -> String) {
        self.by_path.entry(key(rec.path.without_ext())).or_insert_with(|| rec.path.clone());
        self.by_name.entry(key(&rec.name)).or_default().push(rec.path.clone());
        for a in &rec.aliases {
            self.by_alias.entry(key(a)).or_insert_with(|| rec.path.clone());
        }
    }

    fn sort(&mut self) {
        for v in self.by_name.values_mut() {
            v.sort_by_key(|p| (p.as_str().len(), p.as_str().to_string()));
            v.dedup();
        }
    }

    /// Path, then name (preferring `from`'s folder), without aliases.
    fn path_or_name(&self, t: &str, from: Option<&NotePath>, key: &dyn Fn(&str) -> String) -> Option<NotePath> {
        if let Some(p) = self.by_path.get(t) {
            return Some(p.clone());
        }
        let name = t.rsplit('/').next().unwrap_or(t);
        let cands = self.by_name.get(name)?;
        let here = from.map(NotePath::folder);
        let mut best: Option<&NotePath> = None;
        for c in cands.iter().filter(|c| !t.contains('/') || key(c.without_ext()).ends_with(t)) {
            if Some(c.folder()) == here {
                return Some(c.clone());
            }
            best = best.or(Some(c));
        }
        best.cloned()
    }
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

impl Resolver {
    pub fn build<'a>(records: impl Iterator<Item = &'a NoteRecord>) -> Self {
        let mut r = Self::default();
        for rec in records {
            r.exact.add(rec, &lower);
            r.folded.add(rec, &fold_str);
        }
        r.exact.sort();
        r.folded.sort();
        r
    }

    /// Same rules as `Vault::resolve`, plus aliases, then the same again
    /// ignoring accents: `[[Lodz]]` finds `Łódź.md` when nothing matches exactly.
    pub fn resolve(&self, target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        let raw = target.trim().trim_end_matches(".md");
        if raw.is_empty() {
            return from.cloned();
        }
        let t = raw.to_lowercase();
        if let Some(p) = self.exact.path_or_name(&t, from, &lower) {
            return Some(p);
        }
        if let Some(p) = self.exact.by_alias.get(&t) {
            return Some(p.clone());
        }
        let f = fold_str(raw);
        self.folded.path_or_name(&f, from, &fold_str).or_else(|| self.folded.by_alias.get(&f).cloned())
    }
}

/// Which links [`LinkGraph::neighbors`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Notes linking here.
    In,
    /// Notes this one links to.
    Out,
    #[default]
    Both,
}

#[derive(Debug, Default, Clone)]
pub struct LinkGraph {
    pub resolver: Resolver,
    outgoing: HashMap<NotePath, Vec<NotePath>>,
    backlinks: HashMap<NotePath, Vec<Backlink>>,
    unresolved: HashMap<String, Vec<NotePath>>,
    /// 0.12: typed links and the hierarchy, built in the same pass.
    relations: Relations,
}

fn line_around(src_plain_free: &str, link: &Link) -> String {
    let start = src_plain_free[..link.range.start.min(src_plain_free.len())].rfind('\n').map_or(0, |i| i + 1);
    let end = src_plain_free[link.range.end.min(src_plain_free.len())..].find('\n').map_or(src_plain_free.len(), |i| link.range.end + i);
    src_plain_free[start..end].trim().to_string()
}

impl LinkGraph {
    /// `sources` provides raw text for backlink context lines; `cfg` says
    /// which typed links are hierarchy edges.
    pub fn build<'a>(
        records: impl Iterator<Item = &'a NoteRecord> + Clone,
        source: &dyn Fn(&NotePath) -> Option<&'a str>,
        cfg: &LinkConfig,
    ) -> Self {
        let records_again = records.clone();
        let resolver = Resolver::build(records.clone());
        let mut g = Self { resolver, ..Default::default() };
        let mut typed: Vec<(NotePath, String, NotePath)> = Vec::new();
        for rec in records {
            let src = source(&rec.path);
            let mut seen = HashSet::new();
            for link in rec.links.iter().filter(|l| l.is_internal()) {
                if link.target.is_empty() {
                    continue; // same-note heading link
                }
                match g.resolver.resolve(&link.target, Some(&rec.path)) {
                    Some(to) => {
                        if let Some(field) = &link.field {
                            typed.push((rec.path.clone(), field.clone(), to.clone()));
                        }
                        if seen.insert(to.clone()) {
                            g.outgoing.entry(rec.path.clone()).or_default().push(to.clone());
                        }
                        if to != rec.path {
                            let context = src.map_or_else(|| link.display.clone(), |s| line_around(s, link));
                            g.backlinks.entry(to).or_default().push(Backlink { from: rec.path.clone(), link: link.clone(), context });
                        }
                    }
                    None => {
                        // Attachments that aren't notes (images, PDFs) aren't "unresolved notes".
                        if !link.target.contains('.') {
                            g.unresolved.entry(link.target.clone()).or_default().push(rec.path.clone());
                        }
                    }
                }
            }
        }
        let derived = crate::derived::derive(records_again, source, &g.resolver, cfg);
        g.relations = Relations::build(typed, derived, cfg);
        g
    }

    /// Typed links and the hierarchy (0.12).
    pub const fn relations(&self) -> &Relations {
        &self.relations
    }

    pub fn backlinks(&self, p: &NotePath) -> &[Backlink] {
        self.backlinks.get(p).map_or(&[], Vec::as_slice)
    }

    pub fn outgoing(&self, p: &NotePath) -> &[NotePath] {
        self.outgoing.get(p).map_or(&[], Vec::as_slice)
    }

    /// Link targets with no note, most referenced first.
    pub fn unresolved(&self) -> Vec<(&str, &[NotePath])> {
        let mut v: Vec<_> = self.unresolved.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
        v.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        v
    }

    pub fn is_orphan(&self, p: &NotePath) -> bool {
        self.backlinks(p).is_empty() && self.outgoing(p).is_empty()
    }

    /// Notes within `depth` links of `start` (at most 3), breadth first,
    /// with their distance; `start` itself isn't listed. Stops after 500 notes.
    pub fn neighbors(&self, start: &NotePath, depth: usize, dir: Direction) -> Vec<(NotePath, usize)> {
        const VISITED_CAP: usize = 500;
        let depth = depth.clamp(1, 3);
        let mut seen: HashSet<NotePath> = HashSet::from([start.clone()]);
        let mut out = Vec::new();
        let mut frontier = vec![start.clone()];
        for d in 1..=depth {
            let mut next = Vec::new();
            for p in &frontier {
                let ins = matches!(dir, Direction::In | Direction::Both).then(|| self.backlinks(p).iter().map(|b| &b.from));
                let outs = matches!(dir, Direction::Out | Direction::Both).then(|| self.outgoing(p).iter());
                for n in ins.into_iter().flatten().chain(outs.into_iter().flatten()) {
                    if seen.len() >= VISITED_CAP {
                        return out;
                    }
                    if seen.insert(n.clone()) {
                        out.push((n.clone(), d));
                        next.push(n.clone());
                    }
                }
            }
            frontier = next;
        }
        out
    }

    pub fn edges(&self) -> impl Iterator<Item = (&NotePath, &NotePath)> {
        self.outgoing.iter().flat_map(|(a, bs)| bs.iter().map(move |b| (a, b)))
    }
}
