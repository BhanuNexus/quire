// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Kanban plugin boards.
//!
//! A board is a note with `kanban-plugin: basic` in its front matter, a
//! `## Column` heading per column and `- [ ] card` items under it. The archive (after `***` and `## Archive`) and the plugin's
//! `%% kanban:settings %%` block are kept but not shown.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub text: String,
    pub done: bool,
    /// 0-based lines of the item, continuation lines included.
    pub lines: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub title: String,
    /// 0-based line of the `##` heading.
    pub heading: usize,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Board {
    pub columns: Vec<Column>,
    /// The board has an archive section (not shown).
    pub has_archive: bool,
}

/// Whether a note is a Kanban plugin board.
pub fn is_board(src: &str) -> bool {
    quire_markdown::frontmatter::parse(src).and_then(|fm| fm.get("kanban-plugin").and_then(|v| v.as_str()).map(str::to_string)).is_some()
}

/// The lines of the note, without line endings.
fn lines(src: &str) -> Vec<&str> {
    src.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Where the board's body ends: the archive or settings block.
fn board_end(ls: &[&str]) -> (usize, bool) {
    for (i, l) in ls.iter().enumerate() {
        let t = l.trim();
        if t == "***" && ls[i + 1..].iter().find(|x| !x.trim().is_empty()).is_some_and(|x| x.trim().eq_ignore_ascii_case("## archive")) {
            return (i, true);
        }
        if t.starts_with("%% kanban:settings") {
            return (i, false);
        }
    }
    (ls.len(), false)
}

pub fn parse(src: &str) -> Board {
    let ls = lines(src);
    let start = quire_markdown::frontmatter::parse(src).map_or(0, |fm| src[..fm.range.end].matches('\n').count());
    let (end, has_archive) = board_end(&ls);
    let mut board = Board { columns: Vec::new(), has_archive };
    let mut i = start;
    while i < end {
        let l = ls[i];
        if let Some(title) = l.strip_prefix("## ") {
            board.columns.push(Column { title: title.trim().to_string(), heading: i, cards: Vec::new() });
            i += 1;
            continue;
        }
        let item = l.strip_prefix("- ").or_else(|| l.strip_prefix("* "));
        if let (Some(rest), Some(col)) = (item, board.columns.last_mut()) {
            let (done, text) = match rest.get(..4) {
                Some("[ ] ") => (false, &rest[4..]),
                Some("[x] " | "[X] ") => (true, &rest[4..]),
                _ => (false, rest),
            };
            let mut j = i + 1;
            let mut text = text.trim().to_string();
            while j < end && (ls[j].starts_with([' ', '\t'])) && !ls[j].trim().is_empty() {
                text.push('\n');
                text.push_str(ls[j].trim());
                j += 1;
            }
            col.cards.push(Card { text, done, lines: i..j });
            i = j;
            continue;
        }
        i += 1;
    }
    board
}

/// Move card `card` of column `from` to position `to_index` of column `to`.
/// Only the item's lines move; everything else stays. `None` if the
/// indexes don't exist.
pub fn move_card(src: &str, from: usize, card: usize, to: usize, to_index: usize) -> Option<String> {
    let board = parse(src);
    let moving = board.columns.get(from)?.cards.get(card)?.clone();
    let target = board.columns.get(to)?;
    let ls = lines(src);
    // Where it goes, in lines of the original note.
    let mut others: Vec<&Card> = target.cards.iter().filter(|c| c.lines != moving.lines).collect();
    let to_index = to_index.min(others.len());
    let at = if let Some(c) = others.get(to_index) {
        c.lines.start
    } else if let Some(last) = others.pop() {
        last.lines.end
    } else {
        // An empty column: after its heading and one blank line.
        let h = target.heading + 1;
        if ls.get(h).is_some_and(|l| l.trim().is_empty()) { h + 1 } else { h }
    };
    let block: Vec<&str> = ls[moving.lines.clone()].to_vec();
    let mut out: Vec<&str> = Vec::with_capacity(ls.len());
    for (i, l) in ls.iter().enumerate() {
        if i == at {
            out.extend(&block);
        }
        if !moving.lines.contains(&i) {
            out.push(l);
        }
    }
    if at >= ls.len() {
        out.extend(&block);
    }
    let crlf = src.contains("\r\n");
    Some(out.join(if crlf { "\r\n" } else { "\n" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = "---\nkanban-plugin: basic\n---\n\n## To do\n\n- [ ] Write spec\n  with details\n- [ ] Review\n\n## Doing\n\n- [ ] Build\n\n## Done\n\n\n***\n\n## Archive\n\n- [x] Old\n\n%% kanban:settings\n```\n{\"kanban-plugin\":\"basic\"}\n```\n%%\n";

    #[test]
    fn reads_columns_and_cards() {
        assert!(is_board(BOARD) && !is_board("# x\n"));
        let b = parse(BOARD);
        assert_eq!(
            b.columns.iter().map(|c| (c.title.as_str(), c.cards.len())).collect::<Vec<_>>(),
            [("To do", 2), ("Doing", 1), ("Done", 0)]
        );
        assert_eq!(b.columns[0].cards[0].text, "Write spec\nwith details");
        assert_eq!(b.columns[0].cards[0].lines, 6..8);
        assert!(b.has_archive);
    }

    #[test]
    fn moves_items_in_the_file() {
        let out = move_card(BOARD, 0, 0, 1, 1).unwrap();
        assert!(out.contains("## To do\n\n- [ ] Review\n\n## Doing\n\n- [ ] Build\n- [ ] Write spec\n  with details\n\n## Done"), "{out}");
        let out = move_card(BOARD, 1, 0, 2, 0).unwrap();
        assert!(out.contains("## Doing\n\n\n## Done\n\n- [ ] Build\n\n***"), "{out}");
        let out = move_card(BOARD, 0, 1, 0, 0).unwrap();
        assert!(out.contains("## To do\n\n- [ ] Review\n- [ ] Write spec\n  with details\n\n## Doing"), "{out}");
        assert_eq!(move_card(BOARD, 0, 0, 0, 0).unwrap(), BOARD, "no move");
        assert!(move_card(BOARD, 5, 0, 0, 0).is_none());
        // Every move keeps every line.
        let mut a: Vec<&str> = BOARD.lines().collect();
        let moved = move_card(BOARD, 0, 1, 2, 0).unwrap();
        let mut b: Vec<&str> = moved.lines().collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }
}
