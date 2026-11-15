// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Screen readers (issue #5): the fast editor's visible lines and its
//! selection, in the accessibility tree.
//!
//! The editor draws its own text, so egui sees one widget. Besides the
//! multi-line text input that stands for it, each visible line becomes a
//! `TextRun` child (long lines in pieces of at most 255 characters, the most
//! AccessKit's word starts can index) and the input carries a `TextSelection`
//! from the anchor to the caret, so a screen reader can read the lines around
//! the caret and announce what is selected. Only the lines on screen are
//! described, so the cost follows the window, not the note; and nothing is
//! built unless a screen reader is attached. A selection that reaches past the
//! window ends at the first or last visible line.
//!
//! Text is the note's source text (what the caret's column counts in), not
//! what live preview draws.

use egui::accesskit::{Node, Rect as AkRect, Role, TextDirection, TextPosition, TextSelection};
use egui::{Id, Rect, Sense, Ui, UiBuilder};

/// Most characters in one `TextRun` (AccessKit's word starts are `u8`).
const MAX_RUN_CHARS: usize = 255;

/// A visible line: its number, its text (with its line ending) and where it is drawn.
pub(super) struct Line {
    pub number: usize,
    pub text: String,
    pub rect: Rect,
}

/// One `TextRun`: a line, or a piece of a long one.
#[derive(Debug, PartialEq)]
pub(super) struct Run {
    pub line: usize,
    pub chunk: usize,
    /// The characters of the line before this piece.
    pub start: usize,
    pub value: String,
    pub lengths: Vec<u8>,
    pub word_starts: Vec<u8>,
    pub rect: Rect,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The runs for `lines`: the text with `\r\n` as `\n`, cut into pieces.
pub(super) fn runs(lines: &[Line]) -> Vec<Run> {
    let mut out = Vec::new();
    for l in lines {
        let ends = l.text.ends_with('\n');
        let mut text = l.text.trim_end_matches(['\n', '\r']).to_string();
        if ends {
            text.push('\n');
        }
        let chars: Vec<char> = text.chars().collect();
        let total = chars.len().max(1);
        for (chunk, start) in (0..total).step_by(MAX_RUN_CHARS).enumerate() {
            let piece = &chars[start.min(chars.len())..(start + MAX_RUN_CHARS).min(chars.len())];
            let mut value = String::new();
            let mut lengths = Vec::new();
            let mut word_starts = Vec::new();
            // A piece that continues a word is not a new word at its start.
            let mut in_word =
                start > 0 && chars.get(start - 1).copied().is_some_and(is_word_char) && piece.first().copied().is_some_and(is_word_char);
            for (i, c) in piece.iter().enumerate() {
                let word = is_word_char(*c);
                if word && !in_word {
                    word_starts.push(u8::try_from(i).unwrap_or(u8::MAX));
                }
                in_word = word;
                lengths.push(u8::try_from(c.len_utf8()).unwrap_or(1));
                value.push(*c);
            }
            out.push(Run { line: l.number, chunk, start, value, lengths, word_starts, rect: l.rect });
        }
    }
    out
}

/// Where (line, column in characters) is, as a run and an index in it. A
/// position past the visible lines is the start of the first or the end of the
/// last run; a boundary between two pieces of a line belongs to the earlier.
pub(super) fn locate(runs: &[Run], line: usize, col: usize) -> Option<(usize, usize)> {
    let (first, last) = (runs.first()?, runs.last()?);
    if line < first.line {
        return Some((0, 0));
    }
    if line > last.line {
        return Some((runs.len() - 1, last.lengths.len()));
    }
    let mut found = None;
    for (i, r) in runs.iter().enumerate().filter(|(_, r)| r.line == line) {
        found = Some(i);
        if col <= r.start + r.lengths.len() {
            return Some((i, col.saturating_sub(r.start)));
        }
    }
    // A line that isn't drawn (folded away): the nearest run after it, else the last of the one before.
    found.map(|i| (i, runs[i].lengths.len())).or_else(|| runs.iter().position(|r| r.line > line).map(|i| (i, 0)))
}

fn run_id(editor: Id, r: &Run) -> Id {
    editor.with(("quire-a11y-run", r.line, r.chunk))
}

/// What a `TextRun` node holds.
fn describe(node: &mut Node, r: &Run) {
    node.set_role(Role::TextRun);
    node.set_text_direction(TextDirection::LeftToRight);
    node.set_bounds(AkRect {
        x0: f64::from(r.rect.min.x),
        y0: f64::from(r.rect.min.y),
        x1: f64::from(r.rect.max.x),
        y1: f64::from(r.rect.max.y),
    });
    node.set_value(r.value.clone());
    node.set_character_lengths(r.lengths.clone());
    node.set_word_starts(r.word_starts.clone());
}

fn position(editor: Id, runs: &[Run], line: usize, col: usize) -> Option<TextPosition> {
    let (i, character_index) = locate(runs, line, col)?;
    Some(TextPosition { node: run_id(editor, &runs[i]).accesskit_id(), character_index })
}

/// Describe `lines` and the selection (`anchor`, `head`: line and column in
/// characters) under the editor's own node, `editor`. Does nothing when no
/// screen reader is attached (the editor has no node then).
pub(super) fn publish(ui: &mut Ui, editor: Id, lines: &[Line], anchor: (usize, usize), head: (usize, usize)) {
    let ctx = ui.ctx().clone();
    if lines.is_empty() || !ctx.has_accesskit_node(editor) {
        return;
    }
    let runs = runs(lines);
    // The runs are children of a container whose accessibility parent is the editor.
    let top = lines[0].rect;
    let holder = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(top.min, egui::Vec2::splat(1.0))).accessibility_parent(editor));
    for r in &runs {
        let id = run_id(editor, r);
        // Not a hit target (a hover-only sense is none); it only gives the node its place in the tree.
        holder.interact(r.rect, id, Sense::hover());
        ctx.accesskit_node_builder(id, |node| describe(node, r));
    }
    if let (Some(a), Some(f)) = (position(editor, &runs, anchor.0, anchor.1), position(editor, &runs, head.0, head.1)) {
        ctx.accesskit_node_builder(editor, |node| node.set_text_selection(TextSelection { anchor: a, focus: f }));
    }
}

/// The ids of the run nodes (for tests).
#[cfg(test)]
pub(super) fn run_node_ids(editor: Id, lines: &[Line]) -> Vec<egui::accesskit::NodeId> {
    runs(lines).iter().map(|r| run_id(editor, r).accesskit_id()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::NodeId;
    use egui::{Pos2, Vec2};

    fn line(n: usize, text: &str) -> Line {
        Line { number: n, text: text.to_string(), rect: Rect::from_min_size(Pos2::new(10.0, 20.0 * n as f32), Vec2::new(300.0, 18.0)) }
    }

    #[test]
    fn a_line_becomes_a_run_with_its_characters_and_words() {
        let r = runs(&[line(3, "héllo wörld_1 – ok\r\n")]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].value, "héllo wörld_1 – ok\n", "the line ending is one newline");
        assert_eq!(r[0].lengths.iter().map(|l| usize::from(*l)).sum::<usize>(), r[0].value.len(), "lengths are UTF-8 bytes");
        assert_eq!(r[0].lengths.len(), r[0].value.chars().count());
        assert_eq!(r[0].word_starts, [0, 6, 16], "héllo, wörld_1, ok");
        // A last line with no line ending has no newline; an empty one is one run with nothing in it.
        assert_eq!(runs(&[line(0, "end")])[0].value, "end");
        let e = runs(&[line(1, "")]);
        assert_eq!((e.len(), e[0].lengths.len()), (1, 0));
        assert_eq!(runs(&[line(2, "\n")])[0].value, "\n");
    }

    #[test]
    fn long_lines_are_cut_into_pieces_that_fit_accesskit() {
        let text = format!("{}\n", "ab ".repeat(200));
        let r = runs(&[line(0, &text)]);
        assert_eq!(r.len(), 3, "601 characters in pieces of at most 255");
        assert!(r.iter().all(|p| p.lengths.len() <= MAX_RUN_CHARS));
        assert_eq!(r.iter().map(|p| p.value.as_str()).collect::<String>(), text);
        assert_eq!(r.iter().map(|p| p.start).collect::<Vec<_>>(), [0, 255, 510]);
        assert!(r.iter().all(|p| p.word_starts.iter().all(|w| usize::from(*w) < p.lengths.len())));
        // A word that crosses a cut isn't two words.
        let r = runs(&[line(0, &"x".repeat(300))]);
        assert!(r[1].word_starts.is_empty(), "{:?}", r[1].word_starts);
    }

    #[test]
    fn positions_land_on_the_right_run_and_clamp_to_the_window() {
        let long = format!("{}\n", "y".repeat(300));
        let r = runs(&[line(5, "one\n"), line(6, &long), line(8, "three\n")]);
        assert_eq!(locate(&r, 5, 0), Some((0, 0)));
        assert_eq!(locate(&r, 5, 3), Some((0, 3)), "the end of the line, before its newline");
        assert_eq!(locate(&r, 6, 255), Some((1, 255)), "a boundary belongs to the earlier piece");
        assert_eq!(locate(&r, 6, 256), Some((2, 1)));
        assert_eq!(locate(&r, 8, 2), Some((3, 2)));
        assert_eq!(locate(&r, 2, 4), Some((0, 0)), "above the window: its start");
        assert_eq!(locate(&r, 40, 0), Some((3, 6)), "below the window: its end");
        assert_eq!(locate(&r, 7, 0), Some((3, 0)), "a folded line: the next visible one");
        assert_eq!(locate(&[], 0, 0), None);
    }

    /// With a screen reader attached the editor's node has the visible lines
    /// as `TextRun` descendants and a selection that points into them; with
    /// none, nothing is built.
    #[test]
    fn the_tree_has_the_visible_lines_and_the_selection() {
        fn descendants(nodes: &std::collections::HashMap<NodeId, &Node>, of: NodeId, out: &mut Vec<NodeId>) {
            for c in nodes[&of].children() {
                out.push(*c);
                if nodes.contains_key(c) {
                    descendants(nodes, *c, out);
                }
            }
        }
        let lines = [line(10, "first line\n"), line(11, "second line\n"), line(12, "third")];
        let editor = Id::unique("quire-fast-editor-test");
        let frame = |ctx: &egui::Context| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(300.0, 80.0), Sense::hover());
                let resp = ui.interact(rect, editor, Sense::click_and_drag());
                resp.widget_info(|| egui::WidgetInfo::text_edit(true, "", "", ""));
                ctx.accesskit_node_builder(editor, |n| n.set_role(Role::MultilineTextInput));
                // Select from column 6 of line 10 to column 3 of line 12 (backwards: the caret is at the start).
                publish(ui, editor, &lines, (12, 3), (10, 6));
            });
            out.textures_delta.clear();
            out
        };
        let ctx = egui::Context::default();
        // No screen reader: no update, no work.
        assert!(frame(&ctx).platform_output.accesskit_update.is_none());
        ctx.enable_accesskit();
        let out = frame(&ctx);
        let update = out.platform_output.accesskit_update.expect("a tree update");
        let nodes: std::collections::HashMap<NodeId, &Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
        let editor_node = nodes[&editor.accesskit_id()];
        assert_eq!(editor_node.role(), Role::MultilineTextInput);
        // Every run is a descendant of the editor's node.
        let mut below = Vec::new();
        descendants(&nodes, editor.accesskit_id(), &mut below);
        let ids = run_node_ids(editor, &lines);
        for id in &ids {
            assert!(below.contains(id), "run {id:?} isn't under the editor");
        }
        let values: Vec<&str> = ids.iter().map(|id| nodes[id].value().unwrap_or_default()).collect();
        assert_eq!(values, ["first line\n", "second line\n", "third"]);
        assert!(ids.iter().all(|id| nodes[id].role() == Role::TextRun));
        let sel = editor_node.text_selection().expect("the selection");
        assert_eq!(sel.anchor, TextPosition { node: ids[2], character_index: 3 });
        assert_eq!(sel.focus, TextPosition { node: ids[0], character_index: 6 });
    }
}
