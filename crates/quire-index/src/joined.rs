// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Two indexes read as one (0.12): the vault index and the in-memory index
//! of the notes of unlocked locked folders (docs/design/0.12-loose.md §2).
//!
//! The vault index never holds a locked note, so everything built on it
//! (agents, MCP, jobs, caches) never sees one. The desktop's own features
//! (tasks, agenda, views, graph, backlinks, reminders) read [`Joined`]
//! instead: the vault index and, while a folder is unlocked, its notes.
//! Links between the two sides are resolved here ([`CrossLinks`]), since
//! each index only resolves links to its own notes.

use std::collections::HashSet;

use quire_markdown::Task;
use quire_vault::NotePath;

use crate::graph::Backlink;
use crate::index::Index;
use crate::record::NoteRecord;

/// The line around a link (for a backlink's context).
fn line_around(src: &str, range: &std::ops::Range<usize>) -> String {
    let (s, e) = (range.start.min(src.len()), range.end.min(src.len()));
    let start = src.get(..s).and_then(|h| h.rfind('\n')).map_or(0, |i| i + 1);
    let end = src.get(e..).and_then(|t| t.find('\n')).map_or(src.len(), |i| e + i);
    src.get(start..end).unwrap_or_default().trim().to_string()
}

/// Links from one side to the other: from a plain note to a note of an
/// unlocked folder, and back. Built once per pair of index versions.
#[derive(Debug, Default, Clone)]
pub struct CrossLinks {
    /// (target, the backlink), in the order found.
    links: Vec<(NotePath, Backlink)>,
}

impl CrossLinks {
    /// The links between `main` and `extra` that neither resolves alone.
    pub fn build(main: &Index, extra: &Index) -> Self {
        let mut links = Vec::new();
        let mut add = |from: &NoteRecord, src: Option<&str>, own: &Index, other: &Index| {
            let mut seen = HashSet::new();
            for link in from.links.iter().filter(|l| l.is_internal() && !l.target.is_empty()) {
                if own.resolve(&link.target, Some(&from.path)).is_some() {
                    continue;
                }
                let Some(to) = other.resolve(&link.target, Some(&from.path)) else { continue };
                if to == from.path || !seen.insert((to.clone(), link.range.start)) {
                    continue;
                }
                let context = src.map_or_else(|| link.display.clone(), |s| line_around(s, &link.range));
                links.push((to, Backlink { from: from.path.clone(), link: link.clone(), context }));
            }
        };
        // Plain notes: only those with links their own index can't resolve.
        let mut linking: Vec<&NotePath> = main.graph().unresolved().into_iter().flat_map(|(_, f)| f.iter()).collect();
        linking.sort();
        linking.dedup();
        for p in linking {
            if let Some(r) = main.get(p) {
                add(r, main.source(p), main, extra);
            }
        }
        for r in extra.records() {
            add(r, extra.source(&r.path), extra, main);
        }
        Self { links }
    }

    pub const fn len(&self) -> usize {
        self.links.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.links.is_empty()
    }
}

/// The vault index and, while a locked folder is unlocked, the in-memory
/// index of its notes, read as one. Paths never collide: a note is in a
/// locked folder or it isn't.
#[derive(Clone, Copy)]
pub struct Joined<'a> {
    main: &'a Index,
    extra: Option<(&'a Index, &'a CrossLinks)>,
}

impl<'a> Joined<'a> {
    /// Only the vault index.
    pub const fn main_only(main: &'a Index) -> Self {
        Self { main, extra: None }
    }

    /// The vault index with the notes of unlocked folders and the links between them.
    pub const fn new(main: &'a Index, extra: &'a Index, cross: &'a CrossLinks) -> Self {
        Self { main, extra: Some((extra, cross)) }
    }

    /// The vault index alone.
    pub const fn main(&self) -> &'a Index {
        self.main
    }

    /// The notes of unlocked folders, if any.
    pub fn extra(&self) -> Option<&'a Index> {
        self.extra.map(|(e, _)| e)
    }

    /// Whether `p` is a note of an unlocked folder.
    pub fn is_extra(&self, p: &NotePath) -> bool {
        self.extra().is_some_and(|e| e.get(p).is_some())
    }

    /// Every note of both sides.
    pub fn records(&self) -> impl Iterator<Item = &'a NoteRecord> + Clone + use<'a> {
        self.main.records().chain(self.extra().into_iter().flat_map(Index::records))
    }

    pub fn len(&self) -> usize {
        self.main.len() + self.extra().map_or(0, Index::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, p: &NotePath) -> Option<&'a NoteRecord> {
        self.main.get(p).or_else(|| self.extra()?.get(p))
    }

    pub fn source(&self, p: &NotePath) -> Option<&'a str> {
        self.main.source(p).or_else(|| self.extra()?.source(p))
    }

    pub fn modified(&self, p: &NotePath) -> Option<i64> {
        self.main.modified(p).or_else(|| self.extra()?.modified(p))
    }

    /// Paths of both sides, sorted.
    pub fn paths(&self) -> Vec<NotePath> {
        let mut v = self.main.paths();
        if let Some(e) = self.extra() {
            v.extend(e.paths());
            v.sort();
        }
        v
    }

    /// A link target as a note of either side: the side of `from` first.
    pub fn resolve(&self, target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        let Some(extra) = self.extra() else { return self.main.resolve(target, from) };
        let (first, second) = if from.is_some_and(|f| extra.get(f).is_some()) { (extra, self.main) } else { (self.main, extra) };
        first.resolve(target, from).or_else(|| second.resolve(target, from))
    }

    /// Tasks of both sides, open ones first (as [`Index::tasks`]).
    pub fn tasks(&self, include_done: bool) -> Vec<(NotePath, Task)> {
        let mut v = self.main.tasks(include_done);
        if let Some(e) = self.extra() {
            v.extend(e.tasks(include_done));
            v.sort_by(|a, b| a.1.done.cmp(&b.1.done).then_with(|| a.0.cmp(&b.0)).then(a.1.offset.cmp(&b.1.offset)));
        }
        v
    }

    /// Notes linking to `p`, from either side.
    pub fn backlinks(&self, p: &NotePath) -> Vec<Backlink> {
        let mut v = self.main.backlinks(p).to_vec();
        if let Some((e, cross)) = self.extra {
            v.extend_from_slice(e.backlinks(p));
            v.extend(cross.links.iter().filter(|(to, _)| to == p).map(|(_, b)| b.clone()));
        }
        v
    }

    /// Notes `p` links to, on either side, each once.
    pub fn outgoing(&self, p: &NotePath) -> Vec<NotePath> {
        let mut v = self.main.graph().outgoing(p).to_vec();
        if let Some((e, cross)) = self.extra {
            v.extend_from_slice(e.graph().outgoing(p));
            for (to, b) in &cross.links {
                if b.from == *p && !v.contains(to) {
                    v.push(to.clone());
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[(&str, &str)]) -> Index {
        let mut ix = Index::new();
        for (p, s) in files {
            ix.insert_record(NoteRecord::from_source(NotePath::new(*p), s), (*s).to_string());
        }
        ix.refresh();
        ix
    }

    #[test]
    fn links_across_the_two_sides_resolve() {
        let main = index(&[("Plain.md", "# Plain\nSee [[Day]].\n- [ ] plain task\n"), ("Other.md", "# Other\n[[Plain]]\n")]);
        let extra =
            index(&[("Journal/Day.md", "# Day\nBack to [[Plain]] and [[Old]].\n- [ ] locked task\n"), ("Journal/Old.md", "# Old\n")]);
        let cross = CrossLinks::build(&main, &extra);
        assert_eq!(cross.len(), 2);
        let j = Joined::new(&main, &extra, &cross);
        let from = |v: Vec<Backlink>| {
            let mut f: Vec<String> = v.into_iter().map(|b| b.from.as_str().to_string()).collect();
            f.sort();
            f
        };
        assert_eq!(from(j.backlinks(&NotePath::new("Plain.md"))), ["Journal/Day.md", "Other.md"]);
        assert_eq!(from(j.backlinks(&NotePath::new("Journal/Day.md"))), ["Plain.md"]);
        assert_eq!(j.backlinks(&NotePath::new("Journal/Day.md"))[0].context, "See [[Day]].");
        assert_eq!(from(j.backlinks(&NotePath::new("Journal/Old.md"))), ["Journal/Day.md"]);
        let mut out = j.outgoing(&NotePath::new("Journal/Day.md"));
        out.sort();
        assert_eq!(out, [NotePath::new("Journal/Old.md"), NotePath::new("Plain.md")]);
        assert_eq!(j.outgoing(&NotePath::new("Plain.md")), [NotePath::new("Journal/Day.md")]);
        assert_eq!(j.tasks(false).len(), 2);
        assert_eq!(j.len(), 4);
        assert_eq!(j.paths().len(), 4);
        assert!(j.is_extra(&NotePath::new("Journal/Old.md")) && !j.is_extra(&NotePath::new("Plain.md")));
        assert_eq!(j.resolve("Day", None), Some(NotePath::new("Journal/Day.md")));
        // The vault index alone knows nothing of the locked side.
        let m = Joined::main_only(&main);
        assert!(m.backlinks(&NotePath::new("Plain.md")).iter().all(|b| !b.from.as_str().starts_with("Journal/")));
        assert_eq!(m.tasks(false).len(), 1);
        assert!(m.get(&NotePath::new("Journal/Day.md")).is_none());
    }
}
