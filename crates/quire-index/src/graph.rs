// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Link graph: resolution, backlinks, unresolved links, orphans.

use std::collections::{HashMap, HashSet};

use quire_markdown::Link;
use quire_vault::NotePath;

use crate::record::NoteRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    pub from: NotePath,
    pub link: Link,
    /// The line containing the link, for context.
    pub context: String,
}

/// Name → path resolution tables built from the current records.
#[derive(Debug, Default, Clone)]
#[allow(clippy::struct_field_names, reason = "by_path / by_name / by_alias read as lookup tables")]
pub struct Resolver {
    by_path: HashMap<String, NotePath>,
    by_name: HashMap<String, Vec<NotePath>>,
    by_alias: HashMap<String, NotePath>,
}

impl Resolver {
    pub fn build<'a>(records: impl Iterator<Item = &'a NoteRecord>) -> Self {
        let mut r = Self::default();
        for rec in records {
            r.by_path.insert(rec.path.without_ext().to_lowercase(), rec.path.clone());
            r.by_name.entry(rec.name.to_lowercase()).or_default().push(rec.path.clone());
            for a in &rec.aliases {
                r.by_alias.entry(a.to_lowercase()).or_insert_with(|| rec.path.clone());
            }
        }
        for v in r.by_name.values_mut() {
            v.sort_by_key(|p| (p.as_str().len(), p.as_str().to_string()));
        }
        r
    }

    /// Same rules as `Vault::resolve`, plus aliases.
    pub fn resolve(&self, target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        let t = target.trim().trim_end_matches(".md").to_lowercase();
        if t.is_empty() {
            return from.cloned();
        }
        if let Some(p) = self.by_path.get(&t) {
            return Some(p.clone());
        }
        let name = t.rsplit('/').next().unwrap_or(&t);
        if let Some(cands) = self.by_name.get(name) {
            let here = from.map(NotePath::folder);
            let mut best: Option<&NotePath> = None;
            for c in cands.iter().filter(|c| !t.contains('/') || c.without_ext().to_lowercase().ends_with(&t)) {
                if Some(c.folder()) == here {
                    return Some(c.clone());
                }
                best = best.or(Some(c));
            }
            if let Some(b) = best {
                return Some(b.clone());
            }
        }
        self.by_alias.get(&t).cloned()
    }
}

#[derive(Debug, Default, Clone)]
pub struct LinkGraph {
    pub resolver: Resolver,
    outgoing: HashMap<NotePath, Vec<NotePath>>,
    backlinks: HashMap<NotePath, Vec<Backlink>>,
    unresolved: HashMap<String, Vec<NotePath>>,
}

fn line_around(src_plain_free: &str, link: &Link) -> String {
    let start = src_plain_free[..link.range.start.min(src_plain_free.len())].rfind('\n').map_or(0, |i| i + 1);
    let end = src_plain_free[link.range.end.min(src_plain_free.len())..].find('\n').map_or(src_plain_free.len(), |i| link.range.end + i);
    src_plain_free[start..end].trim().to_string()
}

impl LinkGraph {
    /// `sources` provides raw text for backlink context lines.
    pub fn build<'a>(records: impl Iterator<Item = &'a NoteRecord> + Clone, source: &dyn Fn(&NotePath) -> Option<&'a str>) -> Self {
        let resolver = Resolver::build(records.clone());
        let mut g = Self { resolver, ..Default::default() };
        for rec in records {
            let src = source(&rec.path);
            let mut seen = HashSet::new();
            for link in rec.links.iter().filter(|l| l.is_internal()) {
                if link.target.is_empty() {
                    continue; // same-note heading link
                }
                match g.resolver.resolve(&link.target, Some(&rec.path)) {
                    Some(to) => {
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
        g
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

    pub fn edges(&self) -> impl Iterator<Item = (&NotePath, &NotePath)> {
        self.outgoing.iter().flat_map(|(a, bs)| bs.iter().map(move |b| (a, b)))
    }
}
