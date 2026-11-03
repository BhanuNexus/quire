// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Qwen3's chat template and its Hermes-style tool calls (design §D.3: one
//! model family, so both are written out here instead of interpreting the
//! Jinja template that ships with the model).
//!
//! * [`render`] turns a system prompt, messages (in `quire_intel::chat`
//!   JSON form) and tools into the prompt text, in non-thinking mode.
//! * [`ReplyParser`] turns the streamed reply into text and tool calls:
//!   `<tool_call>{"name": …, "arguments": …}</tool_call>` blocks become
//!   calls, `<think>` blocks are dropped, and text that might be the start
//!   of a tag is held back until it is clear.

use std::fmt::Write as _;

use serde_json::{Value, json};

pub const IM_START: &str = "<|im_start|>";
pub const IM_END: &str = "<|im_end|>";
/// Tokens that end a reply.
pub const STOP_TOKENS: &[&str] = &["<|im_end|>", "<|endoftext|>"];

const TOOLS_PREAMBLE: &str = "# Tools\n\nYou may call one or more functions to assist with the user query.\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>";
const TOOLS_POSTAMBLE: &str = "</tools>\n\nFor each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call>";

fn parts(m: &Value) -> impl Iterator<Item = &Value> {
    m["parts"].as_array().into_iter().flatten()
}

fn text_of(m: &Value) -> String {
    parts(m).filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")
}

/// The prompt for `messages`, ending with the assistant's turn opened (and
/// an empty think block, which is how Qwen3 is told not to think aloud).
pub fn render(system: &str, messages: &[Value], tools: &[Value], json_mode: bool) -> String {
    let mut sys = system.trim().to_string();
    for m in messages.iter().filter(|m| m["role"] == "system") {
        let t = text_of(m);
        if !t.is_empty() {
            if !sys.is_empty() {
                sys.push_str("\n\n");
            }
            sys.push_str(&t);
        }
    }
    if json_mode {
        if !sys.is_empty() {
            sys.push_str("\n\n");
        }
        sys.push_str("Reply with one JSON value only: no prose, no code fence.");
    }
    let mut out = String::new();
    if !tools.is_empty() {
        out.push_str(IM_START);
        out.push_str("system\n");
        if !sys.is_empty() {
            out.push_str(&sys);
            out.push_str("\n\n");
        }
        out.push_str(TOOLS_PREAMBLE);
        for t in tools {
            // Key order as in Qwen's own template: type, function{name, description, parameters}.
            let _ = write!(
                out,
                "\n{{\"type\": \"function\", \"function\": {{\"name\": {}, \"description\": {}, \"parameters\": {}}}}}",
                t["name"], t["description"], t["input_schema"]
            );
        }
        out.push('\n');
        out.push_str(TOOLS_POSTAMBLE);
        out.push_str(IM_END);
        out.push('\n');
    } else if !sys.is_empty() {
        let _ = write!(out, "{IM_START}system\n{sys}{IM_END}\n");
    }
    let mut in_tool_turn = false;
    for m in messages.iter().filter(|m| m["role"] != "system") {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "tool" {
            if !in_tool_turn {
                out.push_str(IM_START);
                out.push_str("user");
                in_tool_turn = true;
            }
            for p in parts(m) {
                let content = match p["type"].as_str() {
                    Some("tool_result") => {
                        let c = p["content"].as_str().unwrap_or_default();
                        if p["is_error"] == true { format!("Error: {c}") } else { c.to_string() }
                    }
                    Some("text") => p["text"].as_str().unwrap_or_default().to_string(),
                    _ => continue,
                };
                out.push_str("\n<tool_response>\n");
                out.push_str(&content);
                out.push_str("\n</tool_response>");
            }
            continue;
        }
        if in_tool_turn {
            out.push_str(IM_END);
            out.push('\n');
            in_tool_turn = false;
        }
        out.push_str(IM_START);
        out.push_str(if role == "assistant" { "assistant" } else { "user" });
        out.push('\n');
        out.push_str(&text_of(m));
        if role == "assistant" {
            let mut first = text_of(m).is_empty();
            for p in parts(m).filter(|p| p["type"] == "tool_use") {
                if !first {
                    out.push('\n');
                }
                first = false;
                let _ = write!(out, "<tool_call>\n{{\"name\": {}, \"arguments\": {}}}\n</tool_call>", p["name"], p["input"]);
            }
        }
        out.push_str(IM_END);
        out.push('\n');
    }
    if in_tool_turn {
        out.push_str(IM_END);
        out.push('\n');
    }
    out.push_str(IM_START);
    out.push_str("assistant\n<think>\n\n</think>\n\n");
    out
}

/// What the parser found in the reply so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    ToolCall { id: String, name: String, input: Value },
}

/// Splits a streamed reply into text and tool calls.
#[derive(Debug, Default)]
pub struct ReplyParser {
    pending: String,
    in_call: bool,
    in_think: bool,
    calls: usize,
    started: bool,
}

const OPEN_CALL: &str = "<tool_call>";
const CLOSE_CALL: &str = "</tool_call>";
const OPEN_THINK: &str = "<think>";
const CLOSE_THINK: &str = "</think>";

impl ReplyParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many tool calls were found.
    pub const fn calls(&self) -> usize {
        self.calls
    }

    /// Feed more of the reply; returns what is now certain.
    pub fn push(&mut self, delta: &str) -> Vec<Piece> {
        self.pending.push_str(delta);
        let mut out = Vec::new();
        loop {
            if self.in_think {
                let Some(i) = self.pending.find(CLOSE_THINK) else {
                    // Keep only what could be the start of the closing tag.
                    let keep = longest_prefix_suffix(&self.pending, CLOSE_THINK);
                    self.pending.drain(..self.pending.len() - keep);
                    return out;
                };
                self.pending.drain(..i + CLOSE_THINK.len());
                self.in_think = false;
                continue;
            }
            if self.in_call {
                let Some(i) = self.pending.find(CLOSE_CALL) else { return out };
                let body = self.pending[..i].trim().to_string();
                self.pending.drain(..i + CLOSE_CALL.len());
                self.in_call = false;
                out.push(self.call(&body));
                continue;
            }
            let call = self.pending.find(OPEN_CALL);
            let think = self.pending.find(OPEN_THINK);
            let next = match (call, think) {
                (Some(c), Some(t)) => Some(c.min(t)),
                (a, b) => a.or(b),
            };
            if let Some(i) = next {
                let text = self.pending[..i].to_string();
                self.emit_text(&text, &mut out);
                if Some(i) == call {
                    self.pending.drain(..i + OPEN_CALL.len());
                    self.in_call = true;
                } else {
                    self.pending.drain(..i + OPEN_THINK.len());
                    self.in_think = true;
                }
                continue;
            }
            let keep = longest_prefix_suffix(&self.pending, OPEN_CALL).max(longest_prefix_suffix(&self.pending, OPEN_THINK));
            let text: String = self.pending.drain(..self.pending.len() - keep).collect();
            self.emit_text(&text, &mut out);
            return out;
        }
    }

    /// The end of the reply: whatever is left (an unclosed call is parsed
    /// if it is complete JSON, otherwise shown as text).
    pub fn finish(&mut self) -> Vec<Piece> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.pending);
        if self.in_call {
            self.in_call = false;
            let body = rest.trim();
            if serde_json::from_str::<Value>(body).is_ok() {
                out.push(self.call(body));
            } else {
                self.emit_text(&format!("{OPEN_CALL}{rest}"), &mut out);
            }
        } else if !self.in_think {
            self.emit_text(&rest, &mut out);
        }
        self.in_think = false;
        out
    }

    fn emit_text(&mut self, text: &str, out: &mut Vec<Piece>) {
        // Leading blank lines after a think block or before the first words.
        let text = if self.started { text } else { text.trim_start_matches(['\n', ' ']) };
        if text.is_empty() {
            return;
        }
        self.started = true;
        match out.last_mut() {
            Some(Piece::Text(t)) => t.push_str(text),
            _ => out.push(Piece::Text(text.to_string())),
        }
    }

    fn call(&mut self, body: &str) -> Piece {
        self.calls += 1;
        let id = format!("call_{}", self.calls);
        let v: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({ "name": "", "arguments": { "_unparsed": body } }));
        let input = match v.get("arguments").or_else(|| v.get("parameters")) {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({ "_unparsed": s })),
            Some(a) => a.clone(),
            None => json!({}),
        };
        Piece::ToolCall { id, name: v["name"].as_str().unwrap_or_default().to_string(), input }
    }
}

/// The length of the longest suffix of `s` that is a proper prefix of `tag`.
fn longest_prefix_suffix(s: &str, tag: &str) -> usize {
    (1..tag.len()).rev().find(|&n| s.len() >= n && s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..])).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, parts: &Value) -> Value {
        json!({ "role": role, "parts": parts })
    }

    #[test]
    fn renders_qwen3_turns_tools_and_tool_results() {
        let tools = vec![json!({"name": "search", "description": "Search notes", "input_schema": {"type": "object"}})];
        let messages = vec![
            msg("user", &json!([{"type": "text", "text": "Find ropes"}])),
            msg("assistant", &json!([{"type": "tool_use", "id": "call_1", "name": "search", "input": {"query": "ropes"}}])),
            msg("tool", &json!([{"type": "tool_result", "id": "call_1", "content": "Rope Notes.md", "is_error": false}])),
            msg("tool", &json!([{"type": "tool_result", "id": "call_2", "content": "boom", "is_error": true}])),
        ];
        let p = render("Be brief.", &messages, &tools, false);
        let expected = "<|im_start|>system\nBe brief.\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>\n{\"type\": \"function\", \"function\": {\"name\": \"search\", \"description\": \"Search notes\", \"parameters\": {\"type\":\"object\"}}}\n</tools>\n\nFor each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call><|im_end|>\n<|im_start|>user\nFind ropes<|im_end|>\n<|im_start|>assistant\n<tool_call>\n{\"name\": \"search\", \"arguments\": {\"query\":\"ropes\"}}\n</tool_call><|im_end|>\n<|im_start|>user\n<tool_response>\nRope Notes.md\n</tool_response>\n<tool_response>\nError: boom\n</tool_response><|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
        assert_eq!(p, expected);
        let plain = render("", &[msg("user", &json!([{"type": "text", "text": "hi"}]))], &[], true);
        assert!(plain.starts_with("<|im_start|>system\nReply with one JSON value only"), "{plain}");
        assert!(render("", &[], &[], false).starts_with("<|im_start|>assistant"), "no system turn without a system prompt");
    }

    #[test]
    fn parses_streamed_tool_calls_and_drops_thinking() {
        let mut p = ReplyParser::new();
        let mut got = Vec::new();
        for piece in [
            "\n\nLet me ",
            "look.<to",
            "ol_call>\n{\"name\": \"view\", ",
            "\"arguments\": {\"path\": \"A.md\"}}\n</tool",
            "_call>",
            "<th",
            "ink>hmm</think>Done <",
            " 3",
        ] {
            got.extend(p.push(piece));
        }
        got.extend(p.finish());
        assert_eq!(
            got,
            vec![
                Piece::Text("Let me ".into()),
                Piece::Text("look.".into()),
                Piece::ToolCall { id: "call_1".into(), name: "view".into(), input: json!({"path": "A.md"}) },
                Piece::Text("Done ".into()),
                Piece::Text("< 3".into()),
            ]
        );
        assert_eq!(p.calls(), 1);
        let mut p = ReplyParser::new();
        assert!(p.push("<tool_call>{\"name\": \"x\", \"arguments\": \"{\\\"a\\\": 1}\"}").is_empty());
        assert_eq!(
            p.finish(),
            vec![Piece::ToolCall { id: "call_1".into(), name: "x".into(), input: json!({"a": 1}) }],
            "an unclosed but complete call at the end counts"
        );
        let mut p = ReplyParser::new();
        p.push("<tool_call>{broken");
        assert_eq!(p.finish(), vec![Piece::Text("<tool_call>{broken".into())]);
        assert_eq!(longest_prefix_suffix("abc<to", "<tool_call>"), 3);
        assert_eq!(longest_prefix_suffix("é", "<think>"), 0);
    }
}
