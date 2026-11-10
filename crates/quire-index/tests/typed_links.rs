// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 typed links: seeded property tests (parsing, byte identity on
//! rename, implied reverse edges, cycles, renames and moves keep typed
//! links) and real-world Dataview and Breadcrumbs vault snippets.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use quire_index::{Hier, Index, IndexService, LinkConfig, NoteRecord};
use quire_markdown::extract;
use quire_vault::{LinkRewrite, NotePath, Vault, rewrite_links};

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[self.below(v.len())]
    }
}

/// Field names as written, and as they normalize.
const KEYS: [(&str, &str); 12] = [
    ("up", "up"),
    ("parent", "parent"),
    ("down", "down"),
    ("child", "child"),
    ("same", "same"),
    ("next", "next"),
    ("prev", "prev"),
    ("supports", "supports"),
    ("contradicts", "contradicts"),
    ("Source", "source"),
    ("See Also", "see-also"),
    ("**Up**", "up"),
];
const TARGETS: [&str; 5] = ["Alpha", "Beta Note", "Gamma", "Folder/Delta", "Ünïcode"];

type Expected = Vec<(Option<String>, String)>;

/// A random note and the typed links it holds, in order.
fn note(rng: &mut Rng) -> (String, Expected) {
    let mut src = String::new();
    let mut want: Expected = Vec::new();
    let key = |rng: &mut Rng| KEYS[rng.below(KEYS.len())];
    if rng.below(2) == 0 {
        src.push_str("---\ntitle: A note\n");
        for _ in 0..=rng.below(2) {
            let (k, n) = key(rng);
            let k = k.trim_matches('*');
            let n = n.to_string();
            match rng.below(3) {
                0 => {
                    let t = rng.pick(&TARGETS);
                    let _ = writeln!(src, "{k}: \"[[{t}]]\"");
                    want.push((Some(n), t.into()));
                }
                1 => {
                    let (a, b) = (rng.pick(&TARGETS), rng.pick(&TARGETS));
                    let _ = writeln!(src, "{k}: [\"[[{a}]]\", '[[{b}|alias]]']");
                    want.push((Some(n.clone()), a.into()));
                    want.push((Some(n), b.into()));
                }
                _ => {
                    let _ = writeln!(src, "{k}:");
                    for _ in 0..=rng.below(1) {
                        let t = rng.pick(&TARGETS);
                        let _ = writeln!(src, "  - \"[[{t}#Part]]\"");
                        want.push((Some(n.clone()), t.into()));
                    }
                }
            }
        }
        src.push_str("tags: [x]\n---\n");
    }
    src.push_str("# Title\n");
    for _ in 0..=rng.below(5) {
        let (k, n) = key(rng);
        let n = Some(n.to_string());
        let t = rng.pick(&TARGETS);
        match rng.below(8) {
            0 => {
                let u = rng.pick(&TARGETS);
                let _ = writeln!(src, "{k}:: [[{t}]], [[{u}]]");
                want.push((n.clone(), t.into()));
                want.push((n, u.into()));
            }
            1 => {
                let _ = writeln!(src, "- {k}:: [[{t}]]");
                want.push((n, t.into()));
            }
            2 => {
                let u = rng.pick(&TARGETS);
                let _ = writeln!(src, "Text [{k}:: [[{t}]]] and [[{u}]].");
                want.push((n, t.into()));
                want.push((None, u.into()));
            }
            3 => {
                let _ = writeln!(src, "Hidden key ({k}:: [[{t}|shown]]).");
                want.push((n, t.into()));
            }
            4 => {
                let _ = writeln!(src, "- [ ] task [due:: 2026-12-01] [{k}:: [[{t}]]]");
                want.push((n, t.into()));
            }
            5 => {
                let _ = writeln!(src, "See [[{t}]] here.");
                want.push((None, t.into()));
            }
            6 => {
                let _ = writeln!(src, "```\n{k}:: [[{t}]]\n```");
            }
            _ => {
                let _ = writeln!(src, "Inline `{k}:: [[{t}]]` code.");
            }
        }
    }
    (src, want)
}

fn typed(src: &str) -> Expected {
    extract(src).links.into_iter().filter(quire_markdown::Link::is_internal).map(|l| (l.field, l.target)).collect()
}

#[test]
fn parsing_finds_every_typed_link_in_order() {
    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let (src, want) = note(&mut rng);
        assert_eq!(typed(&src), want, "seed {seed}:\n{src}");
        // CRLF files read the same.
        assert_eq!(typed(&src.replace('\n', "\r\n")), want, "seed {seed} (CRLF)");
    }
}

/// The bytes between links: what a rename must leave as it was.
fn gaps(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    for l in extract(src).links {
        out.push(src[at..l.range.start].to_string());
        at = l.range.end;
    }
    out.push(src[at..].to_string());
    out
}

#[test]
fn renaming_changes_only_link_targets() {
    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0xd1b5_4a32_d192_ed03));
        let (src, want) = note(&mut rng);
        let old = rng.pick(&TARGETS);
        let (old_path, old_stem) = (old, old.rsplit('/').next().unwrap());
        let new_stem = rng.pick(&["Renamed", "Ada's Note", "Ünï 2"]);
        let rw = LinkRewrite { old_stem, old_path, new_stem, resolves: None, new_path: None };
        let renamed = |t: &str| {
            if t == old {
                old.rsplit_once('/').map_or_else(|| new_stem.to_string(), |(d, _)| format!("{d}/{new_stem}"))
            } else {
                t.to_string()
            }
        };
        let Some(out) = rewrite_links(&src, &rw) else {
            assert!(want.iter().all(|(_, t)| t != old), "seed {seed}: nothing rewritten for {old}");
            continue;
        };
        let expect: Expected = want.iter().map(|(f, t)| (f.clone(), renamed(t))).collect();
        assert_eq!(typed(&out), expect, "seed {seed}:\n{out}");
        assert_eq!(gaps(&out), gaps(&src), "seed {seed}: bytes outside links changed");
        // Front matter still parses, with the same keys.
        let keys = |s: &str| quire_markdown::frontmatter::parse(s).map(|f| f.fields.keys().cloned().collect::<Vec<_>>());
        assert_eq!(keys(&out), keys(&src), "seed {seed}");
    }
}

/// A vault of `n` notes with random typed edges between them (cycles included).
fn random_vault(rng: &mut Rng, n: usize) -> Vec<(String, String)> {
    let mut notes: Vec<(String, String)> = (0..n).map(|i| (format!("N{i}.md"), format!("# N{i}\n"))).collect();
    for _ in 0..n * 2 {
        let a = rng.below(n);
        let b = rng.below(n);
        let (k, _) = KEYS[rng.below(KEYS.len())];
        let line = if rng.below(2) == 0 { format!("{k}:: [[N{b}]]\n") } else { format!("[{k}:: [[N{b}]]]\n") };
        notes[a].1.push_str(&line);
    }
    notes
}

fn index_of(notes: &[(String, String)], cfg: LinkConfig) -> Index {
    let mut ix = Index::new();
    for (p, s) in notes {
        ix.insert_record(NoteRecord::from_source(NotePath::new(p.as_str()), s), s.clone());
    }
    ix.set_link_config(cfg);
    ix.refresh();
    ix
}

#[test]
fn hierarchy_edges_imply_their_reverse() {
    for seed in 1..=60u64 {
        let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
        let n = 8 + rng.below(10);
        let notes = random_vault(&mut rng, n);
        let ix = index_of(&notes, LinkConfig::default());
        let rel = ix.relations();
        for (p, _) in &notes {
            let a = NotePath::new(p.as_str());
            for e in rel.of(&a).iter().filter(|e| !e.implied) {
                let Some(h) = e.hier else { continue };
                assert!(
                    rel.of(&e.to).iter().any(|r| r.hier == Some(h.reverse()) && r.to == a),
                    "seed {seed}: {a} {} {} has no reverse",
                    e.field,
                    e.to
                );
            }
            // Nothing implied that no note wrote.
            for e in rel.of(&a).iter().filter(|e| e.implied) {
                let h = e.hier.unwrap();
                assert!(rel.of(&e.to).iter().any(|r| !r.implied && r.hier == Some(h.reverse()) && r.to == a), "seed {seed}");
            }
        }
        let off = index_of(&notes, LinkConfig { implied: false, ..LinkConfig::default() });
        assert!(notes.iter().all(|(p, _)| off.relations().of(&NotePath::new(p.as_str())).iter().all(|e| !e.implied)));
    }
}

#[test]
fn cycles_never_hang() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut walked = 0;
        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 7);
            let mut notes = random_vault(&mut rng, 30);
            // A ring of parents and one of next links, on top of the random edges.
            for (i, note) in notes.iter_mut().enumerate() {
                let _ = writeln!(note.1, "up:: [[N{}]]\nnext:: [[N{}]]", (i + 1) % 30, (i + 7) % 30);
            }
            let ix = index_of(&notes, LinkConfig::default());
            let rel = ix.relations();
            for (p, _) in &notes {
                let p = NotePath::new(p.as_str());
                for h in Hier::ALL {
                    walked += rel.walk(&p, h, 10_000).len();
                }
                let trails = rel.trails(&p, 8, 10_000);
                assert!(trails.len() <= 8);
                for t in &trails {
                    assert_eq!(t.last(), Some(&p));
                    assert_eq!(t.iter().collect::<BTreeSet<_>>().len(), t.len(), "a trail never repeats a note");
                }
                let _ = rel.siblings(&p);
                let _ = (rel.step(&p, true), rel.step(&p, false));
            }
        }
        tx.send(walked).unwrap();
    });
    let walked = rx.recv_timeout(Duration::from_mins(1)).expect("walks over cyclic hierarchies finish");
    assert!(walked > 0);
}

/// Every explicit typed edge as (from, field, to).
fn edges(ix: &Index) -> BTreeSet<(String, String, String)> {
    ix.paths()
        .iter()
        .flat_map(|p| {
            ix.relations()
                .of(p)
                .iter()
                .filter(|e| !e.implied)
                .map(|e| (p.to_string(), e.field.clone(), e.to.to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn index_vault(v: &Vault) -> Index {
    let mut ix = Index::new();
    for p in v.notes().unwrap() {
        let s = v.read(&p).unwrap();
        ix.insert_record(NoteRecord::from_source(p, &s), s);
    }
    ix.refresh();
    ix
}

#[test]
fn rename_and_move_keep_typed_links() {
    for seed in 1..=30u64 {
        let mut rng = Rng(seed.wrapping_mul(0xd1b5_4a32_d192_ed03) ^ 3);
        let mut notes = random_vault(&mut rng, 10);
        // Front-matter links too, by name and by path.
        notes.push(("Folder/Hub.md".into(), "---\nup: \"[[N1]]\"\nrelated: [\"[[N2]]\"]\n---\n# Hub\n".into()));
        notes[3].1.push_str("parent:: [[Folder/Hub]]\n");
        notes[4].1.insert_str(0, "---\nsource: \"[[Hub]]\"\n---\n");
        let files: Vec<(&str, &str)> = notes.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();
        let v = Vault::in_memory(&files);
        let before = edges(&index_vault(&v));
        let victim = if rng.below(2) == 0 { NotePath::new("Folder/Hub.md") } else { NotePath::new(format!("N{}.md", rng.below(10))) };
        let moved = rng.below(2) == 0;
        let report = if moved { v.move_note(&victim, "Elsewhere").unwrap() } else { v.rename_note(&victim, "Renamed Note").unwrap() };
        let new = report.new_path.to_string();
        let map = |p: &str| if p == victim.as_str() { new.clone() } else { p.to_string() };
        let want: BTreeSet<_> = before.iter().map(|(a, f, b)| (map(a), f.clone(), map(b))).collect();
        assert_eq!(edges(&index_vault(&v)), want, "seed {seed}: {} {victim} → {new}", if moved { "move" } else { "rename" });
    }
}

/// The compatibility vault with its `vault.toml`, in a temporary folder.
fn fixture_vault() -> (tempfile::TempDir, Vault) {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &to.join(e.file_name()));
            } else {
                std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    copy(&here.join("links"), &root);
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::copy(here.join("links-vault.toml"), root.join(".quire/shared/vault.toml")).unwrap();
    let v = Vault::open_local(&root).unwrap();
    (dir, v)
}

fn p(s: &str) -> NotePath {
    NotePath::new(s)
}

#[test]
fn dataview_and_breadcrumbs_vaults_read_as_written() {
    let (_dir, v) = fixture_vault();
    let svc = IndexService::spawn();
    svc.load(v);
    svc.flush();
    let ix = svc.read();
    assert_eq!(ix.link_config().kind("broader"), Some(Hier::Up), "the vault's [links] section is read on load");
    let rel = ix.relations();
    let web = p("Projects/Website Redesign.md");
    let fields: BTreeSet<(String, String)> =
        rel.of(&web).iter().filter(|e| !e.implied).map(|e| (e.field.clone(), e.to.to_string())).collect();
    let want: BTreeSet<(String, String)> = [
        ("up", "Projects.md"),
        ("related", "Design System.md"),
        ("related", "Q4 Goals.md"),
        ("owner", "People/Ada Lovelace.md"),
        ("depends-on", "Hosting Migration.md"),
        ("source", "Meeting 2026-10-01.md"),
        ("blocked-by", "Design System.md"),
        ("contradicts", "Old Roadmap.md"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
    .collect();
    assert_eq!(fields, want);
    // The plain mention of Q4 Goals is still an (untyped) backlink.
    assert!(ix.backlinks(&p("Q4 Goals.md")).iter().any(|b| b.from == web && b.link.field.is_none()));
    assert_eq!(rel.children(&p("Projects.md")), [&web]);
    assert_eq!(rel.incoming(&p("Website Redesign.md")).len(), 0, "not a path");
    assert!(rel.incoming(&web).iter().any(|e| e.field == "supports" && e.to == p("Design System.md")));

    // Breadcrumbs: front-matter lists, inline fields, a custom `broader`, next/prev chains.
    let calc = p("Maths/Calculus.md");
    let mut kids: Vec<String> = rel.children(&calc).into_iter().map(ToString::to_string).collect();
    kids.sort();
    assert_eq!(kids, ["Maths/Derivatives.md", "Maths/Integrals.md", "Maths/Limits.md", "Maths/Series.md"]);
    assert_eq!(rel.trails(&p("Maths/Limits.md"), 4, 10), [vec![p("Maths.md"), calc, p("Maths/Limits.md")]]);
    assert_eq!(rel.step(&p("Maths/Limits.md"), true), Some(p("Maths/Derivatives.md")));
    assert_eq!(rel.step(&p("Maths/Integrals.md"), false), Some(p("Maths/Derivatives.md")), "implied prev");
    assert_eq!(rel.related(&p("Physics.md"), Hier::Same), [&p("Maths.md")]);
    assert!(rel.has_hierarchy());
    // Dataview queries in the notes translate and run over typed links.
    let t = quire_index::views::dataview::translate("LIST WHERE up = [[Projects]]");
    let r = quire_index::views::run(&quire_index::views::parse(&t.spec).unwrap(), &ix, "2026-11-10", None);
    assert_eq!(r.rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), [web]);
}
