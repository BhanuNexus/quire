// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text extraction on the fixtures (design §6.2 WS-G acceptance): born-digital
//! pages match their gold text to at least 95 % of characters; a scan is
//! reported as needing OCR; links are found; highlights re-anchor after the
//! PDF is saved again.

use quire_pdf::PdfDoc;
use quire_pdf::annot;
use quire_pdf::find::{anchor_for, find, find_in, reanchor};
use quire_pdf::text::{LinkTarget, extract};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).expect("fixture")
}

fn gold(name: &str) -> String {
    String::from_utf8(fixture(name)).expect("utf-8 gold")
}

/// 1 − edit distance / length, on text with white space runs folded.
fn accuracy(got: &str, want: &str) -> f64 {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (g, w) = (norm(got), norm(want));
    1.0 - strsim::levenshtein(&g, &w) as f64 / w.chars().count().max(1) as f64
}

#[test]
fn born_digital_pages_match_their_gold_text() {
    for (pdf, txt) in [("text.pdf", "text.txt"), ("two-column.pdf", "two-column.txt"), ("cid.pdf", "cid.txt")] {
        let pages = extract(&fixture(pdf)).unwrap();
        let got = pages.iter().map(quire_pdf::PageText::text).collect::<Vec<_>>().join("\n\u{c}\n");
        let want = gold(txt);
        let acc = accuracy(&got, &want);
        eprintln!("{pdf}: {:.1} % of characters right", acc * 100.0);
        assert!(acc >= 0.95, "{pdf}: {acc:.3}\n--- got\n{got}\n--- want\n{want}");
        for p in &pages {
            assert_eq!(p.chars.len(), p.text().chars().count(), "{pdf}: a box per char");
            assert!(!p.image_only);
        }
    }
}

#[test]
fn reading_order_and_spaces() {
    let pages = extract(&fixture("text.pdf")).unwrap();
    assert_eq!(pages.len(), 2);
    assert!(pages[0].text().contains("Words spaced by gaps."), "gaps become spaces: {}", pages[0].text());
    let cols = extract(&fixture("two-column.pdf")).unwrap();
    let t = cols[0].text();
    let left = t.find("moving to the right one.").unwrap();
    let right = t.find("The right column holds").unwrap();
    assert!(left < right, "the left column is read first:\n{t}");
}

#[test]
fn scans_are_reported_for_ocr() {
    let pages = extract(&fixture("scanned.pdf")).unwrap();
    assert_eq!(pages.len(), 1);
    assert!(pages[0].image_only);
    assert!(pages[0].text().is_empty());
}

#[test]
fn links_and_page_geometry() {
    let doc = PdfDoc::open(fixture("text.pdf")).unwrap();
    let info = doc.page_info(0).unwrap();
    assert_eq!((info.width, info.height), (612.0, 792.0));
    let (x, y) = info.view_point(72.0, 720.0);
    assert!((x - 72.0).abs() < 0.01 && (y - 72.0).abs() < 0.01);
    let links = doc.page_links(0);
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(links[0].target, LinkTarget::Uri("https://en.wikipedia.org/wiki/Rope_(data_structure)".into()));
    assert_eq!(links[1].target, LinkTarget::Page(1));
    assert!(doc.page_links(1).is_empty());
}

#[test]
fn a_highlight_round_trips_through_the_note_and_re_anchors_after_a_re_save() {
    let v1 = extract(&fixture("text.pdf")).unwrap();
    let hit = find(&v1, "undo is cheap because subtrees").remove(0);
    let a = anchor_for(&v1[hit.page], hit.page, hit.start..hit.end, "yellow");
    assert_eq!(a.p, 1);
    assert_eq!(a.t, "undo is cheap because subtrees");
    assert_eq!(a.q.len(), 1);
    let q = a.q[0];
    assert!(q[0] > 72.0 && q[1] > 600.0 && q[2] > q[0] && q[3] > q[1], "{q:?}");

    let pdf_path = "papers/text.pdf".into();
    let note = annot::new_note(&pdf_path, &quire_pdf::sha256_hex(&fixture("text.pdf")));
    let note = annot::add(&note, &pdf_path, "h0001", &a, &a.t, "Why undo is cheap.");
    let back = annot::entries(&note);
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].anchor, a, "the anchor survives the note");

    // The PDF is saved again with a new first line: the highlight moves down.
    let v2 = extract(&fixture("text-v2.pdf")).unwrap();
    let b = reanchor(&back[0].anchor, &v2).expect("found again");
    assert_eq!(b.t, a.t);
    assert!(b.q[0][1] < q[1], "lower on the page now: {:?} vs {q:?}", b.q[0]);
    let note2 = annot::move_anchor(&note, &pdf_path, "h0001", &b).unwrap();
    let note2 = annot::set_pdf_sha(&note2, &quire_pdf::sha256_hex(&fixture("text-v2.pdf")));
    let e = &annot::entries(&note2)[0];
    assert_eq!(e.anchor, b);
    assert_eq!(e.comment, "Why undo is cheap.");
    // The highlight's boxes cover the quoted text on the new page.
    let page = &v2[usize::try_from(b.p).unwrap() - 1];
    let r = find_in(&page.text(), &a.t).remove(0);
    assert_eq!(page.quads(r), b.q);
}

#[test]
fn damaged_files_are_errors_not_crashes() {
    let mut bytes = fixture("text.pdf");
    bytes.truncate(bytes.len() / 2);
    // A truncated file either opens with what can be read, or says it can't.
    let _ = extract(&bytes);
    assert!(PdfDoc::open(b"%PDF-1.4\n garbage".to_vec()).is_err() || extract(b"%PDF-1.4\n garbage").is_ok());
    assert!(PdfDoc::open(b"not a pdf at all".to_vec()).is_err());
}

#[test]
fn extraction_speed() {
    let bytes = fixture("cid.pdf");
    let doc = PdfDoc::open(bytes).unwrap();
    let t = std::time::Instant::now();
    for i in 0..doc.page_count() {
        doc.page_text(i).unwrap();
    }
    let per_page = t.elapsed().as_secs_f64() * 1000.0 / doc.page_count() as f64;
    // §9: ≤ 50 ms a page in release; a dev build of this crate gets slack.
    assert!(per_page < 500.0, "{per_page:.1} ms a page");
    eprintln!("text extraction: {per_page:.1} ms a page (dev build)");
}

#[cfg(feature = "render")]
#[test]
fn renders_pages_and_survives_odd_files() {
    let doc = PdfDoc::open(fixture("text.pdf")).unwrap();
    let img = quire_pdf::render_page(&doc, 0, 1.0).unwrap();
    assert_eq!((img.width, img.height), (612, 792));
    assert_eq!(img.rgba.len(), 612 * 792 * 4);
    assert!(img.rgba.chunks(4).any(|p| p[0] < 100), "some dark text pixels");
    let scan = PdfDoc::open(fixture("scanned.pdf")).unwrap();
    assert!(quire_pdf::render_page(&scan, 0, 0.5).is_ok());
    assert!(quire_pdf::render_page(&doc, 9, 1.0).is_err(), "no such page");
}
