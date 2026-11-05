// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDFs in the vault are searchable over MCP (integration wave 2): the
//! server reads their text in the background when it opens, `search` with
//! `kind: all` lists a page as "paper.pdf p. 1" with `page` in the
//! structured hit, and a bad `where` is refused with what's wrong. With many
//! PDFs (0.10) the server answers at once and says how far reading got.
#![expect(clippy::unwrap_used, reason = "a test: any failure should fail it")]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use quire_agent::DeviceInfo;
use quire_app::vault::{NotePath, Vault};
use quire_mcp::{Server, ServerConfig};
use serde_json::{Value, json};

fn pdf() -> Vec<u8> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../quire-pdf/tests/fixtures/cid.pdf")).unwrap()
}

#[test]
fn pdf_pages_are_found_with_their_page_number() {
    let v = Vault::in_memory(&[("Editors.md", "# Editors\n\nRopes and gap buffers.\n")]);
    v.storage().write_bytes_atomic(&NotePath::new("papers/paper.pdf"), &pdf()).unwrap();
    let cfg = ServerConfig { device: Some(DeviceInfo { id: "d_pdf".into(), name: "t".into() }), ..ServerConfig::default() };
    let srv = Server::with_config(v, None, cfg);
    assert!(srv.wait_for_pdfs(Duration::from_mins(1)));
    let call = |id: u64, args: Value| {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "search", "arguments": args,
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "agent", "version": "1"}}}});
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
        serde_json::from_slice::<Value>(&out).unwrap()
    };

    let r = call(1, json!({"query": "piece table", "filters": {"kind": "all"}}));
    let hits = r["result"]["structuredContent"]["hits"].as_array().unwrap().clone();
    let page = hits.iter().find(|h| h["path"] == "papers/paper.pdf").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(page["page"], 1);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("- paper.pdf p. 1"), "{text}");

    // Notes only (the default): no PDF pages, and notes carry no `page`.
    let r = call(2, json!({"query": "ropes"}));
    let hits = r["result"]["structuredContent"]["hits"].as_array().unwrap().clone();
    assert!(!hits.is_empty() && hits.iter().all(|h| h.get("page").is_none()), "{r}");

    // A `where` that doesn't parse is refused with the reason.
    let r = call(3, json!({"query": "ropes", "filters": {"where": "status is done"}}));
    assert_eq!(r["result"]["structuredContent"]["error"], "invalid_args", "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("`filters.where`"), "{r}");
}

/// A one-page PDF with `text` in Helvetica (as `make_fixtures.py` writes
/// them), then 40 lines of filler, so reading 200 takes a moment.
fn small_pdf(text: &str) -> Vec<u8> {
    let mut content = format!("BT /F1 12 Tf 14 TL 72 740 Td ({text}) Tj T*");
    for i in 0..40 {
        let _ = write!(content, " (Line {i} of a small paper about editors, buffers and undo.) Tj T*");
    }
    content.push_str(" ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ];
    let mut out = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{body}\nendobj\n", i + 1);
    }
    let xref = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for o in offsets {
        let _ = writeln!(out, "{o:010} 00000 n ");
    }
    let _ = write!(out, "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
    out.into_bytes()
}

#[test]
fn the_server_answers_while_it_reads_200_pdfs() {
    let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet"];
    let tag = |i: usize| format!("{}{}", words[i / 100 % 10], words[i / 10 % 10]) + words[i % 10];
    let v = Vault::in_memory(&[("Editors.md", "# Editors\n\nRopes and gap buffers.\n")]);
    for i in 0..200 {
        let text = format!("Paper {i} is about {} and ropes", tag(i));
        v.storage().write_bytes_atomic(&NotePath::new(format!("papers/p{i:03}.pdf")), &small_pdf(&text)).unwrap();
    }
    let cfg = ServerConfig { device: Some(DeviceInfo { id: "d_pdfs".into(), name: "t".into() }), ..ServerConfig::default() };
    let t = Instant::now();
    let srv = Server::with_config(v.clone(), None, cfg.clone());
    let opened = t.elapsed();
    let search = |srv: &Server, q: &str| {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "search", "arguments": {"query": q, "filters": {"kind": "all"}, "limit": 50},
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "agent", "version": "1"}}}});
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
        serde_json::from_slice::<Value>(&out).unwrap()["result"].clone()
    };

    // Asked at once: an answer (notes are there), and how far PDFs got.
    let r = search(&srv, "ropes");
    let answered = t.elapsed();
    let ix = &r["structuredContent"]["indexing"];
    assert!(ix.is_object(), "reading 200 PDFs should still be under way: {r}");
    assert!(ix["done"].as_u64().unwrap() < 200 && ix["total"].as_u64().unwrap() <= 200, "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("still reading the vault's PDFs"), "{text}");
    assert!(text.contains("Editors.md"), "notes are searchable at once: {text}");
    // Notes-only searches are complete, so they don't mention it.
    let notes = {
        let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "search", "arguments": {"query": "ropes"},
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "agent", "version": "1"}}}});
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
        serde_json::from_slice::<Value>(&out).unwrap()["result"].clone()
    };
    assert!(notes["structuredContent"].get("indexing").is_none(), "{notes}");

    // Then every PDF is found, and the note is gone.
    assert!(srv.wait_for_pdfs(Duration::from_mins(2)), "PDFs were not read in time");
    let all_read = t.elapsed();
    for i in [0, 57, 199] {
        let r = search(&srv, &tag(i));
        let hits = r["structuredContent"]["hits"].as_array().unwrap();
        assert!(hits.iter().any(|h| h["path"] == format!("papers/p{i:03}.pdf")), "{i}: {r}");
        assert!(r["structuredContent"].get("indexing").is_none(), "{r}");
    }
    eprintln!("200 PDFs: server open {opened:?}, first answer {answered:?}, all read {all_read:?}");

    // A second start reads the cache: still in the background, and quicker.
    let t = Instant::now();
    let again = Server::with_config(v, None, cfg);
    assert!(again.wait_for_pdfs(Duration::from_mins(1)));
    eprintln!("200 PDFs from the cache: all read {:?}", t.elapsed());
}
