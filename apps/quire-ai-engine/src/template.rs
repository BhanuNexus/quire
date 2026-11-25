// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The chat templates of Qwen3 and Qwen 3.5 and their tool calls.
//!
//! See `docs/design/0.9.md` §D.3 and `docs/design/0.14-local-ai.md` §3.4: two
//! model families, so both are written out here instead of interpreting the
//! Jinja template that ships with the model.
//!
//! * [`render`] turns a system prompt, messages (in `quire_intel::chat`
//!   JSON form) and tools into the prompt text, in non-thinking mode.
//! * [`ReplyParser`] turns the streamed reply into text and tool calls:
//!   `<tool_call>{"name": …, "arguments": …}</tool_call>` blocks become
//!   calls, `<think>` blocks are dropped, and text that might be the start
//!   of a tag is held back until it is clear.

use serde_json::Value;

pub const IM_START: &str = "<|im_start|>";
pub const IM_END: &str = "<|im_end|>";
/// Tokens that end a reply.
pub const STOP_TOKENS: &[&str] = &["<|im_end|>", "<|endoftext|>"];

const TOOLS_PREAMBLE: &str = "# Tools\n\nYou may call one or more functions to assist with the user query.\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>";
const TOOLS_POSTAMBLE: &str = "</tools>\n\nFor each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call>";

/// A rendered prompt: its text, and which `<|im_start|>`/`<|im_end|>` in it
/// the template wrote (`docs/design/0.14-local-ai.md` §3.4, control tokens).
///
/// Only those become control tokens: the text between them is encoded with
/// special-token matching off, so a note, a tool result or a message that
/// contains `<|im_end|>` stays ordinary text and can't end a turn. The tags
/// the reply parser and the model read as structure (`<tool_call>`,
/// `</tool_response>`, `<think>`, `<function=`, …), which Qwen's tokenizers
/// match as tokens even with special-token matching off, get a zero-width
/// space after their `<` inside content, so content
/// can't open or close a block. (transformers and vLLM do neither: they
/// tokenize the rendered string with special tokens parsed.)
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Prompt {
    text: String,
    /// Byte ranges of the control tokens the template wrote, in order.
    specials: Vec<(usize, usize)>,
}

/// Tags that open or close a block, as text: broken inside content.
const STRUCTURE_TAGS: [&str; 12] = [
    "<tool_call>",
    "</tool_call>",
    "<tool_response>",
    "</tool_response>",
    "<think>",
    "</think>",
    "<tools>",
    "</tools>",
    "<function=",
    "</function>",
    "<parameter=",
    "</parameter>",
];

impl Prompt {
    /// Template text (no control tokens).
    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn push(&mut self, c: char) {
        self.text.push(c);
    }

    /// A control token the template writes.
    fn special(&mut self, s: &str) {
        let at = self.text.len();
        self.text.push_str(s);
        self.specials.push((at, self.text.len()));
    }

    /// Text that came from a person, a note, a tool or the model: structure
    /// tags in it are broken with a zero-width space after the `<`.
    fn content(&mut self, s: &str) {
        let mut rest = s;
        while let Some(at) = STRUCTURE_TAGS.iter().filter_map(|t| rest.find(t)).min() {
            self.text.push_str(&rest[..=at]);
            self.text.push('\u{200B}');
            rest = &rest[at + 1..];
        }
        self.text.push_str(rest);
    }

    /// Plain text as a prompt of its own (no structure tags are broken: a
    /// document, as the reference check reads it).
    pub fn push_content(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// The whole prompt as text (what the published template renders when
    /// the content holds no structure tags).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The prompt in order: `(true, token)` for a control token the
    /// template wrote, `(false, text)` for everything between.
    pub fn pieces(&self) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(a, b) in &self.specials {
            if a > at {
                out.push((false, &self.text[at..a]));
            }
            out.push((true, &self.text[a..b]));
            at = b;
        }
        if at < self.text.len() {
            out.push((false, &self.text[at..]));
        }
        out
    }
}

fn parts(m: &Value) -> impl Iterator<Item = &Value> {
    m["parts"].as_array().into_iter().flatten()
}

fn text_of(m: &Value) -> String {
    parts(m).filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")
}

/// The prompt for `messages`, ending with the assistant's turn opened (and
/// an empty think block, which is how Qwen3 is told not to think aloud).
pub fn render(system: &str, messages: &[Value], tools: &[Value], json_mode: bool) -> Prompt {
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
    let mut out = Prompt::default();
    if !tools.is_empty() {
        out.special(IM_START);
        out.push_str("system\n");
        if !sys.is_empty() {
            out.content(&sys);
            out.push_str("\n\n");
        }
        out.push_str(TOOLS_PREAMBLE);
        for t in tools {
            // Key order as in Qwen's own template: type, function{name, description, parameters}.
            out.push('\n');
            out.content(&format!(
                "{{\"type\": \"function\", \"function\": {{\"name\": {}, \"description\": {}, \"parameters\": {}}}}}",
                t["name"], t["description"], t["input_schema"]
            ));
        }
        out.push('\n');
        out.push_str(TOOLS_POSTAMBLE);
        out.special(IM_END);
        out.push('\n');
    } else if !sys.is_empty() {
        out.special(IM_START);
        out.push_str("system\n");
        out.content(&sys);
        out.special(IM_END);
        out.push('\n');
    }
    let mut in_tool_turn = false;
    for m in messages.iter().filter(|m| m["role"] != "system") {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "tool" {
            if !in_tool_turn {
                out.special(IM_START);
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
                out.content(&content);
                out.push_str("\n</tool_response>");
            }
            continue;
        }
        if in_tool_turn {
            out.special(IM_END);
            out.push('\n');
            in_tool_turn = false;
        }
        out.special(IM_START);
        out.push_str(if role == "assistant" { "assistant" } else { "user" });
        out.push('\n');
        out.content(&text_of(m));
        if role == "assistant" {
            let mut first = text_of(m).is_empty();
            for p in parts(m).filter(|p| p["type"] == "tool_use") {
                if !first {
                    out.push('\n');
                }
                first = false;
                out.push_str("<tool_call>\n");
                out.content(&format!("{{\"name\": {}, \"arguments\": {}}}", p["name"], p["input"]));
                out.push_str("\n</tool_call>");
            }
        }
        out.special(IM_END);
        out.push('\n');
    }
    if in_tool_turn {
        out.special(IM_END);
        out.push('\n');
    }
    out.special(IM_START);
    out.push_str("assistant\n<think>\n\n</think>\n\n");
    out
}

/// The prompt for Qwen 3.5, in non-thinking mode.
///
/// The published `chat_template.jinja` of Qwen 3.5 4B and 9B written out
/// (`docs/design/0.14-local-ai.md` §3.4), for the conversation as
/// transformers' `apply_chat_template` would get it (tools as
/// `{"type": "function", "function": {…}}`, each tool result its own `tool`
/// message, `enable_thinking=False`).
///
/// Differences from Qwen3's: the tool list and the XML call format, values
/// trimmed, and assistant turns after the last user message keeping an
/// empty think block (so the rendered history only grows during an agent
/// run, which the prompt-reuse snapshot relies on).
pub fn render_qwen35(system: &str, messages: &[Value], tools: &[Value], json_mode: bool) -> Prompt {
    let sys = system_text(system, messages, json_mode);
    let mut out = Prompt::default();
    if !tools.is_empty() {
        out.special(IM_START);
        out.push_str("system\n# Tools\n\nYou have access to the following functions:\n\n<tools>");
        for t in tools {
            out.push('\n');
            // Key order as the dict is built: type, function; name,
            // description, parameters.
            let mut line = String::from("{\"type\": \"function\", \"function\": {\"name\": ");
            py_json(&t["name"], &mut line);
            line.push_str(", \"description\": ");
            py_json(&t["description"], &mut line);
            line.push_str(", \"parameters\": ");
            py_json(&t["input_schema"], &mut line);
            line.push_str("}}");
            out.content(&line);
        }
        out.push_str("\n</tools>");
        out.push_str(QWEN35_TOOL_RULES);
        let sys = sys.trim();
        if !sys.is_empty() {
            out.push_str("\n\n");
            out.content(sys);
        }
        out.special(IM_END);
        out.push('\n');
    } else if !sys.is_empty() {
        out.special(IM_START);
        out.push_str("system\n");
        out.content(sys.trim());
        out.special(IM_END);
        out.push('\n');
    }
    // As the template sees them: one `tool` message per tool result.
    let turns: Vec<(&str, &Value)> = messages
        .iter()
        .filter(|m| m["role"] != "system")
        .flat_map(|m| {
            let role = match m["role"].as_str() {
                Some("assistant") => "assistant",
                Some("tool") => "tool",
                _ => "user",
            };
            if role == "tool" {
                parts(m).filter(|p| p["type"] == "tool_result" || p["type"] == "text").map(|p| ("tool", p)).collect::<Vec<_>>()
            } else {
                vec![(role, m)]
            }
        })
        .collect();
    let last_query = turns.iter().rposition(|(role, _)| *role == "user");
    for (i, (role, m)) in turns.iter().enumerate() {
        match *role {
            "tool" => {
                if i == 0 || turns[i - 1].0 != "tool" {
                    out.special(IM_START);
                    out.push_str("user");
                }
                let content = match m["type"].as_str() {
                    Some("tool_result") => {
                        let c = m["content"].as_str().unwrap_or_default();
                        if m["is_error"] == true { format!("Error: {c}") } else { c.to_string() }
                    }
                    _ => m["text"].as_str().unwrap_or_default().to_string(),
                };
                out.push_str("\n<tool_response>\n");
                out.content(content.trim());
                out.push_str("\n</tool_response>");
                if turns.get(i + 1).is_none_or(|(r, _)| *r != "tool") {
                    out.special(IM_END);
                    out.push('\n');
                }
            }
            "assistant" => {
                let text = text_of(m);
                let mut content = text.trim();
                let mut reasoning = "";
                if let Some(end) = content.find("</think>") {
                    let before = &content[..end];
                    reasoning = before.rsplit("<think>").next().unwrap_or(before).trim_start_matches('\n');
                    content = content[content.rfind("</think>").map_or(0, |e| e + "</think>".len())..].trim_start_matches('\n');
                }
                let reasoning = reasoning.trim_end_matches('\n').trim();
                out.special(IM_START);
                out.push_str("assistant\n");
                if last_query.is_some_and(|q| i > q) {
                    out.push_str("<think>\n");
                    out.content(reasoning);
                    out.push_str("\n</think>\n\n");
                }
                out.content(content);
                for (n, call) in parts(m).filter(|p| p["type"] == "tool_use").enumerate() {
                    let name = call["name"].as_str().unwrap_or_default();
                    if n > 0 {
                        out.push('\n');
                    } else if !content.trim().is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str("<tool_call>\n<function=");
                    out.content(name);
                    out.push_str(">\n");
                    for (k, v) in call["input"].as_object().into_iter().flatten() {
                        out.push_str("<parameter=");
                        out.content(k);
                        out.push_str(">\n");
                        let mut value = String::new();
                        py_str(v, &mut value);
                        out.content(&value);
                        out.push_str("\n</parameter>\n");
                    }
                    out.push_str("</function>\n</tool_call>");
                }
                out.special(IM_END);
                out.push('\n');
            }
            _ => {
                out.special(IM_START);
                out.push_str("user\n");
                out.content(text_of(m).trim());
                out.special(IM_END);
                out.push('\n');
            }
        }
    }
    out.special(IM_START);
    out.push_str("assistant\n<think>\n\n</think>\n\n");
    out
}

/// The system prompt, the text of any system messages and, in JSON mode,
/// the instruction to reply with JSON alone.
fn system_text(system: &str, messages: &[Value], json_mode: bool) -> String {
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
    sys
}

/// The instructions Qwen 3.5's template puts after the tool list.
const QWEN35_TOOL_RULES: &str = "\n\nIf you choose to call a function ONLY reply in the following format with NO suffix:\n\n<tool_call>\n<function=example_function_name>\n<parameter=example_parameter_1>\nvalue_1\n</parameter>\n<parameter=example_parameter_2>\nThis is the value for the second parameter\nthat can span\nmultiple lines\n</parameter>\n</function>\n</tool_call>\n\n<IMPORTANT>\nReminder:\n- Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n- Required parameters MUST be specified\n- You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n- If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls\n</IMPORTANT>";

/// JSON as Python's `json.dumps` writes it with its default separators
/// (`", "` and `": "`) and `ensure_ascii=False`: the template's `tojson`.
fn py_json(v: &Value, out: &mut String) {
    match v {
        Value::Array(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_json(x, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, x)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push_str(": ");
                py_json(x, out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A tool-call argument as the template writes it: objects and lists as
/// JSON, anything else through Jinja's `string` filter (Python's `str`).
fn py_str(v: &Value, out: &mut String) {
    match v {
        Value::Object(_) | Value::Array(_) => py_json(v, out),
        Value::String(s) => out.push_str(s),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Null => out.push_str("None"),
        Value::Number(n) => out.push_str(&n.to_string()),
    }
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
    /// Their definitions (`name`, `input_schema`), which type the values
    /// of Qwen 3.5's XML calls.
    tool_defs: Vec<Value>,
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

    /// A parser for a reply to a request that offered these tool
    /// definitions; it also reads Qwen 3.5's XML calls, typing their values
    /// by each tool's schema.
    pub fn with_tool_defs(defs: &[Value]) -> Self {
        let tools = defs.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
        Self { tools, tool_defs: defs.to_vec(), ..Self::default() }
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
        // Qwen 3.5's XML form first, then the JSON form with its repairs.
        let read = if body.trim_start().starts_with("<function=") {
            quire_ai::toolcall::read_xml_calls(body, closed, &self.tool_defs)
        } else {
            quire_ai::toolcall::read_calls(body, closed)
        };
        match read {
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
        let p = render("Be brief.", &messages, &tools, false).text().to_string();
        let expected = "<|im_start|>system\nBe brief.\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>\n{\"type\": \"function\", \"function\": {\"name\": \"search\", \"description\": \"Search notes\", \"parameters\": {\"type\":\"object\"}}}\n</tools>\n\nFor each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call><|im_end|>\n<|im_start|>user\nFind ropes<|im_end|>\n<|im_start|>assistant\n<tool_call>\n{\"name\": \"search\", \"arguments\": {\"query\":\"ropes\"}}\n</tool_call><|im_end|>\n<|im_start|>user\n<tool_response>\nRope Notes.md\n</tool_response>\n<tool_response>\nError: boom\n</tool_response><|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
        assert_eq!(p, expected);
        let plain = render("", &[msg("user", &json!([{"type": "text", "text": "hi"}]))], &[], true).text().to_string();
        assert!(plain.starts_with("<|im_start|>system\nReply with one JSON value only"), "{plain}");
        assert!(render("", &[], &[], false).text().starts_with("<|im_start|>assistant"), "no system turn without a system prompt");
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

    /// Qwen 3.5's XML calls, streamed three characters at a time, with the
    /// same cut-off rules as the JSON form.
    #[test]
    fn parses_streamed_xml_tool_calls() {
        let defs =
            vec![json!({"name": "delete_lines", "input_schema": {"properties": {"path": {"type": "string"}, "lines": {"type": "array"}}}})];
        let run = |reply: &str, ended: bool| {
            let mut p = ReplyParser::with_tool_defs(&defs);
            let chars: Vec<char> = reply.chars().collect();
            let mut got = Vec::new();
            for c in chars.chunks(3) {
                got.extend(p.push(&c.iter().collect::<String>()));
            }
            got.extend(p.finish(ended));
            got
        };
        let call = "<tool_call>\n<function=delete_lines>\n<parameter=path>\nPlan.md\n</parameter>\n<parameter=lines>\n[3, 4]\n</parameter>\n</function>\n</tool_call>";
        let got = run(&format!("I'll remove them.\n\n{call}"), true);
        assert_eq!(
            got.iter().filter(|p| !matches!(p, Piece::Text(_))).collect::<Vec<_>>(),
            [&Piece::ToolCall { id: "call_1".into(), name: "delete_lines".into(), input: json!({"path": "Plan.md", "lines": [3, 4]}) }]
        );
        // Complete but unclosed when the reply stopped at its token limit: never run.
        let unclosed = call.trim_end_matches("</tool_call>");
        assert!(matches!(&run(unclosed, false)[..], [Piece::BadCall { .. }]));
        // Cut off inside a parameter, even if the model "ended": never run.
        assert!(matches!(&run("<tool_call>\n<function=delete_lines>\n<parameter=path>\nPla", true)[..], [Piece::BadCall { .. }]));
        // Closed by the model without </function>: read as it stands.
        let got = run("<tool_call>\n<function=delete_lines>\n<parameter=path>\nA.md\n</parameter>\n</tool_call>", true);
        assert!(matches!(&got[..], [Piece::ToolCall { .. }]), "{got:?}");
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

    /// `tests/fixtures/qwen35-template/`: seven conversations (with and
    /// without tools, tool calls with typed arguments, tool results and
    /// errors, several turns, JSON mode) and the bytes Qwen 3.5's
    /// `chat_template.jinja` renders for them
    /// (`packaging/qwen35-template-fixtures.py`; `template.sha256` names the
    /// template text used).
    #[test]
    fn renders_qwen35_like_the_published_template() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qwen35-template");
        let convs: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("conversations.json")).unwrap()).unwrap();
        let mut n = 0;
        for c in convs.as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let want = std::fs::read_to_string(dir.join(format!("{name}.txt"))).unwrap();
            let tools = c["tools"].as_array().cloned().unwrap_or_default();
            let got = render_qwen35(c["system"].as_str().unwrap_or_default(), c["messages"].as_array().unwrap(), &tools, c["json"] == true)
                .text()
                .to_string();
            assert_eq!(got, want, "conversation {name}");
            n += 1;
        }
        assert_eq!(n, 7);
    }

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
