// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Model Context Protocol server over stdio (`quire-cli mcp <vault>`).
//!
//! Lets AI assistants (Claude Desktop, Claude Code, Cursor…) search, read and
//! ask questions of one vault. Nothing runs unless the user adds this command
//! to their client. Writing is off unless started with `--allow-write`, and
//! every write is snapshotted in version history first so it can be undone.

use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use quire_app::find::{FindOptions, compile, replace_in_string};
use quire_app::history_ops::now_ms;
use quire_app::index::IndexService;
use quire_app::intel::{extractive_answer, retrieve};
use quire_app::vault::{NotePath, Vault, VaultEvent, VaultWatcher};

use serde_json::{Value, json};

const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) struct Server {
    vault: Vault,
    index: IndexService,
    watcher: Option<VaultWatcher>,
    allow_write: bool,
}

fn text(s: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": s.into() }], "isError": false })
}

fn tool_error(s: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": s.into() }], "isError": true })
}

fn schema(props: &Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

impl Server {
    pub(crate) fn open(root: &str, allow_write: bool) -> Result<Self, String> {
        let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
        let index = IndexService::spawn();
        index.load(vault.clone());
        index.flush_semantic();
        let watcher = VaultWatcher::start(&PathBuf::from(root).canonicalize().map_err(|e| e.to_string())?).ok();
        Ok(Self { vault, index, watcher, allow_write })
    }

    #[cfg(test)]
    fn in_memory(vault: Vault, allow_write: bool) -> Self {
        let index = IndexService::spawn();
        index.load(vault.clone());
        index.flush_semantic();
        Self { vault, index, watcher: None, allow_write }
    }

    /// Pick up edits made in the app or elsewhere since the last call.
    fn refresh(&self) {
        let Some(w) = &self.watcher else { return };
        let mut changed = false;
        for ev in w.events.try_iter() {
            changed = true;
            match ev {
                VaultEvent::Changed(p) => {
                    if let Ok(src) = self.vault.read(&p) {
                        self.index.upsert(p, src);
                    }
                }
                VaultEvent::Removed(p) => self.index.remove(p),
                VaultEvent::AttachmentChanged(_) | VaultEvent::AttachmentRemoved(_) => {}
            }
        }
        if changed {
            self.index.flush();
        }
    }

    fn tools(&self) -> Vec<Value> {
        let mut v = vec![
            json!({"name": "search_notes", "description": "Search the user's notes by keywords and meaning. Supports \"phrases\", #tag filters and -exclusions. Returns note paths, sections and snippets.",
                   "inputSchema": schema(&json!({"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 50}}), &["query"])}),
            json!({"name": "read_note", "description": "Read a note's full Markdown by path (e.g. \"Projects/Plan.md\") or by title.",
                   "inputSchema": schema(&json!({"note": {"type": "string"}}), &["note"])}),
            json!({"name": "list_notes", "description": "List notes, optionally only in a folder or with a tag.",
                   "inputSchema": schema(&json!({"folder": {"type": "string"}, "tag": {"type": "string"}, "limit": {"type": "integer"}}), &[])}),
            json!({"name": "backlinks", "description": "Notes that link to a note, with the linking line.",
                   "inputSchema": schema(&json!({"note": {"type": "string"}}), &["note"])}),
            json!({"name": "ask_notes", "description": "Retrieve the passages most relevant to a question, with citations like [[Note#Heading]] and a short extractive answer. Use it to ground your own answer in the user's notes.",
                   "inputSchema": schema(&json!({"question": {"type": "string"}}), &["question"])}),
            json!({"name": "list_tags", "description": "All tags with note counts.", "inputSchema": schema(&json!({}), &[])}),
        ];
        if self.allow_write {
            v.push(json!({"name": "create_note", "description": "Create a new note. Never overwrites: a unique name is chosen.",
                          "inputSchema": schema(&json!({"title": {"type": "string"}, "content": {"type": "string"}, "folder": {"type": "string"}}), &["title", "content"])}));
            v.push(json!({"name": "append_to_note", "description": "Append Markdown to the end of an existing note.",
                          "inputSchema": schema(&json!({"note": {"type": "string"}, "text": {"type": "string"}}), &["note", "text"])}));
            v.push(json!({"name": "replace_in_note", "description": "Find and replace text in one note. Set regex=true for regular expressions with $1 captures.",
                          "inputSchema": schema(&json!({"note": {"type": "string"}, "find": {"type": "string"}, "replace": {"type": "string"}, "regex": {"type": "boolean"}}), &["note", "find", "replace"])}));
        }
        v
    }

    fn resolve(&self, note: &str) -> Result<NotePath, String> {
        let ix = self.index.read();
        let p = NotePath::new(note);
        if ix.get(&p).is_some() {
            return Ok(p);
        }
        ix.resolve(note.trim_end_matches(".md"), None)
            .ok_or_else(|| format!("No note matches “{note}”. Use search_notes or list_notes to find it."))
    }

    fn write_note(&self, path: &NotePath, content: &str) -> Result<(), String> {
        // Keep what was there, then the new version, so the change is reversible.
        if let Ok(old) = self.vault.read(path) {
            let _ = self.vault.snapshot(path, &old, now_ms() - 1, true);
        }
        self.vault.write(path, content).map_err(|e| e.to_string())?;
        let _ = self.vault.snapshot(path, content, now_ms(), true);
        self.index.upsert(path.clone(), content.to_string());
        self.index.flush();
        Ok(())
    }

    fn call(&self, name: &str, a: &Value) -> Value {
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 50) as usize;
        match name {
            "search_notes" => {
                let hits = self.index.hybrid_search(&s("query"), limit);
                if hits.is_empty() {
                    return text("No matching notes.");
                }
                let out: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        let sec = h.heading.as_deref().map(|x| format!(" › {x}")).unwrap_or_default();
                        let how = if h.by_meaning { " (by meaning)" } else { "" };
                        format!("- {}{sec}{how}\n  {}", h.path, h.snippet)
                    })
                    .collect();
                text(out.join("\n"))
            }
            "read_note" => match self.resolve(&s("note")) {
                Ok(p) => match self.vault.read(&p) {
                    Ok(src) => text(format!("# File: {p}\n\n{src}")),
                    Err(e) => tool_error(e.to_string()),
                },
                Err(e) => tool_error(e),
            },
            "list_notes" => {
                let ix = self.index.read();
                let (folder, tag) = (s("folder"), s("tag"));
                let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
                let list: Vec<String> = ix
                    .paths()
                    .into_iter()
                    .filter(|p| folder.is_empty() || p.folder().eq_ignore_ascii_case(folder.trim_matches('/')))
                    .filter(|p| tag.is_empty() || ix.get(p).is_some_and(|r| r.has_tag(&tag)))
                    .take(limit)
                    .map(|p| format!("- {p}"))
                    .collect();
                text(if list.is_empty() { "No notes.".into() } else { list.join("\n") })
            }
            "backlinks" => match self.resolve(&s("note")) {
                Ok(p) => {
                    let ix = self.index.read();
                    let b: Vec<String> = ix.backlinks(&p).iter().map(|b| format!("- {}: {}", b.from, b.context)).collect();
                    text(if b.is_empty() { format!("Nothing links to {p}.") } else { b.join("\n") })
                }
                Err(e) => tool_error(e),
            },
            "ask_notes" => {
                let q = s("question");
                let sem = self.index.semantic();
                let ix = self.index.read();
                let sources = retrieve(&ix, sem.as_deref(), &q, 6);
                let mut out = extractive_answer(&q, &sources, sem.as_deref());
                out.push_str("\n\nSources:\n");
                for src in &sources {
                    let body = src.text.split_once('\n').map_or(src.text.as_str(), |(_, b)| b);
                    let _ =
                        write!(out, "\n[{}] {} ({})\n{}\n", src.n, src.citation(), src.path, body.chars().take(1500).collect::<String>());
                }
                text(out)
            }
            "list_tags" => {
                let tags: Vec<String> = self.index.read().tags().into_iter().map(|(t, n)| format!("#{t} ({n})")).collect();
                text(if tags.is_empty() { "No tags.".into() } else { tags.join(", ") })
            }
            "create_note" | "append_to_note" | "replace_in_note" if !self.allow_write => {
                tool_error("This vault is read-only for AI tools. Start the server with --allow-write to permit edits.")
            }
            "create_note" => match self.vault.unique_path(&s("folder"), &s("title")) {
                Ok(p) => match self.write_note(&p, &s("content")) {
                    Ok(()) => text(format!("Created {p}")),
                    Err(e) => tool_error(e),
                },
                Err(e) => tool_error(e.to_string()),
            },
            "append_to_note" => match self.resolve(&s("note")) {
                Ok(p) => {
                    let mut src = self.vault.read(&p).unwrap_or_default();
                    if !src.is_empty() && !src.ends_with('\n') {
                        src.push('\n');
                    }
                    src.push_str(&s("text"));
                    if !src.ends_with('\n') {
                        src.push('\n');
                    }
                    match self.write_note(&p, &src) {
                        Ok(()) => text(format!("Appended to {p}")),
                        Err(e) => tool_error(e),
                    }
                }
                Err(e) => tool_error(e),
            },
            "replace_in_note" => {
                let opts = FindOptions { regex: a.get("regex").and_then(Value::as_bool).unwrap_or(false), ..Default::default() };
                let re = match compile(&s("find"), opts) {
                    Ok(r) => r,
                    Err(e) => return tool_error(e),
                };
                match self.resolve(&s("note")) {
                    Ok(p) => {
                        let src = self.vault.read(&p).unwrap_or_default();
                        let (out, n) = replace_in_string(&src, &re, &s("replace"), opts);
                        if n == 0 {
                            return text(format!("No matches in {p}."));
                        }
                        match self.write_note(&p, &out) {
                            Ok(()) => text(format!("Replaced {n} match{} in {p}", if n == 1 { "" } else { "es" })),
                            Err(e) => tool_error(e),
                        }
                    }
                    Err(e) => tool_error(e),
                }
            }
            other => tool_error(format!("Unknown tool {other}")),
        }
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub(crate) fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
                let version = if SUPPORTED.contains(&asked) { asked } else { SUPPORTED[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "quire", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": format!(
                        "Tools for the user's Quire notes vault ({} notes){}. Cite notes as [[Note]] when you use them.",
                        self.index.read().len(),
                        if self.allow_write { "; edits are allowed and every edit is saved in version history" } else { "; read-only" }
                    ),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                self.refresh();
                Ok(json!({ "tools": self.tools() }))
            }
            "tools/call" => {
                self.refresh();
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(self.call(name, &args))
            }
            m if m.starts_with("notifications/") => return None,
            _ => Err(json!({ "code": -32601, "message": format!("Method not found: {method}") })),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    /// Serve newline-delimited JSON-RPC until the input closes.
    pub(crate) fn serve(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
            };
            if let Some(r) = reply {
                writeln!(output, "{r}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(allow_write: bool, lines: &[Value]) -> (Vec<Value>, Vault) {
        let vault = Vault::in_memory(&[
            ("Rope Notes.md", "# Rope Notes\n## Undo\nRopes make undo cheap because subtrees are shared. #cs\n"),
            ("Ideas.md", "# Ideas\nWrite about [[Rope Notes]].\n"),
        ]);
        let srv = Server::in_memory(vault.clone(), allow_write);
        let input = lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(input), &mut out).unwrap();
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (replies, vault)
    }

    fn call(id: u64, name: &str, args: &Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
    }

    fn body(v: &Value) -> &str {
        v["result"]["content"][0]["text"].as_str().unwrap_or_default()
    }

    #[test]
    fn read_only_session() {
        let (r, _) = session(
            false,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                call(3, "search_notes", &json!({"query": "undo"})),
                call(4, "read_note", &json!({"note": "Rope Notes"})),
                call(5, "backlinks", &json!({"note": "Rope Notes.md"})),
                call(6, "ask_notes", &json!({"question": "why is undo cheap?"})),
                call(7, "append_to_note", &json!({"note": "Ideas", "text": "x"})),
                json!({"jsonrpc": "2.0", "id": 8, "method": "bogus"}),
            ],
        );
        assert_eq!(r.len(), 8, "the notification gets no reply");
        assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
        let names: Vec<&str> = r[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"search_notes") && !names.contains(&"create_note"));
        assert!(body(&r[2]).contains("Rope Notes.md"));
        assert!(body(&r[3]).contains("subtrees are shared"));
        assert!(body(&r[4]).contains("Ideas.md"));
        assert!(body(&r[5]).contains("[[Rope Notes#Undo]]"));
        assert_eq!(r[6]["result"]["isError"], true);
        assert_eq!(r[7]["error"]["code"], -32601);
    }

    #[test]
    fn writes_are_gated_and_versioned() {
        let (r, vault) = session(
            true,
            &[
                call(1, "create_note", &json!({"title": "From AI", "content": "hello", "folder": "Inbox"})),
                call(2, "append_to_note", &json!({"note": "Ideas", "text": "- added by AI"})),
                call(3, "replace_in_note", &json!({"note": "Ideas", "find": "Write", "replace": "Draft"})),
                call(4, "search_notes", &json!({"query": "hello"})),
            ],
        );
        assert!(body(&r[0]).contains("Inbox/From AI.md"));
        let ideas = vault.read(&NotePath::new("Ideas.md")).unwrap();
        assert!(ideas.contains("- added by AI") && ideas.starts_with("# Ideas\nDraft about"));
        let h = vault.history(&NotePath::new("Ideas.md"));
        assert!(h.len() >= 2);
        assert_eq!(vault.read_snapshot(h.last().unwrap()).unwrap(), "# Ideas\nWrite about [[Rope Notes]].\n", "original kept");
        assert!(body(&r[3]).contains("From AI.md"), "index updated after writes");
    }
}
