// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pipe-table auto-formatting (Typora/Obsidian "format table").

use quire_text::{Buffer, Selection, Transaction};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") { &t[..t.len() - 1] } else { t };
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push_str("\\|");
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn parse_align(cell: &str) -> Option<Align> {
    let c = cell.trim();
    let left = c.starts_with(':');
    let right = c.ends_with(':');
    let dashes = c.trim_matches(':');
    if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (true, false) => Align::Left,
        (false, true) => Align::Right,
        _ => Align::None,
    })
}

fn pad(s: &str, width: usize, align: Align) -> String {
    let w = UnicodeWidthStr::width(s);
    let gap = width.saturating_sub(w);
    match align {
        Align::Right => format!("{}{s}", " ".repeat(gap)),
        Align::Center => format!("{}{s}{}", " ".repeat(gap / 2), " ".repeat(gap - gap / 2)),
        _ => format!("{s}{}", " ".repeat(gap)),
    }
}

/// Reformat table lines. Returns `None` if they are not a GFM table.
pub fn format(lines: &[&str]) -> Option<Vec<String>> {
    if lines.len() < 2 {
        return None;
    }
    let rows: Vec<Vec<String>> = lines.iter().map(|l| split_row(l)).collect();
    let aligns: Vec<Align> = rows[1].iter().map(|c| parse_align(c)).collect::<Option<_>>()?;
    let cols = rows.iter().map(Vec::len).max()?.max(aligns.len());
    let mut widths = vec![3usize; cols];
    for (i, row) in rows.iter().enumerate() {
        if i == 1 {
            continue;
        }
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    let align_of = |c: usize| aligns.get(c).copied().unwrap_or(Align::None);
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..cols)
            .map(|c| {
                if i == 1 {
                    let w = widths[c];
                    match align_of(c) {
                        Align::None => "-".repeat(w),
                        Align::Left => format!(":{}", "-".repeat(w - 1)),
                        Align::Right => format!("{}:", "-".repeat(w - 1)),
                        Align::Center => format!(":{}:", "-".repeat(w - 2)),
                    }
                } else {
                    pad(row.get(c).map_or("", String::as_str), widths[c], align_of(c))
                }
            })
            .collect();
        out.push(format!("| {} |", cells.join(" | ")));
    }
    Some(out)
}

/// Format the table under the caret. The caret stays in the same cell.
pub fn format_at(buf: &Buffer, sel: Selection) -> Option<Transaction> {
    let line = buf.char_to_line(sel.head);
    let is_row = |l: usize| buf.line(l).trim_start().starts_with('|');
    if !is_row(line) {
        return None;
    }
    let mut first = line;
    while first > 0 && is_row(first - 1) {
        first -= 1;
    }
    let mut last = line;
    while last + 1 < buf.len_lines() && is_row(last + 1) {
        last += 1;
    }
    let originals: Vec<String> = (first..=last).map(|l| buf.line(l)).collect();
    let refs: Vec<&str> = originals.iter().map(String::as_str).collect();
    let formatted = format(&refs)?;

    let start = buf.line_range(first).start;
    let end = buf.line_range(last).end;
    let joined = formatted.join("\n");

    // Which cell was the caret in?
    let col = sel.head - buf.line_range(line).start;
    let cell_idx = originals[line - first].chars().take(col).filter(|&c| c == '|').count().max(1);
    let row_off: usize = formatted[..line - first].iter().map(|r| r.chars().count() + 1).sum();
    let row = &formatted[line - first];
    let pipe_pos = row.char_indices().filter(|(_, c)| *c == '|').nth(cell_idx - 1).map(|(i, _)| row[..i].chars().count());
    let caret = start + row_off + pipe_pos.map_or(0, |p| (p + 2).min(row.chars().count()));

    (joined != refs.join("\n")).then(|| Transaction::replace(start, end, joined).with_selection(Selection::cursor(caret)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_columns() {
        let out = format(&["|Name|Qty|", "|:--|--:|", "|Ropes|12|", "|B-trees|3|"]).unwrap();
        assert_eq!(out, vec!["| Name    | Qty |", "| :------ | --: |", "| Ropes   |  12 |", "| B-trees |   3 |"]);
    }

    #[test]
    fn rejects_non_tables() {
        assert!(format(&["| a | b |", "| not | delim |"]).is_none());
    }

    #[test]
    fn format_at_keeps_cell() {
        let text = "intro\n|a|bb|\n|-|-|\n|ccc|d|\nafter";
        let buf = Buffer::from(text);
        let caret = text.find("bb").unwrap();
        let tx = format_at(&buf, Selection::cursor(caret)).unwrap();
        let mut b = buf;
        tx.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "intro\n| a   | bb  |\n| --- | --- |\n| ccc | d   |\nafter");
        let sel = tx.selection().unwrap();
        assert_eq!(b.slice(sel.head..sel.head + 2), "bb");
    }
}
