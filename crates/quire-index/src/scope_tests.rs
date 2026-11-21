// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Agent views (`docs/design/0.14-agent-index.md` §2 G1, G3, G9; §6): a
//! view ranks exactly as an index of only its notes, its patched
//! statistics equal a rebuild, and the registry keeps at most 32.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use quire_vault::NotePath;

use super::*;
use crate::attach::{AttachmentKind, AttachmentText, TextSource};
use crate::sections::{SearchFilters, SearchKind};
use crate::{IndexService, NoteRecord};

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

/// A small xorshift: tests are deterministic (P10).
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % n as u64).expect("small")
    }
}

const WORDS: &[&str] = &[
    "rope", "ropes", "tree", "trees", "editor", "edit", "buffer", "undo", "redo", "cursor", "merge", "sync", "note", "index", "query",
    "vault", "plain", "file", "graph", "link", "tag", "fast", "local", "quick", "search", "rank", "score", "word", "term", "page", "café",
    "cafe", "zebra", "zeal", "zest", "apple", "apply", "april", "plan", "plane",
];

fn pdf(path: &str, pages: Vec<String>) -> AttachmentText {
    AttachmentText { path: np(path), kind: AttachmentKind::Pdf, pages, source_hash: "ab".into(), by: TextSource::Embedded }
}

/// An index of `notes` and `pdfs`.
fn index_of(notes: &BTreeMap<NotePath, String>, pdfs: &[AttachmentText]) -> Index {
    let mut ix = Index::new();
    for (p, t) in notes {
        ix.insert_record(NoteRecord::from_source(p.clone(), t), t.clone());
    }
    for a in pdfs {
        ix.upsert_attachment(a.clone());
    }
    ix.recount();
    ix.refresh();
    ix
}

/// What a person or agent is told of a search: paths, exact scores, snippets.
fn hits(ix: &Index, scope: ScopeRead<'_>, queries: &[String]) -> String {
    let mut out = String::new();
    for q in queries {
        for h in ix.search_in(q, 50, scope) {
            let _ = writeln!(out, "{q}: {} {} {}", h.path, h.score.to_bits(), h.snippet);
        }
        for kind in [SearchKind::Notes, SearchKind::All, SearchKind::Attachments] {
            let f = SearchFilters { kind, ..SearchFilters::default() };
            for h in ix.search_sections_in(None, q, &f, 8, scope) {
                let _ = writeln!(out, "{q} {kind:?}: {} {:?} {:?} {} {}", h.place(), h.heading, h.lines, h.score.to_bits(), h.snippet);
            }
        }
        for t in crate::tokenize::tokens(q) {
            let _ = writeln!(out, "idf {t}: {:?}", ix.idf_in(&t, scope).map(f32::to_bits));
        }
    }
    let _ = writeln!(out, "notes {}", ix.len_in(scope));
    out
}

/// A note of random words from [`WORDS`], with tags.
fn random_note(rng: &mut Rng) -> String {
    let mut s = String::new();
    if rng.next(3) == 0 {
        let _ = writeln!(s, "---\ntags: [{}]\n---", ["plan", "work", "home"][rng.next(3)]);
    }
    let _ = writeln!(s, "# {}", WORDS[rng.next(WORDS.len())]);
    for _ in 0..=rng.next(4) {
        let words: Vec<&str> = (0..=rng.next(12)).map(|_| WORDS[rng.next(WORDS.len())]).collect();
        let _ = writeln!(s, "\n## {}\n\n{}", WORDS[rng.next(WORDS.len())], words.join(" "));
    }
    if rng.next(4) == 0 {
        s.push_str("#topic\n");
    }
    s
}

fn random_query(rng: &mut Rng) -> String {
    let w = |rng: &mut Rng| WORDS[rng.next(WORDS.len())];
    match rng.next(7) {
        0 => w(rng).to_string(),
        1 => format!("{} {}", w(rng), w(rng)),
        2 => w(rng).chars().take(rng.next(3) + 1).collect(),
        3 => format!("\"{} {}\"", w(rng), w(rng)),
        4 => format!("{} #plan", w(rng)),
        5 => format!("{} -{}", w(rng), w(rng)),
        _ => format!("{} {}", w(rng), w(rng).chars().take(2).collect::<String>()),
    }
}

/// A view keeps the notes `visible` accepts (by path and tags).
fn view(ix: &Index, visible: impl Fn(&NotePath, &[String]) -> bool + Send + Sync + 'static) -> Scope {
    Scope::build(ix, Arc::new(visible))
}

/// The leak (`docs/design/0.14-agent-index.md` §1): a word found only in
/// notes a caller can't see changes nothing it is told: hits, order and
/// every score equal an index holding only its notes. Two callers with
/// different views get different statistics.
#[test]
fn a_word_only_in_hidden_notes_changes_nothing_a_view_ranks() {
    let mut visible: BTreeMap<NotePath, String> = BTreeMap::new();
    visible.insert(np("Ideas.md"), "# Ideas\n\nshared words here, about ropes.\n".into());
    visible.insert(np("Plan.md"), "# Plan\n\nshared plan, shared words.\n".into());
    visible.insert(np("wiki/Page.md"), "# Page\n\nwords in the wiki.\n".into());
    let mut all = visible.clone();
    for i in 0..10 {
        all.insert(np(&format!("Private/diary {i}.md")), format!("# Diary {i}\n\nshared shared words secretzq secretzq{i}\n"));
    }
    let pdfs = vec![pdf("wiki/scan.pdf", vec!["shared words, scanned".into()]), pdf("Private/d.pdf", vec!["shared secretzq".into()])];
    let full = index_of(&all, &pdfs);
    let mine = index_of(&visible, &pdfs[..1]);
    let not_private = |p: &NotePath, _: &[String]| !p.as_str().starts_with("Private/");
    let v = view(&full, not_private);
    let queries: Vec<String> = ["shared", "shared words", "words", "secretzq", "sh", "secret", "plan"].map(String::from).to_vec();
    assert_eq!(hits(&full, ScopeRead::View(&v), &queries), hits(&mine, ScopeRead::Full, &queries));
    // The whole index ranks differently: the hidden notes' words count there.
    assert_ne!(hits(&full, ScopeRead::Full, &queries), hits(&mine, ScopeRead::Full, &queries));
    // Another caller who also may not see the wiki: its own statistics.
    let narrower = view(&full, move |p: &NotePath, t: &[String]| not_private(p, t) && !p.as_str().starts_with("wiki/"));
    assert_ne!(v.notes().n(), narrower.notes().n());
    let only: BTreeMap<NotePath, String> =
        visible.iter().filter(|(p, _)| !p.as_str().starts_with("wiki/")).map(|(p, t)| (p.clone(), t.clone())).collect();
    assert_eq!(hits(&full, ScopeRead::View(&narrower), &queries), hits(&index_of(&only, &[]), ScopeRead::Full, &queries));
    assert_ne!(hits(&full, ScopeRead::View(&narrower), &queries), hits(&full, ScopeRead::View(&v), &queries));
}

/// G1, randomised: random notes, attachments, members (by path or tag) and
/// queries (words, prefixes, phrases, tags, exclusions); a view ranks as an
/// index of only its notes, bit for bit.
#[test]
fn keyword_scope_ranks_like_an_index_of_its_notes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..120 {
        let mut notes: BTreeMap<NotePath, String> = BTreeMap::new();
        for i in 0..rng.next(25) + 2 {
            notes.insert(np(&format!("{}/N{i}.md", ["A", "B", "C"][rng.next(3)])), random_note(&mut rng));
        }
        let pdfs: Vec<AttachmentText> = (0..rng.next(3))
            .map(|i| {
                let pages = (0..=rng.next(3)).map(|_| random_note(&mut rng)).collect();
                pdf(&format!("{}/f{i}.pdf", ["A", "B"][rng.next(2)]), pages)
            })
            .collect();
        let full = index_of(&notes, &pdfs);
        let rule = rng.next(3);
        let hidden_folder = ["A/", "B/", "C/"][rng.next(3)];
        let visible = move |p: &NotePath, tags: &[String]| match rule {
            0 => !p.as_str().starts_with(hidden_folder),
            1 => !p.as_str().starts_with(hidden_folder) && !tags.iter().any(|t| t == "home"),
            _ => true,
        };
        let v = view(&full, visible);
        let kept: BTreeMap<NotePath, String> = notes
            .iter()
            .filter(|(p, _)| visible(p, &full.get(p).map(|r| r.tags.clone()).unwrap_or_default()))
            .map(|(p, t)| (p.clone(), t.clone()))
            .collect();
        let kept_pdfs: Vec<AttachmentText> = pdfs.iter().filter(|a| visible(&a.path, &[])).cloned().collect();
        let mine = index_of(&kept, &kept_pdfs);
        let queries: Vec<String> = (0..6).map(|_| random_query(&mut rng)).collect();
        assert_eq!(hits(&full, ScopeRead::View(&v), &queries), hits(&mine, ScopeRead::Full, &queries), "case {case}, rule {rule}");
        assert_eq!(v.hides_nothing(&full), kept.len() == notes.len() && kept_pdfs.len() == pdfs.len());
    }
}

/// Prefix completions count only members: 70 terms of hidden notes that
/// share a prefix take none of the 64 places.
#[test]
fn prefix_completions_count_only_members() {
    let mut all: BTreeMap<NotePath, String> = BTreeMap::new();
    let mut hidden = String::new();
    for i in 0..70 {
        let _ = write!(hidden, "zz{i:03}a ");
    }
    all.insert(np("Private/H.md"), format!("# H\n\n{hidden}\n"));
    let mut visible = BTreeMap::new();
    visible.insert(np("V.md"), "# V\n\nzzvisible words\n".to_string());
    all.extend(visible.clone());
    let full = index_of(&all, &[]);
    let v = view(&full, |p: &NotePath, _: &[String]| !p.as_str().starts_with("Private/"));
    let q = ["zz".to_string(), "zzv".to_string()];
    assert_eq!(hits(&full, ScopeRead::View(&v), &q), hits(&index_of(&visible, &[]), ScopeRead::Full, &q));
    assert_eq!(full.search_in("zz", 5, ScopeRead::View(&v)).len(), 1);
}

/// The membership and totals of a view, as text.
fn stats_text(ix: &Index, s: &Scope) -> String {
    let ft = ix.fulltext();
    let members: Vec<String> = (0..ft.doc_slots())
        .filter_map(|i| u32::try_from(i).ok().map(DocId))
        .filter(|id| s.notes().contains(*id))
        .map(|id| ft.path_of(id).map(ToString::to_string).unwrap_or_default())
        .collect();
    format!("{members:?} n={} total={} pages={} ptotal={}", s.notes().n(), s.notes().total_len(), s.pages().n(), s.pages().total_len())
}

/// G3: a view the worker patches through random edits, removals, tag
/// changes, attachments and a load equals one built afresh.
#[test]
fn scope_stats_patched_equal_a_rebuild() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let files: Vec<(String, String)> = (0..12).map(|i| (format!("{}/N{i}.md", ["A", "B"][i % 2]), random_note(&mut rng))).collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let vault = quire_vault::Vault::in_memory(&refs);
    let svc = IndexService::spawn();
    svc.load(vault.clone());
    svc.flush();
    let visible: Visible = Arc::new(|p: &NotePath, tags: &[String]| !p.as_str().starts_with("B/") && !tags.iter().any(|t| t == "home"));
    let key = [7u8; 32];
    drop(svc.scoped(key, Arc::clone(&visible)));
    for round in 0..300 {
        let path = np(&format!("{}/N{}.md", ["A", "B"][rng.next(2)], rng.next(16)));
        match rng.next(8) {
            0 => svc.remove(path),
            1 => svc.upsert_attachment(pdf(&format!("{}/f{}.pdf", ["A", "B"][rng.next(2)], rng.next(3)), vec![random_note(&mut rng)])),
            2 => svc.remove_attachment(np(&format!("{}/f{}.pdf", ["A", "B"][rng.next(2)], rng.next(3)))),
            3 if round == 150 => svc.load(vault.clone()),
            _ => svc.upsert(path, random_note(&mut rng)),
        }
        if rng.next(3) == 0 {
            svc.flush();
            let sc = svc.scoped(key, Arc::clone(&visible));
            let fresh = Scope::build(&sc, Arc::clone(&visible));
            assert_eq!(stats_text(&sc, sc.view()), stats_text(&sc, &fresh), "round {round}");
            assert_eq!(sc.view().build_of(), sc.build());
        }
    }
}

/// G9: at most 32 views; past that the least recently used goes, and a
/// view built again answers as a fresh one.
#[test]
fn at_most_32_views_and_a_dropped_one_comes_back_the_same() {
    let svc = IndexService::spawn();
    svc.load(quire_vault::Vault::in_memory(&[("A/x.md", "# x\nrope tree\n"), ("B/y.md", "# y\nrope\n"), ("C/z.md", "# z\ntree\n")]));
    svc.flush();
    let rule =
        |i: u8| -> Visible { Arc::new(move |p: &NotePath, _: &[String]| !p.as_str().starts_with(["A/", "B/", "C/"][usize::from(i % 3)])) };
    let mut first: BTreeMap<u8, String> = BTreeMap::new();
    for round in 0..3 {
        for i in 0..40u8 {
            let sc = svc.scoped([i; 32], rule(i));
            let got = hits(&sc, sc.scope(), &["rope".to_string(), "tree".to_string()]);
            let fresh = Scope::build(&sc, rule(i));
            assert_eq!(got, hits(&sc, ScopeRead::View(&fresh), &["rope".to_string(), "tree".to_string()]));
            if round == 0 {
                first.insert(i, got);
            } else {
                assert_eq!(first[&i], got);
            }
            drop(sc);
            assert!(svc.scope_count() <= MAX_SCOPES);
        }
    }
    assert_eq!(svc.scope_count(), MAX_SCOPES);
}

/// Views built on many threads while the worker applies edits agree with
/// the records once the worker is done (`docs/design/0.14-agent-index.md` §6).
#[test]
fn views_built_during_writes_are_consistent() {
    let svc = Arc::new(IndexService::spawn());
    svc.load(quire_vault::Vault::in_memory(&[("A/x.md", "# x\nrope\n")]));
    svc.flush();
    let visible: Visible = Arc::new(|p: &NotePath, _: &[String]| !p.as_str().starts_with("B/"));
    std::thread::scope(|s| {
        for t in 0..4u8 {
            let (svc, visible) = (Arc::clone(&svc), Arc::clone(&visible));
            s.spawn(move || {
                for _ in 0..50 {
                    let sc = svc.scoped([t; 32], Arc::clone(&visible));
                    assert!(sc.view().notes().n() <= sc.len());
                }
            });
        }
        let mut rng = Rng(42);
        for i in 0..300 {
            let p = format!("{}/n{}.md", ["A", "B"][rng.next(2)], rng.next(40));
            if i % 5 == 0 { svc.remove(np(&p)) } else { svc.upsert(np(&p), random_note(&mut rng)) }
        }
    });
    svc.flush();
    for t in 0..4u8 {
        let sc = svc.scoped([t; 32], Arc::clone(&visible));
        assert_eq!(stats_text(&sc, sc.view()), stats_text(&sc, &Scope::build(&sc, Arc::clone(&visible))));
    }
}
