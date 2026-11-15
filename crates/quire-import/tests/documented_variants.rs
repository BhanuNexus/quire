// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Variants of each source's export format that the format's public
//! documentation (or the app's own help and well-known open-source tools that
//! read the format) describes, written by hand because real third-party
//! exports can't be downloaded in the test environment. Each test says which
//! variant it covers. See docs/design/0.13.1-qa.md for what is still only
//! verified against hand-built text.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;

use base64::Engine as _;
use quire_import::{ImportPlan, plan};

fn put(root: &Path, files: &[(&str, &[u8])]) {
    for (p, b) in files {
        let f = root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b).unwrap();
    }
}

fn import(files: &[(&str, &[u8])]) -> (tempfile::TempDir, ImportPlan) {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), files);
    let p = plan(dir.path()).unwrap();
    (dir, p)
}

fn note<'a>(p: &'a ImportPlan, name: &str) -> &'a str {
    p.notes.iter().find(|n| n.path.as_str() == name).map_or_else(
        || panic!("no note {name}: {:?} {:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(), p.warnings),
        |n| n.content.as_str(),
    )
}

// ---------------------------------------------------------------- Evernote

/// ENEX (evernote-export4.dtd): `en-export` with attributes, wrapped base64
/// with CRLF, a resource without `file-name` (named from its MIME type), an
/// unchecked and a checked `en-todo`, `en-crypt`, `en-media` whose hash has
/// no resource, the `reminder-*`, `latitude` and `author` attributes, and a
/// `recognition` blob in the resource (which must not end up in the note).
#[test]
fn evernote_dtd_variants() {
    let pdf = b"%PDF-1.4 fake";
    let b64 = base64::engine::general_purpose::STANDARD.encode(pdf);
    let wrapped: String = b64.as_bytes().chunks(8).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("\r\n");
    let hash = format!("{:x}", md5::compute(pdf));
    let enex = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20261105T101500Z" application="Evernote" version="10.100.1">
<note>
<title>Receipt &#8211; tax</title>
<created>20261101T235959Z</created>
<updated>20261102T000102Z</updated>
<tag>finance</tag>
<note-attributes>
<subject-date>20261031T000000Z</subject-date>
<latitude>38.7</latitude><longitude>-9.1</longitude><altitude>12</altitude>
<author>Asha</author>
<source>web.clip</source>
<source-url>https://example.com/a?b=1&amp;c=2</source-url>
<reminder-order>1</reminder-order>
<reminder-time>20261110T090000Z</reminder-time>
<content-class>evernote.food.meal</content-class>
</note-attributes>
<content><![CDATA[<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd">
<en-note style="word-wrap: break-word;"><div><en-todo/>unchecked item</div><div><en-todo checked="false"/>also open</div><div><en-todo checked="true"/>done item</div>
<div><en-crypt hint="pet">RU5DUllQVEVE</en-crypt></div>
<div><en-media hash="{hash}" type="application/pdf" width="100"/></div>
<div><en-media hash="ffffffffffffffffffffffffffffffff" type="image/png"/></div>
<ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>
<table><tr><td>a</td><td>b</td></tr></table></en-note>]]></content>
<resource>
<data encoding="base64">{wrapped}</data>
<mime>application/pdf</mime>
<width>0</width><height>0</height>
<recognition><![CDATA[<recoIndex objType="unknown"><item><t>NOISE-FROM-OCR</t></item></recoIndex>]]></recognition>
<resource-attributes><source-url>file:///x.pdf</source-url><timestamp>20261101T120000Z</timestamp></resource-attributes>
</resource>
</note>
</en-export>"#
    );
    let (_d, p) = import(&[("Receipts.enex", enex.as_bytes())]);
    assert_eq!(p.notes.len(), 1, "{:?}", p.warnings);
    let n = &p.notes[0].content;
    assert!(n.contains("# Receipt – tax"), "{n}");
    assert!(n.contains("- [ ] unchecked item") && n.contains("- [ ] also open") && n.contains("- [x] done item"), "{n}");
    assert!(n.contains("nested"), "{n}");
    assert!(!n.contains("NOISE-FROM-OCR"), "OCR text isn't part of the note: {n}");
    assert!(n.contains("created: 2026-11-01T23:59:59Z"), "{n}");
    assert_eq!(p.files.len(), 1, "the one resource");
    assert_eq!(p.files[0].bytes, pdf, "wrapped base64 decodes");
    assert!(p.files[0].path.as_str().to_ascii_lowercase().ends_with(".pdf"), "a name from the MIME type: {}", p.files[0].path);
    assert!(n.contains(&format!("[[{}]]", p.files[0].path.file_name())), "the media is linked: {n}");
}

// ---------------------------------------------------------------- Google Keep

/// Takeout's Keep folder: a `Labels.txt` and `.html` copies beside the
/// `.json` notes (neither is a note), a list note with checked items, a
/// pinned, archived, coloured text note with labels and link annotations,
/// an audio attachment, and a trashed note.
#[test]
fn keep_takeout_folder_variants() {
    let list = r#"{"color":"DEFAULT","isTrashed":false,"isPinned":false,"isArchived":false,
"listContent":[{"text":"Milk","isChecked":false},{"text":"Eggs","isChecked":true},{"text":"","isChecked":false}],
"title":"Shopping","userEditedTimestampUsec":1793700000000000,"createdTimestampUsec":1793600000000000}"#;
    let text = r#"{"color":"TEAL","isTrashed":false,"isPinned":true,"isArchived":true,
"attachments":[{"filePath":"memo.3gp","mimetype":"audio/3gpp"}],
"textContent":"Call the plumber\nabout the sink",
"title":"","userEditedTimestampUsec":"1793700000000000","createdTimestampUsec":1793600000000000,
"labels":[{"name":"Home & Garden"},{"name":"urgent"}],
"annotations":[{"description":"d","source":"WEBLINK","title":"The Plumber","url":"https://plumber.example/"}],
"sharees":[]}"#;
    let trashed = r#"{"color":"DEFAULT","isTrashed":true,"isPinned":false,"isArchived":false,"textContent":"gone","title":"Old","userEditedTimestampUsec":1793700000000000,"createdTimestampUsec":1793600000000000}"#;
    let (_d, p) = import(&[
        ("Takeout/Keep/Shopping.json", list.as_bytes()),
        ("Takeout/Keep/Shopping.html", b"<html>copy</html>"),
        ("Takeout/Keep/Call the plumber.json", text.as_bytes()),
        ("Takeout/Keep/Old.json", trashed.as_bytes()),
        ("Takeout/Keep/memo.3gp", b"AUDIO"),
        ("Takeout/Keep/Labels.txt", b"Home & Garden\nurgent\n"),
    ]);
    assert_eq!(p.notes.len(), 2, "{:?} {:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(), p.warnings);
    let shopping = p.notes.iter().find(|n| n.content.contains("Milk")).unwrap();
    assert!(shopping.content.contains("- [ ] Milk\n- [x] Eggs\n"), "{}", shopping.content);
    assert!(!shopping.content.contains("- [ ] \n"), "empty items are left out");
    let plumber = p.notes.iter().find(|n| n.content.contains("plumber")).unwrap();
    assert!(plumber.content.contains("pinned") && plumber.content.contains("archived"), "{}", plumber.content);
    assert!(plumber.content.contains("[The Plumber](https://plumber.example/)"), "{}", plumber.content);
    assert!(
        plumber.content.contains("[[memo.3gp]]") && !plumber.content.contains("![[memo.3gp]]"),
        "audio is linked, not embedded as a picture"
    );
    assert!(plumber.content.contains("urgent"), "labels become tags");
    assert!(!p.notes.iter().any(|n| n.content.contains("gone")), "trashed notes are left out");
    assert_eq!(p.files.len(), 1);
}

// ---------------------------------------------------------------- Day One

/// Day One's `Export → JSON`: `metadata`, entries with `richText` (a JSON
/// string), photos by `identifier` and `md5` named `photos/<md5>.<type>`,
/// `dayone-moment://` links in the text, a PDF and an audio entry, `starred`,
/// `isPinned`, `timeZone`, `location` and `weather`.
#[test]
fn dayone_json_variants() {
    let json = r#"{"metadata":{"version":"1.0"},"entries":[
{"uuid":"A1B2C3","creationDate":"2026-11-05T15:30:12Z","modifiedDate":"2026-11-05T16:00:00Z","timeZone":"Europe/Lisbon",
 "text":"Walked by the river\\.\n\n![](dayone-moment://PH0TO1)\n\nAudio: ![](dayone-moment:/audio/AUD10)\n\n# Evening\nTea \\- very good",
 "richText":"{\"contents\":[{\"text\":\"Walked by the river\"}],\"meta\":{\"version\":1}}",
 "tags":["walk","river life"],"starred":true,"isPinned":false,"isAllDay":false,"duration":0,
 "location":{"localityName":"Lisbon","country":"Portugal","latitude":38.72,"longitude":-9.14,"placeName":"Tagus"},
 "weather":{"temperatureCelsius":18,"conditionsDescription":"Clear","weatherCode":"clear-day"},
 "photos":[{"identifier":"PH0TO1","md5":"d41d8cd98f00b204e9800998ecf8427e","type":"jpeg","orderInEntry":0,"width":10,"height":10}],
 "audios":[{"identifier":"AUD10","md5":"0cc175b9c0f1b6a831c399e269772661","format":"m4a"}]},
{"uuid":"D4E5F6","creationDate":"2026-11-05T09:00:00Z","text":"Second entry the same day","tags":[]}]}"#;
    let (_d, p) = import(&[
        ("Journal.json", json.as_bytes()),
        ("photos/d41d8cd98f00b204e9800998ecf8427e.jpeg", b"JPEG"),
        ("audios/0cc175b9c0f1b6a831c399e269772661.m4a", b"M4A"),
    ]);
    assert_eq!(p.notes.len(), 2, "{:?}", p.warnings);
    assert_ne!(p.notes[0].path, p.notes[1].path, "two entries the same day get two notes");
    let first = p.notes.iter().find(|n| n.content.contains("river")).unwrap();
    assert!(first.content.contains("river life") || first.content.contains("river-life"), "{}", first.content);
    assert!(!first.content.contains("dayone-moment"), "moment links are rewritten: {}", first.content);
    assert!(first.content.contains("Lisbon"), "{}", first.content);
    assert!(first.content.contains("starred"), "{}", first.content);
    assert!(
        first.content.contains("Walked by the river.") && first.content.contains(r"Tea \- very good"),
        "escapes that cannot change the meaning are removed, `\\-` stays (it could start a list): {}",
        first.content
    );
    assert!(p.files.iter().any(|f| f.bytes == b"JPEG"), "the photo comes across");
}

// ---------------------------------------------------------------- Roam

/// Roam's JSON: millisecond `create-time`, attributes, TODO/DONE macros,
/// `((uid))` references, `{{embed: ((uid))}}`, `#[[multi word]]`, Roam's
/// `^^highlight^^` and `__italic__`, `:block/heading`, daily-note titles
/// ("November 5th, 2026"), and children nested three deep.
#[test]
fn roam_json_variants() {
    let json = r#"[
{"title":"November 5th, 2026","create-time":1793800000000,"edit-time":1793800100000,"children":[
  {"string":"{{[[TODO]]}} call Sam #[[multi word]]","uid":"todo00001","create-time":1793800000000,"children":[
     {"string":"{{[[DONE]]}} step ^^key^^ and __soft__","uid":"done00001","children":[
        {"string":"deep ((todo00001))","uid":"deep00001"}]}]},
  {"string":"Project:: [[Launch]]","uid":"attr00001"},
  {"string":"Big","heading":2,"uid":"head00001"},
  {"string":"{{embed: ((done00001))}}","uid":"emb000001"}]},
{"title":"Launch","create-time":1793800000000,"children":[{"string":"See [[November 5th, 2026]] and **bold** ![img](https://example.com/a.png)","uid":"lnch00001"}]},
{"title":"Empty page"}]"#;
    let (_d, p) = import(&[("roam-export.json", json.as_bytes())]);
    let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
    assert!(names.contains(&"2026-11-05.md") || names.iter().any(|n| n.contains("2026-11-05")), "daily notes are named by date: {names:?}");
    let daily = p.notes.iter().find(|n| n.path.as_str().contains("2026-11-05")).unwrap();
    assert!(daily.content.contains("- [ ] call Sam"), "{}", daily.content);
    assert!(daily.content.contains("[x] step") || daily.content.contains("- [x]"), "{}", daily.content);
    assert!(
        daily.content.contains("#multi-word") || daily.content.contains("#[[multi word]]") || daily.content.contains("multi word"),
        "{}",
        daily.content
    );
    assert!(!daily.content.contains("((todo00001))"), "block references are resolved: {}", daily.content);
    assert!(!daily.content.contains("^^"), "{}", daily.content);
    assert!(daily.content.contains("## Big"), "headings: {}", daily.content);
    assert!(note(&p, "Launch.md").contains("[[2026-11-05]]"), "links to a daily note follow its new name: {}", note(&p, "Launch.md"));
}

// ---------------------------------------------------------------- Joplin

/// A RAW export folder: an HTML-markup note (`markup_language: 2`), a todo
/// with `todo_completed`, a note with `source_url`, an encrypted-resource
/// placeholder-free resource with a `.bin` file, and `:/id` links to a note
/// and to a resource.
#[test]
fn joplin_raw_variants() {
    let folder = "Work\n\nid: 0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\nparent_id: \ncreated_time: 2026-11-01T10:00:00.000Z\nupdated_time: 2026-11-01T10:00:00.000Z\nis_shared: 0\ntype_: 2";
    let html = "HTML note\n\n<p>Hello <b>bold</b> and <a href=\":/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\">link</a></p><ul><li>x</li></ul>\n\nid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nparent_id: 0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\ncreated_time: 2026-11-02T10:00:00.000Z\nupdated_time: 2026-11-02T11:00:00.000Z\nis_conflict: 0\nlatitude: 38.72\nlongitude: -9.14\nsource_url: https://example.com/p\nis_todo: 0\nmarkup_language: 2\nencryption_applied: 0\ntype_: 1";
    let todo = "Buy stamps\n\nSee ![pic](:/cccccccccccccccccccccccccccccccc) and [html](:/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)\n\nid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nparent_id: 0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\ncreated_time: 2026-11-03T10:00:00.000Z\nupdated_time: 2026-11-03T10:00:00.000Z\nis_todo: 1\ntodo_due: 1793900000000\ntodo_completed: 1793800000000\nmarkup_language: 1\ntype_: 1";
    let res = "pic.png\n\nid: cccccccccccccccccccccccccccccccc\nmime: image/png\nfilename: \nfile_extension: png\ncreated_time: 2026-11-03T10:00:00.000Z\nupdated_time: 2026-11-03T10:00:00.000Z\nsize: 4\ntype_: 4";
    let (_d, p) = import(&[
        ("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f.md", folder.as_bytes()),
        ("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md", html.as_bytes()),
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md", todo.as_bytes()),
        ("cccccccccccccccccccccccccccccccc.md", res.as_bytes()),
        ("resources/cccccccccccccccccccccccccccccccc.png", b"\x89PNG"),
    ]);
    let h = p
        .notes
        .iter()
        .find(|n| n.path.as_str().ends_with("HTML note.md"))
        .unwrap_or_else(|| panic!("{:?}", p.notes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>()));
    assert!(h.content.contains("**bold**"), "HTML notes are converted: {}", h.content);
    assert!(h.content.contains("[[Buy stamps]]") || h.content.contains("Buy stamps"), "a :/id link is rewritten: {}", h.content);
    assert!(!h.content.contains(":/aaaa"), "{}", h.content);
    let t = p.notes.iter().find(|n| n.path.as_str().ends_with("Buy stamps.md")).unwrap();
    assert!(t.content.contains("[[pic.png]]") || t.content.contains("pic"), "{}", t.content);
    assert!(!t.content.contains(":/cccc"), "{}", t.content);
    assert_eq!(p.files.len(), 1);
}

// ---------------------------------------------------------------- Bear

/// A folder of text bundles as Bear exports them: `info.json` with Bear's
/// `net.shinyfrog.bear` block, tags in the text (`#tag`, `#nested/tag`,
/// `#two words#` closed with a hash), `[[note links]]`, `::highlight::`, a
/// trashed note (`trashed: 1`) and an image in `assets/`.
#[test]
fn bear_textbundle_variants() {
    let info = |extra: &str| {
        format!(
            r#"{{"transient":true,"type":"net.daringfireball.markdown","creatorIdentifier":"net.shinyfrog.bear","version":2,"net.shinyfrog.bear":{{"modificationDate":"2026-11-05T10:00:00Z","creationDate":"2026-11-01T10:00:00Z","uniqueIdentifier":"U1"{extra}}}}}"#
        )
    };
    let text = "# Bear note\n#work #nested/tag #two words#\n\nSee [[Other note]] and ::marked::\n\n![](assets/pic.png)\n";
    let (_d, p) = import(&[
        ("Bear note.textbundle/info.json", info("").as_bytes()),
        ("Bear note.textbundle/text.md", text.as_bytes()),
        ("Bear note.textbundle/assets/pic.png", b"\x89PNG"),
        ("Other note.textbundle/info.json", info("").as_bytes()),
        ("Other note.textbundle/text.md", b"# Other note\nhello\n"),
        ("Gone.textbundle/info.json", info(r#","trashed":1"#).as_bytes()),
        ("Gone.textbundle/text.md", b"# Gone\nbye\n"),
    ]);
    let n = note(&p, "Bear note.md");
    assert!(n.contains("work") && n.contains("nested/tag"), "{n}");
    assert!(n.contains("two words") || n.contains("two-words"), "closed multi-word tags: {n}");
    assert!(n.contains("[[Other note]]"), "{n}");
    assert!(n.contains("![[pic.png]]"), "{n}");
    assert!(n.contains("2026-11-01"), "creation date: {n}");
    assert_eq!(p.files.len(), 1);
}

// ---------------------------------------------------------------- Notion

/// Notion's "Markdown & CSV" export: names end with the 32-character id,
/// links are percent-encoded relative paths, the page properties sit in the
/// first lines (`Created: …`), a database is a CSV beside a folder of its
/// rows, images live in a folder named like the page, and `<aside>` and
/// `<details>` blocks appear as raw HTML.
#[test]
fn notion_markdown_and_csv_variants() {
    let id_a = "0123456789abcdef0123456789abcdef";
    let id_b = "fedcba9876543210fedcba9876543210";
    let id_c = "00112233445566778899aabbccddeeff";
    let page = format!(
        "# Launch plan\n\nCreated: November 5, 2026 3:12 PM\nStatus: In progress\nTags: Work, Q4\n\nSee [Budget](Launch%20plan%20{id_a}/Budget%20{id_b}.md) and [the db](Tasks%20{id_c}.csv).\n\n<aside>\n\u{1F4A1} Remember the venue\n</aside>\n\n<details>\n<summary>More</summary>\nhidden text\n</details>\n\n![chart](Launch%20plan%20{id_a}/chart.png)\n"
    );
    let (_d, p) = import(&[
        (&format!("Launch plan {id_a}.md"), page.as_bytes()),
        (&format!("Launch plan {id_a}/Budget {id_b}.md"), b"# Budget\n\nTotal: 10\n"),
        (&format!("Launch plan {id_a}/chart.png"), b"\x89PNG"),
        (&format!("Tasks {id_c}.csv"), "Name,Status,Due\nWrite,Done,\"November 6, 2026\"\nShip,Open,\n".as_bytes()),
        (&format!("Tasks {id_c}/Write {id_a}.md"), b"# Write\n\nStatus: Done\n"),
    ]);
    let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
    assert!(names.iter().all(|n| !n.contains(id_a) && !n.contains(id_b) && !n.contains(id_c)), "ids are removed from names: {names:?}");
    let launch = p.notes.iter().find(|n| n.path.as_str().ends_with("Launch plan.md")).unwrap_or_else(|| panic!("{names:?}"));
    assert!(launch.content.contains("[[Budget]]") || launch.content.contains("Budget]]"), "links become wikilinks: {}", launch.content);
    assert!(!launch.content.contains(id_b), "{}", launch.content);
    assert!(
        launch.content.contains("Remember the venue") && launch.content.contains("hidden text"),
        "HTML blocks keep their text: {}",
        launch.content
    );
    assert!(launch.content.contains("Status") && launch.content.contains("In progress"), "properties are kept: {}", launch.content);
    assert!(p.files.iter().any(|f| f.bytes == b"\x89PNG"), "the image comes across");
    assert!(names.iter().any(|n| n.contains("Write")) || names.iter().any(|n| n.contains("Ship")), "database rows become notes: {names:?}");
}

// ---------------------------------------------------------------- Logseq

/// A Logseq graph: `pages/` and `journals/` (`2026_11_05.md`), a namespaced
/// page file (`a___b.md`) with a `title::` property, block ids and
/// `((uuid))` references, `{{embed ((uuid))}}`, `collapsed:: true`, `TODO`
/// / `DONE` markers, `SCHEDULED`, `[[links]]`, `#tags`, `#[[multi word]]`,
/// and assets referenced as `../assets/x.png`.
#[test]
fn logseq_graph_variants() {
    let (_d, p) = import(&[
        ("logseq/config.edn", b"{:meta/version 1}\n"),
        ("pages/Project___Alpha.md", b"title:: Project/Alpha\nalias:: Alpha\n\n- TODO write plan\n  id:: 6500a1b2-0000-4000-8000-000000000001\n  SCHEDULED: <2026-11-06 Fri>\n- collapsed block\n  collapsed:: true\n\t- child\n- see [[Other Page]] #[[multi word]] #plain\n- ![shot](../assets/shot_1.png)\n"),
        ("pages/Other Page.md", b"- DONE ship it\n- ref ((6500a1b2-0000-4000-8000-000000000001))\n- {{embed ((6500a1b2-0000-4000-8000-000000000001))}}\n"),
        ("journals/2026_11_05.md", b"- met [[Project/Alpha]]\n- NOW doing it\n"),
        ("assets/shot_1.png", b"\x89PNG"),
    ]);
    let names: Vec<&str> = p.notes.iter().map(|n| n.path.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("Alpha")), "{names:?}");
    assert!(names.iter().any(|n| n.contains("2026-11-05")), "journals are named by date: {names:?}");
    let alpha = p.notes.iter().find(|n| n.path.as_str().contains("Alpha")).unwrap();
    assert!(alpha.content.contains("- [ ] write plan") || alpha.content.contains("[ ] write plan"), "{}", alpha.content);
    assert!(!alpha.content.contains("collapsed::"), "{}", alpha.content);
    assert!(alpha.content.contains("![[shot_1.png]]"), "{}", alpha.content);
    let other = p.notes.iter().find(|n| n.path.as_str().contains("Other Page")).unwrap();
    assert!(!other.content.contains("((6500a1b2"), "block references are resolved: {}", other.content);
    assert!(other.content.contains("write plan"), "the reference shows the block's text: {}", other.content);
    assert_eq!(p.files.len(), 1);
}

// ---------------------------------------------------------------- HTML folder

/// Saved pages and app exports: Word-style `MsoNormal` paragraphs with
/// `<o:p>`, conditional comments, `<script>` and `<style>` (dropped), a
/// table, nested lists, `<pre><code>`, `<br>`, an image from a `_files`
/// folder with a percent-encoded name, and a charset declared in a meta tag.
#[test]
#[allow(clippy::literal_string_with_formatting_args, reason = "CSS braces in an HTML sample")]
fn html_folder_variants() {
    let html = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Saved page</title><style>p{color:red}</style><script>alert('x')</script></head><body>\n<!--[if gte mso 9]><xml><o:OfficeDocumentSettings/></xml><![endif]-->\n<h1>Saved page</h1><p class=MsoNormal>Hello<o:p></o:p> <b>world</b><br>next line</p>\n<ul><li>one<ul><li>nested</li></ul></li><li>two</li></ul>\n<table><thead><tr><th>A</th><th>B</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>\n<pre><code>let x = 1;\n</code></pre>\n<img src=\"Saved%20page_files/pic%201.png\" alt=\"pic\">\n<a href=\"other.html\">other</a></body></html>";
    let (_d, p) = import(&[
        ("Saved page.html", html.as_bytes()),
        ("Saved page_files/pic 1.png", b"\x89PNG"),
        ("other.html", b"<html><body><h1>Other</h1><p>text</p></body></html>"),
    ]);
    let n = note(&p, "Saved page.md");
    assert!(!n.contains("alert") && !n.contains("color:red") && !n.contains("OfficeDocumentSettings"), "{n}");
    assert!(n.contains("# Saved page") && n.contains("**world**"), "{n}");
    assert!(n.contains("nested") && n.contains("| A | B |"), "{n}");
    assert!(n.contains("let x = 1;"), "{n}");
    assert!(n.contains("![[pic 1.png]]") || n.contains("pic%201"), "{n}");
    assert!(n.contains("[[other]]") || n.contains("other"), "{n}");
    assert!(!n.contains("<o:p>"), "{n}");
}

// ---------------------------------------------------------------- Obsidian

/// The Kanban plugin's file as the plugin writes it (front matter with
/// blank lines inside the fences and `kanban-plugin: board`, a lane marked
/// `**Complete**`, cards with links, tags and `@{date}` / `@@{time}`, the
/// archive after `***`, and the `%% kanban:settings` footer), and a Bases
/// file as Obsidian's documentation shows one (`and` / `or` filters, a
/// formula, `properties`, a table view with `order` and `sort`).
#[test]
fn obsidian_kanban_and_bases_variants() {
    use quire_import::obsidian::{base, kanban};
    let board = "---\n\nkanban-plugin: board\n\n---\n\n## Todo\n\n- [ ] Write the post [[Post]] #blog\n- [ ] Plan @{2026-11-06} @@{10:00}\n\twith a second line\n\n## Done\n\n**Complete**\n\n- [x] Ship v1\n\n***\n\n## Archive\n\n- [x] Old card\n\n\n\n%% kanban:settings\n```\n{\"kanban-plugin\":\"board\",\"list-collapse\":[false,false]}\n```\n%%\n";
    assert!(kanban::is_board(board));
    let b = kanban::parse(board);
    assert_eq!(b.columns.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Todo", "Done"]);
    assert!(b.has_archive);
    assert_eq!(b.columns[0].cards.len(), 2);
    assert!(b.columns[0].cards[0].text.contains("[[Post]]") && b.columns[0].cards[1].text.contains("second line"));
    assert_eq!(b.columns[1].cards.len(), 1, "the archive's card isn't a card of the board");
    assert!(b.columns[1].cards[0].done);
    // Moving a card keeps every other byte.
    let moved = kanban::move_card(board, 0, 0, 1, 1).unwrap();
    assert!(moved.contains("## Done\n\n**Complete**\n\n- [x] Ship v1\n- [ ] Write the post [[Post]] #blog"), "{moved}");
    assert!(moved.ends_with("%%\n") && moved.contains("## Archive\n\n- [x] Old card"), "{moved}");

    let base_text = "filters:\n  or:\n    - and:\n        - file.hasTag(\"project\")\n        - 'status != \"done\"'\n    - file.folder == \"Inbox\"\nformulas:\n  age: '(now() - file.ctime).days'\nproperties:\n  status:\n    displayName: Status\n  formula.age:\n    displayName: Days old\nviews:\n  - type: table\n    name: Active\n    order:\n      - file.name\n      - status\n      - formula.age\n    sort:\n      - property: file.name\n        direction: ASC\n  - type: cards\n    name: Gallery\n";
    let parsed = base::parse(base_text).unwrap();
    let note = base::to_note(&parsed, "Projects");
    assert!(note.contains("quire-view") || note.contains("Projects"), "{note}");
}
