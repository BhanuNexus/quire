// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Markdown linter and formatter (0.12): tidy a note the way Obsidian
//! Linter does, with every rule a switch in `vault.toml` `[lint.format]`.
//!
//! [`lint`] takes the note's text and returns the tidied text; it never
//! reads or writes a file. What it promises:
//!
//! * **Code, math, HTML and front matter it can't read are never touched.**
//!   They are found with the same parser as the rest of Quire.
//! * **Only whitespace and markup change.** After every rule the note is
//!   compared with how it was (the parser's events, text without its
//!   whitespace, code exactly). A rule that changed anything else is undone
//!   and named in [`Outcome::reverted`]; the other rules still run.
//! * **Running it twice gives the same as running it once.**
//! * **Line endings stay as the file has them:** CRLF stays CRLF, lines
//!   added use the file's majority ending.
//!
//! See `docs/formats/lint-format.md` for the settings and
//! `docs/design/0.12-switch.md` for the decisions.

mod analysis;
mod front;
mod options;
mod rules;
#[cfg(test)]
mod tests;

pub use options::{
    Emphasis, HeadingStyle, LineEndings, ListIndent, ListMarker, Options, RULES, Rule, Sort, TagsCase, TagsFormat, TwoSpace,
};

use analysis::{analyze, signature, split_lines};

/// Facts about this run that aren't in the text.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cx<'a> {
    /// The value for the "date modified" property, already formatted
    /// (`Options::date_format` is the caller's to apply: this crate has no clock).
    pub stamp: &'a str,
    /// The note was edited by hand (saving it): "date modified" is set even
    /// if no other rule changed anything.
    pub edited: bool,
}

/// What one rule did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub rule: &'static str,
    /// Lines that differ after the rule ran (added, removed or changed).
    pub lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    pub text: String,
    pub fixes: Vec<Fix>,
    /// Rules whose change was undone because it would have changed more than
    /// whitespace and markup. Always empty unless there is a bug to report.
    pub reverted: Vec<&'static str>,
    /// Why part of the note was left alone ("front matter can't be read").
    pub notes: Vec<String>,
}

impl Outcome {
    pub fn changed(&self, original: &str) -> bool {
        self.text != original
    }
}

/// Lines that differ between `a` and `b`, ignoring the common start and end.
fn changed_lines(a: &str, b: &str) -> usize {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.split_inclusive('\n').collect(), b.split_inclusive('\n').collect());
    let pre = la.iter().zip(&lb).take_while(|(x, y)| x == y).count();
    let suf = la[pre..].iter().rev().zip(lb[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    (la.len() - pre - suf).max(lb.len() - pre - suf)
}

/// Run one rule on `cur`: keep its result if the note still means the same.
fn step(cur: &mut String, name: &'static str, structure_levels: bool, f: &dyn Fn(&str) -> Option<String>, out: &mut Outcome) {
    let Some(new) = f(cur) else { return };
    if new == *cur {
        return;
    }
    if signature(cur, structure_levels) != signature(&new, structure_levels) {
        out.reverted.push(name);
        return;
    }
    out.fixes.push(Fix { rule: name, lines: changed_lines(cur, &new) });
    *cur = new;
}

/// Tidy `text` with the rules `opts` turns on.
pub fn lint(text: &str, opts: &Options, cx: &Cx<'_>) -> Outcome {
    let mut out = Outcome { text: text.to_string(), ..Outcome::default() };
    // Old Mac line endings (a lone CR) aren't lines to us; leave such a file alone.
    if has_lone_cr(text) {
        out.notes.push("The file has old Mac line endings (a lone CR); it was left alone".into());
        return out;
    }
    let eol = rules::dominant_eol(text);
    let mut cur = text.to_string();

    // Only what the parser reads as front matter is treated as such.
    let fm_read = analyze(&cur).front.is_some();
    let fence_first = cur.lines().next().is_some_and(|l| l.trim_end() == "---");
    if fm_read
        && (opts.front_matter
            || opts.tags_format != TagsFormat::Keep
            || opts.tags_case != TagsCase::Keep
            || !opts.front_matter_order.is_empty()
            || opts.front_matter_sort != Sort::Keep)
    {
        tidy_front(&mut cur, opts, &mut out);
    }
    // Headings change level on purpose: the check ignores levels for them.
    if opts.heading_increment || opts.heading_start > 0 || opts.heading_style == HeadingStyle::Atx {
        step(&mut cur, "headings", false, &|t| Some(rules::headings(t, &analyze(t), opts)), &mut out);
    }
    if opts.list_marker != ListMarker::Keep {
        step(&mut cur, "list-marker", true, &|t| Some(rules::list_marker(t, &analyze(t), opts)), &mut out);
    }
    if opts.list_indent != ListIndent::Keep {
        step(&mut cur, "list-indent", true, &|t| rules::list_indent(t, &analyze(t), opts), &mut out);
    }
    if opts.emphasis != Emphasis::Keep || opts.strong != Emphasis::Keep {
        step(&mut cur, "emphasis", true, &|t| Some(rules::emphasis(t, &analyze(t), opts)), &mut out);
    }
    if opts.link_spacing {
        step(&mut cur, "link-spacing", true, &|t| Some(rules::link_spacing(t, &analyze(t))), &mut out);
    }
    if opts.trailing_spaces {
        step(&mut cur, "trailing-spaces", true, &|t| Some(rules::trailing_spaces(t, &analyze(t), opts)), &mut out);
    }
    if opts.blank_lines {
        step(&mut cur, "blank-lines", true, &|t| Some(rules::blank_lines(t, &analyze(t), opts, eol)), &mut out);
    }
    if opts.final_newline {
        step(&mut cur, "final-newline", true, &|t| Some(rules::final_newline(t, &analyze(t), eol)), &mut out);
    }
    if opts.date_modified && !cx.stamp.is_empty() && fence_first && !fm_read {
        out.notes.push("The front matter isn't read as front matter; \"date modified\" wasn't set".into());
    } else if opts.date_modified && !cx.stamp.is_empty() && (cx.edited || cur != text) {
        let before = cur.clone();
        match front::set_modified(&cur, &opts.date_modified_key, cx.stamp, eol) {
            front::Result::Same => {}
            front::Result::Changed(new) => {
                if signature(&before, true) == signature(&new, true) && analyze(&new).front.is_some() {
                    out.fixes.push(Fix { rule: "date-modified", lines: changed_lines(&before, &new) });
                    cur = new;
                    // The new key takes its place in the order.
                    if (!opts.front_matter_order.is_empty() || opts.front_matter_sort != Sort::Keep)
                        && let front::Result::Changed(sorted) = front::tidy(&cur, opts)
                        && front::same_meaning(&cur, &sorted)
                        && analyze(&sorted).front.is_some()
                    {
                        cur = sorted;
                    }
                } else {
                    out.reverted.push("date-modified");
                }
            }
            front::Result::Unreadable => out.notes.push("The front matter can't be read; \"date modified\" wasn't set".into()),
        }
    }
    if opts.line_endings != LineEndings::Keep {
        step(&mut cur, "line-endings", true, &|t| Some(rules::line_endings(t, opts.line_endings)), &mut out);
    }
    out.text = cur;
    out
}

/// Front matter: spacing, tags and order, checked to mean the same.
fn tidy_front(cur: &mut String, opts: &Options, out: &mut Outcome) {
    match front::tidy(cur, opts) {
        front::Result::Same => {}
        front::Result::Changed(new) => {
            if front::same_meaning(cur, &new) && analyze(&new).front.is_some() {
                out.fixes.push(Fix { rule: "front-matter", lines: changed_lines(cur, &new) });
                *cur = new;
            } else {
                out.reverted.push("front-matter");
            }
        }
        front::Result::Unreadable => out.notes.push("The front matter can't be read; it was left alone".into()),
    }
}

fn has_lone_cr(text: &str) -> bool {
    let b = text.as_bytes();
    b.iter().enumerate().any(|(i, c)| *c == b'\r' && b.get(i + 1) != Some(&b'\n'))
}

/// How many lines `text` has (for tests and reports).
pub fn line_count(text: &str) -> usize {
    split_lines(text).len()
}
