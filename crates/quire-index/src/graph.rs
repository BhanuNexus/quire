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
/// lowercase text, and by folded text (accents and case ignored, `docs/design/0.9.md` §C.2).
#[derive(Debug, Default, Clone)]
pub struct Resolver {
    exact: Tables,
    folded: Tables,
}

#[derive(Debug, Default, Clone)]
struct Tables {
    by_path: HashMap<String, NotePath>,
    by_name: HashMap<String, Vec<NotePath>>,
    by_alias: HashMap<String, NotePath>,
    /// How many notes have each path key and each alias key (the first one
    /// added owns the entry): a note whose keys no other note shares can be
    /// taken out of the tables without a rebuild (`docs/design/0.14-index-startup.md` §3.4).
    path_n: HashMap<String, usize>,
    alias_n: HashMap<String, usize>,
}

impl Tables {
    fn add(&mut self, rec: &NoteRecord, key: &dyn Fn(&str) -> String) {
        // Two notes with one key (paths differing by case or accents, one alias on two notes):
        // the smaller path owns it, whatever order the notes come in (it was hash order).
        let own = |slot: &mut NotePath| {
            if rec.path < *slot {
                *slot = rec.path.clone();
            }
        };
        let pk = key(rec.path.without_ext());
        *self.path_n.entry(pk.clone()).or_default() += 1;
        self.by_path.entry(pk).and_modify(own).or_insert_with(|| rec.path.clone());
        self.by_name.entry(key(&rec.name)).or_default().push(rec.path.clone());
        for a in &rec.aliases {
            let ak = key(a);
            *self.alias_n.entry(ak.clone()).or_default() += 1;
            self.by_alias.entry(ak).and_modify(own).or_insert_with(|| rec.path.clone());
        }
    }

    /// Take out a note whose path key and aliases no other note shares.
    fn remove(&mut self, rec: &NoteRecord, key: &dyn Fn(&str) -> String) {
        let pk = key(rec.path.without_ext());
        self.path_n.remove(&pk);
        self.by_path.remove(&pk);
        let nk = key(&rec.name);
        if let Some(v) = self.by_name.get_mut(&nk) {
            v.retain(|p| p != &rec.path);
            if v.is_empty() {
                self.by_name.remove(&nk);
            }
        }
        for a in &rec.aliases {
            let ak = key(a);
            // A note can list one alias twice.
            if let Some(n) = self.alias_n.get_mut(&ak) {
                *n -= 1;
                if *n == 0 {
                    self.alias_n.remove(&ak);
                    self.by_alias.remove(&ak);
                }
            }
        }
    }

    /// Whether `rec`'s path key and aliases are its own alone (a note can
    /// list one alias twice).
    fn keys_alone(&self, rec: &NoteRecord, key: &dyn Fn(&str) -> String) -> bool {
        let mine = |k: &String| rec.aliases.iter().filter(|a| key(a) == *k).count();
        self.path_n.get(&key(rec.path.without_ext())) == Some(&1)
            && rec.aliases.iter().map(|a| key(a)).all(|k| self.alias_n.get(&k).copied().unwrap_or(0) == mine(&k))
    }

    fn sort(&mut self) {
        #[expect(clippy::iter_over_hash_type, reason = "sorts each list in place")]
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

/// The keys a link can find `rec` by, folded: its name, its aliases and its
/// path (without `.md`).
pub(crate) fn note_keys(rec: &NoteRecord) -> Vec<String> {
    let mut k = vec![fold_str(&rec.name), fold_str(rec.path.without_ext())];
    k.extend(rec.aliases.iter().map(|a| fold_str(a)));
    k
}

/// The keys an unresolved link target could come to resolve by, folded:
/// the whole target (a path or an alias) and its last segment (a name).
fn target_keys(target: &str) -> Vec<String> {
    let raw = target.trim().trim_end_matches(".md");
    let whole = fold_str(raw);
    let last = fold_str(raw.rsplit('/').next().unwrap_or(raw));
    if last == whole { vec![whole] } else { vec![whole, last] }
}

impl Resolver {
    /// Whether `p` is in the tables.
    fn has(&self, p: &NotePath) -> bool {
        self.exact.by_name.get(&lower(p.stem())).is_some_and(|v| v.contains(p))
    }

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
    /// Every internal link target that resolves to no note (attachments too),
    /// by [`target_keys`], with a count, and each note's: a new note none of
    /// whose keys is here changes where no link points (`docs/design/0.14-index-startup.md` §3.4).
    missing: HashMap<String, usize>,
    missing_by: HashMap<NotePath, Vec<String>>,
    /// The resolved typed links each note writes, `(field, to)` in the
    /// order written: kept so [`LinkGraph::patch`] can rebuild `relations`
    /// without resolving every note's links again.
    typed: HashMap<NotePath, Vec<(String, NotePath)>>,
    /// The derived edges `relations` was built with (to tell when they change).
    derived: Vec<crate::relations::DerivedEdge>,
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
        let resolver = Resolver::build(records.clone());
        let mut g = Self { resolver, ..Default::default() };
        for rec in records.clone() {
            g.add_note(rec, source(&rec.path), false);
        }
        g.order_all();
        g.relate(records, source, cfg);
        g
    }

    /// Notes arrive in hash order: put every backlink list and unresolved
    /// list in path order (a note's own links keep the order written), so a
    /// list cut at a limit is the same on every run.
    fn order_all(&mut self) {
        #[expect(clippy::iter_over_hash_type, reason = "sorts each list in place")]
        for v in self.backlinks.values_mut() {
            v.sort_by(|a, b| a.from.cmp(&b.from));
        }
        #[expect(clippy::iter_over_hash_type, reason = "sorts each list in place")]
        for v in self.unresolved.values_mut() {
            v.sort();
        }
    }

    /// One note's links: its outgoing list, its backlinks in the notes it
    /// links to, its unresolved targets and its typed links.
    fn add_note(&mut self, rec: &NoteRecord, src: Option<&str>, in_order: bool) {
        let mut seen = HashSet::new();
        let mut typed: Vec<(String, NotePath)> = Vec::new();
        for link in rec.links.iter().filter(|l| l.is_internal()) {
            if link.target.is_empty() {
                continue; // same-note heading link
            }
            if let Some(to) = self.resolver.resolve(&link.target, Some(&rec.path)) {
                if let Some(field) = &link.field {
                    typed.push((field.clone(), to.clone()));
                }
                if seen.insert(to.clone()) {
                    self.outgoing.entry(rec.path.clone()).or_default().push(to.clone());
                }
                if to != rec.path {
                    let context = src.map_or_else(|| link.display.clone(), |s| line_around(s, link));
                    self.backlinks.entry(to).or_default().push(Backlink { from: rec.path.clone(), link: link.clone(), context });
                }
            } else {
                for k in target_keys(&link.target) {
                    *self.missing.entry(k.clone()).or_default() += 1;
                    self.missing_by.entry(rec.path.clone()).or_default().push(k);
                }
                // Attachments that aren't notes (images, PDFs) aren't "unresolved notes".
                if !link.target.contains('.') {
                    // In path order: a build sorts once at the end
                    // (`order_all`), a patch inserts in place.
                    let v = self.unresolved.entry(link.target.clone()).or_default();
                    if in_order {
                        let at = v.partition_point(|q| q <= &rec.path);
                        v.insert(at, rec.path.clone());
                    } else {
                        v.push(rec.path.clone());
                    }
                }
            }
        }
        if !typed.is_empty() {
            self.typed.insert(rec.path.clone(), typed);
        }
    }

    /// Typed links, the edges derived from naming schemes, and the hierarchy.
    fn relate<'a>(
        &mut self,
        records: impl Iterator<Item = &'a NoteRecord> + Clone,
        source: &dyn Fn(&NotePath) -> Option<&'a str>,
        cfg: &LinkConfig,
    ) {
        let mut typed: Vec<(NotePath, String, NotePath)> =
            self.typed.iter().flat_map(|(from, list)| list.iter().map(|(f, to)| (from.clone(), f.clone(), to.clone()))).collect();
        // Path order (each note's links in the order written), not hash order.
        typed.sort_by(|a, b| a.0.cmp(&b.0));
        self.derived = crate::derived::derive(records, source, &self.resolver, cfg);
        self.relations = Relations::build(typed, self.derived.clone(), cfg);
    }

    /// Whether adding `rec`, a note not in the graph, changes where no link
    /// points: none of its keys (name, aliases, path; folded) is a key of
    /// another note or of a link that resolves to nothing
    /// (`docs/design/0.14-index-startup.md` §3.4). Then [`Self::patch`] can add it.
    pub fn can_add(&self, rec: &NoteRecord) -> bool {
        let t = &self.resolver.folded;
        !self.resolver.has(&rec.path)
            && note_keys(rec).iter().all(|k| {
                !t.by_name.contains_key(k) && !t.alias_n.contains_key(k) && !t.path_n.contains_key(k) && !self.missing.contains_key(k)
            })
    }

    /// Whether taking out `rec`, a note in the graph, changes where no link
    /// points: nothing links to it, and no other note shares its path key or
    /// an alias (whose table entry it may own). Then [`Self::patch`] can
    /// remove it.
    pub fn can_remove(&self, rec: &NoteRecord) -> bool {
        self.resolver.has(&rec.path)
            && self.backlinks(&rec.path).is_empty()
            && self.resolver.exact.keys_alone(rec, &lower)
            && self.resolver.folded.keys_alone(rec, &fold_str)
    }

    /// Update the graph in place after the notes `changed` were stored and
    /// the notes `removed` (their records before) were taken out (0.13.1;
    /// adds and removes since 0.14): the same graph [`LinkGraph::build`]
    /// makes from `records`, as long as every link resolves as it did:
    /// each changed note kept its name and aliases or was added as
    /// [`Self::can_add`] allows, and each removed note was
    /// [`Self::can_remove`]. The changed notes' own links are redone;
    /// derived edges are made again from the records (one pass, no link
    /// resolution), and the typed links and the hierarchy only when those or
    /// the notes' typed links changed.
    pub fn patch<'a>(
        &mut self,
        changed: &[&'a NoteRecord],
        removed: &[NoteRecord],
        records: impl Iterator<Item = &'a NoteRecord> + Clone,
        source: &dyn Fn(&NotePath) -> Option<&'a str>,
        cfg: &LinkConfig,
    ) {
        for rec in removed {
            self.resolver.exact.remove(rec, &lower);
            self.resolver.folded.remove(rec, &fold_str);
        }
        for rec in changed {
            if !self.resolver.has(&rec.path) {
                // Its keys are its own (`can_add`): each name list it starts has one entry, in order.
                self.resolver.exact.add(rec, &lower);
                self.resolver.folded.add(rec, &fold_str);
            }
        }
        let gone: HashSet<&NotePath> = changed.iter().map(|r| &r.path).chain(removed.iter().map(|r| &r.path)).collect();
        let typed_before: Vec<Option<Vec<(String, NotePath)>>> = gone.iter().map(|p| self.typed.get(*p).cloned()).collect();
        #[expect(clippy::iter_over_hash_type, reason = "each removed note is unlinked on its own")]
        for p in &gone {
            if let Some(outs) = self.outgoing.remove(*p) {
                for t in outs {
                    if let Some(v) = self.backlinks.get_mut(&t) {
                        v.retain(|b| &b.from != *p);
                        if v.is_empty() {
                            self.backlinks.remove(&t);
                        }
                    }
                }
            }
            self.typed.remove(*p);
            for k in self.missing_by.remove(*p).unwrap_or_default() {
                if let Some(n) = self.missing.get_mut(&k) {
                    *n -= 1;
                    if *n == 0 {
                        self.missing.remove(&k);
                    }
                }
            }
        }
        self.unresolved.retain(|_, v| {
            v.retain(|q| !gone.contains(q));
            !v.is_empty()
        });
        let mut done: HashSet<&NotePath> = HashSet::new();
        for rec in changed {
            if done.insert(&rec.path) {
                self.add_note(rec, source(&rec.path), true);
            }
        }
        // The lists the changed notes were appended to, back in path order.
        let mut touched: Vec<NotePath> = changed.iter().flat_map(|r| self.outgoing(&r.path).to_vec()).collect();
        touched.sort();
        touched.dedup();
        for t in touched {
            if let Some(v) = self.backlinks.get_mut(&t) {
                v.sort_by(|a, b| a.from.cmp(&b.from));
            }
        }
        let typed_same = gone.iter().zip(typed_before).all(|(p, before)| self.typed.get(*p) == before.as_ref());
        let derived = crate::derived::derive(records.clone(), source, &self.resolver, cfg);
        if !typed_same || derived != self.derived {
            self.relate(records, source, cfg);
        }
    }

    /// The resolver's tables and the unresolved-target counts, as sorted
    /// text (tests compare a patched graph with a rebuild).
    #[cfg(test)]
    pub(crate) fn tables_text(&self) -> String {
        fn sorted<K: std::fmt::Debug + Ord, V: std::fmt::Debug>(m: &HashMap<K, V>) -> Vec<String> {
            let mut v: Vec<(&K, &V)> = m.iter().collect();
            v.sort_by(|a, b| a.0.cmp(b.0));
            v.into_iter().map(|(k, x)| format!("{k:?}={x:?}")).collect()
        }
        let t = |t: &Tables| {
            format!(
                "path {:?}\nname {:?}\nalias {:?}\npath_n {:?}\nalias_n {:?}",
                sorted(&t.by_path),
                sorted(&t.by_name),
                sorted(&t.by_alias),
                sorted(&t.path_n),
                sorted(&t.alias_n)
            )
        };
        let mut by: Vec<String> = self
            .missing_by
            .iter()
            .map(|(p, k)| {
                let mut k = k.clone();
                k.sort();
                format!("{p} {k:?}")
            })
            .collect();
        by.sort();
        format!("{}\n{}\nmissing {:?}\n{by:?}", t(&self.resolver.exact), t(&self.resolver.folded), sorted(&self.missing))
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
