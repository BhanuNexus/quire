// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The link graph as something to look at: which notes to show, and a
//! force-directed layout that settles (and then stops costing anything).
//!
//! Notes are dots, links are lines. Linked notes pull together, every note
//! pushes its neighbours away, and a gentle pull to the middle keeps
//! unlinked notes from drifting off. Repulsion only counts notes within a
//! few cells of a spatial grid, so a step costs about O(n) and thousands of
//! notes stay interactive.

use std::collections::{BTreeSet, HashMap, VecDeque};

use quire_index::Index;
use quire_vault::NotePath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub path: NotePath,
    pub title: String,
    /// Links in and out (each pair counted once).
    pub degree: usize,
    /// Steps from the focus note in a local graph (0 = the note itself).
    pub depth: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    /// Undirected, each pair once, `a < b`.
    pub edges: Vec<(usize, usize)>,
    /// 0.12: each edge's type (the field of a typed link between the two
    /// notes, either way), parallel to `edges`; `None` for plain links.
    pub edge_types: Vec<Option<String>>,
    /// The types on edges, most used first.
    pub types: Vec<String>,
}

/// What to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every note (optionally leaving out notes with no links).
    All { orphans: bool },
    /// Notes within `depth` links of `focus`.
    Around { focus: NotePath, depth: usize },
}

impl Graph {
    pub fn build(ix: &Index, scope: &Scope) -> Self {
        // Undirected neighbours from resolved links, skipping self-links.
        let mut adj: HashMap<NotePath, BTreeSet<NotePath>> = HashMap::new();
        for p in ix.paths() {
            adj.entry(p.clone()).or_default();
            for t in ix.graph().outgoing(&p) {
                if *t != p && ix.get(t).is_some() {
                    adj.entry(p.clone()).or_default().insert(t.clone());
                    adj.entry(t.clone()).or_default().insert(p.clone());
                }
            }
        }
        let mut depth: HashMap<NotePath, usize> = HashMap::new();
        match scope {
            Scope::All { orphans } => {
                for (p, n) in &adj {
                    if *orphans || !n.is_empty() {
                        depth.insert(p.clone(), 0);
                    }
                }
            }
            Scope::Around { focus, depth: max } => {
                if adj.contains_key(focus) {
                    let mut queue = VecDeque::from([(focus.clone(), 0)]);
                    depth.insert(focus.clone(), 0);
                    while let Some((p, d)) = queue.pop_front() {
                        if d == *max {
                            continue;
                        }
                        for n in &adj[&p] {
                            if !depth.contains_key(n) {
                                depth.insert(n.clone(), d + 1);
                                queue.push_back((n.clone(), d + 1));
                            }
                        }
                    }
                }
            }
        }
        let mut paths: Vec<&NotePath> = depth.keys().collect();
        paths.sort();
        let index: HashMap<&NotePath, usize> = paths.iter().enumerate().map(|(i, p)| (*p, i)).collect();
        let mut edges = BTreeSet::new();
        for (i, p) in paths.iter().enumerate() {
            for n in &adj[*p] {
                if let Some(&j) = index.get(n) {
                    edges.insert((i.min(j), i.max(j)));
                }
            }
        }
        let mut degree = vec![0; paths.len()];
        for &(a, b) in &edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let nodes = paths
            .iter()
            .enumerate()
            .map(|(i, p)| GraphNode {
                path: (*p).clone(),
                title: ix.get(p).map_or_else(|| p.stem().to_string(), |r| r.title.clone()),
                degree: degree[i],
                depth: depth[*p],
            })
            .collect();
        let edges: Vec<(usize, usize)> = edges.into_iter().collect();
        // A typed link either way names the edge (the note's own link first).
        let rel = ix.relations();
        let typed = |a: &NotePath, b: &NotePath| rel.of(a).iter().find(|e| !e.implied && e.to == *b).map(|e| e.field.clone());
        let edge_types: Vec<Option<String>> =
            edges.iter().map(|&(a, b)| typed(paths[a], paths[b]).or_else(|| typed(paths[b], paths[a]))).collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for t in edge_types.iter().flatten() {
            *counts.entry(t.as_str()).or_default() += 1;
        }
        let mut types: Vec<(&str, usize)> = counts.into_iter().collect();
        types.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let types = types.into_iter().map(|(t, _)| t.to_string()).collect();
        Self { nodes, edges, edge_types, types }
    }

    /// Indices of the nodes linked to `i`.
    pub fn neighbours(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.edges.iter().filter_map(move |&(a, b)| {
            if a == i {
                Some(b)
            } else if b == i {
                Some(a)
            } else {
                None
            }
        })
    }
}

/// Positions for a [`Graph`], improved a step at a time.
#[derive(Debug, Clone)]
pub struct Layout {
    pub pos: Vec<[f32; 2]>,
    vel: Vec<[f32; 2]>,
    /// Ideal edge length.
    k: f32,
    /// Shrinks each step so the layout settles.
    heat: f32,
    /// A node being dragged stays where the pointer puts it.
    pub pinned: Option<usize>,
}

/// A stable pseudo-random start, so the same vault opens to the same picture.
fn seed(p: &NotePath) -> [f32; 2] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_str().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let x = (h & 0xffff) as f32 / 65535.0 - 0.5;
    let y = ((h >> 16) & 0xffff) as f32 / 65535.0 - 0.5;
    [x, y]
}

impl Layout {
    pub fn new(g: &Graph) -> Self {
        let k = 60.0;
        let spread = k * (g.nodes.len() as f32).sqrt().max(1.0) * 1.2;
        let pos = g.nodes.iter().map(|n| seed(&n.path)).map(|[x, y]| [x * spread, y * spread]).collect();
        Self { pos, vel: vec![[0.0; 2]; g.nodes.len()], k, heat: 1.0, pinned: None }
    }

    /// Keep positions of notes already laid out when the graph changes.
    pub fn carry_over(&mut self, old_graph: &Graph, old: &Self, g: &Graph) {
        let at: HashMap<&NotePath, [f32; 2]> = old_graph.nodes.iter().zip(&old.pos).map(|(n, p)| (&n.path, *p)).collect();
        for (i, n) in g.nodes.iter().enumerate() {
            if let Some(p) = at.get(&n.path) {
                self.pos[i] = *p;
            }
        }
        self.heat = 0.5;
    }

    /// Settled: nothing moves enough to see.
    pub fn settled(&self) -> bool {
        self.heat < 0.02
    }

    /// Wake the layout up (after a drag, or a new graph).
    pub fn reheat(&mut self, to: f32) {
        self.heat = self.heat.max(to);
    }

    /// One step of the simulation. Returns how far the furthest node moved.
    pub fn step(&mut self, g: &Graph) -> f32 {
        let n = g.nodes.len();
        if n == 0 {
            return 0.0;
        }
        let k = self.k;
        let mut force = vec![[0.0f32; 2]; n];
        // Repulsion between nearby notes, via a grid with cells of 2k.
        let cell = 2.0 * k;
        let key = |p: [f32; 2]| ((p[0] / cell).floor() as i32, (p[1] / cell).floor() as i32);
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (i, p) in self.pos.iter().enumerate() {
            grid.entry(key(*p)).or_default().push(i);
        }
        for (i, p) in self.pos.iter().enumerate() {
            let (cx, cy) = key(*p);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    let Some(bucket) = grid.get(&(cx + dx, cy + dy)) else { continue };
                    for &j in bucket {
                        if j == i {
                            continue;
                        }
                        let q = self.pos[j];
                        let (mut ddx, mut ddy) = (p[0] - q[0], p[1] - q[1]);
                        let mut d2 = ddx * ddx + ddy * ddy;
                        if d2 < 0.01 {
                            // Two notes on one spot: nudge them apart deterministically.
                            ddx = if i < j { 0.1 } else { -0.1 };
                            ddy = 0.05;
                            d2 = 0.0125;
                        }
                        // Only within one cell, fading to zero at the edge, so
                        // the grid itself leaves no pattern in the picture.
                        if d2 >= cell * cell {
                            continue;
                        }
                        let f = k * k / d2 * (1.0 - d2 / (cell * cell));
                        force[i][0] += ddx * f;
                        force[i][1] += ddy * f;
                    }
                }
            }
        }
        // Links pull their ends together.
        for &(a, b) in &g.edges {
            let (dx, dy) = (self.pos[b][0] - self.pos[a][0], self.pos[b][1] - self.pos[a][1]);
            let d = dx.hypot(dy).max(0.01);
            let f = d / k;
            force[a][0] += dx * f;
            force[a][1] += dy * f;
            force[b][0] -= dx * f;
            force[b][1] -= dy * f;
        }
        // A gentle pull to the middle, stronger for unlinked notes.
        for (i, p) in self.pos.iter().enumerate() {
            let pull = if g.nodes[i].degree == 0 { 0.06 } else { 0.02 };
            force[i][0] -= p[0] * pull;
            force[i][1] -= p[1] * pull;
        }
        let max_step = k * self.heat;
        let mut moved = 0.0f32;
        for (i, ((v, p), f)) in self.vel.iter_mut().zip(self.pos.iter_mut()).zip(&force).enumerate() {
            if self.pinned == Some(i) {
                *v = [0.0; 2];
                continue;
            }
            *v = [(v[0] + f[0] * 0.02) * 0.55, (v[1] + f[1] * 0.02) * 0.55];
            let len = v[0].hypot(v[1]);
            let scale = if len > max_step { max_step / len } else { 1.0 };
            p[0] += v[0] * scale;
            p[1] += v[1] * scale;
            moved = moved.max(len * scale);
        }
        self.heat *= 0.985;
        moved
    }

    /// The box around every note: (min, max).
    pub fn bounds(&self) -> ([f32; 2], [f32; 2]) {
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for p in &self.pos {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        if lo[0].is_infinite() { ([0.0; 2], [0.0; 2]) } else { (lo, hi) }
    }
}

impl crate::workspace::Workspace {
    /// The graph to draw: everything, or the notes around the open one.
    pub fn link_graph(&self, scope: &Scope) -> Graph {
        let mut g = Graph::build(&self.index().read(), scope);
        // Titles only with Settings → Editing → "Show note titles"; file names otherwise.
        if !self.settings.editing.display_titles {
            for n in &mut g.nodes {
                n.title = n.path.stem().to_string();
            }
        }
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_index::NoteRecord;

    fn index(files: &[(&str, &str)]) -> Index {
        let mut ix = Index::new();
        for (p, s) in files {
            ix.insert_record(NoteRecord::from_source(NotePath::new(*p), s), (*s).to_string());
        }
        ix.refresh();
        ix
    }

    fn files() -> Vec<(&'static str, &'static str)> {
        vec![
            ("A.md", "[[B]] [[C]] [[Missing]] [[A]]"),
            ("B.md", "[[C]]"),
            ("C.md", "[[D]]"),
            ("D.md", "end"),
            ("Lonely.md", "nobody links here"),
        ]
    }

    #[test]
    fn builds_undirected_edges_and_scopes() {
        let ix = index(&files());
        let all = Graph::build(&ix, &Scope::All { orphans: true });
        assert_eq!(all.nodes.len(), 5);
        let name = |i: usize| all.nodes[i].path.stem().to_string();
        let edges: Vec<(String, String)> = all.edges.iter().map(|&(a, b)| (name(a), name(b))).collect();
        assert_eq!(edges, [("A", "B"), ("A", "C"), ("B", "C"), ("C", "D")].map(|(a, b)| (a.to_string(), b.to_string())));
        assert_eq!(all.nodes.iter().find(|n| n.path.stem() == "C").unwrap().degree, 3);

        let linked = Graph::build(&ix, &Scope::All { orphans: false });
        assert!(!linked.nodes.iter().any(|n| n.path.stem() == "Lonely"));

        let near = Graph::build(&ix, &Scope::Around { focus: NotePath::new("A.md"), depth: 1 });
        let mut names: Vec<&str> = near.nodes.iter().map(|n| n.path.stem()).collect();
        names.sort_unstable();
        assert_eq!(names, ["A", "B", "C"]);
        assert_eq!(near.nodes.iter().find(|n| n.path.stem() == "A").unwrap().depth, 0);
        let two = Graph::build(&ix, &Scope::Around { focus: NotePath::new("A.md"), depth: 2 });
        assert_eq!(two.nodes.len(), 4);
        assert!(all.edge_types.iter().all(Option::is_none) && all.types.is_empty());
    }

    #[test]
    fn edges_carry_their_link_type() {
        let ix = index(&[("A.md", "up:: [[B]]\n[[C]]"), ("B.md", "[supports:: [[C]]]"), ("C.md", "up:: [[B]]\n[[A]]")]);
        let g = Graph::build(&ix, &Scope::All { orphans: true });
        let name = |i: usize| g.nodes[i].path.stem().to_string();
        let typed: Vec<(String, String, Option<&str>)> =
            g.edges.iter().zip(&g.edge_types).map(|(&(a, b), t)| (name(a), name(b), t.as_deref())).collect();
        assert_eq!(
            typed,
            [("A".into(), "B".into(), Some("up")), ("A".into(), "C".into(), None), ("B".into(), "C".into(), Some("supports"))]
        );
        assert_eq!(g.types, ["supports", "up"]);
    }

    #[test]
    fn layout_settles_with_linked_notes_closer_than_unlinked() {
        let ix = index(&files());
        let g = Graph::build(&ix, &Scope::All { orphans: true });
        let mut l = Layout::new(&g);
        let mut steps = 0;
        while !l.settled() && steps < 2000 {
            l.step(&g);
            steps += 1;
        }
        assert!(l.settled(), "settles in {steps} steps");
        let at = |s: &str| l.pos[g.nodes.iter().position(|n| n.path.stem() == s).unwrap()];
        let dist = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
        assert!(dist(at("C"), at("D")) < dist(at("D"), at("Lonely")));
        assert!(l.pos.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
        // Same vault, same picture.
        let mut again = Layout::new(&g);
        for _ in 0..steps {
            again.step(&g);
        }
        assert_eq!(again.pos, l.pos);
    }

    #[test]
    fn thousands_of_notes_step_quickly() {
        let files: Vec<(String, String)> =
            (0..3000).map(|i| (format!("N{i}.md"), format!("[[N{}]] [[N{}]]", (i * 7 + 1) % 3000, (i + 1) % 3000))).collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let g = Graph::build(&index(&refs), &Scope::All { orphans: true });
        let mut l = Layout::new(&g);
        let t = std::time::Instant::now();
        for _ in 0..10 {
            l.step(&g);
        }
        let per_step = t.elapsed() / 10;
        assert!(per_step < std::time::Duration::from_millis(if cfg!(debug_assertions) { 400 } else { 40 }), "{per_step:?} per step");
    }
}
