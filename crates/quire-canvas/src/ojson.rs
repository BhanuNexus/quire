// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! JSON that keeps its key order and its numbers as written (0.13).
//!
//! Excalidraw writes its files with `JSON.stringify(scene, null, 2)` (the
//! Obsidian plugin with a tab). To save a drawing so that only what was
//! edited changes, a value is read with its keys in order, its numbers as
//! their original text, and written back the way `JSON.stringify` writes
//! it: the same indent, the same escapes, the same number forms. An
//! unedited file written back is byte-identical (tested on the fixtures).
//!
//! Strings with a lone UTF-16 surrogate (`"\ud800"`) can't be held in a
//! Rust `String`; such a string is read with U+FFFD in its place. Such text
//! is changed on save only if its element is.

use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A number as written (`120`, `0.5`, `1e-7`).
    Num(Num),
    Str(String),
    Arr(Vec<Self>),
    Obj(Obj),
}

/// A number, kept as its text so an unedited value is written back as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Num(String);

impl Num {
    /// A number written the way JavaScript's `String(n)` writes it.
    pub fn from_f64(v: f64) -> Self {
        Self(js_number(v))
    }

    pub fn as_f64(&self) -> f64 {
        self.0.parse().unwrap_or(0.0)
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

/// An object: its entries in order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Obj(pub Vec<(String, Json)>);

impl Obj {
    pub fn get(&self, k: &str) -> Option<&Json> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, k: &str) -> Option<&mut Json> {
        self.0.iter_mut().find(|(key, _)| key == k).map(|(_, v)| v)
    }

    /// Set `k`: in its place when present, else at the end.
    pub fn set(&mut self, k: &str, v: Json) {
        match self.get_mut(k) {
            Some(old) => *old = v,
            None => self.0.push((k.to_string(), v)),
        }
    }

    pub fn remove(&mut self, k: &str) -> Option<Json> {
        let i = self.0.iter().position(|(key, _)| key == k)?;
        Some(self.0.remove(i).1)
    }

    pub fn num(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(Json::as_f64)
    }

    pub fn str(&self, k: &str) -> Option<&str> {
        self.get(k).and_then(Json::as_str)
    }

    pub fn bool(&self, k: &str) -> Option<bool> {
        match self.get(k)? {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self, k: &str) -> Option<&[Json]> {
        match self.get(k)? {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn obj(&self, k: &str) -> Option<&Self> {
        match self.get(k)? {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn set_num(&mut self, k: &str, v: f64) {
        // Unchanged numbers keep their text.
        if self.num(k).is_some_and(|old| old.to_bits() == v.to_bits()) {
            return;
        }
        self.set(k, Json::num(v));
    }

    pub fn set_str(&mut self, k: &str, v: &str) {
        self.set(k, Json::Str(v.to_string()));
    }
}

impl Json {
    pub fn num(v: f64) -> Self {
        if v.is_finite() { Self::Num(Num::from_f64(v)) } else { Self::Null }
    }

    pub fn str(s: &str) -> Self {
        Self::Str(s.to_string())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(n.as_f64()).filter(|v| v.is_finite()),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub const fn as_obj(&self) -> Option<&Obj> {
        match self {
            Self::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub const fn as_obj_mut(&mut self) -> Option<&mut Obj> {
        match self {
            Self::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub const fn as_arr(&self) -> Option<&Vec<Self>> {
        match self {
            Self::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// As a `serde_json` value (for code that reads with serde).
    pub fn to_value(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(*b),
            Self::Num(n) => serde_json::from_str::<Value>(&n.0).unwrap_or(Value::Null),
            Self::Str(s) => Value::String(s.clone()),
            Self::Arr(a) => Value::Array(a.iter().map(Self::to_value).collect()),
            Self::Obj(o) => Value::Object(o.0.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
        }
    }
}

/// JavaScript's `Number.prototype.toString` for a finite number.
pub fn js_number(v: f64) -> String {
    if !v.is_finite() {
        return "null".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    let a = v.abs();
    if (1e-6..1e21).contains(&a) {
        // Rust's Display gives the shortest text that reads back, without an exponent.
        let s = format!("{v}");
        return s.strip_suffix(".0").map_or_else(|| s.clone(), str::to_string);
    }
    // 1.5e-7, 1e+21
    let s = format!("{v:e}");
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

// ---------------------------------------------------------------- reading

/// Nesting deeper than this is refused (a drawing never comes close).
const MAX_DEPTH: usize = 200;

/// A JSON document read: the value, the text before it and the text after
/// it (a final newline), and the indent it was written with.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub value: Json,
    pub before: String,
    pub after: String,
    pub indent: Indent,
}

/// How a document is indented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Indent {
    /// `JSON.stringify(v)`: one line.
    Compact,
    /// `JSON.stringify(v, null, unit)`: `"  "`, `"\t"`…
    Unit(String),
}

impl Doc {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut p = Parser { s: text.as_bytes(), text, i: 0 };
        p.ws();
        let start = p.i;
        let value = p.value(0)?;
        let end = p.i;
        p.ws();
        if p.i != text.len() {
            return Err(format!("unexpected text after the JSON at byte {}", p.i));
        }
        Ok(Self { value, before: text[..start].to_string(), after: text[end..].to_string(), indent: detect_indent(&text[start..end]) })
    }

    /// The document written back.
    pub fn write(&self) -> String {
        let mut out = self.before.clone();
        write_value(&mut out, &self.value, &self.indent, 0);
        out.push_str(&self.after);
        out
    }
}

/// The indent of the first indented line, or compact.
fn detect_indent(json: &str) -> Indent {
    let Some(nl) = json.find('\n') else { return Indent::Compact };
    let line = &json[nl + 1..];
    let unit: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    if unit.is_empty() { Indent::Compact } else { Indent::Unit(unit) }
}

/// Read one JSON value.
pub fn parse(text: &str) -> Result<Json, String> {
    Doc::parse(text).map(|d| d.value)
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at byte {}", self.i))
    }

    fn eat(&mut self, lit: &str) -> Result<(), String> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            self.err("unexpected character")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return self.err("nested too deeply");
        }
        self.ws();
        match self.s.get(self.i) {
            None => self.err("unexpected end"),
            Some(b'{') => {
                self.i += 1;
                let mut o = Obj::default();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(o));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err("expected a key");
                    }
                    let k = self.string()?;
                    self.ws();
                    self.eat(":")?;
                    let v = self.value(depth + 1)?;
                    o.0.push((k, v));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(o));
                        }
                        _ => return self.err("expected , or }"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return self.err("expected , or ]"),
                    }
                }
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.eat("true").map(|()| Json::Bool(true)),
            Some(b'f') => self.eat("false").map(|()| Json::Bool(false)),
            Some(b'n') => self.eat("null").map(|()| Json::Null),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.i;
                if self.s[self.i] == b'-' {
                    self.i += 1;
                }
                let digits = |p: &mut Self| {
                    let s = p.i;
                    while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                        p.i += 1;
                    }
                    p.i > s
                };
                if !digits(self) {
                    return self.err("bad number");
                }
                if self.s.get(self.i) == Some(&b'.') {
                    self.i += 1;
                    if !digits(self) {
                        return self.err("bad number");
                    }
                }
                if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
                    self.i += 1;
                    if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                        self.i += 1;
                    }
                    if !digits(self) {
                        return self.err("bad number");
                    }
                }
                Ok(Json::Num(Num(self.text[start..self.i].to_string())))
            }
            Some(_) => self.err("unexpected character"),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.text.get(self.i..self.i + 4).ok_or_else(|| format!("bad escape at byte {}", self.i))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| format!("bad escape at byte {}", self.i))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                if self.s[self.i] < 0x20 {
                    return self.err("control character in a string");
                }
                self.i += 1;
            }
            out.push_str(&self.text[start..self.i]);
            match self.s.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(_) => {
                    self.i += 1;
                    let Some(&c) = self.s.get(self.i) else { return self.err("unterminated string") };
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let u = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&u) && self.s[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    char::from_u32(0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00))
                                } else {
                                    self.i = save;
                                    None
                                }
                            } else {
                                char::from_u32(u)
                            };
                            out.push(ch.unwrap_or('\u{FFFD}'));
                        }
                        _ => return self.err("bad escape"),
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- writing

/// `JSON.stringify`'s string escapes.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn newline(out: &mut String, indent: &Indent, depth: usize) {
    if let Indent::Unit(u) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(u);
        }
    }
}

/// Write `v` as `JSON.stringify(v, null, indent)` would, at `depth`.
pub fn write_value(out: &mut String, v: &Json, indent: &Indent, depth: usize) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(&n.0),
        Json::Str(s) => write_str(out, s),
        Json::Arr(a) if a.is_empty() => out.push_str("[]"),
        Json::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, x, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Json::Obj(o) if o.0.is_empty() => out.push_str("{}"),
        Json::Obj(o) => {
            out.push('{');
            for (i, (k, x)) in o.0.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_str(out, k);
                out.push(':');
                if matches!(indent, Indent::Unit(_)) {
                    out.push(' ');
                }
                write_value(out, x, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

/// `v` as one compact line.
pub fn to_compact(v: &Json) -> String {
    let mut s = String::new();
    write_value(&mut s, v, &Indent::Compact, 0);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_back_what_it_read() {
        for text in [
            "{\n  \"a\": 1,\n  \"b\": [\n    1.5,\n    -0.25,\n    1e-7\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"e\": \"x\\ny\\u0001\\\"é☃\"\n}\n",
            "{\n\t\"z\": null,\n\t\"a\": true\n}",
            "{\"b\":2,\"a\":[1,2,{\"x\":\"y\"}]}",
            "  [1,2]  \n",
        ] {
            assert_eq!(Doc::parse(text).unwrap().write(), text);
        }
    }

    #[test]
    fn numbers_are_javascripts() {
        for (v, s) in [
            (1.0, "1"),
            (-0.0, "0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (123_456.789, "123456.789"),
            (-42.0, "-42"),
        ] {
            assert_eq!(js_number(v), s, "{v}");
        }
        let mut o = Obj::default();
        o.set("x", Json::Num(Num("1.50".into())));
        o.set_num("x", 1.5);
        assert_eq!(to_compact(&Json::Obj(o.clone())), "{\"x\":1.50}", "an unchanged number keeps its text");
        o.set_num("x", 2.0);
        assert_eq!(to_compact(&Json::Obj(o)), "{\"x\":2}");
    }

    #[test]
    fn refuses_bad_json_and_deep_nesting() {
        for bad in ["{", "{\"a\" 1}", "[1,]", "\"\u{1}\"", "01x", "{} x", "[\"\\q\"]"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(parse(&deep).is_err());
        assert_eq!(parse("\"\\ud83d\\ude00 \\ud800x\"").unwrap(), Json::str("😀 \u{FFFD}x"));
    }

    #[test]
    fn same_as_serde_for_values() {
        let text = "{\"a\":[1,2.5,\"s\",null,true,{\"k\":-3e2}]}";
        let ours = parse(text).unwrap().to_value();
        let theirs: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(ours, theirs);
    }
}
