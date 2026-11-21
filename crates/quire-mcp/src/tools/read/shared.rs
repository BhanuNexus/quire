// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Helpers several read tools share: the stale-version reply, line numbering,
//! the view window, and the caller's own pending suggestion.

use std::fmt::Write as _;

use super::{
    ContentHash, Cursor, NotePath, ProposalFilter, ProposalStatus, Start, ToolCx, ToolOutput, Value, bounds, json, line_count, opt,
    section_lines,
};

/// The text of the caller's own pending proposal for `path`, if any.
pub fn own_pending(cx: &ToolCx<'_>, path: &NotePath) -> Option<(quire_agent::ProposalId, String)> {
    let filter = ProposalFilter { status: Some(ProposalStatus::Pending), actor: Some(cx.actor.id.clone()), path: Some(path.clone()) };
    let mut list = cx.agent.proposals(&filter);
    list.sort_by_key(|p| std::cmp::Reverse(p.updated));
    list.into_iter().find_map(|sum| {
        let p = cx.agent.proposal(&sum.id)?;
        let ch = p.changes.iter().find(|c| c.path.as_ref() == Some(path))?;
        let text = ch.new_text.clone().or_else(|| ch.text.clone())?;
        Some((sum.id, text))
    })
}

/// The line that ends a cut list: how many rows follow and how to get them.
pub(super) fn more_line(text: &mut String, tool: &str, page: &bounds::Page) {
    if let Some(c) = &page.next_cursor {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        write!(text, "… {} more; call {tool} again with cursor=\"{c}\"", page.more()).expect("a String takes any text");
    }
}

pub(crate) fn stale(version: &str) -> ToolOutput {
    ToolOutput::Err {
        code: "cursor_stale",
        message: format!("The cursor is for an older version (now {version}). Start again without a cursor."),
        structured: json!({"error": "cursor_stale", "version": version}),
    }
}

/// A line range as an agent sends it: `[10, 40]` or `"10-40"` (the resource template's form, which
/// it also takes as `"10,40"`).
pub(super) fn line_range(v: &Value) -> Option<(usize, usize)> {
    let n = |x: &Value| x.as_u64().and_then(|x| usize::try_from(x).ok());
    match v {
        Value::Array(r) if r.len() == 2 => Some((n(&r[0])?, n(&r[1])?)),
        Value::String(t) => {
            let (a, b) = t.trim().trim_matches(['[', ']']).split_once(['-', ','])?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        }
        _ => None,
    }
}

/// Lines `first..=last` of `text` numbered, cut to fit `budget` tokens.
/// Returns the rendered text and the last line shown.
pub(super) fn numbered(text: &str, first: usize, last: usize, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    let mut shown = first.saturating_sub(1);
    for (i, line) in text.lines().enumerate().skip(first.saturating_sub(1)).take(last + 1 - first.min(last + 1)) {
        let row = format!("{:>4}│ {line}\n", i + 1);
        let t = bounds::est_tokens(&row);
        if shown >= first && used + t > budget {
            break;
        }
        used += t;
        out.push_str(&row);
        shown = i + 1;
    }
    (out, shown)
}

/// Pick lines of `text` by section, range or cursor, bounded by tokens.
pub(super) struct Window {
    pub(super) first: usize,
    pub(super) last: usize,
    pub(super) shown_to: usize,
    pub(super) total: usize,
    pub(super) body: String,
    pub(super) next: Option<String>,
}

pub(super) fn window(text: &str, version: &ContentHash, a: &Value) -> Result<Window, ToolOutput> {
    let total = line_count(text);
    let (mut first, last) = if let Some(sec) = opt(a, "section") {
        section_lines(text, &sec).map_err(|e| ToolOutput::error(e.code(), e.message("the note")))?
    } else if let Some(range) = a.get("lines").filter(|v| !v.is_null()) {
        let Some((s, e)) = line_range(range) else {
            return Err(ToolOutput::error("invalid_args", "`lines` is [first, last] or \"first-last\", e.g. [10, 40] or \"10-40\"."));
        };
        let s = s.max(1);
        if s > total.max(1) || e < s {
            return Err(ToolOutput::error("invalid_args", format!("lines [{s}, {e}] are outside the note (1–{total}).")));
        }
        (s, e.min(total))
    } else {
        (1, total)
    };
    match bounds::start(a, version.as_str()) {
        Start::Beginning => {}
        Start::At(n) if n >= first && n <= last.max(first) => first = n,
        Start::At(_) => return Err(ToolOutput::error("invalid_args", "The cursor is outside the requested lines.")),
        Start::Stale => return Err(stale(version.as_str())),
    }
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let (body, shown_to) = if total == 0 { (String::new(), 0) } else { numbered(text, first, last, budget) };
    let next = (shown_to < last).then(|| Cursor::at(shown_to + 1, version.to_string()).encode());
    Ok(Window { first, last, shown_to, total, body, next })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering_respects_the_budget() {
        let text = "a\nb\nc\nd\n";
        let (out, last) = numbered(text, 2, 4, 1000);
        assert_eq!(out, "   2│ b\n   3│ c\n   4│ d\n");
        assert_eq!(last, 4);
        let (out, last) = numbered(text, 1, 4, 8);
        assert_eq!(last, 2, "{out}");
        let (_, last) = numbered(&"x".repeat(100), 1, 1, 1);
        assert_eq!(last, 1, "one line always fits");
    }
}
