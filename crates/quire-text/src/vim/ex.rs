// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `:` command line. Commands that only move or edit text are done by
//! the state machine; the rest come back to the editor as [`Ex`].

use std::ops::Range;

/// A `:` command for the editor to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ex {
    /// `:w`
    Write,
    /// `:q` (closes the note's tab)
    Quit,
    /// `:wq`, `:x`, `ZZ`
    WriteQuit,
    /// `:wa`
    WriteAll,
    /// `:qa` (closes every tab)
    QuitAll,
    /// `:wqa`, `:xa`
    WriteQuitAll,
    /// `:s` and `:%s`: replace `pattern` (a regular expression) on these lines.
    Substitute(Substitute),
    /// `:noh`: nothing to clear (search matches aren't highlighted).
    NoHighlight,
}

/// `:[range]s/pattern/replacement/[g][i]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitute {
    /// 0-based lines, end exclusive.
    pub lines: Range<usize>,
    pub pattern: String,
    /// In Vim's syntax: `&` is the match, `\1`… are groups.
    pub replacement: String,
    pub global: bool,
    pub ignore_case: bool,
}

impl Substitute {
    /// The replacement in Rust `regex` syntax (`$0`, `${1}`).
    pub fn regex_replacement(&self) -> String {
        let mut out = String::new();
        let mut chars = self.replacement.chars();
        while let Some(c) = chars.next() {
            match c {
                '&' => out.push_str("${0}"),
                '$' => out.push_str("$$"),
                '\\' => match chars.next() {
                    Some(d @ '0'..='9') => {
                        out.push_str("${");
                        out.push(d);
                        out.push('}');
                    }
                    Some('n' | 'r') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                },
                c => out.push(c),
            }
        }
        out
    }

    /// The pattern in Rust `regex` syntax: Vim's `\(`, `\)`, `\|`, `\{`, `\<` and `\>`
    /// become their regex forms.
    pub fn regex_pattern(&self) -> String {
        vim_to_regex(&self.pattern)
    }
}

/// Vim's "magic" pattern escapes to Rust regex.
pub fn vim_to_regex(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(m @ ('(' | ')' | '|' | '{' | '}' | '+' | '?' | '=')) => out.push(if m == '=' { '?' } else { m }),
            Some('<' | '>') => out.push_str("\\b"),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push_str("\\\\"),
        }
    }
    out
}

/// What a command line asks for, before the state machine acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Parsed {
    Nothing,
    Goto(usize),
    DeleteLines(Range<usize>),
    YankLines(Range<usize>),
    Ex(Ex),
    Error(String),
}

/// Where a command runs: `cur` is the cursor's line, `last` the last line,
/// `visual` the lines of the last Visual selection (for `'<,'>`).
pub(super) struct Lines {
    pub cur: usize,
    pub last: usize,
    pub visual: Option<(usize, usize)>,
}

fn address(s: &str, at: &Lines) -> Option<(usize, usize)> {
    // (line, bytes used); lines 0-based.
    let bytes = s.as_bytes();
    match bytes.first()? {
        b'.' => Some((at.cur, 1)),
        b'$' => Some((at.last, 1)),
        b'\'' if bytes.get(1) == Some(&b'<') => Some((at.visual.map_or(at.cur, |v| v.0), 2)),
        b'\'' if bytes.get(1) == Some(&b'>') => Some((at.visual.map_or(at.cur, |v| v.1), 2)),
        b'0'..=b'9' => {
            let n = s.bytes().take_while(u8::is_ascii_digit).count();
            let v: usize = s[..n].parse().ok()?;
            Some((v.saturating_sub(1).min(at.last), n))
        }
        _ => None,
    }
}

/// `%`, `a,b` or `a`: the range and the rest of the command.
fn range<'a>(s: &'a str, at: &Lines) -> (Option<(usize, usize)>, &'a str) {
    if let Some(rest) = s.strip_prefix('%') {
        return (Some((0, at.last)), rest);
    }
    let Some((a, n)) = address(s, at) else { return (None, s) };
    let rest = &s[n..];
    if let Some(r2) = rest.strip_prefix(',')
        && let Some((b, m)) = address(r2, at)
    {
        return (Some((a.min(b), a.max(b))), &r2[m..]);
    }
    (Some((a, a)), rest)
}

fn substitute(rest: &str, lines: (usize, usize)) -> Parsed {
    let mut chars = rest.chars();
    let Some(sep) = chars.next().filter(|c| !c.is_alphanumeric() && *c != '\\' && *c != ' ') else {
        return Parsed::Error("Usage: :s/pattern/replacement/g".into());
    };
    // Split on unescaped separators.
    let mut parts = vec![String::new()];
    let mut esc = false;
    for c in chars {
        if esc {
            if c != sep {
                parts.last_mut().expect("never empty").push('\\');
            }
            parts.last_mut().expect("never empty").push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == sep && parts.len() < 3 {
            parts.push(String::new());
        } else {
            parts.last_mut().expect("never empty").push(c);
        }
    }
    if esc {
        parts.last_mut().expect("never empty").push('\\');
    }
    let pattern = parts[0].clone();
    let replacement = parts.get(1).cloned().unwrap_or_default();
    let flags = parts.get(2).cloned().unwrap_or_default();
    if let Some(bad) = flags.chars().find(|c| !matches!(c, 'g' | 'i' | 'I' | 'c' | 'e')) {
        return Parsed::Error(format!("Unknown flag: {bad}"));
    }
    Parsed::Ex(Ex::Substitute(Substitute {
        lines: lines.0..lines.1 + 1,
        pattern,
        replacement,
        global: flags.contains('g'),
        ignore_case: flags.contains('i'),
    }))
}

pub(super) fn parse(line: &str, at: &Lines) -> Parsed {
    let s = line.trim_start_matches([' ', ':']).trim_end();
    if s.is_empty() {
        return Parsed::Nothing;
    }
    let (r, rest) = range(s, at);
    let rest = rest.trim_start();
    let lines = r.unwrap_or((at.cur, at.cur));
    if rest.is_empty() {
        return r.map_or(Parsed::Nothing, |(_, b)| Parsed::Goto(b));
    }
    // The command's name runs to the first non-letter.
    let name_len = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
    let (name, args) = rest.split_at(name_len);
    let bang = args.starts_with('!');
    let args = args.trim_start_matches('!').trim();
    let ex = |e: Ex| Parsed::Ex(e);
    match name {
        "s" | "su" | "sub" | "substitute" => substitute(args, lines),
        "w" | "write" if args.is_empty() => ex(Ex::Write),
        "q" | "quit" | "clo" | "close" if args.is_empty() => ex(Ex::Quit),
        "wq" | "x" | "xi" | "xit" | "exi" | "exit" if args.is_empty() => ex(Ex::WriteQuit),
        "wa" | "wall" => ex(Ex::WriteAll),
        "qa" | "qall" | "quita" | "quitall" => ex(Ex::QuitAll),
        "wqa" | "wqall" | "xa" | "xall" => ex(Ex::WriteQuitAll),
        "noh" | "nohl" | "nohlsearch" => ex(Ex::NoHighlight),
        "d" | "de" | "del" | "delete" => Parsed::DeleteLines(lines.0..lines.1 + 1),
        "y" | "ya" | "yank" => Parsed::YankLines(lines.0..lines.1 + 1),
        "w" | "write" | "wq" | "x" | "e" | "edit" | "saveas" => {
            Parsed::Error(format!("Files are named in Quire, not on the command line (:{name}{})", if bang { "!" } else { "" }))
        }
        _ => Parsed::Error(format!("Not an editor command: {s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Lines = Lines { cur: 4, last: 9, visual: Some((2, 3)) };

    #[test]
    fn ranges_and_commands() {
        assert_eq!(parse("12", &AT), Parsed::Goto(9));
        assert_eq!(parse("3", &AT), Parsed::Goto(2));
        assert_eq!(parse("$", &AT), Parsed::Goto(9));
        assert_eq!(parse("wq", &AT), Parsed::Ex(Ex::WriteQuit));
        assert_eq!(parse("q!", &AT), Parsed::Ex(Ex::Quit));
        assert_eq!(parse("2,3d", &AT), Parsed::DeleteLines(1..3));
        let Parsed::Ex(Ex::Substitute(s)) = parse("%s/a\\/b/x&\\1/gi", &AT) else { panic!() };
        assert_eq!((s.lines.clone(), s.pattern.as_str(), s.global, s.ignore_case), (0..10, "a/b", true, true));
        assert_eq!(s.regex_replacement(), "x${0}${1}");
        let Parsed::Ex(Ex::Substitute(s)) = parse("'<,'>s#x##", &AT) else { panic!() };
        assert_eq!((s.lines.clone(), s.replacement.as_str(), s.global), (2..4, "", false));
        assert!(matches!(parse("frobnicate", &AT), Parsed::Error(_)));
        assert_eq!(vim_to_regex(r"\<foo\(x\|y\)\>"), r"\bfoo(x|y)\b");
    }
}
