// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The rules that change the body of a note. Each takes the text and its
//! [`Analysis`] and returns new text; none touches a protected line.

use std::fmt::Write as _;

use super::analysis::{Analysis, Kind, signature};
use super::options::{Emphasis, HeadingStyle, LineEndings, ListIndent, ListMarker, Options, TwoSpace};

/// The ending most lines have (`"\r\n"` when most do).
pub(super) fn dominant_eol(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf { "\r\n" } else { "\n" }
}

fn line_text<'t>(text: &'t str, a: &Analysis, i: usize) -> &'t str {
    &text[a.lines[i].start..a.lines[i].end]
}

fn eol_of<'t>(text: &'t str, a: &Analysis, i: usize) -> &'t str {
    &text[a.lines[i].end..a.lines[i].next]
}

/// Rebuild the text with some lines replaced (their endings stay).
fn replace_lines(text: &str, a: &Analysis, mut new: impl FnMut(usize, &str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    for i in 0..a.lines.len() {
        let t = line_text(text, a, i);
        match new(i, t) {
            Some(n) => out.push_str(&n),
            None => out.push_str(t),
        }
        out.push_str(eol_of(text, a, i));
    }
    out
}

fn is_blank(t: &str) -> bool {
    t.trim().is_empty()
}

// ------------------------------------------------------------ trailing spaces

pub(super) fn trailing_spaces(text: &str, a: &Analysis, o: &Options) -> String {
    // Spaces after a fence (`---  `) decide whether front matter is front matter: leave them.
    let fenced = a.lines.first().is_some_and(|_| line_text(text, a, 0).trim_end() == "---");
    replace_lines(text, a, |i, t| {
        if a.protected[i] {
            return None;
        }
        let trimmed = t.trim_end_matches([' ', '\t']);
        if trimmed.len() == t.len() || (fenced && matches!(trimmed, "---" | "...")) {
            return None;
        }
        if o.two_space_linebreak == TwoSpace::Keep && a.hard_breaks.contains(&i) && !trimmed.is_empty() {
            return Some(format!("{trimmed}  "));
        }
        Some(trimmed.to_string())
    })
}

// -------------------------------------------------------------------- headings

pub(super) fn headings(text: &str, a: &Analysis, o: &Options) -> String {
    // New level of each top-level heading, keeping the shape of the outline:
    // a heading is one deeper than its parent at most, and siblings stay siblings.
    let tops: Vec<usize> = (0..a.headings.len()).filter(|&i| a.headings[i].top && !a.protected[a.headings[i].first]).collect();
    let mut new_level = vec![0u8; a.headings.len()];
    let mut stack: Vec<(u8, u8)> = Vec::new();
    for &i in &tops {
        let orig = a.headings[i].level;
        while stack.last().is_some_and(|&(ol, _)| ol >= orig) {
            stack.pop();
        }
        let mut lvl = orig;
        match stack.last() {
            Some(&(parent_orig, parent)) => {
                // The subtree moves with its parent, then may be no deeper than one below it.
                lvl = (i16::from(orig) + i16::from(parent) - i16::from(parent_orig)).clamp(1, 6) as u8;
                if o.heading_increment {
                    lvl = lvl.min(parent + 1);
                }
            }
            None if o.heading_start > 0 => lvl = lvl.min(o.heading_start),
            None => {}
        }
        // A setext heading kept as it is can only be level 1 or 2.
        if a.headings[i].setext && (o.heading_style == HeadingStyle::Keep || a.headings[i].last - a.headings[i].first > 1) {
            lvl = orig;
        }
        new_level[i] = lvl;
        stack.push((orig, lvl));
    }
    let mut at_line: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &i in &tops {
        at_line.insert(a.headings[i].first, i);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < a.lines.len() {
        let Some(&h) = at_line.get(&i) else {
            out.push_str(line_text(text, a, i));
            out.push_str(eol_of(text, a, i));
            i += 1;
            continue;
        };
        let head = a.headings[h];
        let lvl = new_level[h];
        let first = line_text(text, a, head.first);
        if head.setext {
            // A title over several lines can't be one ATX line: it stays as it is.
            if o.heading_style == HeadingStyle::Atx && head.last - head.first == 1 {
                // `Title` over `====` becomes one ATX line.
                let words: Vec<&str> = (head.first..head.last).map(|l| line_text(text, a, l).trim()).collect();
                let _ = write!(out, "{} {}", "#".repeat(lvl as usize), words.join(" "));
                out.push_str(eol_of(text, a, head.last));
            } else {
                for l in head.first..=head.last {
                    out.push_str(line_text(text, a, l));
                    out.push_str(eol_of(text, a, l));
                }
            }
        } else {
            out.push_str(&atx_line(first, lvl, o.heading_style == HeadingStyle::Atx));
            out.push_str(eol_of(text, a, head.first));
        }
        i = head.last + 1;
    }
    out
}

/// `  ##   Title ##` at level `lvl`: its indent kept, one space, no closing hashes.
fn atx_line(line: &str, lvl: u8, tidy: bool) -> String {
    let indent_len = line.len() - line.trim_start_matches(' ').len();
    let (indent, rest) = line.split_at(indent_len);
    let hashes = rest.bytes().take_while(|b| *b == b'#').count();
    let body = &rest[hashes..];
    let mut body = body.trim();
    if tidy {
        // A closing run of hashes needs a space before it (`# C#` is a heading about C#).
        let t = body.trim_end_matches('#');
        if t.len() < body.len() && (t.is_empty() || t.ends_with([' ', '\t'])) {
            body = t.trim_end();
        }
    }
    if body.is_empty() { format!("{indent}{}", "#".repeat(lvl as usize)) } else { format!("{indent}{} {body}", "#".repeat(lvl as usize)) }
}

// ------------------------------------------------------------------- lists

pub(super) fn list_marker(text: &str, a: &Analysis, o: &Options) -> String {
    let target = match o.list_marker {
        ListMarker::Dash => '-',
        ListMarker::Star => '*',
        ListMarker::Plus => '+',
        ListMarker::Keep => return text.to_string(),
    };
    let mut edits: Vec<usize> = Vec::new();
    for it in &a.items {
        if it.ordered || it.list_adjacent || !it.plain || a.protected[it.line] {
            continue;
        }
        let c = text[it.marker..].chars().next();
        if matches!(c, Some('-' | '*' | '+')) && c != Some(target) {
            edits.push(it.marker);
        }
    }
    let mut out = text.to_string();
    let mut buf = [0u8; 4];
    for pos in edits {
        out.replace_range(pos..=pos, target.encode_utf8(&mut buf));
    }
    out
}

/// Columns taken by the leading whitespace of `s` (tabs go to the next multiple of 4).
fn columns(ws: &str) -> usize {
    let mut c = 0;
    for ch in ws.chars() {
        c = if ch == '\t' { (c / 4 + 1) * 4 } else { c + 1 };
    }
    c
}

/// Width of an item's marker and the spaces after it (where its text starts).
fn marker_width(text: &str, marker: usize) -> usize {
    let rest = &text[marker..];
    let m = if rest.starts_with(|c: char| c.is_ascii_digit()) { rest.bytes().take_while(u8::is_ascii_digit).count() + 1 } else { 1 };
    let spaces = rest[m.min(rest.len())..].bytes().take_while(|b| *b == b' ').count();
    m + if (1..=4).contains(&spaces) { spaces } else { 1 }
}

pub(super) fn list_indent(text: &str, a: &Analysis, o: &Options) -> Option<String> {
    let step = match o.list_indent {
        ListIndent::Two => 2,
        ListIndent::Four | ListIndent::Tab => 4,
        ListIndent::Keep => return None,
    };
    let tab = o.list_indent == ListIndent::Tab;
    // Each item's new indentation (a string) and the columns it takes.
    let mut new_ws: Vec<Option<String>> = vec![None; a.items.len()];
    let mut old_col: Vec<usize> = vec![0; a.items.len()];
    let mut owner: Vec<Option<usize>> = vec![None; a.lines.len()];
    for (idx, it) in a.items.iter().enumerate() {
        if !it.plain {
            continue;
        }
        let start = a.lines[it.line].start;
        old_col[idx] = columns(&text[start..it.marker]);
        for slot in &mut owner[it.line..=it.last.min(a.lines.len() - 1)] {
            *slot = Some(idx);
        }
        let ws = match it.parent {
            None => String::new(),
            Some(p) => {
                let parent = new_ws[p].as_ref()?;
                let pw = marker_width(text, a.items[p].marker);
                if tab {
                    if pw > 4 {
                        return None;
                    }
                    format!("{parent}\t")
                } else {
                    format!("{parent}{}", " ".repeat(step.max(pw)))
                }
            }
        };
        new_ws[idx] = Some(ws);
    }
    // A line indented after an item but not in it would be pulled into it when the
    // item's text moves left. Leave such lists as they are.
    for it in a.items.iter().filter(|it| it.plain) {
        let next = (it.last + 1..a.lines.len()).find(|&j| !is_blank(line_text(text, a, j)));
        if let Some(j) = next
            && owner[j].is_none()
            && line_text(text, a, j).starts_with([' ', '\t'])
        {
            return None;
        }
    }
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for (i, own) in owner.iter().enumerate() {
        let t = line_text(text, a, i);
        let Some(idx) = *own else {
            out.push_str(t);
            out.push_str(eol_of(text, a, i));
            continue;
        };
        let it = a.items[idx];
        let new_indent = new_ws[idx].as_deref().unwrap_or("");
        let lead_len = t.len() - t.trim_start_matches([' ', '\t']).len();
        let (lead, body) = t.split_at(lead_len);
        let line = if body.is_empty() {
            t.to_string()
        } else if i == it.line {
            format!("{new_indent}{body}")
        } else {
            let cols = columns(lead);
            if cols < old_col[idx] + marker_width(text, it.marker) || (a.protected[i] && lead.contains('\t')) {
                // Less indented than the item's text (a lazy line), or code with tabs to keep: untouched,
                // unless the item's text now starts left of it and would take the line in.
                if cols >= columns(new_indent) + marker_width(text, it.marker) && cols < old_col[idx] + marker_width(text, it.marker) {
                    return None;
                }
                t.to_string()
            } else {
                format!("{new_indent}{}{body}", " ".repeat(cols - old_col[idx]))
            }
        };
        changed |= line != t;
        out.push_str(&line);
        out.push_str(eol_of(text, a, i));
    }
    // Moving a list left can change what a later line belongs to in ways that are
    // hard to see from columns alone: if the notes' shape changes, decline.
    (changed && signature(text, true) == signature(&out, true)).then_some(out)
}

// ------------------------------------------------------------------ emphasis

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphanumeric)
}

pub(super) fn emphasis(text: &str, a: &Analysis, o: &Options) -> String {
    let mut edits: Vec<(usize, usize, char)> = Vec::new();
    for e in &a.emphasis {
        let want = if e.strong { o.strong } else { o.emphasis };
        let target = match want {
            Emphasis::Asterisk => '*',
            Emphasis::Underscore => '_',
            Emphasis::Keep => continue,
        };
        let n = if e.strong { 2 } else { 1 };
        let (s, end) = (e.range.start, e.range.end);
        if end < s + 2 * n + 1 || !text.is_char_boundary(s) || !text.is_char_boundary(end) {
            continue;
        }
        let open = &text[s..s + n];
        let close = &text[end - n..end];
        let c = open.chars().next().unwrap_or(' ');
        if !matches!(c, '*' | '_') || c == target || open.chars().any(|x| x != c) || close.chars().any(|x| x != c) {
            continue;
        }
        let before = text[..s].chars().next_back();
        let after = text[end..].chars().next();
        // `***x***` and nested runs: leave to the parser.
        if before == Some(c) || after == Some(c) || text[s + n..].starts_with(c) || text[..end - n].ends_with(c) {
            continue;
        }
        // An underscore inside a word is not emphasis.
        if target == '_' && (is_word(before) || is_word(after)) {
            continue;
        }
        if a.in_inline_protected(s) || a.in_wikilink(s) || a.in_wikilink(end - 1) {
            continue;
        }
        edits.push((s, n, target));
        edits.push((end - n, n, target));
    }
    edits.sort_unstable();
    let mut out = text.to_string();
    for (pos, n, t) in edits.into_iter().rev() {
        out.replace_range(pos..pos + n, &t.to_string().repeat(n));
    }
    out
}

// -------------------------------------------------------------- link spacing

/// `Note # Heading | alias` → `Note#Heading|alias`.
fn tidy_link(inner: &str) -> String {
    let (target, alias) = match inner.split_once('|') {
        Some((t, al)) => (t, Some(al.trim())),
        None => (inner, None),
    };
    let target = match target.split_once('#') {
        Some((n, h)) => format!("{}#{}", n.trim(), h.trim()),
        None => target.trim().to_string(),
    };
    alias.map_or_else(|| target.clone(), |al| format!("{target}|{al}"))
}

pub(super) fn link_spacing(text: &str, a: &Analysis) -> String {
    replace_lines(text, a, |i, t| {
        if a.protected[i] || !t.contains("[[") {
            return None;
        }
        let base = a.lines[i].start;
        let mut out = String::with_capacity(t.len());
        let mut pos = 0;
        let mut changed = false;
        while let Some(rel) = t[pos..].find("[[") {
            let open = pos + rel;
            let Some(close_rel) = t[open + 2..].find("]]") else { break };
            let close = open + 2 + close_rel;
            let inner = &t[open + 2..close];
            out.push_str(&t[pos..open + 2]);
            if inner.contains("[[") || inner.trim().is_empty() || a.in_inline_protected(base + open) {
                out.push_str(inner);
            } else {
                let tidy = tidy_link(inner);
                changed |= tidy != inner;
                out.push_str(&tidy);
            }
            out.push_str("]]");
            pos = close + 2;
        }
        out.push_str(&t[pos..]);
        changed.then_some(out)
    })
}

// -------------------------------------------------------------- blank lines

pub(super) fn blank_lines(text: &str, a: &Analysis, o: &Options, eol: &str) -> String {
    let n = a.lines.len();
    // A blank line wanted before line `i`.
    let mut before = vec![false; n];
    for b in &a.blocks {
        if !matches!(b.kind, Kind::Heading | Kind::List | Kind::Code) {
            continue;
        }
        let after_front = a.front.is_some_and(|(_, l)| b.first == l + 1);
        if b.first > 0 && !after_front {
            before[b.first] = true;
        }
        if b.last + 1 < n {
            before[b.last + 1] = true;
        }
    }
    let max = o.max_blank_lines.max(1) as usize;
    let mut out = String::with_capacity(text.len() + 16);
    let mut run = 0usize;
    // Blank lines at the very start of a note without front matter go.
    let mut seen_content = false;
    let mut last_blank = false;
    for (i, &wanted) in before.iter().enumerate() {
        let t = line_text(text, a, i);
        let blank = !a.protected[i] && is_blank(t);
        if blank {
            run += 1;
            if !seen_content && a.front.is_none() || run > max {
                continue;
            }
            out.push_str(t);
            out.push_str(eol_of(text, a, i));
            last_blank = true;
            continue;
        }
        run = 0;
        // A wanted blank line, unless the line before is blank or this is the first line.
        if wanted && seen_content && !last_blank {
            out.push_str(eol);
        }
        seen_content = true;
        last_blank = false;
        out.push_str(t);
        out.push_str(eol_of(text, a, i));
    }
    out
}

pub(super) fn final_newline(text: &str, a: &Analysis, eol: &str) -> String {
    if text.is_empty() || a.protected.last().copied().unwrap_or(false) {
        return text.to_string();
    }
    // Drop blank lines at the end; one line ending closes the last line.
    let mut keep = a.lines.len();
    while keep > 0 && !a.protected[keep - 1] && is_blank(line_text(text, a, keep - 1)) {
        keep -= 1;
    }
    if keep == 0 {
        return text.to_string();
    }
    let mut out = text[..a.lines[keep - 1].end].to_string();
    // The last kept line's own ending if it has one, else the file's.
    let own = eol_of(text, a, keep - 1);
    out.push_str(if own.is_empty() { eol } else { own });
    out
}

pub(super) fn line_endings(text: &str, to: LineEndings) -> String {
    let lf = text.replace("\r\n", "\n");
    match to {
        LineEndings::Crlf => lf.replace('\n', "\r\n"),
        _ => lf,
    }
}
