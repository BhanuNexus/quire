// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Obsidian Bases expressions: formulas (`formula.x: 'price * qty'`),
//! filters and custom summaries.
//!
//! A small parser and evaluator, no scripting: arithmetic, comparisons,
//! `&&` `||` `!`, strings, lists, `if()`, `date()`, `now()`, `today()`,
//! `duration()`, `file.*` fields and methods, note properties
//! (`price`, `note.price`, `note["my price"]`), other formulas
//! (`formula.total`), and the string, number, date, list and duration
//! methods Obsidian documents. Anything else evaluates to an error that the
//! table shows in the cell ("`foo()` isn't supported").
//!
//! The note a row stands for is a [`Row`]; the app supplies it.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

/// A value in an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// A date, and whether it has a time of day.
    Date(NaiveDateTime, bool),
    /// Milliseconds.
    Dur(i64),
    List(Vec<Self>),
}

/// The note an expression is evaluated for.
pub trait Row {
    /// A front-matter property (`price`, `author.name`), `None` when missing.
    fn property(&self, name: &str) -> Option<Val>;
    /// `file.name`, `file.basename`, `file.path`, `file.folder`, `file.ext`,
    /// `file.size`, `file.ctime`, `file.mtime`, `file.tags`, `file.links`.
    fn file(&self, field: &str) -> Option<Val>;
    /// Does the note have this tag (or a tag under it)?
    fn has_tag(&self, tag: &str) -> bool;
    /// Does the note link to this note?
    fn has_link(&self, target: &str) -> bool;
}

/// Where "now" is, so results are the same for every row of a table.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub now: NaiveDateTime,
}

/// An expression's error, shown in the cell.
pub type Error = String;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Val),
    /// A bare name: a property, or `file`/`note`/`formula`/`values`.
    Name(String),
    List(Vec<Self>),
    Unary(char, Box<Self>),
    Bin(Box<Self>, &'static str, Box<Self>),
    /// `a.b`
    Field(Box<Self>, String),
    /// `a[b]`
    Index(Box<Self>, Box<Self>),
    /// `f(args)` (a global function)
    Call(String, Vec<Self>),
    /// `a.f(args)` (a method)
    Method(Box<Self>, String, Vec<Self>),
}

// ---------------------------------------------------------------- parsing

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
}

const OPS: [&str; 22] =
    ["&&", "||", "==", "!=", ">=", "<=", "=>", ">", "<", "+", "-", "*", "/", "%", "!", "(", ")", "[", "]", ",", ".", "?"];

fn lex(src: &str) -> Result<Vec<Tok>, Error> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("a string isn't closed".into()),
                    Some('\\') => {
                        if let Some(n) = chars.get(i + 1) {
                            s.push(match n {
                                'n' => '\n',
                                't' => '\t',
                                o => *o,
                            });
                        }
                        i += 2;
                    }
                    Some(q) if *q == c => {
                        i += 1;
                        break;
                    }
                    Some(o) => {
                        s.push(*o);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                // `1.round()`: a dot followed by a letter ends the number.
                if chars[i] == '.' && !chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                    break;
                }
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            out.push(Tok::Num(s.parse().map_err(|_| format!("“{s}” isn't a number"))?));
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
        if let Some(op) = OPS.iter().find(|o| o.len() == 2 && **o == two) {
            out.push(Tok::Op(op));
            i += 2;
            continue;
        }
        match OPS.iter().find(|o| o.len() == 1 && o.starts_with(c)) {
            Some(op) => out.push(Tok::Op(op)),
            None => return Err(format!("“{c}” isn't understood here")),
        }
        i += 1;
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }

    fn eat(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, op: &str) -> Result<(), Error> {
        if self.eat(op) { Ok(()) } else { Err(format!("“{op}” expected")) }
    }

    fn binary(&mut self, ops: &[&'static str], next: fn(&mut Self) -> Result<Expr, Error>) -> Result<Expr, Error> {
        let mut left = next(self)?;
        while let Some(op) = ops.iter().copied().find(|o| matches!(self.peek(), Some(Tok::Op(x)) if x == o)) {
            self.i += 1;
            let right = next(self)?;
            left = Expr::Bin(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn or(&mut self) -> Result<Expr, Error> {
        self.binary(&["||"], Self::and)
    }

    fn and(&mut self) -> Result<Expr, Error> {
        self.binary(&["&&"], Self::cmp)
    }

    fn cmp(&mut self) -> Result<Expr, Error> {
        self.binary(&["==", "!=", ">=", "<=", ">", "<"], Self::add)
    }

    fn add(&mut self) -> Result<Expr, Error> {
        self.binary(&["+", "-"], Self::mul)
    }

    fn mul(&mut self) -> Result<Expr, Error> {
        self.binary(&["*", "/", "%"], Self::unary)
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        if self.eat("!") {
            return Ok(Expr::Unary('!', Box::new(self.unary()?)));
        }
        if self.eat("-") {
            return Ok(Expr::Unary('-', Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn args(&mut self) -> Result<Vec<Expr>, Error> {
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.or()?);
            if self.eat(")") {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    fn postfix(&mut self) -> Result<Expr, Error> {
        let mut e = self.primary()?;
        loop {
            if self.eat(".") {
                let Some(Tok::Ident(name)) = self.peek().cloned() else { return Err("a name is expected after “.”".into()) };
                self.i += 1;
                e = if self.eat("(") { Expr::Method(Box::new(e), name, self.args()?) } else { Expr::Field(Box::new(e), name) };
            } else if self.eat("[") {
                let ix = self.or()?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(ix));
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        let t = self.peek().cloned().ok_or_else(|| "the expression ends too soon".to_string())?;
        self.i += 1;
        Ok(match t {
            Tok::Num(n) => Expr::Lit(Val::Num(n)),
            Tok::Str(s) => Expr::Lit(Val::Str(s)),
            Tok::Ident(id) => match id.as_str() {
                "true" => Expr::Lit(Val::Bool(true)),
                "false" => Expr::Lit(Val::Bool(false)),
                "null" => Expr::Lit(Val::Null),
                _ if self.eat("(") => Expr::Call(id, self.args()?),
                _ => Expr::Name(id),
            },
            Tok::Op("(") => {
                let e = self.or()?;
                self.expect(")")?;
                e
            }
            Tok::Op("[") => {
                let mut items = Vec::new();
                if !self.eat("]") {
                    loop {
                        items.push(self.or()?);
                        if self.eat("]") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                Expr::List(items)
            }
            Tok::Op(o) => return Err(format!("“{o}” isn't expected here")),
        })
    }
}

/// Parse an expression.
pub fn parse(src: &str) -> Result<Expr, Error> {
    let mut p = Parser { toks: lex(src)?, i: 0 };
    let e = p.or()?;
    if p.i < p.toks.len() {
        return Err(format!("unexpected “{}”", tok_text(&p.toks[p.i])));
    }
    Ok(e)
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Num(n) => n.to_string(),
        Tok::Str(s) => format!("\"{s}\""),
        Tok::Ident(s) => s.clone(),
        Tok::Op(o) => (*o).to_string(),
    }
}

// ---------------------------------------------------------------- values

/// A string as the value it looks like: `true`, `3.5`, `2026-11-05`.
pub fn infer(s: &str) -> Val {
    let t = s.trim();
    match t {
        "" => Val::Null,
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        _ => {
            if let Ok(n) = t.parse::<f64>()
                && n.is_finite()
                && !t.starts_with('+')
            {
                return Val::Num(n);
            }
            parse_date(t).map_or_else(|| Val::Str(s.to_string()), |(d, time)| Val::Date(d, time))
        }
    }
}

fn parse_date(s: &str) -> Option<(NaiveDateTime, bool)> {
    let s = s.trim();
    if s.len() < 10 {
        return None;
    }
    let norm = s.replacen('T', " ", 1);
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(norm.get(..19.min(norm.len())).unwrap_or(&norm), f) {
            return Some((d, true));
        }
        if let Ok(d) = NaiveDateTime::parse_from_str(&norm, f) {
            return Some((d, true));
        }
    }
    let d = NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?;
    (s.len() == 10).then(|| (d.and_time(NaiveTime::MIN), false))
}

/// `"7d"`, `"1 week"`, `"2h 30m"` as milliseconds; months and years count
/// as 30 and 365 days.
fn parse_duration(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut rest = s.trim();
    let mut any = false;
    while !rest.is_empty() {
        let sign = if let Some(r) = rest.strip_prefix('-') {
            rest = r.trim_start();
            -1
        } else {
            1
        };
        let n_len = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
        let n: f64 = rest[..n_len].parse().ok()?;
        rest = rest[n_len..].trim_start();
        let u_len = rest.find(|c: char| !c.is_alphabetic()).unwrap_or(rest.len());
        let unit = &rest[..u_len];
        rest = rest[u_len..].trim_start_matches([' ', ',']);
        let ms: f64 = match unit {
            "y" | "yr" | "yrs" | "year" | "years" => 365.0 * 86_400_000.0,
            "M" | "month" | "months" => 30.0 * 86_400_000.0,
            "w" | "week" | "weeks" => 7.0 * 86_400_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "ms" | "millisecond" | "milliseconds" => 1.0,
            _ => return None,
        };
        total += sign * (n * ms) as i64;
        any = true;
    }
    any.then_some(total)
}

/// Add a duration to a date. Whole months and years move by the calendar.
fn shift(d: NaiveDateTime, text: Option<&str>, ms: i64) -> NaiveDateTime {
    if let Some(t) = text {
        let t = t.trim();
        let (sign, body) = t.strip_prefix('-').map_or((1, t), |r| (-1, r.trim_start()));
        let n_len = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
        if let Ok(n) = body[..n_len].parse::<i32>() {
            let months = match body[n_len..].trim() {
                "M" | "month" | "months" => Some(n),
                "y" | "year" | "years" => Some(n * 12),
                _ => None,
            };
            if let Some(m) = months {
                let total = d.year() * 12 + d.month0() as i32 + sign * m;
                let (y, mo) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
                let last = (28..=31).rev().find(|day| NaiveDate::from_ymd_opt(y, mo, *day).is_some()).unwrap_or(28);
                if let Some(day) = NaiveDate::from_ymd_opt(y, mo, d.day().min(last)) {
                    return day.and_time(d.time());
                }
            }
        }
    }
    d + Duration::milliseconds(ms)
}

impl Val {
    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Num(n) => *n != 0.0 && !n.is_nan(),
            Self::Str(s) => !s.is_empty(),
            Self::List(l) => !l.is_empty(),
            Self::Date(..) | Self::Dur(_) => true,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Num(_) => "number",
            Self::Str(_) => "string",
            Self::Date(..) => "date",
            Self::Dur(_) => "duration",
            Self::List(_) => "list",
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Bool(b) => Some(f64::from(u8::from(*b))),
            Self::Str(s) => s.trim().parse().ok(),
            Self::Dur(ms) => Some(*ms as f64),
            _ => None,
        }
    }

    fn date(&self) -> Option<(NaiveDateTime, bool)> {
        match self {
            Self::Date(d, t) => Some((*d, *t)),
            Self::Str(s) => parse_date(s),
            _ => None,
        }
    }

    /// The value as the table shows it.
    pub fn show(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(b) => b.to_string(),
            Self::Num(n) => fmt_num(*n),
            Self::Str(s) => s.clone(),
            Self::Date(d, true) => d.format("%Y-%m-%d %H:%M").to_string(),
            Self::Date(d, false) => d.format("%Y-%m-%d").to_string(),
            Self::Dur(ms) => fmt_duration(*ms),
            Self::List(l) => l.iter().map(Self::show).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
        }
    }
}

fn fmt_num(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{n:.0}");
    }
    let s = format!("{n:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn fmt_duration(ms: i64) -> String {
    let (sign, mut ms) = if ms < 0 { ("-", -ms) } else { ("", ms) };
    let mut parts = Vec::new();
    for (unit, size) in [("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1000)] {
        if ms >= size {
            parts.push(format!("{}{unit}", ms / size));
            ms %= size;
        }
    }
    if parts.is_empty() {
        return format!("{sign}0s");
    }
    format!("{sign}{}", parts.join(" "))
}

/// Moment.js-style tokens (`YYYY-MM-DD HH:mm`) for a date.
fn format_date(d: NaiveDateTime, fmt: &str) -> String {
    const TOKENS: [(&str, &str); 16] = [
        ("YYYY", "%Y"),
        ("MMMM", "%B"),
        ("dddd", "%A"),
        ("MMM", "%b"),
        ("ddd", "%a"),
        ("YY", "%y"),
        ("MM", "%m"),
        ("DD", "%d"),
        ("HH", "%H"),
        ("hh", "%I"),
        ("mm", "%M"),
        ("ss", "%S"),
        ("M", "%-m"),
        ("D", "%-d"),
        ("H", "%-H"),
        ("A", "%p"),
    ];
    let mut out = String::new();
    let mut rest = fmt;
    'outer: while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            // `[literal]`
            let end = r.find(']').unwrap_or(r.len());
            out.push_str(&r[..end].replace('%', "%%"));
            rest = r.get(end + 1..).unwrap_or("");
            continue;
        }
        for (t, f) in TOKENS {
            if let Some(r) = rest.strip_prefix(t) {
                out.push_str(f);
                rest = r;
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        if c == '%' {
            out.push_str("%%");
        } else {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    let mut s = String::new();
    let _ = write!(s, "{}", d.format(&out));
    s
}

fn relative(d: NaiveDateTime, now: NaiveDateTime) -> String {
    let secs = (now - d).num_seconds();
    let (n, past) = (secs.abs(), secs >= 0);
    let text = match n {
        0..45 => return "just now".into(),
        45..5400 => format!("{} minutes", (n / 60).max(1)),
        5400..129_600 => format!("{} hours", (n / 3600).max(1)),
        129_600..2_592_000 => format!("{} days", n / 86_400),
        2_592_000..31_536_000 => format!("{} months", n / 2_592_000),
        _ => format!("{} years", n / 31_536_000),
    };
    if past { format!("{text} ago") } else { format!("in {text}") }
}

// ---------------------------------------------------------------- evaluation

/// Evaluates expressions for rows of one base.
pub struct Eval<'a> {
    /// Formulas by name (`formula.x` refers to them).
    pub formulas: &'a BTreeMap<String, Result<Expr, Error>>,
    pub clock: Clock,
}

struct Scope<'r> {
    row: &'r dyn Row,
    /// Formulas being evaluated (a formula that uses itself is an error).
    stack: Vec<String>,
    /// `value` and `index` inside `filter()` and `map()`; `values` in summaries.
    vars: Vec<(String, Val)>,
}

fn unsupported(what: &str) -> Error {
    format!("“{what}” isn't supported")
}

impl Eval<'_> {
    /// One expression for one row.
    pub fn eval(&self, e: &Expr, row: &dyn Row) -> Result<Val, Error> {
        self.eval_in(e, &mut Scope { row, stack: Vec::new(), vars: Vec::new() })
    }

    /// A formula by name for one row.
    pub fn formula(&self, name: &str, row: &dyn Row) -> Result<Val, Error> {
        self.formula_in(name, &mut Scope { row, stack: Vec::new(), vars: Vec::new() })
    }

    /// A custom summary: the expression with `values` bound to a column.
    pub fn summary(&self, e: &Expr, values: Vec<Val>, row: &dyn Row) -> Result<Val, Error> {
        self.eval_in(e, &mut Scope { row, stack: Vec::new(), vars: vec![("values".into(), Val::List(values))] })
    }

    fn formula_in(&self, name: &str, sc: &mut Scope<'_>) -> Result<Val, Error> {
        let Some(f) = self.formulas.get(name) else { return Err(format!("there is no formula “{name}”")) };
        let e = f.as_ref().map_err(Clone::clone)?;
        if sc.stack.iter().any(|s| s == name) || sc.stack.len() > 32 {
            return Err(format!("the formula “{name}” uses itself"));
        }
        sc.stack.push(name.to_string());
        let v = self.eval_in(e, sc);
        sc.stack.pop();
        v
    }

    fn eval_in(&self, e: &Expr, sc: &mut Scope<'_>) -> Result<Val, Error> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::List(items) => Ok(Val::List(items.iter().map(|i| self.eval_in(i, sc)).collect::<Result<_, _>>()?)),
            Expr::Name(n) => {
                if let Some((_, v)) = sc.vars.iter().rev().find(|(k, _)| k == n) {
                    return Ok(v.clone());
                }
                match n.as_str() {
                    "file" | "note" | "formula" | "this" => Err(format!("“{n}” needs a field, like {n}.name")),
                    _ => Ok(sc.row.property(n).unwrap_or(Val::Null)),
                }
            }
            Expr::Field(base, f) => match base.as_ref() {
                Expr::Name(b) if b == "file" && !sc.vars.iter().any(|(k, _)| k == b) => match f.as_str() {
                    "name" | "basename" | "path" | "folder" | "ext" | "size" | "ctime" | "mtime" | "tags" | "links" => {
                        Ok(sc.row.file(f).unwrap_or(Val::Null))
                    }
                    _ => Err(unsupported(&format!("file.{f}"))),
                },
                Expr::Name(b) if b == "note" => Ok(sc.row.property(f).unwrap_or(Val::Null)),
                Expr::Name(b) if b == "formula" => self.formula_in(f, sc),
                Expr::Name(b) if b == "this" => Err(unsupported("this")),
                _ => {
                    let v = self.eval_in(base, sc)?;
                    field(&v, f)
                }
            },
            Expr::Index(base, ix) => {
                let i = self.eval_in(ix, sc)?;
                if let Expr::Name(b) = base.as_ref()
                    && b == "note"
                    && let Val::Str(k) = &i
                {
                    return Ok(sc.row.property(k).unwrap_or(Val::Null));
                }
                let v = self.eval_in(base, sc)?;
                match (&v, &i) {
                    (Val::List(l), _) => {
                        let n = i.num().ok_or("a list index is a number")? as i64;
                        let n = if n < 0 { l.len() as i64 + n } else { n };
                        Ok(usize::try_from(n).ok().and_then(|n| l.get(n)).cloned().unwrap_or(Val::Null))
                    }
                    (Val::Str(s), Val::Num(_)) => {
                        let n = i.num().unwrap_or(0.0) as usize;
                        Ok(s.chars().nth(n).map_or(Val::Null, |c| Val::Str(c.to_string())))
                    }
                    _ => Ok(Val::Null),
                }
            }
            Expr::Unary(op, x) => {
                let v = self.eval_in(x, sc)?;
                match op {
                    '!' => Ok(Val::Bool(!v.truthy())),
                    _ => match v {
                        Val::Dur(ms) => Ok(Val::Dur(-ms)),
                        v => v.num().map(|n| Val::Num(-n)).ok_or_else(|| format!("can't negate a {}", v.type_name())),
                    },
                }
            }
            Expr::Bin(a, op, b) => {
                if *op == "&&" {
                    let l = self.eval_in(a, sc)?;
                    return if l.truthy() { Ok(Val::Bool(self.eval_in(b, sc)?.truthy())) } else { Ok(Val::Bool(false)) };
                }
                if *op == "||" {
                    let l = self.eval_in(a, sc)?;
                    return if l.truthy() { Ok(Val::Bool(true)) } else { Ok(Val::Bool(self.eval_in(b, sc)?.truthy())) };
                }
                let (l, r) = (self.eval_in(a, sc)?, self.eval_in(b, sc)?);
                binary(&l, op, &r)
            }
            Expr::Call(name, args) => self.call(name, args, sc),
            Expr::Method(base, name, args) => {
                if let Expr::Name(b) = base.as_ref()
                    && b == "file"
                    && !sc.vars.iter().any(|(k, _)| k == b)
                {
                    return self.file_method(name, args, sc);
                }
                let v = self.eval_in(base, sc)?;
                if matches!(name.as_str(), "filter" | "map" | "reduce") {
                    return self.higher(&v, name, args, sc);
                }
                let args: Vec<Val> = args.iter().map(|x| self.eval_in(x, sc)).collect::<Result<_, _>>()?;
                method(&v, name, &args, self.clock)
            }
        }
    }

    fn file_method(&self, name: &str, args: &[Expr], sc: &mut Scope<'_>) -> Result<Val, Error> {
        let args: Vec<Val> = args.iter().map(|x| self.eval_in(x, sc)).collect::<Result<_, _>>()?;
        let strs: Vec<String> = args.iter().map(Val::show).collect();
        Ok(Val::Bool(match name {
            "hasTag" => strs.iter().any(|t| sc.row.has_tag(t.trim_start_matches('#'))),
            "inFolder" => {
                let folder = sc.row.file("folder").map(|v| v.show()).unwrap_or_default();
                strs.iter().any(|f| {
                    let f = f.trim_matches('/');
                    f.is_empty() || folder.eq_ignore_ascii_case(f) || folder.to_lowercase().starts_with(&format!("{}/", f.to_lowercase()))
                })
            }
            "hasLink" => strs.iter().any(|t| sc.row.has_link(t)),
            "hasProperty" => strs.iter().any(|p| sc.row.property(p).is_some()),
            _ => return Err(unsupported(&format!("file.{name}()"))),
        }))
    }

    fn higher(&self, v: &Val, name: &str, args: &[Expr], sc: &mut Scope<'_>) -> Result<Val, Error> {
        let Val::List(items) = v else { return Err(format!("{name}() works on lists")) };
        let f = args.first().ok_or_else(|| format!("{name}() needs an expression"))?;
        if name == "reduce" {
            let mut acc = match args.get(1) {
                Some(init) => self.eval_in(init, sc)?,
                None => Val::Null,
            };
            for (i, item) in items.iter().enumerate() {
                sc.vars.push(("value".into(), item.clone()));
                sc.vars.push(("index".into(), Val::Num(i as f64)));
                sc.vars.push(("acc".into(), acc.clone()));
                let r = self.eval_in(f, sc);
                sc.vars.truncate(sc.vars.len() - 3);
                acc = r?;
            }
            return Ok(acc);
        }
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            sc.vars.push(("value".into(), item.clone()));
            sc.vars.push(("index".into(), Val::Num(i as f64)));
            let r = self.eval_in(f, sc);
            sc.vars.truncate(sc.vars.len() - 2);
            let r = r?;
            if name == "map" {
                out.push(r);
            } else if r.truthy() {
                out.push(item.clone());
            }
        }
        Ok(Val::List(out))
    }

    fn call(&self, name: &str, args: &[Expr], sc: &mut Scope<'_>) -> Result<Val, Error> {
        if name == "if" {
            let cond = self.eval_in(args.first().ok_or("if() needs a condition")?, sc)?;
            return match (cond.truthy(), args.get(1), args.get(2)) {
                (true, Some(a), _) => self.eval_in(a, sc),
                (false, _, Some(b)) => self.eval_in(b, sc),
                _ => Ok(Val::Null),
            };
        }
        let v: Vec<Val> = args.iter().map(|x| self.eval_in(x, sc)).collect::<Result<_, _>>()?;
        let now = self.clock.now;
        match name {
            "now" => Ok(Val::Date(now, true)),
            "today" => Ok(Val::Date(now.date().and_time(NaiveTime::MIN), false)),
            "date" => {
                let a = v.first().ok_or("date() needs a date")?;
                a.date().map(|(d, t)| Val::Date(d, t)).ok_or_else(|| format!("“{}” isn't a date", a.show()))
            }
            "duration" => {
                let a = v.first().ok_or("duration() needs text like \"7d\"")?;
                parse_duration(&a.show()).map(Val::Dur).ok_or_else(|| format!("“{}” isn't a duration", a.show()))
            }
            "number" => {
                let a = v.first().cloned().unwrap_or(Val::Null);
                Ok(match a {
                    Val::Date(d, _) => Val::Num(d.and_utc().timestamp_millis() as f64),
                    a => a.num().map_or(Val::Null, Val::Num),
                })
            }
            "min" | "max" => {
                let nums: Vec<f64> =
                    v.iter().flat_map(|x| if let Val::List(l) = x { l.clone() } else { vec![x.clone()] }).filter_map(|x| x.num()).collect();
                let pick = if name == "min" { nums.iter().copied().reduce(f64::min) } else { nums.iter().copied().reduce(f64::max) };
                Ok(pick.map_or(Val::Null, Val::Num))
            }
            "list" => Ok(match v.into_iter().next() {
                Some(Val::List(l)) => Val::List(l),
                Some(Val::Null) | None => Val::List(Vec::new()),
                Some(x) => Val::List(vec![x]),
            }),
            "string" => Ok(Val::Str(v.first().map(Val::show).unwrap_or_default())),
            "link" => Ok(Val::Str(v.get(1).or_else(|| v.first()).map(Val::show).unwrap_or_default())),
            _ => Err(unsupported(&format!("{name}()"))),
        }
    }
}

fn field(v: &Val, f: &str) -> Result<Val, Error> {
    Ok(match (v, f) {
        (Val::Null, _) => Val::Null,
        (Val::Str(s), "length") => Val::Num(s.chars().count() as f64),
        (Val::List(l), "length") => Val::Num(l.len() as f64),
        (Val::Date(d, _), _) => match f {
            "year" => Val::Num(f64::from(d.year())),
            "month" => Val::Num(f64::from(d.month())),
            "day" => Val::Num(f64::from(d.day())),
            "hour" => Val::Num(f64::from(d.hour())),
            "minute" => Val::Num(f64::from(d.minute())),
            "second" => Val::Num(f64::from(d.second())),
            "millisecond" => Val::Num(f64::from(d.and_utc().timestamp_subsec_millis())),
            _ => return Err(unsupported(&format!("date.{f}"))),
        },
        (Val::Dur(ms), _) => {
            let ms = *ms as f64;
            match f {
                "days" => Val::Num((ms / 86_400_000.0).trunc()),
                "hours" => Val::Num((ms / 3_600_000.0).trunc()),
                "minutes" => Val::Num((ms / 60_000.0).trunc()),
                "seconds" => Val::Num((ms / 1000.0).trunc()),
                "milliseconds" => Val::Num(ms),
                _ => return Err(unsupported(&format!("duration.{f}"))),
            }
        }
        (Val::Str(s), _) if f == "date" => infer(s),
        (v, f) => return Err(format!("a {} has no “{f}”", v.type_name())),
    })
}

fn add_dur(d: NaiveDateTime, t: bool, r: &Val, neg: bool) -> Option<Val> {
    let (ms, text) = match r {
        Val::Dur(ms) => (*ms, None),
        Val::Str(s) => (parse_duration(s)?, Some(s.as_str())),
        _ => return None,
    };
    let text = text.map(|s| if neg { format!("-{s}") } else { s.to_string() });
    let moved = shift(d, text.as_deref(), if neg { -ms } else { ms });
    Some(Val::Date(moved, t || moved.time() != NaiveTime::MIN))
}

fn binary(l: &Val, op: &str, r: &Val) -> Result<Val, Error> {
    use std::cmp::Ordering;
    match op {
        "==" => return Ok(Val::Bool(equal(l, r))),
        "!=" => return Ok(Val::Bool(!equal(l, r))),
        ">" | "<" | ">=" | "<=" => {
            let ord = order(l, r);
            return Ok(Val::Bool(match (op, ord) {
                (_, None) => false,
                (">", Some(o)) => o == Ordering::Greater,
                ("<", Some(o)) => o == Ordering::Less,
                (">=", Some(o)) => o != Ordering::Less,
                (_, Some(o)) => o != Ordering::Greater,
            }));
        }
        _ => {}
    }
    match (l, op, r) {
        (Val::Null, _, _) | (_, _, Val::Null) => Ok(Val::Null),
        (Val::Date(d, t), "+" | "-", r) if !matches!(r, Val::Date(..)) => add_dur(*d, *t, r, op == "-")
            .ok_or_else(|| format!("can't {} a {} and a date", if op == "+" { "add" } else { "subtract" }, r.type_name())),
        (Val::Date(a, _), "-", Val::Date(b, _)) => Ok(Val::Dur((*a - *b).num_milliseconds())),
        (Val::Dur(a), "+", Val::Dur(b)) => Ok(Val::Dur(a + b)),
        (Val::Dur(a), "-", Val::Dur(b)) => Ok(Val::Dur(a - b)),
        (Val::Dur(a), "*" | "/", Val::Num(n)) => Ok(Val::Dur(if op == "*" { (*a as f64 * n) as i64 } else { (*a as f64 / n) as i64 })),
        (Val::Str(a), "+", b) => Ok(Val::Str(format!("{a}{}", b.show()))),
        (a, "+", Val::Str(b)) if !matches!(a, Val::Num(_)) || b.trim().parse::<f64>().is_err() => Ok(Val::Str(format!("{}{b}", a.show()))),
        (Val::List(a), "+", Val::List(b)) => Ok(Val::List(a.iter().chain(b).cloned().collect())),
        _ => {
            let (Some(a), Some(b)) = (l.num(), r.num()) else {
                return Err(format!("can't use “{op}” on a {} and a {}", l.type_name(), r.type_name()));
            };
            Ok(Val::Num(match op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                "/" => a / b,
                _ => a % b,
            }))
        }
    }
}

fn equal(l: &Val, r: &Val) -> bool {
    match (l, r) {
        (Val::Null, Val::Null) => true,
        (Val::Null, _) | (_, Val::Null) => false,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::List(a), Val::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| equal(x, y)),
        _ => order(l, r) == Some(std::cmp::Ordering::Equal) || l.show() == r.show(),
    }
}

/// Sort order of two values (numbers, dates, text); empty values go last.
pub fn compare(l: &Val, r: &Val) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (l, r) {
        (Val::Null, Val::Null) => Ordering::Equal,
        (Val::Null, _) => Ordering::Greater,
        (_, Val::Null) => Ordering::Less,
        _ => order(l, r).unwrap_or_else(|| l.show().cmp(&r.show())),
    }
}

fn order(l: &Val, r: &Val) -> Option<std::cmp::Ordering> {
    match (l, r) {
        (Val::Null, _) | (_, Val::Null) => None,
        (Val::Date(..), _) | (_, Val::Date(..)) => Some(l.date()?.0.cmp(&r.date()?.0)),
        (Val::Str(a), Val::Str(b)) => Some(a.to_lowercase().cmp(&b.to_lowercase())),
        (Val::Dur(a), Val::Str(s)) => parse_duration(s).map(|b| a.cmp(&b)),
        (Val::Str(s), Val::Dur(b)) => parse_duration(s).map(|a| a.cmp(b)),
        _ => l.num()?.partial_cmp(&r.num()?),
    }
}

fn arg_str(args: &[Val], i: usize) -> String {
    args.get(i).map(Val::show).unwrap_or_default()
}

fn arg_num(args: &[Val], i: usize) -> Option<f64> {
    args.get(i).and_then(Val::num)
}

fn round_to(n: f64, digits: f64) -> f64 {
    let p = 10f64.powi(digits as i32);
    (n * p).round() / p
}

fn method(v: &Val, name: &str, args: &[Val], clock: Clock) -> Result<Val, Error> {
    // Methods every type has.
    match name {
        "isEmpty" => {
            return Ok(Val::Bool(match v {
                Val::Null => true,
                Val::Str(s) => s.is_empty(),
                Val::List(l) => l.is_empty(),
                _ => false,
            }));
        }
        "isTruthy" => return Ok(Val::Bool(v.truthy())),
        "toString" => return Ok(Val::Str(v.show())),
        "isType" => return Ok(Val::Bool(v.type_name() == arg_str(args, 0).to_lowercase())),
        _ => {}
    }
    match v {
        Val::Null => Ok(Val::Null),
        Val::Str(s) => {
            let a0 = arg_str(args, 0);
            Ok(match name {
                "contains" => Val::Bool(s.contains(&a0)),
                "containsAll" => Val::Bool(args.iter().all(|a| s.contains(&a.show()))),
                "containsAny" => Val::Bool(args.iter().any(|a| s.contains(&a.show()))),
                "startsWith" => Val::Bool(s.starts_with(&a0)),
                "endsWith" => Val::Bool(s.ends_with(&a0)),
                "lower" => Val::Str(s.to_lowercase()),
                "upper" => Val::Str(s.to_uppercase()),
                "title" => Val::Str(
                    s.split(' ')
                        .map(|w| {
                            let mut c = w.chars();
                            c.next().map(|f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect::<String>()).unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                "trim" => Val::Str(s.trim().to_string()),
                "replace" => Val::Str(s.replace(&a0, &arg_str(args, 1))),
                "repeat" => Val::Str(s.repeat(arg_num(args, 0).unwrap_or(1.0).clamp(0.0, 10_000.0) as usize)),
                "reverse" => Val::Str(s.chars().rev().collect()),
                "slice" => {
                    let chars: Vec<char> = s.chars().collect();
                    let (a, b) = slice_bounds(chars.len(), arg_num(args, 0), arg_num(args, 1));
                    Val::Str(chars[a..b].iter().collect())
                }
                "split" => {
                    let parts: Vec<Val> = if a0.is_empty() {
                        s.chars().map(|c| Val::Str(c.to_string())).collect()
                    } else {
                        s.split(a0.as_str()).map(|p| Val::Str(p.to_string())).collect()
                    };
                    Val::List(parts)
                }
                "date" => infer(s),
                _ => return Err(unsupported(&format!("{name}()"))),
            })
        }
        Val::Num(n) => Ok(match name {
            "abs" => Val::Num(n.abs()),
            "ceil" => Val::Num(n.ceil()),
            "floor" => Val::Num(n.floor()),
            "round" => Val::Num(round_to(*n, arg_num(args, 0).unwrap_or(0.0))),
            "toFixed" => Val::Str(format!("{n:.*}", arg_num(args, 0).unwrap_or(0.0).clamp(0.0, 20.0) as usize)),
            _ => return Err(unsupported(&format!("{name}()"))),
        }),
        Val::Date(d, _) => Ok(match name {
            "date" => Val::Date(d.date().and_time(NaiveTime::MIN), false),
            "time" => Val::Str(d.format("%H:%M:%S").to_string()),
            "format" => Val::Str(format_date(*d, &if args.is_empty() { "YYYY-MM-DD".to_string() } else { arg_str(args, 0) })),
            "relative" => Val::Str(relative(*d, clock.now)),
            _ => return Err(unsupported(&format!("{name}()"))),
        }),
        Val::Bool(_) | Val::Dur(_) => Err(unsupported(&format!("{name}()"))),
        Val::List(l) => {
            let nums = || l.iter().filter_map(Val::num).collect::<Vec<f64>>();
            Ok(match name {
                "contains" => Val::Bool(args.first().is_some_and(|a| l.iter().any(|x| equal(x, a)))),
                "containsAll" => Val::Bool(args.iter().all(|a| l.iter().any(|x| equal(x, a)))),
                "containsAny" => Val::Bool(args.iter().any(|a| l.iter().any(|x| equal(x, a)))),
                "join" => Val::Str(l.iter().map(Val::show).collect::<Vec<_>>().join(&if args.is_empty() {
                    ", ".into()
                } else {
                    arg_str(args, 0)
                })),
                "reverse" => Val::List(l.iter().rev().cloned().collect()),
                "sort" => {
                    let mut s = l.clone();
                    s.sort_by(|a, b| order(a, b).unwrap_or(std::cmp::Ordering::Equal));
                    Val::List(s)
                }
                "unique" => {
                    let mut out: Vec<Val> = Vec::new();
                    for x in l {
                        if !out.iter().any(|y| equal(x, y)) {
                            out.push(x.clone());
                        }
                    }
                    Val::List(out)
                }
                "flat" => Val::List(l.iter().flat_map(|x| if let Val::List(i) = x { i.clone() } else { vec![x.clone()] }).collect()),
                "slice" => {
                    let (a, b) = slice_bounds(l.len(), arg_num(args, 0), arg_num(args, 1));
                    Val::List(l[a..b].to_vec())
                }
                "sum" => Val::Num(nums().iter().sum()),
                "mean" | "average" => {
                    let n = nums();
                    if n.is_empty() { Val::Null } else { Val::Num(n.iter().sum::<f64>() / n.len() as f64) }
                }
                "min" => nums().into_iter().reduce(f64::min).map_or(Val::Null, Val::Num),
                "max" => nums().into_iter().reduce(f64::max).map_or(Val::Null, Val::Num),
                "median" => median(&nums()).map_or(Val::Null, Val::Num),
                _ => return Err(unsupported(&format!("{name}()"))),
            })
        }
    }
}

fn slice_bounds(len: usize, a: Option<f64>, b: Option<f64>) -> (usize, usize) {
    let fix = |x: f64| -> usize {
        let x = x as i64;
        let x = if x < 0 { len as i64 + x } else { x };
        x.clamp(0, len as i64) as usize
    };
    let s = a.map_or(0, fix);
    let e = b.map_or(len, fix);
    (s.min(e), e)
}

fn median(n: &[f64]) -> Option<f64> {
    if n.is_empty() {
        return None;
    }
    let mut v = n.to_vec();
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    Some(if v.len().is_multiple_of(2) { v[m - 1].midpoint(v[m]) } else { v[m] })
}

// ---------------------------------------------------------------- summaries

/// The summaries Obsidian offers by name.
pub const SUMMARIES: [&str; 14] = [
    "Average",
    "Min",
    "Max",
    "Sum",
    "Range",
    "Median",
    "Stddev",
    "Earliest",
    "Latest",
    "Checked",
    "Unchecked",
    "Empty",
    "Filled",
    "Unique",
];

/// A named summary of a column's values; `None` when the name isn't one of [`SUMMARIES`].
pub fn summarize(name: &str, values: &[Val]) -> Option<Result<Val, Error>> {
    let key = name.to_lowercase();
    let filled: Vec<&Val> = values.iter().filter(|v| !matches!(v, Val::Null) && !matches!(v, Val::Str(s) if s.is_empty())).collect();
    let nums: Vec<f64> = filled.iter().filter_map(|v| if let Val::Date(..) = v { None } else { v.num() }).collect();
    let dates: Vec<NaiveDateTime> = filled.iter().filter_map(|v| v.date().map(|d| d.0)).collect();
    let has_time = filled.iter().any(|v| matches!(v, Val::Date(_, true)));
    let num_or_null = |o: Option<f64>| Ok(o.map_or(Val::Null, Val::Num));
    Some(match key.as_str() {
        "average" | "mean" => num_or_null((!nums.is_empty()).then(|| nums.iter().sum::<f64>() / nums.len() as f64)),
        "sum" => Ok(Val::Num(nums.iter().sum())),
        "min" => num_or_null(nums.iter().copied().reduce(f64::min)),
        "max" => num_or_null(nums.iter().copied().reduce(f64::max)),
        "median" => num_or_null(median(&nums)),
        "stddev" => num_or_null((nums.len() > 1).then(|| {
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            (nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64).sqrt()
        })),
        "range" => {
            if nums.is_empty() && !dates.is_empty() {
                let (lo, hi) = (dates.iter().min(), dates.iter().max());
                Ok(lo.zip(hi).map_or(Val::Null, |(a, b)| Val::Dur((*b - *a).num_milliseconds())))
            } else {
                num_or_null(nums.iter().copied().reduce(f64::max).zip(nums.iter().copied().reduce(f64::min)).map(|(a, b)| a - b))
            }
        }
        "earliest" => Ok(dates.iter().min().map_or(Val::Null, |d| Val::Date(*d, has_time))),
        "latest" => Ok(dates.iter().max().map_or(Val::Null, |d| Val::Date(*d, has_time))),
        "checked" => Ok(Val::Num(values.iter().filter(|v| matches!(v, Val::Bool(true))).count() as f64)),
        "unchecked" => Ok(Val::Num(values.iter().filter(|v| !matches!(v, Val::Bool(true))).count() as f64)),
        "empty" => Ok(Val::Num((values.len() - filled.len()) as f64)),
        "filled" => Ok(Val::Num(filled.len() as f64)),
        "unique" => {
            let mut seen: Vec<String> = filled.iter().map(|v| v.show()).collect();
            seen.sort();
            seen.dedup();
            Ok(Val::Num(seen.len() as f64))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Note {
        props: Vec<(&'static str, Val)>,
        tags: Vec<&'static str>,
    }

    impl Row for Note {
        fn property(&self, name: &str) -> Option<Val> {
            self.props.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
        }
        fn file(&self, field: &str) -> Option<Val> {
            Some(match field {
                "name" | "basename" => Val::Str("Rent".into()),
                "path" => Val::Str("Bills/Rent.md".into()),
                "folder" => Val::Str("Bills".into()),
                "ext" => Val::Str("md".into()),
                "size" => Val::Num(1200.0),
                "ctime" | "mtime" => infer("2026-11-01 08:30"),
                "tags" => Val::List(self.tags.iter().map(|t| Val::Str((*t).into())).collect()),
                _ => return None,
            })
        }
        fn has_tag(&self, tag: &str) -> bool {
            self.tags.iter().any(|t| t.eq_ignore_ascii_case(tag))
        }
        fn has_link(&self, target: &str) -> bool {
            target == "Landlord"
        }
    }

    fn note() -> Note {
        Note {
            props: vec![
                ("price", Val::Num(12.5)),
                ("qty", infer("3")),
                ("due", infer("2026-11-20")),
                ("status", Val::Str("open".into())),
                ("my price", Val::Num(2.0)),
                ("tags", Val::List(vec![Val::Str("bill".into()), Val::Str("home".into())])),
            ],
            tags: vec!["bill", "home"],
        }
    }

    fn run(src: &str) -> String {
        let mut formulas = BTreeMap::new();
        formulas.insert("total".to_string(), parse("price * qty"));
        formulas.insert("loop".to_string(), parse("formula.loop + 1"));
        let clock = Clock { now: infer("2026-11-05 10:00").date().map(|d| d.0).unwrap_or_default() };
        let ev = Eval { formulas: &formulas, clock };
        match parse(src).and_then(|e| ev.eval(&e, &note())) {
            Ok(v) => v.show(),
            Err(e) => format!("error: {e}"),
        }
    }

    #[test]
    fn arithmetic_and_comparisons() {
        assert_eq!(run("price * qty"), "37.5");
        assert_eq!(run("(price + 0.5) / 2 - 1"), "5.5");
        assert_eq!(run("10 % 4 + -2"), "0");
        assert_eq!(run("formula.total > 30 && status == \"open\""), "true");
        assert_eq!(run("!(qty >= 3) || price < 1"), "false");
        assert_eq!(run("status != 'done'"), "true");
        assert_eq!(run("note.price + note[\"my price\"]"), "14.5");
        assert_eq!(run("missing + 1"), "", "a missing property is null");
        assert_eq!(run("\"Total: \" + formula.total"), "Total: 37.5");
    }

    #[test]
    fn functions_and_methods() {
        assert_eq!(run("if(price > 10, \"dear\", \"cheap\")"), "dear");
        assert_eq!(run("if(false, 1)"), "");
        assert_eq!(run("status.upper() + \"/\" + status.length"), "OPEN/4");
        assert_eq!(run("file.name.lower().contains('ren')"), "true");
        assert_eq!(run("\"a,b,c\".split(\",\").reverse().join(\"-\")"), "c-b-a");
        assert_eq!(run("(price / 3).round(2)"), "4.17");
        assert_eq!(run("(price / 3).toFixed(1)"), "4.2");
        assert_eq!(run("tags.contains(\"home\") && file.hasTag(\"bill\")"), "true");
        assert_eq!(run("file.inFolder(\"Bills\") && file.hasLink(\"Landlord\")"), "true");
        assert_eq!(run("[1, 2, 3].map(value * 2).filter(value > 2).sum()"), "10");
        assert_eq!(run("max(price, qty, 40)"), "40");
        assert_eq!(run("number(\"4.5\") * 2"), "9");
        assert_eq!(run("\"hello world\".title().slice(0, 7)"), "Hello W");
    }

    #[test]
    fn dates_and_durations() {
        assert_eq!(run("due - today()"), "15d");
        assert_eq!(run("(due - today()).days"), "15");
        assert_eq!(run("due + \"1 week\""), "2026-11-27");
        assert_eq!(run("date(\"2026-01-31\") + \"1M\""), "2026-02-28", "months follow the calendar");
        assert_eq!(run("due.format(\"DD MMM YYYY\")"), "20 Nov 2026");
        assert_eq!(run("due.year * 100 + due.month"), "202611");
        assert_eq!(run("file.mtime < now() - duration(\"3d\")"), "true");
        assert_eq!(run("due > \"2026-11-19\""), "true");
        assert_eq!(run("today()"), "2026-11-05");
        assert_eq!(run("now() - file.ctime"), "4d 1h 30m");
        assert_eq!(run("file.ctime.relative()"), "4 days ago");
    }

    #[test]
    fn errors_say_what_is_missing() {
        assert_eq!(run("price.frobnicate()"), "error: “frobnicate()” isn't supported");
        assert_eq!(run("image(\"x.png\")"), "error: “image()” isn't supported");
        assert_eq!(run("file.backlinks"), "error: “file.backlinks” isn't supported");
        assert_eq!(run("formula.loop"), "error: the formula “loop” uses itself");
        assert_eq!(run("formula.nope"), "error: there is no formula “nope”");
        assert!(run("price * (qty").starts_with("error:"));
        assert!(run("price ** 2").starts_with("error:"));
    }

    #[test]
    fn summaries_of_a_column() {
        let col = [Val::Num(3.0), Val::Num(5.0), Val::Null, infer("4")];
        let s = |n: &str| summarize(n, &col).map(|r| r.map(|v| v.show()).unwrap_or_default());
        assert_eq!(s("Sum").as_deref(), Some("12"));
        assert_eq!(s("Average").as_deref(), Some("4"));
        assert_eq!(s("Median").as_deref(), Some("4"));
        assert_eq!(s("Range").as_deref(), Some("2"));
        assert_eq!(s("Empty").as_deref(), Some("1"));
        assert_eq!(s("Filled").as_deref(), Some("3"));
        assert_eq!(s("Unique").as_deref(), Some("3"));
        assert_eq!(s("Stddev").as_deref(), Some("0.8164965809"));
        assert_eq!(s("nope"), None);
        let dates = [infer("2026-11-20"), infer("2026-10-01"), Val::Null];
        assert_eq!(summarize("Earliest", &dates).and_then(Result::ok).map(|v| v.show()).as_deref(), Some("2026-10-01"));
        assert_eq!(summarize("Latest", &dates).and_then(Result::ok).map(|v| v.show()).as_deref(), Some("2026-11-20"));
        assert_eq!(summarize("Range", &dates).and_then(Result::ok).map(|v| v.show()).as_deref(), Some("50d"));
        let checks = [Val::Bool(true), Val::Bool(false), Val::Null];
        assert_eq!(summarize("Checked", &checks).and_then(Result::ok).map(|v| v.show()).as_deref(), Some("1"));
    }
}
