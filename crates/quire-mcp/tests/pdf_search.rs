// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! PDFs in the vault are searchable over MCP (integration wave 2): the
//! server reads their text when it opens, `search` with `kind: all` lists a
//! page as "paper.pdf p. 1" with `page` in the structured hit, and a bad
//! `where` is refused with what's wrong.
#![expect(clippy::unwrap_used, reason = "a test: any failure should fail it")]

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
