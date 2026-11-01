// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inline calculator (Heynote / Soulver style).
//!
//! * `2 * (3 + 4) =`       → `= 14`
//! * `rent = 1200`         defines a variable
//! * `rent * 12 =`         → `= 14,400`
//! * `ans / 4 =`           uses the previous result
//! * `20% * 150 =`, `2^10 =`, `sqrt(2) =`, `max(3, 9) =`
//!
//! A precedence-climbing parser; anything that doesn't parse completely is
//! simply not math, so prose with an `=` is left alone.

use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct Calculator {
    vars: HashMap<String, f64>,
    ans: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Comma,
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            ' ' | '\t' => i += 1,
            '0'..='9' | '.' => {
                let st = i;
                // `,` is a thousands separator only when exactly three digits follow.
                let thousands = |i: usize| {
                    cs[i] == ','
                        && (1..=3).all(|k| cs.get(i + k).is_some_and(char::is_ascii_digit))
                        && cs.get(i + 4).is_none_or(|c| !c.is_ascii_digit())
                };
                while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.' || cs[i] == '_' || thousands(i)) {
                    i += 1;
                }
                let text: String = cs[st..i].iter().filter(|c| **c != '_' && **c != ',').collect();
                out.push(Tok::Num(text.parse().ok()?));
            }
            c if c.is_alphabetic() || c == '_' => {
                let st = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Ident(cs[st..i].iter().collect::<String>().to_lowercase()));
            }
            '+' | '-' | '*' | '/' | '^' | '%' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            '×' => {
                out.push(Tok::Op('*'));
                i += 1;
            }
            '÷' => {
                out.push(Tok::Op('/'));
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '$' | '₹' | '€' | '£' => i += 1, // currency symbols are decoration
            _ => return None,
        }
    }
    Some(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    calc: &'a Calculator,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expr(&mut self, min_prec: u8) -> Option<f64> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(c)) if *c != '%' => *c,
                _ => break,
            };
            let (prec, right_assoc) = match op {
                '+' | '-' => (1, false),
                '*' | '/' => (2, false),
                '^' => (4, true),
                _ => return None,
            };
            if prec < min_prec {
                break;
            }
            self.next();
            let rhs = self.expr(if right_assoc { prec } else { prec + 1 })?;
            lhs = match op {
                '+' => lhs + rhs,
                '-' => lhs - rhs,
                '*' => lhs * rhs,
                '/' => lhs / rhs,
                '^' => lhs.powf(rhs),
                _ => unreachable!(),
            };
        }
        Some(lhs)
    }

    fn unary(&mut self) -> Option<f64> {
        if matches!(self.peek(), Some(Tok::Op('-'))) {
            self.next();
            return Some(-self.expr(3)?);
        }
        if matches!(self.peek(), Some(Tok::Op('+'))) {
            self.next();
            return self.expr(3);
        }
        let mut v = self.atom()?;
        while matches!(self.peek(), Some(Tok::Op('%'))) {
            self.next();
            v /= 100.0;
        }
        Some(v)
    }

    fn atom(&mut self) -> Option<f64> {
        match self.next()? {
            Tok::Num(n) => Some(n),
            Tok::LParen => {
                let v = self.expr(0)?;
                matches!(self.next()?, Tok::RParen).then_some(v)
            }
            Tok::Ident(name) => {
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.next();
                    let mut args = vec![self.expr(0)?];
                    while matches!(self.peek(), Some(Tok::Comma)) {
                        self.next();
                        args.push(self.expr(0)?);
                    }
                    if !matches!(self.next()?, Tok::RParen) {
                        return None;
                    }
                    return call(&name, &args);
                }
                match name.as_str() {
                    "pi" => Some(std::f64::consts::PI),
                    "e" => Some(std::f64::consts::E),
                    "ans" | "prev" => self.calc.ans,
                    _ => self.calc.vars.get(&name).copied(),
                }
            }
            _ => None,
        }
    }
}

fn call(name: &str, a: &[f64]) -> Option<f64> {
    let one = || (a.len() == 1).then(|| a[0]);
    Some(match name {
        "sqrt" => one()?.sqrt(),
        "abs" => one()?.abs(),
        "ln" => one()?.ln(),
        "log" => one()?.log10(),
        "sin" => one()?.sin(),
        "cos" => one()?.cos(),
        "tan" => one()?.tan(),
        "round" => match a {
            [x] => x.round(),
            [x, d] => (x * 10f64.powi(*d as i32)).round() / 10f64.powi(*d as i32),
            _ => return None,
        },
        "floor" => one()?.floor(),
        "ceil" => one()?.ceil(),
        "min" => a.iter().copied().fold(f64::INFINITY, f64::min),
        "max" => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "avg" | "mean" => a.iter().sum::<f64>() / a.len() as f64,
        "sum" => a.iter().sum(),
        _ => return None,
    })
}

fn is_ident(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_alphabetic() || c == '_') && cs.all(|c| c.is_alphanumeric() || c == '_')
}

/// Drop a list or quote marker so `- 2 + 2 =` works inside lists.
fn strip_marker(line: &str) -> &str {
    let t = line.trim();
    for m in ["- ", "* ", "+ ", "> "] {
        if let Some(rest) = t.strip_prefix(m) {
            return rest.trim();
        }
    }
    t
}

/// Group thousands with `,` and keep up to 10 significant decimals.
pub fn format_number(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".into()
        } else if x > 0.0 {
            "∞".into()
        } else {
            "-∞".into()
        };
    }
    if x.abs() >= 1e15 || (x != 0.0 && x.abs() < 1e-9) {
        return format!("{x:e}");
    }
    let s = format!("{x:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if neg && (grouped != "0" || !frac.is_empty()) { "-" } else { "" };
    if frac.is_empty() { format!("{sign}{grouped}") } else { format!("{sign}{grouped}.{frac}") }
}

impl Calculator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate an expression, without side effects.
    pub fn eval(&self, expr: &str) -> Option<f64> {
        let toks = lex(expr)?;
        if toks.is_empty() {
            return None;
        }
        let mut p = Parser { toks: &toks, pos: 0, calc: self };
        let v = p.expr(0)?;
        (p.pos == toks.len()).then_some(v)
    }

    /// Process one line. Returns the value for lines of the form `expr =`
    /// or `name = expr` (the latter also defines `name`).
    pub fn line(&mut self, line: &str) -> Option<f64> {
        let t = strip_marker(line);
        if let Some(expr) = t.strip_suffix('=') {
            let v = self.eval(expr)?;
            self.ans = Some(v);
            return Some(v);
        }
        let (name, expr) = t.split_once('=')?;
        let name = name.trim().to_lowercase();
        if !is_ident(&name) || expr.trim().is_empty() || matches!(name.as_str(), "pi" | "e" | "ans") {
            return None;
        }
        let v = self.eval(expr)?;
        self.vars.insert(name, v);
        self.ans = Some(v);
        Some(v)
    }

    /// Feed every line before the caret so variables and `ans` are defined.
    /// Lines already completed (`expr = 14`) are re-evaluated as `expr =`.
    pub fn prime<'a>(&mut self, lines: impl Iterator<Item = &'a str>) {
        for l in lines {
            let t = strip_marker(l);
            if let Some((lhs, rhs)) = t.rsplit_once('=')
                && !is_ident(lhs.trim())
                && self.eval(rhs).is_some()
                && self.eval(lhs).is_some()
            {
                let _ = self.line(&format!("{lhs}="));
                continue;
            }
            let _ = self.line(t);
        }
    }

    /// Evaluate a trailing-`=` line and return the text to append (`" 14"`).
    pub fn completion_for(&mut self, line: &str) -> Option<String> {
        if !line.trim_end().ends_with('=') {
            return None;
        }
        let v = self.line(line)?;
        let pad = if line.ends_with(' ') { "" } else { " " };
        Some(format!("{pad}{}", format_number(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_expressions() {
        let c = Calculator::new();
        assert_eq!(c.eval("2 * (3 + 4)"), Some(14.0));
        assert_eq!(c.eval("2^3^2"), Some(512.0));
        assert_eq!(c.eval("-2^2"), Some(-4.0));
        assert_eq!(c.eval("20% * 150"), Some(30.0));
        assert_eq!(c.eval("1,000 + 2_500"), Some(3500.0));
        assert_eq!(c.eval("max(3, 9, 4)"), Some(9.0));
        assert_eq!(c.eval("round(2.71828, 2)"), Some(2.72));
        assert_eq!(c.eval("₹ 1200 × 3"), Some(3600.0));
        assert_eq!(c.eval("hello world"), None);
        assert_eq!(c.eval("2 +"), None);
    }

    #[test]
    fn variables_and_ans() {
        let mut c = Calculator::new();
        assert_eq!(c.line("rent = 1200"), Some(1200.0));
        assert_eq!(c.completion_for("rent * 12 ="), Some(" 14,400".into()));
        assert_eq!(c.completion_for("ans / 4 = "), Some("3,600".into()));
        assert_eq!(c.line("The result = unclear"), None);
        assert_eq!(c.completion_for("Total ="), None);
    }

    #[test]
    fn primes_from_previous_lines() {
        let mut c = Calculator::new();
        c.prime(["salary = 5000", "salary * 12 = 60,000", "Some prose."].into_iter());
        assert_eq!(c.eval("ans / 2"), Some(30000.0));
        assert_eq!(c.eval("salary"), Some(5000.0));
    }

    #[test]
    fn formats() {
        assert_eq!(format_number(1_234_567.5), "1,234,567.5");
        assert_eq!(format_number(-0.25), "-0.25");
        assert_eq!(format_number(1.0 / 3.0), "0.3333333333");
        assert_eq!(format_number(100.0), "100");
    }
}
