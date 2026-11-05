// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reading tool calls the way small models write them.
//!
//! Qwen3 is asked for `{"name": …, "arguments": {…}}` inside
//! `<tool_call>` tags. Small models get close but not always exact: single
//! quotes, a code fence around the JSON, a trailing comma, `True`/`None`,
//! words after the object, two calls in one block, `arguments` as a JSON
//! string, or the arguments spread beside `name`. [`read_calls`] repairs
//! those and nothing more.
//!
//! **Safety.** A call is returned only when the repaired text is a whole
//! JSON object with a tool name. Missing closing brackets are added only
//! when the model closed the block itself (`closed`), never for a reply
//! that was cut off. Anything else is an error, and the caller must not run
//! any part of it.

use serde_json::{Map, Value};

/// One call read from a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

/// Why a block isn't a call (for the corrective message to the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCall {
    pub message: String,
}

impl BadCall {
    fn new(m: impl Into<String>) -> Self {
        Self { message: m.into() }
    }
}

/// The key a caller puts in a call's input when the call couldn't be read
/// (with the raw text): such a call is never run.
pub const UNPARSED: &str = "_unparsed";

/// Whether a call's input marks it as unreadable.
pub fn is_unparsed(input: &Value) -> bool {
    input.get(UNPARSED).is_some()
}

/// The calls in one `<tool_call>` block (or a reply that is only JSON).
/// `closed` says the model ended the block itself.
pub fn read_calls(body: &str, closed: bool) -> Result<Vec<ToolCall>, BadCall> {
    let body = strip_fence(body.trim());
    if body.is_empty() {
        return Err(BadCall::new("the tool call was empty"));
    }
    let values = values(body, closed)?;
    let mut calls = Vec::new();
    for v in values {
        match v {
            Value::Array(items) => {
                for item in items {
                    calls.push(call(item)?);
                }
            }
            other => calls.push(call(other)?),
        }
    }
    if calls.is_empty() {
        return Err(BadCall::new("the tool call had no JSON object"));
    }
    Ok(calls)
}

/// `{"name", "arguments"}` (or `parameters`, `input`, the `OpenAI`
/// form `{"function": {…}}`, or the arguments beside the name).
fn call(v: Value) -> Result<ToolCall, BadCall> {
    let Value::Object(mut obj) = v else { return Err(BadCall::new("a tool call must be a JSON object")) };
    if let Some(Value::Object(f)) = obj.remove("function") {
        return call(Value::Object(f));
    }
    let name = match obj.remove("name").or_else(|| obj.remove("tool")) {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(_) => return Err(BadCall::new("the tool call's \"name\" must be a string")),
        None => return Err(BadCall::new("the tool call has no \"name\"")),
    };
    let args = ["arguments", "parameters", "input", "args"].iter().find_map(|k| obj.remove(*k));
    let arguments = match args {
        None => {
            obj.remove("id");
            obj.remove("type");
            Value::Object(obj)
        }
        Some(Value::Object(a)) => Value::Object(a),
        Some(Value::Null) => Value::Object(Map::new()),
        Some(Value::String(s)) if s.trim().is_empty() => Value::Object(Map::new()),
        Some(Value::String(s)) => match values(strip_fence(s.trim()), true)?.into_iter().next() {
            Some(Value::Object(a)) => Value::Object(a),
            _ => return Err(BadCall::new(format!("the arguments of {name} must be a JSON object"))),
        },
        Some(_) => return Err(BadCall::new(format!("the arguments of {name} must be a JSON object"))),
    };
    Ok(ToolCall { name, arguments })
}

/// A Markdown code fence around the text, if any.
fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    rest.trim()
}

/// The top-level JSON values in `s`, repaired; text around them is ignored.
fn values(s: &str, closed: bool) -> Result<Vec<Value>, BadCall> {
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return Ok(vec![v]);
    }
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find(['{', '[']) {
        let (seg, complete) = balanced(&rest[start..]);
        // Words after a call ("… {see above}") don't spoil it, unless they
        // look like another call.
        if !out.is_empty() && !seg.contains("name") {
            break;
        }
        let text = if complete {
            seg.to_string()
        } else if closed {
            close(seg).ok_or_else(|| BadCall::new("the tool call's JSON has an unclosed string"))?
        } else {
            return Err(BadCall::new("the tool call was cut off"));
        };
        let v = serde_json::from_str::<Value>(&text).or_else(|_| serde_json::from_str::<Value>(&repair(&text)));
        match v {
            Ok(v) => out.push(v),
            Err(e) => return Err(BadCall::new(format!("the tool call isn't valid JSON ({e})"))),
        }
        rest = &rest[start + seg.len()..];
    }
    if out.is_empty() {
        return Err(BadCall::new("the tool call had no JSON object"));
    }
    Ok(out)
}

/// The first bracketed value of `s` (which starts with `{` or `[`), quote-aware
/// for both `"` and `'`; `false` when it never closes.
fn balanced(s: &str) -> (&str, bool) {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return (&s[..=i], true);
                }
            }
            _ => {}
        }
    }
    (s, false)
}

/// Close the brackets left open at the end of `s`; `None` inside a string.
fn close(s: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    let mut out = s.trim_end().trim_end_matches(',').to_string();
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    Some(out)
}

/// Python-ish JSON to JSON: single-quoted strings, bare keys, trailing
/// commas, `True`/`False`/`None`.
fn repair(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                // Copy the string, re-quoted with `"`.
                out.push('"');
                i += 1;
                while i < chars.len() && chars[i] != c {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            if chars[i + 1] == '\'' {
                                out.push('\'');
                            } else {
                                out.push('\\');
                                out.push(chars[i + 1]);
                            }
                            i += 2;
                            continue;
                        }
                        '"' => out.push_str("\\\""),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        other => out.push(other),
                    }
                    i += 1;
                }
                out.push('"');
                i += 1;
            }
            ',' => {
                // Drop a comma that only precedes a closing bracket.
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some('}' | ']')) {
                    out.push(',');
                }
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let next = chars[i..].iter().find(|c| !c.is_whitespace());
                match word.as_str() {
                    "True" | "true" => out.push_str("true"),
                    "False" | "false" => out.push_str("false"),
                    "None" | "null" => out.push_str("null"),
                    // A bare key.
                    _ if next == Some(&':') => {
                        out.push('"');
                        out.push_str(&word);
                        out.push('"');
                    }
                    _ => out.push_str(&word),
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(body: &str) -> ToolCall {
        let mut c = read_calls(body, true).unwrap_or_else(|e| panic!("{body}: {}", e.message));
        assert_eq!(c.len(), 1, "{body}");
        c.remove(0)
    }

    #[test]
    fn repairs_what_small_models_write() {
        let want = ToolCall { name: "search".into(), arguments: json!({"query": "rope", "limit": 3}) };
        for body in [
            r#"{"name": "search", "arguments": {"query": "rope", "limit": 3}}"#,
            r"{'name': 'search', 'arguments': {'query': 'rope', 'limit': 3}}",
            "```json\n{\"name\": \"search\", \"arguments\": {\"query\": \"rope\", \"limit\": 3}}\n```",
            r#"{"name": "search", "arguments": {"query": "rope", "limit": 3,},}"#,
            r#"{"name": "search", "arguments": "{\"query\": \"rope\", \"limit\": 3}"}"#,
            r#"{"name": "search", "query": "rope", "limit": 3}"#,
            r#"{name: "search", arguments: {query: "rope", limit: 3}}"#,
            r#"{"name": "search", "arguments": {"query": "rope", "limit": 3}} I'll look now."#,
            r#"{"name": "search", "arguments": {"query": "rope", "limit": 3}"#,
            r#"{"type": "function", "function": {"name": "search", "arguments": {"query": "rope", "limit": 3}}}"#,
            r#"{"name": "search", "parameters": {"query": "rope", "limit": 3}}"#,
        ] {
            assert_eq!(one(body), want, "{body}");
        }
        assert_eq!(
            one(r#"{'name': 'x', 'arguments': {'ok': True, 'none': None, 'q': "it's"}}"#).arguments,
            json!({"ok": true, "none": null, "q": "it's"})
        );
        assert_eq!(one(r#"{'name': 'x', 'arguments': {'q': 'say "hi"\n'}}"#).arguments, json!({"q": "say \"hi\"\n"}));
        assert_eq!(one(r#"{"name": "list"}"#).arguments, json!({}));
    }

    #[test]
    fn reads_several_calls_in_one_block() {
        let c = read_calls("{\"name\": \"a\", \"arguments\": {}}\n{\"name\": \"b\", \"arguments\": {\"x\": 1}}", true).unwrap();
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        let c = read_calls(r#"[{"name": "a", "arguments": {}}, {"name": "b", "arguments": {}}]"#, true).unwrap();
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn refuses_what_it_cannot_be_sure_of() {
        for (body, closed) in [
            ("", true),
            ("search for rope", true),
            (r#"{"arguments": {"q": 1}}"#, true),
            (r#"{"name": 3}"#, true),
            (r#"{"name": "a", "arguments": [1]}"#, true),
            (r#"{"name": "a", "arguments": {"q": "unterminated"#, true),
            // Cut off: no brackets are added when the model didn't close the block.
            (r#"{"name": "a", "arguments": {"q": 1}"#, false),
            (r#"{"name": "a", "arguments": {"q": 1} oops: {]"#, true),
        ] {
            assert!(read_calls(body, closed).is_err(), "{body}");
        }
        assert!(is_unparsed(&json!({ UNPARSED: "x" })));
    }
}
