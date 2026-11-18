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
    fn centres_a_column_and_puts_the_odd_space_on_the_right() {
        // Widths 5 and 3. "Item" has one spare space, "ab" three, "1" two.
        let out = format(&["|Item|Qty|", "|:-:|:-:|", "|Ropes|1|", "|ab|22|"]).unwrap();
        assert_eq!(out, vec!["| Item  | Qty |", "| :---: | :-: |", "| Ropes |  1  |", "|  ab   | 22  |"]);
    }

    #[test]
    fn a_column_is_at_least_three_wide_and_grows_with_its_widest_cell() {
        let out = format(&["|a|abc|abcd|", "|-|:-:|--:|", "|x|y|z|"]).unwrap();
        assert_eq!(out, vec!["| a   | abc | abcd |", "| --- | :-: | ---: |", "| x   |  y  |    z |"]);
        // Wide characters count by their display width (two columns each).
        let out = format(&["|日本|b|", "|-|-|", "|c|d|"]).unwrap();
        assert_eq!(out, vec!["| 日本 | b   |", "| ---- | --- |", "| c    | d   |"]);
    }

    #[test]
    fn a_ragged_row_is_padded_to_every_column() {
        let out = format(&["|a|b|c|", "|-|-|-|", "|x|"]).unwrap();
        assert_eq!(out[2], "| x   |     |     |");
    }

    /// Reformats `text` with the caret at the start, in the middle and at the end of every
    /// cell, and checks the caret is still on the same row, in the same cell, just after the
    /// cell's opening `| `.
    fn caret_stays_in_its_cell(text: &str) {
        let mut line_start = 0;
        for (n, line) in text.split('\n').enumerate() {
            let parts: Vec<&str> = line.split('|').collect();
            let is_table_row = line.starts_with('|');
            for k in 1..parts.len().saturating_sub(1) {
                let content = parts[k];
                if !is_table_row || content.is_empty() {
                    continue;
                }
                let cell_start = line_start + parts[..k].iter().map(|p| p.chars().count()).sum::<usize>() + k;
                let len = content.chars().count();
                for caret in [cell_start, cell_start + len / 2, cell_start + len] {
                    let buf = Buffer::from(text);
                    let tx = format_at(&buf, Selection::cursor(caret)).unwrap_or_else(|| panic!("{text:?} at {caret}: no change"));
                    let mut b = buf;
                    tx.apply(&mut b).unwrap();
                    let head = tx.selection().unwrap().head;
                    let (row, col) = (b.char_to_line(head), head - b.line_range(b.char_to_line(head)).start);
                    let formatted = b.line(row);
                    let ctx = format!("{text:?}: caret {caret} (cell {k} of row {n}) moved to {head} in {:?}", b.to_string());
                    assert_eq!(row, n, "{ctx}");
                    assert_eq!(formatted.chars().skip(col.saturating_sub(2)).take(2).collect::<String>(), "| ", "{ctx}");
                    assert_eq!(formatted.chars().take(col).filter(|&c| c == '|').count(), k, "{ctx}");
                }
            }
            line_start += line.chars().count() + 1;
        }
    }

    #[test]
    fn format_at_keeps_the_caret_in_every_cell_of_every_row() {
        // Surrounded by text, at the very start of the buffer, at the very end, and alone.
        caret_stays_in_its_cell("intro\n|a|bb|\n|-|-|\n|ccc|日本|\n|é|dddd|\nafter");
        caret_stays_in_its_cell("|a|bb|\n|-|-|\n|ccc|日本|\n|é|dddd|\nafter");
        caret_stays_in_its_cell("intro\n\n|a|bb|\n|-|-|\n|ccc|日本|\n|é|dddd|");
        caret_stays_in_its_cell("|a|bb|\n|-|-|\n|ccc|d|");
        caret_stays_in_its_cell("|a|bb|ccc|\n|-|-|-|\n|x|y|z|\n");
        caret_stays_in_its_cell("text\n\n| x | yy |\n|:-:|--:|\n|1|2|\n\nmore |not| a table");
    }

    #[test]
    fn a_caret_left_of_the_first_pipe_goes_to_the_first_cell() {
        let text = "|a|bb|\n|-|-|\n|ccc|d|";
        let buf = Buffer::from(text);
        let tx = format_at(&buf, Selection::cursor(0)).unwrap();
        let mut b = buf;
        tx.apply(&mut b).unwrap();
        assert_eq!(tx.selection().unwrap().head, 2, "after the opening `| `: {:?}", b.to_string());
    }

    #[test]
    fn format_at_changes_nothing_outside_a_table_or_in_a_formatted_one() {
        let prose = Buffer::from("just text\n| not a table\nmore");
        assert!(format_at(&prose, Selection::cursor(2)).is_none(), "a caret outside a table row");
        assert!(format_at(&prose, Selection::cursor(12)).is_none(), "one row is not a table");
        let done = Buffer::from("| a   | bb  |\n| --- | --- |\n| ccc | d   |");
        assert!(format_at(&done, Selection::cursor(3)).is_none());
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
