// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hostile exports: every importer must plan without panicking, and without
//! output out of proportion to its input, whatever the export holds.
//!
//! The mutations are fuzz-style but deterministic (a fixed-seed generator),
//! so a failure reproduces. Regression tests for the reviewed cases come first.

use std::path::Path;

use crate::{ImportPlan, Source, Tree, plan_tree};

fn plan_of(files: &[(&str, &[u8])], kind: Source) -> ImportPlan {
    plan_tree(&Tree::from_files(files.iter().copied()), Path::new("export"), Some(kind))
}

#[test]
fn keep_timestamp_out_of_range_does_not_panic() {
    let j = br#"{"title":"","textContent":"x","userEditedTimestampUsec":9000000000000000000,"createdTimestampUsec":9000000000000000000}"#;
    let p = plan_of(&[("Keep/a.json", j)], Source::Keep);
    assert_eq!(p.notes.len(), 1);
}

#[test]
fn joplin_due_date_out_of_range_does_not_panic() {
    let n = b"Todo\n\nbody\n\nid: 11111111111111111111111111111111\nparent_id: \nis_todo: 1\ntodo_due: 9000000000000000000\ntype_: 1";
    let p = plan_of(&[("11111111111111111111111111111111.md", n)], Source::Joplin);
    assert_eq!(p.notes.len(), 1);
}

#[test]
fn percent_escapes_cut_in_a_character_do_not_panic() {
    for s in ["%a€", "%", "%%", "%€€", "100%", "a%2", "%zz%41", "%e2%82", "€%a"] {
        let _ = crate::percent_decode(s);
    }
    assert_eq!(crate::percent_decode("a%20b%41"), "a bA");
    assert_eq!(crate::percent_decode("%a€"), "%a€");
    // And through the HTML-folder importer that reads it from a link.
    let p = plan_of(&[("a.html", &b"<p><img src=\"%a\xe2\x82\xac.png\"> <a href=\"b%\">x</a></p>"[..])], Source::Html);
    assert_eq!(p.notes.len(), 1);
}

#[test]
fn an_attachment_listed_many_times_is_copied_once() {
    // Day One: one photo, listed by 500 entries.
    let entry = r#"{"text":"x","photos":[{"identifier":"P","md5":"abc"}]}"#;
    let json = format!("{{\"metadata\":{{}},\"entries\":[{}]}}", vec![entry; 500].join(","));
    let photo = vec![7u8; 1024];
    let p = plan_of(&[("Journal.json", json.as_bytes()), ("photos/abc.jpeg", &photo)], Source::DayOne);
    assert_eq!(p.notes.len(), 500);
    assert_eq!(p.files.len(), 1, "one copy of the source file");
    assert!(p.notes.iter().all(|n| n.content.contains("![[abc.jpeg]]")), "{}", p.notes[0].content);

    // Keep: one picture, attached to 200 notes.
    let note =
        r#"{"title":"n","textContent":"t","userEditedTimestampUsec":1,"attachments":[{"filePath":"pic.jpg","mimetype":"image/jpeg"}]}"#;
    let files: Vec<(String, Vec<u8>)> = (0..200)
        .map(|i| (format!("Keep/n{i}.json"), note.as_bytes().to_vec()))
        .chain([("Keep/pic.jpg".to_string(), photo.clone())])
        .collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let p = plan_of(&refs, Source::Keep);
    assert_eq!(p.notes.len(), 200);
    assert_eq!(p.files.len(), 1);
    assert!(p.notes.iter().all(|n| n.content.contains("![[pic.jpg]]")));

    // Joplin: one resource file named by 100 resource records.
    let res = |i: usize| format!("pic\n\nid: 44444444444444444444444444444444\nfilename: p.png\ntype_: 4\nx: {i}");
    let mut files: Vec<(String, Vec<u8>)> = (0..100).map(|i| (format!("{i:032}.md"), res(i).into_bytes())).collect();
    files.push(("resources/44444444444444444444444444444444.png".into(), photo));
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    let p = plan_of(&refs, Source::Joplin);
    assert_eq!(p.files.len(), 1, "{:?}", p.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>());
}

#[test]
fn roam_references_are_bounded_in_size_and_depth() {
    // One 1 MB block, referenced 20 000 times, and a chain of blocks that each quote the next.
    let big = "x".repeat(1 << 20);
    let refs = "((big)) ".repeat(20_000);
    let json = format!(
        r#"[{{"title":"P","children":[{{"uid":"a","string":"{refs}"}},{{"uid":"b","string":"b ((c))"}},{{"uid":"c","string":"c ((b))"}},{{"uid":"d","string":"{{{{embed: ((big))}}}}"}}]}},{{"title":"Other","children":[{{"uid":"big","string":"{big}"}}]}}]"#
    );
    let p = plan_of(&[("g.json", json.as_bytes())], Source::Roam);
    let out: usize = p.notes.iter().map(|n| n.content.len()).sum();
    assert!(out < 12 << 20, "output was {out} bytes for a {} byte export", json.len());
    assert!(p.warnings.iter().any(|w| w.contains("too large")), "{:?}", p.warnings);
    // A reference copies at most a short quote, and never expands the references inside it.
    let note = &p.notes.iter().find(|n| n.path.as_str() == "P.md").unwrap().content;
    assert!(note.contains("“b ((c))”") || note.contains("“c ((b))”"), "references inside a quote stay as written");
    assert!(!note.contains(&"x".repeat(400)), "a quote is cut");
}

// ---- fuzz-style ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const SNIPPETS: &[&str] = &[
    "%a€",
    "%",
    "%%%",
    "%ff%fe",
    "9000000000000000000",
    "-9000000000000000000",
    "99999999999999999999999999",
    "((x))",
    "{{embed: ((x))}}",
    "[[",
    "]]",
    "![[",
    "](",
    "](<",
    "\u{0}",
    "\u{feff}",
    "€",
    "..",
    "../../etc/passwd",
    "C:\\x",
    "<![CDATA[",
    "]]>",
    "<en-media hash=\"",
    "\"",
    "\\",
    "{",
    "}",
    "null",
    "[]",
    "[[[[[[[[",
    "---\n",
    "id: ",
    "type_: 4\n",
    "type_: 1\n",
    "todo_due: 9000000000000000000\n",
    "<img src=\"%a€\">",
];

fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    for _ in 0..=rng.below(4) {
        match rng.below(6) {
            0 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] = rng.next().to_le_bytes()[0];
            }
            1 if !b.is_empty() => b.truncate(rng.below(b.len())),
            2 => {
                let s = SNIPPETS[rng.below(SNIPPETS.len())].as_bytes();
                let i = rng.below(b.len() + 1);
                b.splice(i..i, s.iter().copied());
            }
            3 if !b.is_empty() => {
                let i = rng.below(b.len());
                let j = (i + rng.below(40)).min(b.len());
                b.drain(i..j);
            }
            4 if !b.is_empty() => {
                let i = rng.below(b.len());
                let j = (i + rng.below(200)).min(b.len());
                let chunk = b[i..j].to_vec();
                b.splice(i..i, chunk);
            }
            _ => {
                let i = rng.below(b.len() + 1);
                b.splice(i..i, "€é".bytes());
            }
        }
    }
    b
}

fn seeds(kind: Source) -> Vec<(&'static str, Vec<u8>)> {
    let f = |p: &'static str, b: &[u8]| (p, b.to_vec());
    match kind {
        Source::Keep => vec![
            f("Keep/Groceries.json", include_bytes!("../tests/fixtures/keep/Groceries.json")),
            f("Keep/Idea.json", include_bytes!("../tests/fixtures/keep/Idea.json")),
            f("Keep/Old.json", include_bytes!("../tests/fixtures/keep/Old.json")),
            f("Keep/photo.jpg", b"jpg"),
        ],
        Source::Joplin => vec![
            f("11111111111111111111111111111111.md", include_bytes!("../tests/fixtures/joplin/note1.md")),
            f("22222222222222222222222222222222.md", include_bytes!("../tests/fixtures/joplin/note2.md")),
            f("33333333333333333333333333333333.md", include_bytes!("../tests/fixtures/joplin/folder.md")),
            f("44444444444444444444444444444444.md", include_bytes!("../tests/fixtures/joplin/resource.md")),
            f("55555555555555555555555555555555.md", include_bytes!("../tests/fixtures/joplin/tag.md")),
            f("66666666666666666666666666666666.md", include_bytes!("../tests/fixtures/joplin/note_tag.md")),
            f("resources/44444444444444444444444444444444.png", b"\x89PNG"),
        ],
        Source::DayOne => vec![
            f("Journal.json", include_bytes!("../tests/fixtures/dayone/Journal.json")),
            f("photos/abc123.jpeg", b"jpg"),
            f("audios/def456.m4a", b"m4a"),
        ],
        Source::Roam => vec![f("graph.json", include_bytes!("../tests/fixtures/roam.json"))],
        Source::Logseq => vec![
            f("pages/Quire.md", b"- Plan #[[big idea]]\n  id:: 6500aa00-0000-0000-0000-000000000001\n  - child ((6500aa00-0000-0000-0000-000000000001))\n- ![img](../assets/a%20b.png)\n- {{embed: [[Other]]}}\n"),
            f("pages/Other.md", b"tags:: a, b\n\n- text [[Quire]] ((6500aa00-0000-0000-0000-000000000001))\n"),
            f("journals/2026_11_01.md", b"- today [[Quire]]\n"),
            f("logseq/config.edn", b"{}"),
            f("assets/a b.png", b"png"),
        ],
        Source::Bear => vec![
            f("A.textbundle/text.md", b"# A\n\nbody #tag ![x](assets/p.png) [f](assets/p.png)\n"),
            f("A.textbundle/assets/p.png", b"png"),
            f("A.textbundle/info.json", b"{\"version\":2}"),
        ],
        Source::Notion => vec![
            f("Home aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md", b"# Home\n\n[Road](Road%20bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md) ![c](Home/chart.png)\n"),
            f("Road bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md", b"# Road\n\nx\n"),
            f("Home/chart.png", b"png"),
            f("DB cccccccccccccccccccccccccccccccc.csv", b"Name,Tags,Date\nA,\"x, y\",2026-01-01\n"),
            f("DB/A dddddddddddddddddddddddddddddddd.md", b"# A\n\nTags: x\n"),
        ],
        Source::Evernote => vec![f(
            "export.enex",
            b"<en-export><note><title>T &amp; t</title><created>20260101T101010Z</created><tag>a</tag><content><![CDATA[<en-note><div>hi <en-media hash=\"abc\" type=\"image/png\"/></div></en-note>]]></content><resource><data encoding=\"base64\">aGVsbG8=</data><mime>image/png</mime><resource-attributes><file-name>a.png</file-name></resource-attributes></resource></note></en-export>",
        )],
        Source::Markdown => vec![
            f("a.md", b"---\ntitle: A\ncreated: 2026-01-01\n---\n# A\n[b](sub/b.md) ![i](sub/img%20x.png) [[c]]\n"),
            f("sub/b.markdown", b"B #t"),
            f("sub/img x.png", b"png"),
            f("c.txt", b"plain"),
        ],
        Source::Html => vec![
            f("a.html", b"<html><head><title>A</title></head><body><h1>A</h1><p><a href=\"b.html#x\">b</a> <img src=\"pics/p%20q.png\"></p><table><tr><td>1</td></tr></table></body></html>"),
            f("b.html", b"<body><ul><li>x<li>y</ul><pre>code</pre></body>"),
            f("pics/p q.png", b"png"),
        ],
    }
}

#[test]
fn every_importer_survives_mutated_exports() {
    for (k, kind) in Source::ALL.into_iter().enumerate() {
        let base = seeds(kind);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (k as u64 + 1));
        // The untouched seeds plan fine.
        let clean: Vec<(&str, &[u8])> = base.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        let _ = plan_of(&clean, kind);
        let rounds = std::env::var("QUIRE_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
        for round in 0..rounds {
            let files: Vec<(&str, Vec<u8>)> =
                base.iter().map(|(p, b)| (*p, if rng.below(3) == 0 || round == 0 { mutate(&mut rng, b) } else { b.clone() })).collect();
            let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
            let plan = plan_of(&refs, kind);
            // Output stays in proportion to the input.
            let input: usize = files.iter().map(|(_, b)| b.len()).sum();
            let out: usize =
                plan.notes.iter().map(|n| n.content.len()).sum::<usize>() + plan.files.iter().map(|f| f.bytes.len()).sum::<usize>();
            assert!(out <= input * 8 + 4096, "{kind:?}: {out} bytes out for {input} in (round {round})");
        }
    }
}

#[test]
fn detection_survives_hostile_trees() {
    let mut rng = Rng(42);
    for _ in 0..200 {
        let kind = Source::ALL[rng.below(Source::ALL.len())];
        let base = seeds(kind);
        let files: Vec<(&str, Vec<u8>)> = base.iter().map(|(p, b)| (*p, mutate(&mut rng, b))).collect();
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        let t = Tree::from_files(refs);
        let _ = plan_tree(&t, Path::new("x.zip"), None);
    }
}
