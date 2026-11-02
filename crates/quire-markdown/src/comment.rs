// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Machine data kept next to readable text in HTML comments:
//! `<!-- quire:KIND BODY -->` (design P1).
//!
//! `KIND` is a word that may contain `:` and `-` (`cite`, `index:start`,
//! `pdf-anchor`). `BODY` is free text, usually `key=value` pairs
//! ([`parse_args`]) or JSON. Any `-->` inside a body is written as `--&gt;`
//! so the comment can't end early. See docs/formats/comments.md.

use std::ops::Range;

/// One `<!-- quire:… -->` comment in a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuireComment {
    /// `cite`, `index:start`, `pdf-anchor`…
    pub kind: String,
    /// The body with escapes undone; empty when there is none.
    pub body: String,
    /// Byte range of the whole comment, `<!--` to `-->`.
    pub range: Range<usize>,
}

const OPEN: &str = "<!-- quire:";
const CLOSE: &str = "-->";

/// Every Quire comment in `src`, in order. Comments inside code are not
/// excluded; callers that care check [`crate::verbatim_ranges`].
pub fn find_all(src: &str) -> Vec<QuireComment> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = src[from..].find(OPEN) {
        let start = from + i;
        let after = start + OPEN.len();
        let Some(j) = src[after..].find(CLOSE) else { break };
        let inner = &src[after..after + j];
        let (kind, body) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        let end = after + j + CLOSE.len();
        if !kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_')) {
            out.push(QuireComment { kind: kind.to_string(), body: unescape(body.trim()), range: start..end });
        }
        from = end;
    }
    out
}

/// The Quire comments of one kind.
pub fn find(src: &str, kind: &str) -> Vec<QuireComment> {
    find_all(src).into_iter().filter(|c| c.kind == kind).collect()
}

/// `<!-- quire:KIND BODY -->` (or `<!-- quire:KIND -->` for an empty body).
pub fn format(kind: &str, body: &str) -> String {
    if body.trim().is_empty() { format!("{OPEN}{kind} {CLOSE}") } else { format!("{OPEN}{kind} {} {CLOSE}", escape(body.trim())) }
}

fn escape(body: &str) -> String {
    body.replace("-->", "--&gt;")
}

fn unescape(body: &str) -> String {
    body.replace("--&gt;", "-->")
}

/// `key=value` pairs, space separated. Values may be double-quoted, with
/// `\"`, `\\` and `\n` escapes: `v=1 l=3-3 p="## Undo\n"`.
pub fn parse_args(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        if key.is_empty() {
            break;
        }
        let mut val = String::new();
        if chars.next_if_eq(&'=').is_some() {
            if chars.next_if_eq(&'"').is_some() {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => val.push('\n'),
                            Some(other) => val.push(other),
                            None => {}
                        },
                        c => val.push(c),
                    }
                }
            } else {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    val.push(c);
                }
            }
        }
        out.push((key, val));
    }
    out
}

/// The inverse of [`parse_args`]: values with spaces, quotes, `=` or
/// newlines are quoted.
pub fn format_args(args: &[(&str, &str)]) -> String {
    let parts: Vec<String> = args
        .iter()
        .map(|(k, v)| {
            if !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '=')) {
                format!("{k}={v}")
            } else {
                let q = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                format!("{k}=\"{q}\"")
            }
        })
        .collect();
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_round_trips() {
        let cite = "[^1]: [[Rope Notes#Undo]] — “quote” <!-- quire:cite v=1 sha=1a2b l=3-3 p=\"## Undo\\n\" s=\" #cs\" -->";
        let src = format!("<!-- quire:index:start -->\n- [[A]]\n<!-- quire:index:end -->\n{cite}\n<!-- not ours -->\n");
        let all = find_all(&src);
        assert_eq!(all.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(), ["index:start", "index:end", "cite"]);
        let c = &find(&src, "cite")[0];
        assert_eq!(&src[c.range.clone()], &cite[cite.find("<!--").unwrap()..]);
        let args = parse_args(&c.body);
        assert_eq!(
            args,
            [("v", "1"), ("sha", "1a2b"), ("l", "3-3"), ("p", "## Undo\n"), ("s", " #cs")].map(|(a, b)| (a.to_string(), b.to_string()))
        );
        let borrowed: Vec<(&str, &str)> = args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(parse_args(&format_args(&borrowed)), args);
    }

    #[test]
    fn bodies_cannot_close_the_comment() {
        let body = r#"{"t":"a --> b"}"#;
        let c = format("pdf-anchor", body);
        assert_eq!(c.matches("-->").count(), 1);
        let found = find_all(&c);
        assert_eq!(found[0].body, body);
        assert_eq!(format("index:end", ""), "<!-- quire:index:end -->");
        assert!(find_all("<!-- quire:cite never closed").is_empty());
    }
}
