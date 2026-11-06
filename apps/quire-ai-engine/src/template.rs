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

use serde_json::Value;

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
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// A block that couldn't be read as a call (sent to the app with an
    /// empty name and `_unparsed`, so it is never run).
    BadCall {
        id: String,
        raw: String,
        error: String,
    },
}

/// Splits a streamed reply into text and tool calls.
///
/// Small models don't always follow the format, so it also:
/// repairs the JSON inside a block ([`quire_ai::toolcall`]); reads several
/// calls in one block; ends a block at the next `<tool_call>` when the
/// closing tag is missing; drops `<think>` blocks and a stray `</think>`;
/// stops at a `<tool_response>` the model writes itself (it is imagining
/// the result); and, when tools were offered, reads a reply that is only a
/// JSON call (bare or in a code fence) as a call, if it names one of them.
#[derive(Debug, Default)]
pub struct ReplyParser {
    pending: String,
    in_call: bool,
    in_think: bool,
    calls: usize,
    started: bool,
    /// The names of the tools offered.
    tools: Vec<String>,
    /// The reply so far looks like bare JSON: hold it until the end.
    holding: bool,
    /// The model began imagining a tool's answer: ignore the rest.
    ended: bool,
}

const OPEN_CALL: &str = "<tool_call>";
const CLOSE_CALL: &str = "</tool_call>";
const OPEN_THINK: &str = "<think>";
const CLOSE_THINK: &str = "</think>";
const OPEN_RESPONSE: &str = "<tool_response>";
/// Why a call in a reply that stopped at a limit isn't run.
const CUT_OFF: &str = "the reply was cut off at its length limit";

/// Tags found in plain text, in the order they are tried.
const TAGS: [&str; 4] = [OPEN_CALL, OPEN_THINK, CLOSE_THINK, OPEN_RESPONSE];

impl ReplyParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// A parser for a reply to a request that offered `tools` (by name).
    pub fn with_tools(tools: Vec<String>) -> Self {
        Self { tools, ..Self::default() }
    }

    /// How many tool calls were found (read or not).
    pub const fn calls(&self) -> usize {
        self.calls
    }

    /// Feed more of the reply; returns what is now certain.
    pub fn push(&mut self, delta: &str) -> Vec<Piece> {
        if self.ended {
            return Vec::new();
        }
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
                let close = self.pending.find(CLOSE_CALL);
                let reopen = self.pending.find(OPEN_CALL);
                // A new call starts before this one closed: this one ended,
                // but the model never closed it, so nothing is added to it.
                let (end, skip, closed) = match (close, reopen) {
                    (Some(c), Some(o)) if o < c => (o, 0, false),
                    (Some(c), _) => (c, CLOSE_CALL.len(), true),
                    (None, Some(o)) => (o, 0, false),
                    (None, None) => return out,
                };
                let body = self.pending[..end].to_string();
                self.pending.drain(..end + skip);
                self.in_call = false;
                self.read_block(&body, closed, &mut out);
                continue;
            }
            if !self.started && !self.tools.is_empty() {
                let lead = self.pending.trim_start();
                if lead.is_empty() {
                    return out;
                }
                if lead.starts_with('{') || lead.starts_with('[') || lead.starts_with("```") {
                    self.holding = true;
                }
            }
            if self.holding {
                if !self.pending.contains(OPEN_CALL) {
                    return out;
                }
                // Tags after all: read it the usual way.
                self.holding = false;
            }
            let next = TAGS.iter().filter_map(|t| self.pending.find(t).map(|i| (i, *t))).min_by_key(|(i, _)| *i);
            if let Some((i, tag)) = next {
                let text = self.pending[..i].to_string();
                self.emit_text(&text, &mut out);
                self.pending.drain(..i + tag.len());
                match tag {
                    OPEN_CALL => self.in_call = true,
                    OPEN_THINK => self.in_think = true,
                    OPEN_RESPONSE => {
                        self.ended = true;
                        self.pending.clear();
                        return out;
                    }
                    _ => {} // a stray </think>
                }
                continue;
            }
            let keep = TAGS.iter().map(|t| longest_prefix_suffix(&self.pending, t)).max().unwrap_or(0);
            let text: String = self.pending.drain(..self.pending.len() - keep).collect();
            self.emit_text(&text, &mut out);
            return out;
        }
    }

    /// The end of the reply: whatever is left. `ended_turn` says the model
    /// ended its turn itself (not the token or context limit, nor a stop
    /// sequence). An unclosed call is read only if it is complete as it
    /// stands and the model ended its turn: nothing is added to a reply that
    /// was cut off, and a call in a reply that stopped at a limit is a bad
    /// call (0.10.1 security review #3). A reply that is only JSON never had
    /// a closing tag, so it is never repaired with added brackets either.
    pub fn finish(&mut self, ended_turn: bool) -> Vec<Piece> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.pending);
        if self.ended {
            return out;
        }
        if self.in_call {
            self.in_call = false;
            if ended_turn {
                self.read_block(&rest, false, &mut out);
            } else {
                self.bad_call(&rest, CUT_OFF, &mut out);
            }
        } else if self.holding {
            self.holding = false;
            let offered = |calls: &[quire_ai::toolcall::ToolCall]| calls.iter().all(|c| self.tools.contains(&c.name));
            match quire_ai::toolcall::read_calls(&rest, false) {
                Ok(calls) if offered(&calls) && ended_turn => {
                    for c in calls {
                        self.calls += 1;
                        out.push(Piece::ToolCall { id: format!("call_{}", self.calls), name: c.name, input: c.arguments });
                    }
                }
                Ok(calls) if offered(&calls) => self.bad_call(&rest, CUT_OFF, &mut out),
                // Cut off, but it would name an offered tool: a bad call, so
                // the model is asked to write it again (never run).
                Err(e) if quire_ai::toolcall::read_calls(&rest, true).is_ok_and(|c| offered(&c)) => {
                    self.bad_call(&rest, &e.message, &mut out);
                }
                _ => self.emit_text(&rest, &mut out),
            }
        } else if !self.in_think {
            self.emit_text(&rest, &mut out);
        }
        self.in_think = false;
        out
    }

    fn bad_call(&mut self, raw: &str, error: &str, out: &mut Vec<Piece>) {
        self.calls += 1;
        out.push(Piece::BadCall { id: format!("call_{}", self.calls), raw: raw.trim().to_string(), error: error.to_string() });
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

    fn read_block(&mut self, body: &str, closed: bool, out: &mut Vec<Piece>) {
        match quire_ai::toolcall::read_calls(body, closed) {
            Ok(calls) => {
                for c in calls {
                    self.calls += 1;
                    out.push(Piece::ToolCall { id: format!("call_{}", self.calls), name: c.name, input: c.arguments });
                }
            }
            Err(e) => self.bad_call(body, &e.message, out),
        }
    }
}

/// The length of the longest suffix of `s` that is a proper prefix of `tag`.
fn longest_prefix_suffix(s: &str, tag: &str) -> usize {
    (1..tag.len()).rev().find(|&n| s.len() >= n && s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..])).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        got.extend(p.finish(true));
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
            p.finish(true),
            vec![Piece::ToolCall { id: "call_1".into(), name: "x".into(), input: json!({"a": 1}) }],
            "an unclosed but complete call at the end counts"
        );
        let mut p = ReplyParser::new();
        p.push("<tool_call>{broken");
        assert!(matches!(&p.finish(true)[..], [Piece::BadCall { raw, .. }] if raw == "{broken"), "a cut-off call is never run");
        assert_eq!(longest_prefix_suffix("abc<to", "<tool_call>"), 3);
        assert_eq!(longest_prefix_suffix("é", "<think>"), 0);
    }

    /// 0.10.1 security review #3: a call is never repaired into running when
    /// the model didn't close it, nor run when the reply hit its limit.
    #[test]
    fn cut_off_calls_are_never_run() {
        let tools = || vec!["delete_lines".to_string()];
        let only_bad = |pieces: &[Piece]| matches!(pieces, [Piece::BadCall { .. }]);
        // Bare JSON missing its brackets: never closed for it.
        let mut p = ReplyParser::with_tools(tools());
        p.push("{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4");
        let got = p.finish(true);
        assert!(only_bad(&got), "{got:?}");
        // Bare JSON that is complete, but the reply stopped at max_tokens.
        let mut p = ReplyParser::with_tools(tools());
        p.push("{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4]}}");
        assert!(only_bad(&p.finish(false)));
        // The same reply ended by the model is a call.
        let mut p = ReplyParser::with_tools(tools());
        p.push("{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4]}}");
        assert!(matches!(&p.finish(true)[..], [Piece::ToolCall { .. }]));
        // Tagged, unclosed and incomplete at the limit, or complete at the limit.
        let mut p = ReplyParser::with_tools(tools());
        p.push("<tool_call>{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4");
        assert!(only_bad(&p.finish(false)));
        let mut p = ReplyParser::with_tools(tools());
        p.push("<tool_call>{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4]}}");
        assert!(only_bad(&p.finish(false)));
        // A block ended by the next `<tool_call>` was never closed: no brackets added.
        let mut p = ReplyParser::with_tools(tools());
        let mut got = p.push("<tool_call>{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"Plan.md\",\"lines\":[3,4<tool_call>");
        got.extend(p.push("{\"name\":\"delete_lines\",\"arguments\":{\"path\":\"A.md\",\"lines\":[1]}}</tool_call>"));
        got.extend(p.finish(true));
        assert!(matches!(&got[..], [Piece::BadCall { .. }, Piece::ToolCall { .. }]), "{got:?}");
        // The library itself: brackets are added only for a closed block.
        assert!(quire_ai::toolcall::read_calls("{\"name\":\"delete_lines\",\"arguments\":{\"lines\":[3,4", false).is_err());
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use serde_json::json;

    /// `tests/fixtures/qwen3-tool-calls.jsonl`: replies in Qwen3's published
    /// tool-call format with the mistakes small models make, streamed in
    /// 3-character pieces.
    #[test]
    fn small_model_replies_parse_as_expected() {
        let src = include_str!("../tests/fixtures/qwen3-tool-calls.jsonl");
        let mut n = 0;
        for line in src.lines().filter(|l| !l.contains("\"_comment\"")) {
            let case: Value = serde_json::from_str(line).unwrap();
            let name = case["case"].as_str().unwrap();
            let tools = case["tools"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
            let mut p = ReplyParser::with_tools(tools);
            let reply: Vec<char> = case["reply"].as_str().unwrap().chars().collect();
            let mut pieces = Vec::new();
            for chunk in reply.chunks(3) {
                pieces.extend(p.push(&chunk.iter().collect::<String>()));
            }
            // `"stop": "max_tokens"`: the reply stopped at its limit.
            pieces.extend(p.finish(case["stop"].as_str() != Some("max_tokens")));
            // Merge neighbouring text so the chunking doesn't matter.
            let mut got: Vec<Value> = Vec::new();
            for piece in pieces {
                match piece {
                    Piece::Text(t) => match got.last_mut() {
                        Some(Value::Object(o)) if o.contains_key("text") => {
                            let joined = format!("{}{t}", o["text"].as_str().unwrap());
                            o.insert("text".into(), joined.into());
                        }
                        _ => got.push(json!({ "text": t })),
                    },
                    Piece::ToolCall { name, input, .. } => got.push(json!({ "call": { "name": name, "arguments": input } })),
                    Piece::BadCall { .. } => got.push(json!({ "bad": true })),
                }
            }
            assert_eq!(Value::Array(got), case["expect"], "case: {name}");
            n += 1;
        }
        assert!(n >= 20, "{n} cases");
    }
}
