// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Round trips over the fixtures, Excalidraw's edit rules, and undo.

use super::*;

/// Every fixture: (name, text).
const CORPUS: &[(&str, &str)] = &[
    ("Release.excalidraw", include_str!("../../testdata/excalidraw/Release.excalidraw")),
    ("legacy.excalidraw", include_str!("../../testdata/excalidraw/legacy.excalidraw")),
    ("shapes.excalidraw", include_str!("../../testdata/excalidraw/shapes.excalidraw")),
    ("Roadmap sketch.excalidraw.md", include_str!("../../testdata/excalidraw/Roadmap sketch.excalidraw.md")),
    ("Roadmap plain.excalidraw.md", include_str!("../../testdata/excalidraw/Roadmap plain.excalidraw.md")),
    ("Commented.excalidraw.md", include_str!("../../testdata/excalidraw/Commented.excalidraw.md")),
    ("Sketch.excalidraw.md", include_str!("../../testdata/excalidraw/Sketch.excalidraw.md")),
    ("Plain.excalidraw.md", include_str!("../../testdata/excalidraw/Plain.excalidraw.md")),
];

fn cx() -> Cx {
    Cx::new(1_800_000_000_000, 42)
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn fixture(name: &str) -> Drawing {
    Drawing::parse(CORPUS.iter().find(|(n, _)| *n == name).expect("a fixture").1).expect("reads")
}

/// Open → save without edits: byte for byte.
#[test]
fn every_fixture_writes_back_byte_identical() {
    for (name, text) in CORPUS {
        let d = Drawing::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(d.to_text(), *text, "{name}");
        // The JSON itself, written again from the elements, is the same too.
        let json = d.json();
        assert!(ojson::parse(&json).is_ok(), "{name}");
    }
}

/// Re-encoding (not just keeping the block) gives the same bytes: the
/// JSON writer is `JSON.stringify`'s, the compressor lz-string's.
#[test]
fn rewritten_blocks_match_the_originals() {
    for (name, text) in CORPUS {
        let d = Drawing::parse(text).unwrap();
        let json = d.json();
        assert_eq!(json, d.json_read, "{name}: the scene's JSON written again");
        if let Form::Note(n) = &d.form
            && n.encoding == Encoding::Compressed
            && name.starts_with("Roadmap")
        {
            assert_eq!(format!("{}\n", excalidraw::lz_compress_note(&json)), n.body, "{name}: compressed again");
        }
    }
}

#[test]
fn notes_keep_their_form_and_sections() {
    let d = fixture("Roadmap sketch.excalidraw.md");
    assert_eq!(d.encoding(), Some(Encoding::Compressed));
    assert_eq!(fixture("Roadmap plain.excalidraw.md").encoding(), Some(Encoding::Plain));
    assert_eq!(fixture("Release.excalidraw").encoding(), None);
    assert_eq!(d.wikilinks(), ["Meeting notes#Decisions", "Roadmap"]);
    assert!(matches!(d.file_ref("3f2a9c"), Some(excalidraw::FileRef::Vault(p)) if p == "attachments/dot.png"));
    // Both forms are the same drawing.
    assert_eq!(d.state.elements, fixture("Roadmap plain.excalidraw.md").state.elements);
    assert_eq!(d.scene().elements.len(), 4);
}

/// An edit changes the edited element only; every other byte of the
/// drawing's JSON stays, unknown fields and element types included.
#[test]
fn an_edit_changes_only_that_element() {
    let mut d = fixture("Release.excalidraw");
    let before = d.to_text();
    let mut c = cx();
    d.translate(&ids(&["dia1"]), 10.0, 0.0, &mut c);
    let after = d.to_text();
    let changed: Vec<(&str, &str)> = before.lines().zip(after.lines()).filter(|(a, b)| a != b).collect();
    assert_eq!(before.lines().count(), after.lines().count());
    assert_eq!(changed.len(), 4, "x, version, versionNonce, updated: {changed:?}");
    assert!(changed.iter().any(|(a, b)| a.contains("\"x\": 560") && b.contains("\"x\": 570")));
    assert!(changed.iter().any(|(a, b)| a.contains("\"version\": 12") && b.contains("\"version\": 13")));
    assert!(changed.iter().any(|(_, b)| b.contains("\"updated\": 1800000000000")));
    for kept in [
        "\"customData\"",
        "\"hexagon-from-the-future\"",
        "\"sides\": 6",
        "\"validated\": true",
        "\"lockedMultiSelections\": {}",
        "\"gone\"",
    ] {
        assert!(after.contains(kept), "{kept}");
    }
}

#[test]
fn a_compressed_note_stays_compressed_and_its_text_sections_follow() {
    let mut d = fixture("Roadmap sketch.excalidraw.md");
    let mut c = cx();
    d.set_text("pTq8nWm2", "now with [[Inbox|a link]]", (200.0, 25.0), &mut c);
    let t = d.to_text();
    assert!(t.starts_with("---\n\nexcalidraw-plugin: parsed\n"), "the note's own text is kept");
    assert!(t.contains("Some notes of my own, above the drawing: see [[Inbox]].\n\n# Excalidraw Data\n\n## Text Elements\n"));
    assert!(t.contains("now with [[Inbox|a link]] ^pTq8nWm2\n\n"), "{t}");
    assert!(t.contains("pTq8nWm2: [[Inbox]]\n\n"), "the text's link is an element link");
    assert!(t.contains("## Embedded Files\n3f2a9c: [[attachments/dot.png]]\n\n%%\n## Drawing\n```compressed-json\n"));
    assert!(t.ends_with("\n```\n%%"));
    let again = Drawing::parse(&t).unwrap();
    let e = again.get("pTq8nWm2").unwrap();
    assert_eq!(e.str("rawText"), Some("now with [[Inbox|a link]]"));
    assert_eq!(e.str("text"), Some("now with a link"));
    assert_eq!(again.wikilinks(), ["Inbox", "Meeting notes#Decisions", "Roadmap"]);
    // The plugin's 256-character lines with a blank line between.
    let block = t.split("```compressed-json\n").nth(1).unwrap().split("\n```").next().unwrap();
    assert!(block.split("\n\n").all(|l| l.len() <= 256 && !l.contains('\n')));
    // Plain stays plain, with a tab indent.
    let mut p = fixture("Roadmap plain.excalidraw.md");
    p.translate(&ids(&["Kq3sXz1a"]), 1.0, 1.0, &mut c);
    let t = p.to_text();
    assert!(t.contains("```json\n{\n\t\"type\": \"excalidraw\""), "{t}");
    // The commented-out form keeps its `%%` place.
    let mut k = fixture("Commented.excalidraw.md");
    k.translate(&ids(&["Kq3sXz1a"]), 1.0, 1.0, &mut c);
    let t = k.to_text();
    assert!(t.contains("\n%%\n# Excalidraw Data\n") && t.contains("^pTq8nWm2\n\n## Drawing\n```compressed-json\n"), "{t}");
    assert!(Drawing::parse(&t).is_ok());
}

/// What Excalidraw's `mutateElement` does on each edit, and `newElement`
/// on each new one.
#[test]
fn edits_follow_excalidraws_rules() {
    let mut d = fixture("Release.excalidraw");
    let mut c = cx();
    let old = Arc::clone(d.get("ell1").unwrap());
    d.set_field(&ids(&["ell1"]), "strokeWidth", &Json::num(4.0), &mut c);
    let new = d.get("ell1").unwrap();
    assert_eq!(new.num("version"), Some(old.num("version").unwrap() + 1.0));
    assert_ne!(new.num("versionNonce"), old.num("versionNonce"));
    assert_eq!(new.num("updated"), Some(1_800_000_000_000.0));
    assert_eq!(new.num("seed"), old.num("seed"), "the seed stays (the shape's look)");
    let r = d.new_element("rectangle", 5.0, 6.0, &Style::default(), &mut c);
    for k in [
        "id",
        "type",
        "x",
        "y",
        "width",
        "height",
        "angle",
        "strokeColor",
        "backgroundColor",
        "fillStyle",
        "strokeWidth",
        "strokeStyle",
        "roughness",
        "opacity",
        "groupIds",
        "frameId",
        "index",
        "roundness",
        "seed",
        "version",
        "versionNonce",
        "isDeleted",
        "boundElements",
        "updated",
        "link",
        "locked",
    ] {
        assert!(r.get(k).is_some(), "{k}");
    }
    assert_eq!(r.str("id").unwrap().len(), 21);
    assert!(r.num("seed").unwrap() >= 0.0 && r.num("seed").unwrap() < 2f64.powi(31));
    let id = d.add(r, &mut c);
    // A fractional index after the last one.
    let idx = d.get(&id).unwrap().str("index").unwrap().to_string();
    assert!(idx.as_str() > "aC" && fractional::valid(&idx), "{idx}");
    // In a note, a new text gets the plugin's 8-character id.
    let n = fixture("Roadmap sketch.excalidraw.md");
    let tid = n.new_element("text", 0.0, 0.0, &Style::default(), &mut c).str("id").unwrap().to_string();
    assert!(tid.len() == 8 && tid.bytes().all(|b| b.is_ascii_alphanumeric()), "{tid}: a block reference");
    // A legacy drawing without indices gets none.
    let mut l = fixture("legacy.excalidraw");
    let e = l.new_element("ellipse", 0.0, 0.0, &Style::default(), &mut c);
    assert!(e.get("index").is_none());
    let id = l.add(e, &mut c);
    assert!(l.get(&id).unwrap().get("index").is_none());
}

#[test]
fn fractional_keys_sort_between_their_neighbours() {
    use fractional::between;
    assert_eq!(between(None, None).as_deref(), Some("a0"));
    assert_eq!(between(Some("a0"), None).as_deref(), Some("a1"));
    assert_eq!(between(Some("a0"), Some("a1")).as_deref(), Some("a0V"));
    assert_eq!(between(None, Some("a0")).as_deref(), Some("Zz"));
    assert_eq!(between(Some("az"), None).as_deref(), Some("b00"));
    assert_eq!(between(Some("Zz"), None).as_deref(), Some("a0"));
    assert_eq!(between(Some("a0V"), Some("a1")).as_deref(), Some("a0l"));
    assert_eq!(between(Some("a1"), Some("a0")), None);
    // Many inserts at the same place stay ordered and valid.
    let mut lo = "a0".to_string();
    let hi = "a1".to_string();
    for _ in 0..200 {
        let k = between(Some(&lo), Some(&hi)).unwrap();
        assert!(k > lo && k < hi && fractional::valid(&k), "{lo} < {k} < {hi}");
        lo = k;
    }
}

#[test]
fn z_order_keeps_indices_in_array_order() {
    let mut d = fixture("Release.excalidraw");
    let mut c = cx();
    let check = |d: &Drawing| {
        let keys: Vec<&str> = d.state.elements.iter().filter_map(|e| e.str("index")).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
    };
    d.reorder(&ids(&["box1"]), Z::Front, &mut c);
    assert_eq!(d.state.elements.last().unwrap().str("id"), Some("box1"));
    check(&d);
    d.reorder(&ids(&["box1"]), Z::Back, &mut c);
    assert_eq!(d.state.elements[0].str("id"), Some("box1"));
    check(&d);
    d.reorder(&ids(&["box1"]), Z::Forward, &mut c);
    assert_eq!(d.state.elements[1].str("id"), Some("box1"));
    check(&d);
    d.reorder(&ids(&["box1"]), Z::Backward, &mut c);
    assert_eq!(d.state.elements[0].str("id"), Some("box1"));
    check(&d);
}

#[test]
fn bound_text_and_arrows_follow_their_shapes() {
    let mut d = fixture("Release.excalidraw");
    let mut c = cx();
    d.translate(&ids(&["box1"]), 100.0, 50.0, &mut c);
    let lbl = d.get("lbl1").unwrap();
    assert_eq!((lbl.num("x"), lbl.num("y")), (Some(152.5), Some(82.5)), "the label moves with its box");
    let a = d.get("arr1").unwrap();
    let start = (a.num("x").unwrap(), a.num("y").unwrap());
    // The arrow now starts on the moved box's outline (gap 4 outside it).
    let (bx, by, bw, bh) = bbox(d.get("box1").unwrap());
    assert!(start.0 >= bx - 5.0 && start.0 <= bx + bw + 5.0 && start.1 >= by - 5.0 && start.1 <= by + bh + 5.0, "{start:?}");
    // Deleting the box deletes its label and lets the arrow go.
    d.erase(&ids(&["box1"]), &mut c);
    assert_eq!(d.get("lbl1").unwrap().bool("isDeleted"), Some(true));
    assert!(d.get("arr1").unwrap().get("startBinding").is_some_and(Json::is_null));
    assert!(d.get("arr1").unwrap().obj("endBinding").is_some());
    // An arrow moved alone lets go too; binding again puts it back on.
    d.translate(&ids(&["arr1"]), 0.0, 300.0, &mut c);
    assert!(d.get("arr1").unwrap().get("endBinding").is_some_and(Json::is_null));
    assert!(d.get("ell1").unwrap().arr("boundElements").unwrap().is_empty());
    d.bind("arr1", false, "ell1", &mut c);
    assert_eq!(d.get("arr1").unwrap().obj("endBinding").unwrap().str("elementId"), Some("ell1"));
    assert_eq!(d.get("ell1").unwrap().arr("boundElements").unwrap().len(), 1);
}

#[test]
fn duplicate_group_align_lock_and_paste() {
    let mut d = fixture("Release.excalidraw");
    let mut c = cx();
    let n = d.state.elements.len();
    let copies = d.duplicate(&ids(&["box1"]), 10.0, 10.0, &mut c);
    assert_eq!(copies.len(), 1);
    assert_eq!(d.state.elements.len(), n + 2, "the box and its label");
    let copy = d.get(&copies[0]).unwrap();
    let label = d.state.elements.last().unwrap();
    assert_eq!(label.str("containerId"), Some(copies[0].as_str()));
    assert_eq!(copy.arr("boundElements").unwrap().len(), 1, "the label only: the arrow isn't copied");
    assert_ne!(copy.arr("groupIds"), d.get("box1").unwrap().arr("groupIds"), "a new group");
    let g = d.group(&ids(&["ell1", "dia1"]), &mut c);
    assert_eq!(d.group_of("ell1").len(), 2);
    assert_eq!(d.get("dia1").unwrap().arr("groupIds").unwrap().last().and_then(Json::as_str), Some(g.as_str()));
    d.ungroup(&ids(&["ell1", "dia1"]), &mut c);
    assert_eq!(d.group_of("ell1"), ["ell1"]);
    d.align(&ids(&["ell1", "dia1", "lin1"]), Align::Left, &mut c);
    assert!((aabb(d.get("dia1").unwrap()).0 - aabb(d.get("lin1").unwrap()).0).abs() < 1e-9);
    d.lock(&ids(&["dia1"]), true, &mut c);
    assert_eq!(d.get("dia1").unwrap().bool("locked"), Some(true));
    let clip = clipboard_json(&[Arc::clone(d.get("img1").unwrap())], &d.state.files);
    let (els, files) = from_clipboard(&clip).unwrap();
    assert!(files.get("f00d").is_some(), "the picture goes with it");
    let pasted = d.paste(els, &files, (1000.0, 1000.0), &mut c);
    let p = d.get(&pasted[0]).unwrap();
    assert_eq!((p.num("x"), p.num("y")), (Some(1000.0), Some(1000.0)));
    assert_ne!(pasted[0], "img1");
}

#[test]
fn resize_and_rotate() {
    let mut d = fixture("Release.excalidraw");
    let mut c = cx();
    d.resize("lin1", (0.0, 200.0, 440.0, 120.0), &mut c);
    assert_eq!(points(d.get("lin1").unwrap()), [(0.0, 0.0), (220.0, 120.0), (440.0, 0.0)]);
    assert_eq!(d.get("lin1").unwrap().num("width"), Some(440.0));
    d.resize("box1", (0.0, 0.0, 400.0, 180.0), &mut c);
    let lbl = d.get("lbl1").unwrap();
    assert_eq!((lbl.num("x"), lbl.num("y")), (Some(152.5), Some(77.5)), "the label stays centred");
    d.resize("txt1", (420.0, 220.0, 360.0, 100.0), &mut c);
    assert_eq!(d.get("txt1").unwrap().num("fontSize"), Some(40.0));
    d.rotate(&ids(&["dia1"]), std::f64::consts::FRAC_PI_2, center(d.get("dia1").unwrap()), &mut c);
    assert!((d.get("dia1").unwrap().num("angle").unwrap() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    assert!(hit(d.get("dia1").unwrap(), (620.0, 60.0), 2.0), "filled: hit inside");
    assert!(!hit(d.get("ell1").unwrap(), (400.0, 50.0), 2.0), "transparent: not hit in the middle");
    assert!(hit(d.get("ell1").unwrap(), (320.0, 50.0), 3.0), "but on its outline");
    assert!(hit(d.get("lin1").unwrap(), (220.0, 320.0), 3.0));
}

/// A small fixed-seed generator for the property tests.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, v: &'a [String]) -> &'a String {
        &v[(self.next() % v.len() as u64) as usize]
    }
}

/// One random edit, as the editor makes them.
fn random_edit(d: &mut Drawing, rng: &mut Rng, c: &mut Cx) {
    let all: Vec<String> = live(&d.state.elements).filter_map(|e| e.str("id").map(str::to_string)).collect();
    if all.is_empty() {
        let r = d.new_element("rectangle", 0.0, 0.0, &Style::default(), c);
        d.add(r, c);
        return;
    }
    let one = vec![rng.pick(&all).clone()];
    let two = vec![rng.pick(&all).clone(), rng.pick(&all).clone()];
    let f = (rng.next() % 200) as f64 - 100.0;
    match rng.next() % 16 {
        0 => d.translate(&two, f, -f / 2.0, c),
        1 => d.resize(&one[0], (f, f, 50.0 + f.abs(), 20.0), c),
        2 => d.rotate(&two, f / 50.0, (0.0, 0.0), c),
        3 => d.erase(&one, c),
        4 => {
            d.duplicate(&two, 5.0, 5.0, c);
        }
        5 => {
            d.group(&two, c);
        }
        6 => d.ungroup(&two, c),
        7 => d.reorder(&two, [Z::Front, Z::Back, Z::Forward, Z::Backward][(rng.next() % 4) as usize], c),
        8 => d.align(&two, Align::CenterY, c),
        9 => d.lock(&one, rng.next().is_multiple_of(2), c),
        10 => d.set_field(&two, "strokeColor", &Json::str("#e03131"), c),
        11 => {
            let k = ["rectangle", "ellipse", "diamond", "arrow", "line", "freedraw", "text"][(rng.next() % 7) as usize];
            let e = d.new_element(k, f, f, &Style::default(), c);
            d.add(e, c);
        }
        12 => {
            if d.get(&one[0]).is_some_and(|e| e.str("type") == Some("text")) {
                d.set_text(&one[0], "hello [[World]]", (100.0, 25.0), c);
            }
        }
        13 => {
            let arrows: Vec<String> = live(&d.state.elements)
                .filter(|e| e.str("type") == Some("arrow"))
                .filter_map(|e| e.str("id").map(str::to_string))
                .collect();
            if !arrows.is_empty() && one[0] != arrows[0] {
                d.bind(&arrows[0], rng.next().is_multiple_of(2), &one[0], c);
            }
        }
        14 => {
            d.add_file("pics/a.png", "image/png", Some("data:image/png;base64,AA".into()), c);
        }
        _ => {
            let clip = clipboard_json(&d.state.elements[..1], &d.state.files);
            if let Some((els, files)) = from_clipboard(&clip) {
                d.paste(els, &files, (f, f), c);
            }
        }
    }
}

/// Property: after any sequence of edits, undoing them all gives back the
/// exact state (and the exact file text); redoing them all gives back the
/// edited state.
#[test]
fn undo_restores_the_exact_state() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for (case, (name, text)) in CORPUS.iter().cycle().take(40).enumerate() {
        let mut d = Drawing::parse(text).unwrap();
        let mut c = Cx::new(1_800_000_000_000 + case as i64, rng.next());
        let mut h = History::default();
        let start = d.state.clone();
        let start_text = d.to_text();
        let steps = 1 + (rng.next() % 30) as usize;
        let mut states = vec![start.clone()];
        for _ in 0..steps {
            h.checkpoint(&d.state);
            random_edit(&mut d, &mut rng, &mut c);
            c.now_ms += 1;
            states.push(d.state.clone());
        }
        let end = d.state.clone();
        let end_text = d.to_text();
        assert!(Drawing::parse(&end_text).is_ok(), "{name} case {case}: the edited file reads");
        for i in (0..steps).rev() {
            assert!(h.undo(&mut d.state));
            assert_eq!(d.state, states[i], "{name} case {case}: undo step {i}");
        }
        assert!(!h.undo(&mut d.state));
        assert_eq!(d.state, start);
        assert_eq!(d.to_text(), start_text, "{name} case {case}: back to the same bytes");
        for _ in 0..steps {
            assert!(h.redo(&mut d.state));
        }
        assert_eq!(d.state, end);
        assert_eq!(d.to_text(), end_text);
    }
}

/// Property: random edits keep the drawing valid as Excalidraw reads it:
/// unique ids, versions only grow, indices in order, bindings point at
/// elements that exist, and the file reads back the same.
#[test]
fn edits_keep_the_drawing_valid() {
    let mut rng = Rng(0xFACE_FEED_0000_0042);
    for (case, (name, text)) in CORPUS.iter().cycle().take(32).enumerate() {
        let mut d = Drawing::parse(text).unwrap();
        let mut c = Cx::new(1_800_000_000_000, rng.next());
        for _ in 0..25 {
            let versions: Vec<(String, f64)> =
                d.state.elements.iter().filter_map(|e| Some((e.str("id")?.to_string(), e.num("version").unwrap_or(0.0)))).collect();
            random_edit(&mut d, &mut rng, &mut c);
            for (id, v) in versions {
                assert!(d.get(&id).is_some_and(|e| e.num("version").unwrap_or(0.0) >= v), "{name} {case}: {id}'s version went down");
            }
        }
        let all_ids: Vec<&str> = d.state.elements.iter().filter_map(|e| e.str("id")).collect();
        let mut sorted = all_ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all_ids.len(), "{name} {case}: ids are unique");
        let keys: Vec<&str> = d.state.elements.iter().filter_map(|e| e.str("index")).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{name} {case}: {keys:?}");
        for e in live(&d.state.elements) {
            for b in ["startBinding", "endBinding"] {
                if let Some(t) = e.obj(b).and_then(|b| b.str("elementId")) {
                    assert!(all_ids.contains(&t), "{name} {case}: {b} → {t}");
                }
            }
        }
        let again = Drawing::parse(&d.to_text()).unwrap();
        assert_eq!(again.state.elements, d.state.elements, "{name} {case}");
    }
}

#[test]
fn new_drawings_read_back_and_have_the_plugins_front_matter() {
    let note = Drawing::new_empty(true);
    let t = note.to_text();
    assert!(t.starts_with("---\n\nexcalidraw-plugin: parsed\n") && t.contains("```compressed-json\n"));
    assert!(Drawing::parse(&t).unwrap().is_note());
    let file = Drawing::new_empty(false);
    assert!(file.to_text().starts_with("{\n  \"type\": \"excalidraw\""));
    assert_eq!(indent_of(&file.to_text()), Indent::Unit("  ".into()));
    assert!(Drawing::parse("# no drawing").is_err());
    assert!(Drawing::parse("## Drawing\n```json\n{\"elements\": 3}\n```\n").is_err());
}

#[test]
fn display_text_hides_link_brackets() {
    assert_eq!(display_text("See [[A]] and [[B|bee]] [[open"), "See A and bee [[open");
    assert_eq!(wikilinks_in("x [[A#h|t]] [[ ]] [[B]]"), ["A#h", "B"]);
}
