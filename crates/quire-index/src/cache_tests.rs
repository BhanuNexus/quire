// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The startup cache end to end (`docs/design/0.14-index-startup.md` §3.6 and §6):
//! a warm load equals a cold one, every crash point and every damaged file
//! ends in the same answers, no locked, deleted or moved-away text stays in
//! a cache file, only the lock holder writes, and the index takes only what
//! the manifest lists.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use quire_base::cache::{self, Kind};
use quire_vault::changes::Manifest;

use super::*;
use crate::store::{self as st, Store, WRITES_LEFT};

/// A word only the notes that must not stay in the cache carry.
const MARKER: &str = "zqxwvmarker";

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

/// An hour ago: files older than the manifest's racy window, so a scan
/// trusts their size and time.
fn past() -> SystemTime {
    SystemTime::now() - Duration::from_hours(1)
}

fn put(dir: &Path, p: &str, text: &str) {
    let f = dir.join(p);
    std::fs::create_dir_all(f.parent().expect("has a folder")).expect("folder");
    std::fs::write(&f, text).expect("write");
    std::fs::File::options().write(true).open(&f).and_then(|h| h.set_modified(past())).expect("time");
}

/// Notes that link to each other, with tags, tasks, aliases and properties.
fn note(i: usize, salt: usize) -> String {
    let mut s = String::new();
    if i.is_multiple_of(3) {
        let _ = writeln!(s, "---\naliases: [Alias {i}]\nstatus: draft\ntags: [proj/{}]\n---", i % 4);
    }
    let _ =
        writeln!(s, "# Note {i}\n\nrope tree editor word{salt} and [[N{}]] and [[Alias {}]] #topic/{}", (i + 1) % 12, (i + 3) % 12, i % 2);
    let _ = writeln!(s, "- [ ] task {i} 📅 2026-12-0{}\n- [x] done {salt}", 1 + i % 9);
    let _ = writeln!(s, "\n## Part\n\nsee [[Missing {}]] and [[N{}#Part]]", i % 5, (i + 7) % 12);
    s
}

fn vault_of(dir: &Path, n: usize) -> Vault {
    for i in 0..n {
        put(dir, &format!("F{}/N{i}.md", i % 3), &note(i, 0));
    }
    Vault::open_local(dir).expect("vault")
}

/// Everything a person can see of an index, as text: search hits with
/// exact scores and snippets, records (terms by text), sources, tasks,
/// tags, backlinks, unresolved links and times.
fn view(ix: &Index) -> String {
    let mut out = String::new();
    for q in ["rope", "ro", "word1", "editor task", "#topic", "#proj/1", "\"rope tree\"", "missing -draft", "alias"] {
        for h in ix.search(q, 100) {
            let _ = writeln!(out, "{q}: {} {} {}", h.path, h.score.to_bits(), h.snippet);
        }
    }
    for p in ix.paths() {
        let r = ix.get(&p).expect("indexed");
        let mut terms: Vec<(&str, u32)> = r.terms.iter().map(|(t, n)| (ix.term(*t), *n)).collect();
        terms.sort_unstable();
        let doc = ix.doc_id(&p).expect("a document");
        let mut bm25: Vec<(&str, u32)> =
            ix.fulltext().doc_terms(doc).iter().map(|t| (ix.term(*t), ix.fulltext().weight(*t, doc).expect("weight").to_bits())).collect();
        bm25.sort_unstable();
        let back: Vec<String> = ix.backlinks(&p).iter().map(|b| format!("{} {:?} {}", b.from, b.link.range, b.context)).collect();
        let _ = writeln!(
            out,
            "{p}: {} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            r.title,
            r.aliases,
            r.headings,
            r.links,
            r.tags,
            r.tasks,
            r.stats,
            r.properties,
            r.plain,
            terms,
            bm25,
            ix.fulltext().doc_len(doc),
            ix.source(&p),
            ix.modified(&p),
            back
        );
    }
    let _ = writeln!(out, "{:?}\n{:?}\n{:?}", ix.tasks(true), ix.tags(), ix.graph().unresolved());
    let _ = writeln!(out, "total {}", ix.fulltext().total_len().to_bits());
    out
}

/// The index a cold load builds from the files, without any cache.
fn cold(v: &Vault) -> String {
    let svc = IndexService::spawn();
    svc.load(v.clone());
    svc.flush();
    view(&svc.read())
}

fn service(v: &Vault) -> IndexService {
    let svc = IndexService::spawn();
    svc.load_cached(v.clone(), Manifest::open(v));
    svc.flush();
    svc
}

fn cache_dir(v: &Vault) -> std::path::PathBuf {
    v.storage().absolute(&np(st::DIR)).expect("on disk")
}

/// Every byte under `.quire/cache/`, with each segment's sections inflated.
fn cache_bytes(v: &Vault) -> Vec<u8> {
    let root = v.storage().absolute(&np(".quire/cache")).expect("on disk");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            out.extend(std::fs::read(&path).unwrap_or_default());
            if let Some(seg) =
                cache::read(&path, Kind::RECORDS_SEGMENT, st::FORMAT, st::DERIVE_VERSION).and_then(|b| st::decode_segment(&b))
            {
                for (s, _) in seg.notes {
                    out.extend(format!("{:?}", s.rec).bytes());
                }
                out.extend(seg.texts.inflate().unwrap_or_default());
                out.extend(seg.table.join(" ").bytes());
            }
        }
    }
    out
}

fn holds(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Segment files on disk, and those `records.idx` references.
fn segments(v: &Vault) -> (Vec<u64>, Vec<u64>) {
    let dir = cache_dir(v);
    let mut on_disk: Vec<u64> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            u64::from_str_radix(n.strip_prefix("r-")?.strip_suffix(".seg")?, 16).ok()
        })
        .collect();
    on_disk.sort_unstable();
    let mut used: Vec<u64> = cache::read(&dir.join("records.idx"), Kind::RECORDS_INDEX, st::FORMAT, st::DERIVE_VERSION)
        .and_then(|b| st::decode_index(&b))
        .map(|rows| rows.values().map(|r| r.segment).collect())
        .unwrap_or_default();
    used.sort_unstable();
    used.dedup();
    (on_disk, used)
}

#[test]
fn a_warm_start_reads_no_note_and_answers_as_a_cold_one() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 40);
    {
        let svc = service(&v);
        assert_eq!(svc.notes_read(), 40, "a cold start reads every note");
        assert_eq!(view(&svc.read()), cold(&v));
    } // a clean exit writes the cache
    let svc = service(&v);
    assert_eq!(svc.notes_read(), 0, "a warm start opens no note");
    assert_eq!(view(&svc.read()), cold(&v));
    let (on_disk, used) = segments(&v);
    assert_eq!(on_disk, used, "no segment left unreferenced");
}

/// Equivalence (`docs/design/0.14-index-startup.md` §6): random edits while
/// closed, including ones that keep a file's size and time (found by the
/// manifest's verify pass), then a warm load equals a cold one.
#[test]
fn a_warm_start_after_edits_while_closed_equals_a_cold_one() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 24);
    drop(service(&v));
    let mut seed = 0x5eed_cafe_u64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        usize::try_from(seed % n as u64).expect("small")
    };
    for round in 1..8 {
        let mut sneaky = 0;
        for _ in 0..6 {
            let i = next(30);
            let p = format!("F{}/N{i}.md", i % 3);
            match next(4) {
                0 => {
                    // A note already gone is what was wanted.
                    let _ = std::fs::remove_file(dir.path().join(&p));
                }
                1 if dir.path().join(&p).exists() => {
                    // Same size, same time, other bytes: only a verify pass sees it.
                    let f = dir.path().join(&p);
                    let old = std::fs::read_to_string(&f).expect("read");
                    let time = std::fs::metadata(&f).and_then(|m| m.modified()).expect("time");
                    std::fs::write(&f, old.replacen("rope", "ROPE", 1)).expect("write");
                    std::fs::File::options().write(true).open(&f).and_then(|h| h.set_modified(time)).expect("time");
                    sneaky += 1;
                }
                _ => put(dir.path(), &p, &note(i, round * 10 + next(5))),
            }
        }
        let svc = IndexService::spawn();
        let m = Manifest::open(&v);
        svc.load_cached(v.clone(), m.clone());
        svc.flush();
        m.verify().expect("verify");
        svc.flush();
        assert_eq!(view(&svc.read()), cold(&v), "round {round} ({sneaky} edits kept size and time)");
        svc.flush_cache();
        let (on_disk, used) = segments(&v);
        assert_eq!(on_disk, used, "round {round}: no segment left unreferenced");
    }
}

/// Between keyword search being ready and the texts being decoded
/// (`docs/design/0.14-index-startup.md` §3.6, phases 1 and 2): search hits are
/// complete with their snippets, and every caller asking for a note's text
/// gets exactly the text its record was made from, decoded on demand.
#[test]
fn between_the_phases_answers_are_complete_and_texts_come_on_demand() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), st::SEGMENT_NOTES + 40);
    drop(service(&v));
    let want = {
        let svc = IndexService::spawn();
        svc.load(v.clone());
        svc.flush();
        let ix = svc.read();
        let hits: Vec<String> = ["rope", "ro", "editor task", "#topic", "\"rope tree\"", "missing -draft"]
            .iter()
            .flat_map(|q| {
                ix.search(q, 1000)
                    .into_iter()
                    .map(move |h| format!("{q}: {} {} {} {:?}", h.path, h.score.to_bits(), h.snippet, h.highlights))
            })
            .collect();
        let texts: Vec<(NotePath, String, u32)> = ix
            .paths()
            .into_iter()
            .map(|p| {
                let s = ix.source(&p).expect("text").to_string();
                let line = ix.line_of(&p, s.len() / 2);
                (p, s, line)
            })
            .collect();
        (hits, texts, ix.tasks(true), ix.tags())
    };
    let index = RwLock::new(Index::new());
    let shared = Shared::default();
    let (_c, parse, texts) = load_cached(&v, Manifest::open(&v), crate::relations::LinkConfig::default(), &index, &shared);
    assert!(parse.is_empty(), "every note comes from the cache");
    assert_eq!(texts.len(), 2, "two segments, their texts not decoded yet");
    {
        let ix = index.read().expect("lock");
        let hits: Vec<String> = ["rope", "ro", "editor task", "#topic", "\"rope tree\"", "missing -draft"]
            .iter()
            .flat_map(|q| {
                ix.search(q, 1000)
                    .into_iter()
                    .map(move |h| format!("{q}: {} {} {} {:?}", h.path, h.score.to_bits(), h.snippet, h.highlights))
            })
            .collect();
        assert_eq!(hits, want.0, "keyword hits and snippets as a full build gives them");
        assert_eq!((ix.tasks(true), ix.tags()), (want.2.clone(), want.3.clone()));
        // Texts and lines on demand, from both segments.
        for (p, text, line) in want.1.iter().rev() {
            assert_eq!(ix.source(p), Some(text.as_str()), "{p}");
            assert_eq!(ix.line_of(p, text.len() / 2), *line, "{p}");
        }
    }
    assert!(fill_texts(&texts, &index, 2).is_empty());
    let ix = index.read().expect("lock");
    for (p, text, line) in &want.1 {
        assert_eq!(ix.source(p), Some(text.as_str()));
        assert_eq!(ix.line_of(p, text.len() / 2), *line);
    }
}

/// Load the cache as a restarted process would, and finish like the worker.
fn restart(v: &Vault) -> String {
    let svc = service(v);
    let got = view(&svc.read());
    drop(svc);
    got
}

/// Fault injection at each row of the crash table: a flush stopped after N
/// writes, then a restart, answers as a cold build; the next full flush
/// leaves no unreferenced segment and no removed text.
#[test]
fn a_flush_stopped_anywhere_costs_parses_never_results() {
    for stop_after in 0..8 {
        let dir = tempfile::tempdir().expect("temp");
        // More than a segment's worth, so a flush writes several files.
        let v = vault_of(dir.path(), st::SEGMENT_NOTES + 20);
        put(dir.path(), "Gone.md", &format!("# Gone\n{MARKER}\n"));
        let m = Manifest::open(&v);
        m.scan().expect("scan");
        let index = RwLock::new(Index::new());
        let shared = Shared::default();
        let cfg = crate::relations::LinkConfig::default();
        let (mut c, parse, _texts) = load_cached(&v, m.clone(), cfg.clone(), &index, &shared);
        assert_eq!(parse.len(), st::SEGMENT_NOTES + 21);
        let full = |c: &mut Cached, index: &RwLock<Index>| {
            let mut ix = index.write().expect("lock");
            for p in v.notes().expect("notes") {
                let src = v.read(&p).expect("read");
                c.store.as_mut().expect("on disk").note(&p, FileHash::of(src.as_bytes()), 0);
                ix.insert_record(NoteRecord::from_source(p, &src), src);
            }
            ix.refresh();
        };
        full(&mut c, &index);
        let store = c.store.as_mut().expect("on disk");
        assert!(store.flush(&index.read().expect("lock"), None).expect("flush"));
        // Edit one note, remove one, add one; then flush, stopping after `stop_after` writes.
        put(dir.path(), "F0/N0.md", &note(0, 99));
        std::fs::remove_file(dir.path().join("Gone.md")).expect("remove");
        put(dir.path(), "New.md", "# New\nrope\n");
        {
            let mut ix = index.write().expect("lock");
            ix.remove_no_refresh(&np("Gone.md"));
            store.gone(&np("Gone.md"));
            for p in ["F0/N0.md", "New.md"] {
                let src = v.read(&np(p)).expect("read");
                store.note(&np(p), FileHash::of(src.as_bytes()), 0);
                ix.insert_record(NoteRecord::from_source(np(p), &src), src);
            }
        }
        WRITES_LEFT.set(Some(stop_after));
        let done = store.flush(&index.read().expect("lock"), None);
        WRITES_LEFT.set(None);
        drop(c);
        assert_eq!(restart(&v), cold(&v), "stopped after {stop_after} writes ({done:?})");
        // The restarted writer's first flush cleans up.
        drop(service(&v));
        let (on_disk, used) = segments(&v);
        assert_eq!(on_disk, used, "stopped after {stop_after}: unreferenced segments deleted");
        assert!(!holds(&cache_bytes(&v), MARKER), "stopped after {stop_after}: the removed note's text is gone");
        if matches!(done, Ok(true)) {
            break;
        }
        assert!(stop_after < 7, "a flush here takes a few writes");
    }
}

#[test]
fn a_temporary_file_left_by_a_crash_is_swept() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 3);
    drop(service(&v));
    let left = cache_dir(&v).join(".records.idx.0123456789abcdef.tmp");
    std::fs::write(&left, "half").expect("write");
    std::fs::File::options().write(true).open(&left).and_then(|f| f.set_modified(past())).expect("time");
    drop(service(&v));
    assert!(!left.exists());
}

/// One way to damage the cache files.
type Damage<'a> = Box<dyn Fn() + 'a>;

/// Any doubt is "no cache": a flipped byte, a cut file, another vault's
/// files or a newer format each give a cold build's answers.
#[test]
fn a_damaged_or_foreign_cache_equals_a_cold_build() {
    let other = tempfile::tempdir().expect("temp");
    let ov = vault_of(other.path(), 10);
    put(other.path(), "F0/N0.md", "# Other\nanother vault's note\n");
    drop(service(&ov));
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 10);
    drop(service(&v));
    let cd = cache_dir(&v);
    let idx = cd.join("records.idx");
    let good_idx = std::fs::read(&idx).expect("written");
    let seg = segments(&v).0[0];
    let seg_file = cd.join(format!("r-{seg:016x}.seg"));
    let good_seg = std::fs::read(&seg_file).expect("written");
    let want = cold(&v);
    let cases: Vec<(&str, Damage<'_>)> = vec![
        (
            "index byte",
            Box::new(|| {
                let mut b = good_idx.clone();
                let n = b.len() - 3;
                b[n] ^= 1;
                std::fs::write(&idx, b).expect("write");
            }),
        ),
        (
            "segment byte",
            Box::new(|| {
                let mut b = good_seg.clone();
                let n = b.len() / 2;
                b[n] ^= 1;
                std::fs::write(&seg_file, b).expect("write");
            }),
        ),
        ("segment cut", Box::new(|| std::fs::write(&seg_file, &good_seg[..good_seg.len() - 10]).expect("write"))),
        ("segment gone", Box::new(|| std::fs::remove_file(&seg_file).expect("remove"))),
        (
            "another vault's files",
            Box::new(|| {
                for e in std::fs::read_dir(cache_dir(&ov)).expect("other").flatten() {
                    std::fs::copy(e.path(), cd.join(e.file_name())).expect("copy");
                }
            }),
        ),
    ];
    for (what, damage) in cases {
        damage();
        let svc = service(&v);
        assert_eq!(view(&svc.read()), want, "{what}");
        drop(svc);
        // The cache written afterwards is good again.
        let svc = service(&v);
        assert_eq!(svc.notes_read(), 0, "{what}: rewritten");
        drop(svc);
        std::fs::write(&idx, &good_idx).expect("restore");
        std::fs::write(&seg_file, &good_seg).expect("restore");
    }
    // A newer format is left alone (P3), and the index works without it.
    assert_eq!(
        cache::write(&idx, Kind::RECORDS_INDEX, st::FORMAT + 1, st::DERIVE_VERSION, b"from a newer Quire").expect("write"),
        cache::Written::Written
    );
    let newer = std::fs::read(&idx).expect("read");
    let svc = service(&v);
    assert_eq!(view(&svc.read()), want);
    svc.upsert(np("F0/N1.md"), "# Changed\n".into());
    svc.flush();
    svc.flush_cache();
    drop(svc);
    assert_eq!(std::fs::read(&idx).expect("read"), newer, "not overwritten");
}

/// A cache of an older `format` (records as JSON, before the hand codec)
/// is ignored, the notes are parsed, and the cache is written again.
#[test]
fn a_cache_of_an_older_format_is_ignored_and_rebuilt() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 10);
    drop(service(&v));
    for e in std::fs::read_dir(cache_dir(&v)).expect("cache").flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let kind = if name == "records.idx" {
            Kind::RECORDS_INDEX
        } else if name.starts_with("r-") {
            Kind::RECORDS_SEGMENT
        } else {
            continue;
        };
        let body = cache::read(&e.path(), kind, st::FORMAT, st::DERIVE_VERSION).expect("current");
        // An older build's file, as that build left it (a write here would keep the newer one).
        std::fs::remove_file(e.path()).expect("remove");
        assert_eq!(cache::write(&e.path(), kind, st::FORMAT - 1, st::DERIVE_VERSION, &body).expect("write"), cache::Written::Written);
    }
    let svc = service(&v);
    assert_eq!(svc.notes_read(), 10, "an older format is no cache");
    assert_eq!(view(&svc.read()), cold(&v));
    drop(svc);
    let svc = service(&v);
    assert_eq!(svc.notes_read(), 0, "written again in this format");
    assert_eq!(view(&svc.read()), cold(&v));
}

/// The marker test (`docs/design/0.14-index-startup.md` §3.6): with a second
/// service on the vault, (a) a folder locked here, (b) a note moved into a
/// locked folder elsewhere (its plain file removed here, as sync does) and
/// (c) a deleted note leave no byte of their text under `.quire/cache/`
/// after the next flush; and a folder locked while Quire was closed is
/// purged before any segment is read.
#[test]
fn locked_deleted_and_moved_away_text_leaves_the_cache() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 6);
    for p in ["Secret/S1.md", "Secret/S2.md", "Moved/M.md", "Gone/G.md"] {
        put(dir.path(), p, &format!("# {p}\nrope {MARKER}\n"));
    }
    let m = Manifest::open(&v);
    let a = IndexService::spawn();
    a.load_cached(v.clone(), m.clone());
    a.flush();
    let b = service(&v);
    a.flush_cache();
    assert!(holds(&cache_bytes(&v), MARKER), "the test sees the text when it is there");
    // (a) Locking: the app purges the cache and the scan removes the plain files.
    quire_vault::lock::lock_folder(&v, "Secret", Some("correct horse"), quire_vault::lock::Kdf::TEST, 1).expect("lock");
    a.purge_cache();
    // (b) and (c).
    std::fs::remove_file(dir.path().join("Moved/M.md")).expect("remove");
    std::fs::remove_file(dir.path().join("Gone/G.md")).expect("remove");
    m.scan().expect("scan");
    a.flush();
    b.flush_cache();
    a.flush_cache();
    assert!(!holds(&cache_bytes(&v), MARKER), "no marker after the next flush");
    assert!(a.read().search(MARKER, 5).is_empty());
    drop((a, b));
    // Locked while closed: the cache of a plain folder, then it is locked
    // with no Quire running.
    put(dir.path(), "Later/L.md", &format!("# L\n{MARKER}\n"));
    drop(service(&v));
    assert!(holds(&cache_bytes(&v), MARKER));
    quire_vault::lock::lock_folder(&v, "Later", Some("correct horse"), quire_vault::lock::Kdf::TEST, 1).expect("lock");
    let found = Store::open(&v).expect("on disk").load(&[], 1);
    assert!(found.segments.is_empty());
    assert!(!holds(&cache_bytes(&v), MARKER), "purged before any segment is read");
}

/// Only regular files the manifest lists are indexed: a symlinked note and
/// a note in a symlinked folder are refused; a new note is indexed whether
/// its save told the manifest first or only the watcher saw it.
#[cfg(unix)]
#[test]
fn only_files_the_manifest_lists_are_indexed() {
    let outside = tempfile::tempdir().expect("temp");
    put(outside.path(), "Out.md", &format!("# Out\n{MARKER}\n"));
    put(outside.path(), "OutDir/In.md", &format!("# In\n{MARKER}\n"));
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 3);
    std::os::unix::fs::symlink(outside.path().join("Out.md"), dir.path().join("Link.md")).expect("symlink");
    std::os::unix::fs::symlink(outside.path().join("OutDir"), dir.path().join("Linked")).expect("symlink");
    let m = Manifest::open(&v);
    let svc = IndexService::spawn();
    svc.load_cached(v.clone(), m.clone());
    svc.flush();
    let before = view(&svc.read());
    for p in ["Link.md", "Linked/In.md"] {
        svc.upsert(np(p), v.read(&np(p)).expect("readable through the link"));
    }
    svc.flush();
    assert_eq!(view(&svc.read()), before, "the index is unchanged");
    assert!(svc.read().search(MARKER, 5).is_empty());
    // A save: the manifest is told first, then the index.
    v.write(&np("Saved.md"), "# Saved\nfresh words\n").expect("write");
    let hash = m.note_written(&np("Saved.md"), b"# Saved\nfresh words\n");
    svc.upsert_hashed(np("Saved.md"), "# Saved\nfresh words\n".into(), hash);
    // A file only the watcher saw.
    put(dir.path(), "Watched.md", "# Watched\nother words\n");
    svc.upsert(np("Watched.md"), "# Watched\nother words\n".into());
    svc.flush();
    assert!(svc.read().contains(&np("Saved.md")));
    assert!(svc.read().contains(&np("Watched.md")));
    svc.flush_cache();
    assert!(!holds(&cache_bytes(&v), MARKER), "nothing from outside the vault is cached");
}

/// A change another part of Quire read (sync, the editor) reaches the index
/// through the manifest's one-row delta, without a scan; the same bytes
/// again, or a scan finding them, change nothing more.
#[test]
fn an_observed_change_reaches_the_index_once() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 6);
    let m = Manifest::open(&v);
    let svc = IndexService::spawn();
    svc.load_cached(v.clone(), m.clone());
    svc.flush();
    let p = np("F1/N1.md");
    let text = "# Changed\nobservedword here\n";
    std::fs::write(dir.path().join(p.as_str()), text).expect("write");
    m.observed(&p, text.as_bytes());
    svc.flush();
    assert_eq!(svc.read().search("observedword", 5).len(), 1, "taken in without a scan");
    let version = svc.version();
    m.observed(&p, text.as_bytes());
    m.scan().expect("scan");
    svc.flush();
    assert_eq!(svc.version(), version + 1, "only the flush's batch: no second update");
    assert_eq!(view(&svc.read()), cold(&v));
}

/// One writer: the lock holder writes, the other process only reads; when
/// the holder goes, the other takes over at its next flush.
#[test]
fn only_the_lock_holder_writes_and_another_takes_over() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 8);
    let a = service(&v);
    a.flush_cache();
    let b = service(&v);
    assert_eq!(b.notes_read(), 0, "the reader uses the writer's cache");
    let idx = cache_dir(&v).join("records.idx");
    let written = std::fs::read(&idx).expect("written");
    put(dir.path(), "F1/N1.md", &note(1, 7));
    b.upsert(np("F1/N1.md"), note(1, 7));
    b.flush();
    b.flush_cache();
    assert_eq!(std::fs::read(&idx).expect("read"), written, "a reader never writes");
    drop(a);
    b.flush_cache();
    assert_ne!(std::fs::read(&idx).expect("read"), written, "the reader took over");
    drop(b);
    let c = service(&v);
    assert_eq!(c.notes_read(), 0);
    assert_eq!(view(&c.read()), cold(&v));
}

/// A reload (after an import, an unlock for good, a vault switch) scans:
/// an edit saved in the app without telling the manifest, and a file the
/// manifest never saw, are what the index holds after it, never an older
/// cached record or nothing.
#[test]
fn a_reload_scans_so_it_never_installs_an_older_record_or_drops_a_new_note() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 8);
    let m = Manifest::open(&v);
    let svc = IndexService::spawn();
    svc.load_cached(v.clone(), m.clone());
    svc.flush();
    svc.flush_cache();
    put(dir.path(), "F1/N1.md", &note(1, 77));
    svc.upsert(np("F1/N1.md"), note(1, 77));
    svc.flush();
    put(dir.path(), "Fresh.md", "# Fresh\nfreshword\n");
    svc.load_cached(v.clone(), m);
    svc.flush();
    assert_eq!(view(&svc.read()), cold(&v));
}

/// Storage whose scan listing fails, as a disk that went away would.
struct NoListing(Arc<dyn quire_vault::Storage>);

impl quire_vault::Storage for NoListing {
    fn list(&self) -> quire_vault::Result<Vec<NotePath>> {
        self.0.list()
    }
    fn list_stat(&self) -> quire_vault::Result<Vec<(NotePath, Option<quire_vault::FileStat>)>> {
        Err(quire_vault::VaultError::Watch("the listing failed".into()))
    }
    fn read_bytes(&self, path: &NotePath) -> quire_vault::Result<Vec<u8>> {
        self.0.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> quire_vault::Result<()> {
        self.0.write_bytes_atomic(path, bytes)
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> quire_vault::Result<()> {
        self.0.rename(from, to)
    }
    fn trash(&self, path: &NotePath) -> quire_vault::Result<()> {
        self.0.trash(path)
    }
    fn exists(&self, path: &NotePath) -> bool {
        self.0.exists(path)
    }
    fn modified(&self, path: &NotePath) -> quire_vault::Result<SystemTime> {
        self.0.modified(path)
    }
    fn absolute(&self, path: &NotePath) -> Option<std::path::PathBuf> {
        self.0.absolute(path)
    }
    fn list_under(&self, folder: &str) -> quire_vault::Result<Vec<NotePath>> {
        self.0.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> quire_vault::Result<()> {
        self.0.remove(path)
    }
}

/// When the scan at load fails, the index still holds every note, and the
/// cache is neither read nor written: its rows aren't taken for dead.
#[test]
fn a_failed_scan_at_load_leaves_the_cache_alone() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 8);
    drop(service(&v));
    let idx = cache_dir(&v).join("records.idx");
    let before = std::fs::read(&idx).expect("written");
    let broken = Vault::new(Arc::new(NoListing(Arc::clone(v.storage()))));
    let svc = service(&broken);
    assert_eq!(view(&svc.read()), cold(&v), "every note, from the files");
    svc.flush_cache();
    drop(svc);
    assert_eq!(std::fs::read(&idx).expect("still there"), before, "not rewritten without the notes");
}

/// A folder locked while Quire was closed, with a `records.idx` that can't
/// be used (damaged here): the old segments are deleted at load too.
#[test]
fn an_unusable_index_file_purges_the_segments_at_load() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 4);
    put(dir.path(), "Later/L.md", &format!("# L\n{MARKER}\n"));
    drop(service(&v));
    assert!(holds(&cache_bytes(&v), MARKER));
    let idx = cache_dir(&v).join("records.idx");
    let mut b = std::fs::read(&idx).expect("written");
    let n = b.len() - 1;
    b[n] ^= 1;
    std::fs::write(&idx, b).expect("write");
    quire_vault::lock::lock_folder(&v, "Later", Some("correct horse"), quire_vault::lock::Kdf::TEST, 1).expect("lock");
    let found = Store::open(&v).expect("on disk").load(&[], 1);
    assert!(found.segments.is_empty());
    assert!(!holds(&cache_bytes(&v), MARKER), "no segment left from before");
}

/// Every note copy in a segment is one a row points to: a row dropped for
/// damage gets its segment written again, not left holding the old copy.
#[test]
fn a_dropped_row_gets_its_segment_rewritten() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 6);
    drop(service(&v));
    let idx = cache_dir(&v).join("records.idx");
    let mut rows = cache::read(&idx, Kind::RECORDS_INDEX, st::FORMAT, st::DERIVE_VERSION).and_then(|b| st::decode_index(&b)).expect("rows");
    // One row now names another note's slot: its own copy is unreferenced.
    let (a, b) = (np("F0/N0.md"), np("F1/N1.md"));
    let other = rows[&b].slot;
    rows.get_mut(&a).expect("row").slot = other;
    assert_eq!(
        cache::write(&idx, Kind::RECORDS_INDEX, st::FORMAT, st::DERIVE_VERSION, &st::encode_index(&rows)).expect("write"),
        cache::Written::Written
    );
    drop(service(&v));
    let rows = cache::read(&idx, Kind::RECORDS_INDEX, st::FORMAT, st::DERIVE_VERSION).and_then(|b| st::decode_index(&b)).expect("rows");
    let mut copies = 0;
    for e in std::fs::read_dir(cache_dir(&v)).expect("cache").flatten() {
        let Some(id) = e.file_name().to_str().and_then(|n| u64::from_str_radix(n.strip_prefix("r-")?.strip_suffix(".seg")?, 16).ok())
        else {
            continue;
        };
        let seg = cache::read(&e.path(), Kind::RECORDS_SEGMENT, st::FORMAT, st::DERIVE_VERSION)
            .and_then(|b| st::decode_segment(&b))
            .expect("segment");
        for (slot, (s, _)) in seg.notes.iter().enumerate() {
            copies += 1;
            let row = rows.get(&s.rec.path).expect("a row for every copy");
            assert_eq!((row.segment, row.slot as usize), (id, slot), "{}: the copy its row points to", s.rec.path);
        }
    }
    assert_eq!(copies, rows.len());
}

/// A newer build's `records.idx`: this build writes no segment either.
#[test]
fn a_newer_index_file_means_no_segment_is_written() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 4);
    let idx = cache_dir(&v).join("records.idx");
    assert_eq!(
        cache::write(&idx, Kind::RECORDS_INDEX, st::FORMAT + 1, st::DERIVE_VERSION, b"newer").expect("write"),
        cache::Written::Written
    );
    drop(service(&v));
    let names: Vec<String> =
        std::fs::read_dir(cache_dir(&v)).expect("cache").flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.starts_with("r-")), "{names:?}");
}

/// Every load is a new build of the index: document and term ids of the
/// last one mean nothing in it.
#[test]
fn every_load_is_a_new_build() {
    let dir = tempfile::tempdir().expect("temp");
    let v = vault_of(dir.path(), 3);
    let svc = service(&v);
    let first = svc.read().build();
    svc.load_cached(v.clone(), Manifest::open(&v));
    svc.flush();
    let second = svc.read().build();
    svc.load(v);
    svc.flush();
    let third = svc.read().build();
    assert!(first != second && second != third);
    svc.upsert(np("F0/N0.md"), "# edited".into());
    svc.flush();
    assert_eq!(svc.read().build(), third, "an update is the same build");
}
