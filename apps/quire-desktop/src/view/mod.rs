// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fast editor: a rope-backed, virtualized, multi-cursor text view.
//!
//! Only lines inside the viewport are laid out and painted. Line heights live
//! in a [`HeightIndex`]; the document's line-delta log keeps it in sync with
//! edits without re-measuring. Every edit is a `Transaction` built by the
//! core (`quire_text::multi`, `quire_text::motion`, workspace smart edits),
//! so undo, autosave and indexing are unchanged.

mod heights;

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{CCursor, CharIndex};
use egui::{
    Align, Color32, CursorIcon, Event, EventFilter, Galley, Id, ImeEvent, Key, Pos2, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2,
};
use quire_app::document::Document;
use quire_app::index::views::{self as qviews, ViewBlock};
use quire_app::markdown::{StyleRun, blocks};
use quire_app::suggest::{SuggestKind, Suggestions};
use quire_app::text::{Assoc, Buffer, EditKind, Selection, Transaction, motion, multi};
use quire_app::{Command, DocId, Workspace};

use crate::highlight::build_job;
use crate::theme::{FontSupport, Palette};
use crate::view_ui::{self, ViewAction};
use heights::HeightIndex;

const TOP_MARGIN: f32 = 36.0;
const BOTTOM_MARGIN: f32 = 240.0;
/// Notes up to this size are highlighted whole (exact); larger ones by window.
const FULL_HIGHLIGHT_BYTES: usize = 256 * 1024;
/// Lines of context highlighted around the viewport in window mode.
const WINDOW_SLACK_LINES: usize = 120;
const MAX_IMAGE_HEIGHT: f32 = 360.0;
const BLINK: Duration = Duration::from_millis(530);

pub(crate) fn editor_id(doc: DocId) -> Id {
    Id::unique(("quire-editor", doc.0))
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Char(usize),
    Word(usize, usize),
    Line(usize, usize),
}

struct DocView {
    synced_rev: u64,
    layout_key: (u32, u32, bool, bool),
    heights: HeightIndex,
    /// Secondary cursors (the primary lives in the Document).
    extra: Vec<Selection>,
    preedit: Option<String>,
    drag: Option<DragMode>,
    last_click: Option<(Instant, Pos2, u8)>,
    blink_start: Instant,
    preferred_x: Option<f32>,
    runs: Option<(u64, Range<usize>, Rc<Vec<StyleRun>>)>,
    blocks: Option<(u64, Rc<Vec<Range<usize>>>)>,
    /// `quire-view` blocks, rendered under their closing fence.
    view_blocks: Option<(u64, Rc<Vec<ViewBlock>>)>,
    /// Display math and diagrams, for a revision.
    figures: Option<(u64, Rc<Vec<blocks::Figure>>)>,
    spell_menu: Option<SpellMenu>,
    /// `block_of` answers for a revision (it walks up through quotes).
    block_memo: (u64, HashMap<usize, Block>),
    /// Where views were drawn last frame (screen space); the editor
    /// ignores presses there so the view's own widgets get them.
    view_rects: Vec<Rect>,
    /// Scroll-state key last used; when the note is renamed, its scroll
    /// position moves to the new key.
    scroll_key: Option<String>,
    /// Revision produced by this view's own last edit.
    own_rev: u64,
    /// The suggestion list at the caret: which item is highlighted, for
    /// which trigger, and a trigger the user dismissed with Escape.
    suggest_pick: usize,
    suggest_for: Option<(usize, SuggestKind, String)>,
    suggest_dismissed: Option<(usize, SuggestKind)>,
    /// Checkboxes drawn last frame (screen rect, char index of the box's
    /// inside), so a click toggles the task instead of moving the caret.
    task_boxes: Vec<(Rect, usize, bool)>,
    /// Lines of folded headings, sorted.
    folded: Vec<usize>,
    /// Lines hidden by folds, worked out for one revision.
    hidden: Option<HiddenLines>,
    /// Fold arrows drawn last frame: (screen rect, heading line).
    fold_toggles: Vec<(Rect, usize)>,
    /// Pictures of math and diagrams last frame: clicking one edits its source (first line).
    figure_boxes: Vec<(Rect, usize, usize)>,
}

impl DocView {
    fn new() -> Self {
        Self {
            synced_rev: u64::MAX,
            layout_key: (0, 0, false, false),
            heights: HeightIndex::default(),
            extra: Vec::new(),
            preedit: None,
            drag: None,
            last_click: None,
            blink_start: Instant::now(),
            preferred_x: None,
            runs: None,
            blocks: None,
            view_blocks: None,
            figures: None,
            spell_menu: None,
            block_memo: (u64::MAX, HashMap::new()),
            view_rects: Vec::new(),
            scroll_key: None,
            own_rev: u64::MAX,
            suggest_pick: 0,
            suggest_for: None,
            suggest_dismissed: None,
            task_boxes: Vec::new(),
            folded: Vec::new(),
            hidden: None,
            fold_toggles: Vec::new(),
            figure_boxes: Vec::new(),
        }
    }
}

/// Something drawn in place of hidden Markdown syntax in live preview.
/// Columns are char offsets within the line.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Deco {
    /// `-`, `*` or `+` starting a list item: a dot.
    Bullet(usize),
    /// `[ ]` or `[x]`: a checkbox you can click. `col` is the `[`.
    Task { col: usize, done: bool },
    /// `---` on its own line: a thin rule.
    Rule,
    /// Under a table's header row, and between its rows: a line this wide.
    TableLine { width: f32, strong: bool },
    /// A callout's icon (index into `callout::KINDS`) left of the title at
    /// `col`; with `label`, the kind's name is drawn too (no title written).
    Callout { kind: usize, col: usize, label: bool },
    /// Inline math drawn in place of `$…$`, whose first (hidden) char is at `col`.
    Math { col: usize, tex: egui::TextureId, w: f32, h: f32, baseline: f32 },
    /// A misspelled word, chars `start..end`: a wavy underline.
    Misspelled { start: usize, end: usize },
}

/// Width kept free for a callout's icon.
fn callout_icon_width(size: f32) -> f32 {
    (size * 1.55).round()
}

fn callout_color(kind: &quire_app::markdown::callout::CalloutKind) -> Color32 {
    Color32::from_rgb(kind.color[0], kind.color[1], kind.color[2])
}

/// A quote line: up to three spaces, then `>`.
fn is_quote_line(t: &str) -> bool {
    let trimmed = t.trim_start_matches(' ');
    t.len() - trimmed.len() <= 3 && trimmed.starts_with('>')
}

/// How a Markdown table is drawn in live preview: each column as wide as
/// its widest cell. `widths[row][cell]` is each cell's own width.
#[derive(Debug, Clone)]
struct TableLayout {
    first: usize,
    last: usize,
    separator: usize,
    cols: Vec<f32>,
    widths: Vec<Vec<f32>>,
}

impl TableLayout {
    fn gap(size: f32) -> f32 {
        size * 1.6
    }

    fn width(&self, size: f32) -> f32 {
        self.cols.iter().sum::<f32>() + Self::gap(size) * self.cols.len().saturating_sub(1) as f32
    }
}

/// The table `line` is in: (first, last, separator line). A table is a run
/// of lines starting with `|` whose second line is `| --- |`.
fn table_range(buf: &Buffer, line: usize) -> Option<(usize, usize, usize)> {
    let is_row = |l: usize| buf.line(l).trim_start().starts_with('|');
    if !is_row(line) {
        return None;
    }
    let (mut a, mut b) = (line, line);
    while a > 0 && is_row(a - 1) {
        a -= 1;
    }
    while b + 1 < buf.len_lines() && is_row(b + 1) {
        b += 1;
    }
    let sep = buf.line(a + 1);
    let t = sep.trim();
    let is_sep = a < b && t.contains("--") && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'));
    is_sep.then_some((a, b, a + 1))
}

/// Byte ranges of each cell's text (trimmed) in a table row.
fn table_cells(text: &str) -> Vec<Range<usize>> {
    let mut cells = Vec::new();
    let mut start = None;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'|' && (i == 0 || bytes[i - 1] != b'\\') {
            if let Some(s) = start {
                cells.push(s..i);
            }
            start = Some(i + 1);
        }
    }
    if let Some(s) = start
        && !text[s..].trim().is_empty()
    {
        cells.push(s..text.len());
    }
    cells
        .into_iter()
        .map(|r| {
            let inner = &text[r.clone()];
            let lead = inner.len() - inner.trim_start().len();
            let t = inner.trim();
            r.start + lead..r.start + lead + t.len()
        })
        .collect()
}

/// Split sections so none crosses a byte in `cuts`.
fn split_sections(job: &mut egui::text::LayoutJob, cuts: &[usize]) {
    let mut out = Vec::with_capacity(job.sections.len() + cuts.len());
    for sec in job.sections.drain(..) {
        let (start, b) = (sec.byte_range.start.0, sec.byte_range.end.0);
        let mut a = start;
        for &c in cuts.iter().filter(|&&c| c > start && c < b) {
            out.push(egui::text::LayoutSection { byte_range: egui::text::ByteIndex(a)..egui::text::ByteIndex(c), ..sec.clone() });
            a = c;
        }
        out.push(egui::text::LayoutSection { byte_range: egui::text::ByteIndex(a)..egui::text::ByteIndex(b), ..sec });
    }
    job.sections = out;
}

/// Lay a table row out as cells: pipes and padding vanish, the header is
/// bold, and with `place` each cell starts at its column (`place` holds the
/// column widths and this row's own cell widths).
fn table_row_job(
    mut job: egui::text::LayoutJob,
    text: &str,
    header: bool,
    cx: &Cx<'_>,
    place: Option<(&[f32], &[f32])>,
) -> egui::text::LayoutJob {
    let cells = table_cells(text);
    let cuts: Vec<usize> = cells.iter().flat_map(|r| [r.start, r.end]).collect();
    split_sections(&mut job, &cuts);
    let mut x_end = 0.0;
    let gap = TableLayout::gap(cx.size);
    for sec in &mut job.sections {
        let (a, b) = (sec.byte_range.start.0, sec.byte_range.end.0);
        match cells.iter().position(|c| c.start <= a && b <= c.end && a < b) {
            None => {
                sec.format.font_id.size = 0.5;
                sec.format.color = Color32::TRANSPARENT;
                sec.format.background = Color32::TRANSPARENT;
                sec.format.line_height = None;
            }
            Some(i) => {
                if header {
                    if cx.fonts.bold {
                        sec.format.font_id.family = egui::FontFamily::Name(crate::theme::BOLD.into());
                    }
                    sec.format.color = cx.pal.strong;
                }
                if let Some((cols, own)) = place
                    && a == cells[i].start
                {
                    let target: f32 = cols.iter().take(i).map(|w| w + gap).sum();
                    sec.leading_space = (target - x_end).max(0.0);
                    x_end = target + own.get(i).copied().unwrap_or(0.0);
                }
            }
        }
    }
    job
}

/// Lines hidden by folds, for a revision and a set of folded headings.
type HiddenLines = (u64, Vec<usize>, Rc<Vec<Range<usize>>>);

/// Lines drawn as one block: a background behind code and front matter, a
/// bar beside quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    None,
    Code,
    FrontMatter,
    Quote,
    /// A quote that starts with `[!type]`: index into `callout::KINDS`.
    Callout(usize),
}

/// Per-frame layout parameters.
struct Cx<'a> {
    pal: &'a Palette,
    fonts: FontSupport,
    size: f32,
    wrap: f32,
    /// Live preview: hide Markdown syntax on lines outside these ranges.
    live: Option<Vec<Range<usize>>>,
}

/// Should this syntax marker disappear in live preview? List bullets, task
/// boxes, quote markers, HTML, rules and code fences stay visible.
/// `line_start` is true when only whitespace comes before the marker.
fn hideable_marker(text: &str, line_start: bool) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let bullet = line_start
        && (matches!(t, "-" | "*" | "+")
            || (t.ends_with(['.', ')']) && t.len() > 1 && t[..t.len() - 1].bytes().all(|b| b.is_ascii_digit())));
    let task = matches!(t, "[ ]" | "[x]" | "[X]");
    let html = t.starts_with('<') && t.len() > 2 && t.ends_with('>');
    let rule = t.len() >= 3 && (t.bytes().all(|b| b == b'-') || t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'*'));
    !(bullet || task || html || rule || t.starts_with('>') || t.starts_with("```") || t.starts_with("~~~"))
}

/// Heading lines and levels, from the note's outline (so headings inside
/// code blocks don't count).
fn heading_lines(doc: &Document) -> Vec<(usize, u8)> {
    let buf = doc.buffer();
    doc.outline().iter().map(|h| (buf.char_to_line(doc.byte_to_char(h.offset)), h.level)).collect()
}

/// The lines a folded heading hides: up to the next heading of the same or
/// a higher level, keeping blank lines before it visible.
fn fold_range(headings: &[(usize, u8)], line: usize, len_lines: usize, blank: impl Fn(usize) -> bool) -> Option<Range<usize>> {
    let i = headings.iter().position(|(l, _)| *l == line)?;
    let level = headings[i].1;
    let mut end = headings[i + 1..].iter().find(|(_, lv)| *lv <= level).map_or(len_lines, |(l, _)| *l);
    while end > line + 1 && blank(end - 1) {
        end -= 1;
    }
    (line + 1 < end).then(|| line + 1..end)
}

/// Whether a line is empty or only whitespace.
fn blank_line(buf: &Buffer, line: usize) -> bool {
    buf.line(line).trim().is_empty()
}

/// `---`, `***` or `___` (three or more) on a line of its own.
fn hideable_rule(t: &str) -> bool {
    t.len() >= 3 && b"-*_".iter().any(|c| t.bytes().all(|b| b == *c))
}

impl Cx<'_> {
    fn row_h(&self) -> f32 {
        self.size * 1.55
    }
}

#[derive(Default)]
pub(crate) struct FastEditor {
    suggesting: bool,
    views: HashMap<DocId, DocView>,
    pub scroll_to_caret: bool,
    pub request_focus: bool,
    galleys: HashMap<usize, Arc<Galley>>,
    /// Live-preview decorations for the lines in `galleys`.
    decos: HashMap<usize, Vec<Deco>>,
    /// Tables laid out this frame, by first line.
    tables: HashMap<usize, Rc<TableLayout>>,
    /// The spell checker for this frame (`None` while off or loading).
    spell: Option<Arc<quire_spell::Speller>>,
}

/// Right-click on a misspelled word: its char range and what to offer.
#[derive(Debug, Clone)]
struct SpellMenu {
    range: Range<usize>,
    word: String,
    suggestions: Vec<String>,
}

fn all_selections(primary: Selection, extra: &[Selection]) -> Vec<Selection> {
    let mut v = extra.to_vec();
    v.push(primary);
    multi::normalize(v)
}

/// Pick the selection nearest `head` as primary; the rest become secondary.
fn split_primary(sels: Vec<Selection>, head: usize) -> (Selection, Vec<Selection>) {
    let i = sels.iter().enumerate().min_by_key(|(_, s)| s.head.abs_diff(head)).map_or(0, |(i, _)| i);
    let mut rest = sels;
    let p = rest.remove(i);
    (p, rest)
}

/// (row index, row start column) for a caret column in a line galley.
fn row_of(g: &Galley, col: usize) -> (usize, usize) {
    let mut start = 0;
    let last = g.rows.len().saturating_sub(1);
    for (i, r) in g.rows.iter().enumerate() {
        let len = r.glyphs.len();
        if col < start + len || i == last {
            return (i, start);
        }
        start += len;
    }
    (0, 0)
}

/// Where the primary selection sits in a normalized list.
fn primary_index(all: &[Selection], primary: Selection) -> usize {
    all.iter()
        .position(|s| *s == primary)
        .or_else(|| all.iter().position(|s| s.range().contains(&primary.head) || s.head == primary.head))
        .unwrap_or_else(|| all.len().saturating_sub(1))
}

fn row_start(g: &Galley, row: usize) -> usize {
    g.rows.iter().take(row).map(|r| r.glyphs.len()).sum()
}

impl FastEditor {
    pub(crate) fn forget(&mut self, docs: &[DocId]) {
        self.views.retain(|k, _| docs.contains(k));
    }

    pub(crate) fn has_extra_cursors(&self, doc: DocId) -> bool {
        self.views.get(&doc).is_some_and(|v| !v.extra.is_empty())
    }

    /// Whether the suggestion list was showing last frame (it owns Escape).
    pub(crate) const fn suggesting(&self) -> bool {
        self.suggesting
    }

    /// Suggestions at the caret, unless dismissed. Resets the highlight when
    /// the trigger or its query changes.
    fn current_suggestions(view: &mut DocView, ws: &Workspace) -> Option<Suggestions> {
        if !view.extra.is_empty() {
            return None;
        }
        let s = ws.suggestions();
        let key = s.as_ref().map(|s| (s.context.start, s.context.kind));
        if key != view.suggest_dismissed {
            view.suggest_dismissed = None;
        }
        let s = s.filter(|_| view.suggest_dismissed.is_none())?;
        let id = (s.context.start, s.context.kind, s.context.query.clone());
        if view.suggest_for.as_ref() != Some(&id) {
            view.suggest_for = Some(id);
            view.suggest_pick = 0;
        }
        view.suggest_pick = view.suggest_pick.min(s.items.len() - 1);
        Some(s)
    }

    /// The list under the caret. Returns the item clicked.
    fn suggestion_popup(ctx: &egui::Context, id: Id, caret: Rect, s: &Suggestions, pick: usize, pal: &Palette) -> Option<usize> {
        let mut clicked = None;
        let screen = ctx.content_rect();
        let width = 320.0_f32.min(screen.width() - 16.0);
        let rows = s.items.len() as f32 * 26.0 + 12.0;
        // Below the caret, or above it when there's no room.
        let below = caret.bottom() + 4.0;
        let y = if below + rows > screen.bottom() { (caret.top() - 4.0 - rows).max(screen.top()) } else { below };
        let x = caret.left().min(screen.right() - width - 8.0).max(screen.left() + 8.0);
        egui::Area::new(id.with("suggest")).order(egui::Order::Foreground).fixed_pos(Pos2::new(x, y)).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).fill(pal.panel).stroke(Stroke::new(1.0, pal.border)).inner_margin(4.0).show(ui, |ui| {
                ui.set_width(width);
                for (i, item) in s.items.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());
                    if i == pick {
                        ui.painter().rect_filled(rect, 4.0, pal.selection);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, pal.selection.gamma_multiply(0.5));
                    }
                    let font = egui::FontId::proportional(13.5);
                    let label = ui.painter().layout_no_wrap(item.label.clone(), font, pal.strong);
                    let label_w = label.size().x;
                    ui.painter().galley(Pos2::new(rect.left() + 8.0, rect.center().y - label.size().y / 2.0), label, pal.strong);
                    if !item.detail.is_empty() {
                        // Keep the end of long details (a folder path's last parts matter most).
                        let room = ((rect.width() - label_w - 28.0) / 6.5).max(4.0) as usize;
                        let n = item.detail.chars().count();
                        let detail = if n > room {
                            std::iter::once('…').chain(item.detail.chars().skip(n + 1 - room)).collect()
                        } else {
                            item.detail.clone()
                        };
                        let small = egui::FontId::proportional(12.0);
                        ui.painter().text(
                            Pos2::new(rect.right() - 8.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            detail,
                            small,
                            pal.muted,
                        );
                    }
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
        clicked
    }

    // ------------------------------------------------------------ layout

    fn estimate(buf: &Buffer, line: usize, cx: &Cx<'_>) -> f32 {
        let chars = buf.line_range(line).len() as f32;
        let per_row = (cx.wrap / (cx.size * 0.52)).max(8.0);
        cx.row_h() * (chars / per_row).ceil().max(1.0)
    }

    fn sync(view: &mut DocView, doc: &Document, cx: &Cx<'_>, inline_images: bool) {
        let key = (cx.wrap.to_bits(), cx.size.to_bits(), cx.pal.dark, inline_images);
        let buf = doc.buffer();
        let mut reset = view.layout_key != key || view.synced_rev == u64::MAX || view.synced_rev > doc.revision();
        if !reset && view.synced_rev != doc.revision() {
            match doc.line_deltas_since(view.synced_rev) {
                Some(ds) => {
                    for d in ds {
                        view.heights.splice(d.start_line, d.old_lines, d.new_lines, cx.row_h());
                        // Folds move with their heading; an edited heading line keeps its fold.
                        let shift = d.new_lines as isize - d.old_lines as isize;
                        view.folded.retain(|&l| l < d.start_line + 1 || l >= d.start_line + d.old_lines);
                        for l in &mut view.folded {
                            if *l >= d.start_line + d.old_lines && d.old_lines + d.new_lines > 0 {
                                *l = (*l as isize + shift).max(0) as usize;
                            }
                        }
                        view.hidden = None;
                    }
                }
                None => reset = true,
            }
        }
        if !reset && view.heights.len() != buf.len_lines() {
            reset = true;
        }
        if reset {
            view.heights.reset(buf.len_lines(), |l| Self::estimate(buf, l, cx));
            view.layout_key = key;
        }
        if view.synced_rev != doc.revision() {
            // Secondary cursors don't survive edits made elsewhere (undo, commands).
            if doc.revision() != view.own_rev {
                view.extra.clear();
            }
            view.synced_rev = doc.revision();
        }
    }

    /// Style runs covering `lines`, highlighting a window for big notes.
    fn runs_for(view: &mut DocView, doc: &Document, lines: Range<usize>) -> Rc<Vec<StyleRun>> {
        let buf = doc.buffer();
        let rev = doc.revision();
        if buf.len_bytes() <= FULL_HIGHLIGHT_BYTES && !doc.large {
            return doc.style_runs();
        }
        let need = buf.char_to_byte(buf.line_to_char(lines.start))..buf.char_to_byte(buf.line_to_char(lines.end.min(buf.len_lines())));
        if let Some((r, w, runs)) = &view.runs
            && *r == rev
            && w.start <= need.start
            && w.end >= need.end
        {
            return Rc::clone(runs);
        }
        let text = doc.text();
        let bl = match &view.blocks {
            Some((r, b)) if *r == rev => Rc::clone(b),
            _ => {
                let b = Rc::new(blocks::verbatim_blocks(&text));
                view.blocks = Some((rev, Rc::clone(&b)));
                b
            }
        };
        let l0 = lines.start.saturating_sub(WINDOW_SLACK_LINES);
        let l1 = (lines.end + WINDOW_SLACK_LINES).min(buf.len_lines());
        let w = buf.char_to_byte(buf.line_to_char(l0))..buf.char_to_byte(buf.line_to_char(l1));
        let runs = Rc::new(blocks::highlight_window(&text, w.clone(), &bl));
        view.runs = Some((rev, w, Rc::clone(&runs)));
        runs
    }

    /// Line ranges hidden by folds (merged, sorted).
    fn hidden(view: &mut DocView, doc: &Document) -> Rc<Vec<Range<usize>>> {
        if view.folded.is_empty() {
            return Rc::default();
        }
        if let Some((rev, folded, h)) = &view.hidden
            && *rev == doc.revision()
            && *folded == view.folded
        {
            return Rc::clone(h);
        }
        let headings = heading_lines(doc);
        // Folds on lines that are no longer headings are dropped.
        view.folded.retain(|l| headings.iter().any(|(h, _)| h == l));
        let n = doc.buffer().len_lines();
        let mut ranges: Vec<Range<usize>> =
            view.folded.iter().filter_map(|&l| fold_range(&headings, l, n, |x| blank_line(doc.buffer(), x))).collect();
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for r in ranges {
            match merged.last_mut() {
                Some(m) if r.start <= m.end => m.end = m.end.max(r.end),
                _ => merged.push(r),
            }
        }
        let h = Rc::new(merged);
        view.hidden = Some((doc.revision(), view.folded.clone(), Rc::clone(&h)));
        h
    }

    fn is_hidden(view: &mut DocView, doc: &Document, line: usize) -> bool {
        Self::hidden(view, doc).iter().any(|r| r.contains(&line))
    }

    /// Fold or unfold the heading at `line`; lines it shows again get an
    /// estimated height until they're drawn.
    fn toggle_fold(view: &mut DocView, doc: &Document, line: usize, cx_row_h: f32) {
        if let Some(i) = view.folded.iter().position(|l| *l == line) {
            let range = fold_range(&heading_lines(doc), line, doc.buffer().len_lines(), |x| blank_line(doc.buffer(), x));
            view.folded.remove(i);
            for l in range.into_iter().flatten() {
                view.heights.set(l, cx_row_h);
            }
        } else {
            view.folded.push(line);
            view.folded.sort_unstable();
        }
        view.hidden = None;
    }

    /// Make `line` visible by unfolding whatever hides it.
    fn reveal_line(view: &mut DocView, doc: &Document, line: usize, row_h: f32) {
        while Self::is_hidden(view, doc, line) {
            let headings = heading_lines(doc);
            let n = doc.buffer().len_lines();
            let Some(&h) = view
                .folded
                .iter()
                .find(|&&h| fold_range(&headings, h, n, |x| blank_line(doc.buffer(), x)).is_some_and(|r| r.contains(&line)))
            else {
                break;
            };
            Self::toggle_fold(view, doc, h, row_h);
        }
    }

    /// The nearest visible line above (`dir < 0`) or below `line`.
    fn visible_neighbor(view: &mut DocView, doc: &Document, line: usize, dir: i32) -> Option<usize> {
        let hidden = Self::hidden(view, doc);
        let n = doc.buffer().len_lines();
        let mut l = line;
        loop {
            l = if dir < 0 { l.checked_sub(1)? } else { l + 1 };
            if l >= n {
                return None;
            }
            match hidden.iter().find(|r| r.contains(&l)) {
                Some(r) if dir < 0 => l = r.start,
                Some(r) => l = r.end - 1,
                None => return Some(l),
            }
        }
    }

    /// A line's text and the style runs inside it (byte offsets in the line).
    fn line_runs(view: &mut DocView, doc: &Document, line: usize) -> (String, Vec<StyleRun>) {
        let buf = doc.buffer();
        let lr = buf.line_range(line);
        let text = buf.slice(lr.clone());
        let b0 = buf.char_to_byte(lr.start);
        let b1 = b0 + text.len();
        let runs = Self::runs_for(view, doc, line..line + 1);
        let i = runs.partition_point(|r| r.range.end <= b0);
        let local = runs[i..]
            .iter()
            .take_while(|r| r.range.start < b1)
            .map(|r| StyleRun { range: r.range.start.max(b0) - b0..r.range.end.min(b1) - b0, style: r.style })
            .filter(|r| r.range.start < r.range.end)
            .collect();
        (text, local)
    }

    /// Column widths for the table in lines `first..=last`, measured once a frame.
    fn table_layout(
        &mut self,
        ui: &Ui,
        view: &mut DocView,
        doc: &Document,
        (first, last, separator): (usize, usize, usize),
        cx: &Cx<'_>,
    ) -> Rc<TableLayout> {
        if let Some(t) = self.tables.get(&first) {
            return Rc::clone(t);
        }
        let mut widths = Vec::new();
        let mut cols: Vec<f32> = Vec::new();
        for line in first..=last {
            if line == separator {
                widths.push(Vec::new());
                continue;
            }
            let (text, local) = Self::line_runs(view, doc, line);
            let mut job = table_row_job(build_job(&text, &local, cx.pal, cx.size, cx.fonts), &text, line == first, cx, None);
            job.wrap.max_width = f32::INFINITY;
            let g = ui.fonts_mut(|f| f.layout_job(job));
            let x = |byte: usize| g.pos_from_cursor(CCursor::new(text[..byte].chars().count())).left();
            let row: Vec<f32> = table_cells(&text).iter().map(|c| (x(c.end) - x(c.start)).max(0.0)).collect();
            if cols.len() < row.len() {
                cols.resize(row.len(), 0.0);
            }
            for (c, w) in cols.iter_mut().zip(&row) {
                *c = c.max(*w);
            }
            widths.push(row);
        }
        let t = Rc::new(TableLayout { first, last, separator, cols, widths });
        self.tables.insert(first, Rc::clone(&t));
        t
    }

    fn galley(&mut self, ui: &Ui, view: &mut DocView, doc: &Document, line: usize, cx: &Cx<'_>) -> Arc<Galley> {
        if let Some(g) = self.galleys.get(&line) {
            return Arc::clone(g);
        }
        let buf = doc.buffer();
        let lr = buf.line_range(line);
        let text = buf.slice(lr.clone());
        let b0 = buf.char_to_byte(lr.start);
        let b1 = b0 + text.len();
        let runs = Self::runs_for(view, doc, line..line + 1);
        let i = runs.partition_point(|r| r.range.end <= b0);
        let local: Vec<StyleRun> = runs[i..]
            .iter()
            .take_while(|r| r.range.start < b1)
            .map(|r| StyleRun { range: r.range.start.max(b0) - b0..r.range.end.min(b1) - b0, style: r.style })
            .filter(|r| r.range.start < r.range.end)
            .collect();
        // IME pre-edit is laid out inside the caret's line, underlined.
        let caret = doc.selection().head;
        let (text, local, pre_range) = match view.preedit.as_deref().filter(|p| !p.is_empty()) {
            Some(pre) if buf.char_to_line(caret) == line => {
                let off = buf.char_to_byte(caret) - b0;
                let n = pre.len();
                let mut t = text.clone();
                t.insert_str(off, pre);
                let shifted: Vec<StyleRun> = local
                    .into_iter()
                    .flat_map(|r| {
                        if r.range.end <= off {
                            vec![r]
                        } else if r.range.start >= off {
                            vec![StyleRun { range: r.range.start + n..r.range.end + n, style: r.style }]
                        } else {
                            vec![
                                StyleRun { range: r.range.start..off, style: r.style },
                                StyleRun { range: off + n..r.range.end + n, style: r.style },
                            ]
                        }
                    })
                    .collect();
                (std::borrow::Cow::Owned(t), shifted, Some(off..off + n))
            }
            _ => (std::borrow::Cow::Borrowed(text.as_str()), local, None),
        };
        let mut job = build_job(&text, &local, cx.pal, cx.size, cx.fonts);
        let mut decos = Vec::new();
        // Tables render as a grid unless the caret is somewhere in them.
        let table = cx
            .live
            .as_ref()
            .filter(|_| pre_range.is_none())
            .and_then(|revealed| table_range(buf, line).filter(|(a, b, _)| !revealed.iter().any(|r| r.start <= *b && *a < r.end)));
        if let Some(range) = table {
            let t = self.table_layout(ui, view, doc, range, cx);
            let width = t.width(cx.size).min(cx.wrap);
            if line == t.separator {
                for sec in &mut job.sections {
                    sec.format.font_id.size = 0.5;
                    sec.format.color = Color32::TRANSPARENT;
                    sec.format.background = Color32::TRANSPARENT;
                    sec.format.line_height = Some(cx.size * 0.5);
                }
                decos.push(Deco::TableLine { width, strong: true });
            } else {
                let own = &t.widths[line - t.first];
                job = table_row_job(job, &text, line == t.first, cx, Some((&t.cols, own)));
                if line != t.first && line != t.last {
                    decos.push(Deco::TableLine { width, strong: false });
                }
            }
        } else if let Some(revealed) = &cx.live
            && pre_range.is_none()
            && !revealed.iter().any(|r| r.contains(&line))
        {
            let is_task = local.iter().any(|r| r.style.marker && matches!(text[r.range.clone()].trim(), "[ ]" | "[x]" | "[X]"));
            for run in local.iter().filter(|r| r.style.marker && !r.style.code_block && !r.style.conflict && !r.style.front_matter) {
                let raw = &text[run.range.clone()];
                let t = raw.trim();
                let line_start = text[..run.range.start].trim().is_empty();
                let col = text[..run.range.start + (raw.len() - raw.trim_start().len())].chars().count();
                // Markers drawn as something else keep their width (so the text
                // doesn't jump) but turn invisible.
                let replaced = match t {
                    "-" | "*" | "+" if line_start => {
                        if !is_task {
                            decos.push(Deco::Bullet(col));
                        }
                        true
                    }
                    "[ ]" | "[x]" | "[X]" => {
                        decos.push(Deco::Task { col, done: t != "[ ]" });
                        true
                    }
                    _ if line_start && text.trim() == t && hideable_rule(t) => {
                        decos.push(Deco::Rule);
                        true
                    }
                    _ => false,
                };
                // A quote's `>` gives way to the bar drawn beside the quote.
                let hide = !replaced && (hideable_marker(raw, line_start) || (line_start && t == ">"));
                if !replaced && !hide {
                    continue;
                }
                for sec in &mut job.sections {
                    if sec.byte_range.start.0 >= run.range.start && sec.byte_range.end.0 <= run.range.end {
                        sec.format.color = Color32::TRANSPARENT;
                        sec.format.background = Color32::TRANSPARENT;
                        sec.format.strikethrough = Stroke::NONE;
                        sec.format.underline = Stroke::NONE;
                        if hide {
                            sec.format.font_id.size = 0.5;
                            sec.format.line_height = None;
                        }
                    }
                }
            }
        }
        // Spelling: words in prose only (not code, math or front matter), and
        // not the word the caret is in while it's being typed.
        if let Some(sp) = &self.spell
            && pre_range.is_none()
            && !local.iter().any(|r| r.style.code_block || r.style.front_matter || r.style.conflict)
        {
            let caret_col = (buf.char_to_line(caret) == line).then(|| caret - lr.start);
            let mut lc = quire_spell::LineContext::new();
            for r in sp.misspellings(&text, &mut lc) {
                let skip = local.iter().any(|run| (run.style.code || run.style.math) && run.range.start < r.end && r.start < run.range.end);
                let (a, b) = (text[..r.start].chars().count(), text[..r.end].chars().count());
                if skip || caret_col.is_some_and(|c| c >= a && c <= b) {
                    continue;
                }
                decos.push(Deco::Misspelled { start: a, end: b });
            }
        }
        // Live preview: inline `$math$` drawn as math once it's rendered.
        let revealed_line = cx.live.as_ref().is_none_or(|revealed| revealed.iter().any(|r| r.contains(&line)));
        // Math that starts or ends on another line belongs to a display block, drawn whole or not at all.
        let math_crosses_lines =
            runs[i..].iter().take_while(|r| r.range.start < b1).any(|r| r.style.math && (r.range.start < b0 || r.range.end > b1));
        let in_figure = Self::figures_of(view, doc).iter().any(|f| f.first_line <= line && line <= f.last_line);
        if crate::typeset_ui::AVAILABLE
            && !revealed_line
            && pre_range.is_none()
            && table.is_none()
            && !math_crosses_lines
            && !in_figure
            && local.iter().any(|r| r.style.math)
            && !text.trim_start().starts_with("$$")
        {
            let mut spans: Vec<Range<usize>> = Vec::new();
            for r in local.iter().filter(|r| r.style.math && !r.style.code_block) {
                match spans.last_mut() {
                    Some(last) if last.end >= r.range.start => last.end = last.end.max(r.range.end),
                    _ => spans.push(r.range.clone()),
                }
            }
            for sp in &mut spans {
                while sp.start > 0 && text.as_bytes()[sp.start - 1] == b'$' {
                    sp.start -= 1;
                }
                while sp.end < text.len() && text.as_bytes()[sp.end] == b'$' {
                    sp.end += 1;
                }
            }
            for sp in spans {
                let latex = text[sp.clone()].trim_matches('$').trim();
                if latex.is_empty() {
                    continue;
                }
                let math_job = crate::typeset_ui::Job::Math {
                    latex: latex.to_string(),
                    display: text[sp.clone()].starts_with("$$"),
                    size: cx.size.round() as u32,
                    color: crate::typeset_ui::rgba(cx.pal.text),
                };
                let out = crate::typeset_ui::request(ui.ctx(), &math_job);
                match out {
                    crate::typeset_ui::Out::Ready(p) => {
                        split_sections(&mut job, &[sp.start, sp.end]);
                        let row_h = (cx.size * 1.45).max(p.size.y + 4.0);
                        let mut first = true;
                        for sec in &mut job.sections {
                            if sec.byte_range.start.0 >= sp.start && sec.byte_range.end.0 <= sp.end {
                                sec.format.color = Color32::TRANSPARENT;
                                sec.format.background = Color32::TRANSPARENT;
                                sec.format.font_id.size = 0.5;
                                sec.format.line_height = Some(row_h);
                                if first {
                                    sec.leading_space = p.size.x + 2.0;
                                    first = false;
                                }
                            }
                        }
                        let col = text[..sp.start].chars().count();
                        decos.push(Deco::Math { col, tex: p.texture.id(), w: p.size.x, h: p.size.y, baseline: p.baseline });
                    }
                    crate::typeset_ui::Out::Failed(_) => {
                        split_sections(&mut job, &[sp.start, sp.end]);
                        for sec in &mut job.sections {
                            if sec.byte_range.start.0 >= sp.start && sec.byte_range.end.0 <= sp.end {
                                sec.format.color = cx.pal.warn;
                            }
                        }
                    }
                    crate::typeset_ui::Out::Pending => {}
                }
            }
        }
        // Live preview, outside the lines being edited: callout titles and paragraph ids.
        let revealed_here = cx.live.as_ref().is_none_or(|revealed| revealed.iter().any(|r| r.contains(&line)));
        if !revealed_here && pre_range.is_none() && table.is_none() && !local.iter().any(|r| r.style.code_block || r.style.front_matter) {
            if let Some(h) = quire_app::markdown::callout::parse_header(&text) {
                let kind = quire_app::markdown::callout::KINDS.iter().position(|k| k.id == h.kind.id).unwrap_or(0);
                let color = callout_color(h.kind);
                split_sections(&mut job, &[h.marker.end, h.title_range.end]);
                let mut first = true;
                for sec in &mut job.sections {
                    let (a, b) = (sec.byte_range.start.0, sec.byte_range.end.0);
                    if b <= h.marker.end {
                        sec.format.color = Color32::TRANSPARENT;
                        sec.format.background = Color32::TRANSPARENT;
                        sec.format.font_id.size = 0.5;
                        sec.format.line_height = None;
                    } else if a >= h.title_range.start && b <= h.title_range.end && a < b {
                        sec.format.color = color;
                        if cx.fonts.bold {
                            sec.format.font_id.family = egui::FontFamily::Name(crate::theme::BOLD.into());
                        }
                        if first {
                            sec.leading_space = callout_icon_width(cx.size);
                            first = false;
                        }
                    }
                }
                let col = text[..h.title_range.start].chars().count();
                decos.push(Deco::Callout { kind, col, label: h.title.is_empty() });
            }
            if local.iter().any(|r| r.style.quote) && matches!(Self::block_of(view, doc, line), Block::Callout(_)) {
                for sec in &mut job.sections {
                    if sec.format.color == cx.pal.muted {
                        sec.format.color = cx.pal.text;
                    }
                }
            }
            if let Some((r, _)) = quire_app::markdown::callout::block_id(&text) {
                split_sections(&mut job, &[r.start, r.end]);
                for sec in &mut job.sections {
                    if sec.byte_range.start.0 >= r.start && sec.byte_range.end.0 <= r.end {
                        sec.format.color = cx.pal.faint;
                        sec.format.font_id.size = (cx.size * 0.78).round();
                        sec.format.valign = Align::Center;
                    }
                }
            }
        }
        self.decos.insert(line, decos);
        if let Some(r) = pre_range {
            for sec in &mut job.sections {
                if sec.byte_range.start.0 >= r.start && sec.byte_range.end.0 <= r.end {
                    sec.format.underline = Stroke::new(1.5, cx.pal.accent);
                }
            }
        }
        job.wrap.max_width = cx.wrap;
        let g = ui.fonts_mut(|f| f.layout_job(job));
        self.galleys.insert(line, Arc::clone(&g));
        g
    }

    /// Display math and diagram blocks, cached per revision.
    fn figures_of(view: &mut DocView, doc: &Document) -> Rc<Vec<blocks::Figure>> {
        match &view.figures {
            Some((r, f)) if *r == doc.revision() => Rc::clone(f),
            _ => {
                let f = Rc::new(if doc.large { Vec::new() } else { blocks::figures(&doc.text()) });
                view.figures = Some((doc.revision(), Rc::clone(&f)));
                f
            }
        }
    }

    /// Which block a line belongs to (memoized per revision).
    fn block_of(view: &mut DocView, doc: &Document, line: usize) -> Block {
        if view.block_memo.0 != doc.revision() {
            view.block_memo = (doc.revision(), HashMap::new());
        }
        if let Some(b) = view.block_memo.1.get(&line) {
            return *b;
        }
        let b = Self::block_of_uncached(view, doc, line);
        view.block_memo.1.insert(line, b);
        b
    }

    fn block_of_uncached(view: &mut DocView, doc: &Document, line: usize) -> Block {
        let buf = doc.buffer();
        if line >= buf.len_lines() {
            return Block::None;
        }
        let lr = buf.line_range(line);
        let b0 = buf.char_to_byte(lr.start);
        let b1 = buf.char_to_byte(lr.end).max(b0 + 1);
        let runs = Self::runs_for(view, doc, line..line + 1);
        let i = runs.partition_point(|r| r.range.end <= b0);
        let mut quote = false;
        for r in runs[i..].iter().take_while(|r| r.range.start < b1) {
            if r.style.code_block {
                return Block::Code;
            }
            if r.style.front_matter {
                return Block::FrontMatter;
            }
            quote |= r.style.quote;
        }
        if !quote {
            return Block::None;
        }
        // Walk up to the quote's first line: is it a callout?
        let mut l = line;
        loop {
            let t = buf.line(l);
            if let Some(h) = quire_app::markdown::callout::parse_header(&t) {
                let k = quire_app::markdown::callout::KINDS.iter().position(|k| k.id == h.kind.id).unwrap_or(0);
                return Block::Callout(k);
            }
            if l == 0 || line - l > 400 || !is_quote_line(&buf.line(l - 1)) {
                return Block::Quote;
            }
            l -= 1;
        }
    }

    /// Image shown under a line that only embeds an image.
    fn line_image(ws: &Workspace, doc: &Document, line: usize) -> Option<String> {
        if !ws.settings.inline_images {
            return None;
        }
        let text = doc.buffer().line(line);
        let target = quire_app::attach::image_embed(&text)?;
        let path = ws.attachment_path(target)?;
        Some(format!("file://{}", path.display()))
    }

    /// Display size of an inline image (scaled down to fit), if it loads.
    fn image_size(ui: &Ui, uri: &str, width: f32) -> Option<Vec2> {
        let img = egui::Image::new(uri.to_string());
        match img.load_for_size(ui.ctx(), Vec2::new(width, MAX_IMAGE_HEIGHT)) {
            Ok(egui::load::TexturePoll::Ready { texture }) => {
                let s = texture.size;
                let scale = (width / s.x).min(MAX_IMAGE_HEIGHT / s.y).min(1.0);
                Some(s * scale)
            }
            Ok(egui::load::TexturePoll::Pending { .. }) => {
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                Some(Vec2::new(width.min(120.0), 30.0))
            }
            Err(_) => None,
        }
    }

    // ------------------------------------------------------------ geometry

    /// Char index under a point in content coordinates.
    fn hit(&mut self, ui: &Ui, view: &mut DocView, doc: &Document, cx: &Cx<'_>, origin: Pos2, x0: f32, pos: Pos2) -> usize {
        let buf = doc.buffer();
        let y = pos.y - origin.y - TOP_MARGIN;
        if y < 0.0 {
            return 0;
        }
        let line = view.heights.line_at(y).min(buf.len_lines().saturating_sub(1));
        let g = self.galley(ui, view, doc, line, cx);
        let local = Vec2::new(pos.x - x0, (y - view.heights.top(line)).min(g.size().y - 1.0).max(0.0));
        let c = g.cursor_from_pos(local);
        let lr = buf.line_range(line);
        lr.start + c.index.0.min(lr.len())
    }

    /// Caret rectangle in content coordinates.
    fn caret_rect(&mut self, ui: &Ui, view: &mut DocView, doc: &Document, cx: &Cx<'_>, origin: Pos2, x0: f32, pos: usize) -> Rect {
        let buf = doc.buffer();
        let line = buf.char_to_line(pos);
        let g = self.galley(ui, view, doc, line, cx);
        let col = pos - buf.line_to_char(line);
        let r = g.pos_from_cursor(CCursor::new(col));
        r.translate(Vec2::new(x0, origin.y + TOP_MARGIN + view.heights.top(line)))
    }

    /// Move `pos` one visual row up (`dir < 0`) or down, keeping column `x`.
    fn vertical(&mut self, ui: &Ui, view: &mut DocView, doc: &Document, cx: &Cx<'_>, pos: usize, dir: i32, x: Option<f32>) -> (usize, f32) {
        let buf = doc.buffer();
        let line = buf.char_to_line(pos);
        let ls = buf.line_to_char(line);
        let g = self.galley(ui, view, doc, line, cx);
        let (row, rs) = row_of(&g, pos - ls);
        let x = x.unwrap_or_else(|| g.rows.get(row).map_or(0.0, |r| r.x_offset(CharIndex(pos - ls - rs))));
        let (tline, trow) = if dir < 0 {
            if row > 0 {
                (line, row - 1)
            } else if let Some(prev) = Self::visible_neighbor(view, doc, line, -1) {
                let g2 = self.galley(ui, view, doc, prev, cx);
                (prev, g2.rows.len().saturating_sub(1))
            } else {
                return (0, x);
            }
        } else if row + 1 < g.rows.len() {
            (line, row + 1)
        } else if let Some(next) = Self::visible_neighbor(view, doc, line, 1) {
            (next, 0)
        } else {
            return (buf.len_chars(), x);
        };
        let g2 = self.galley(ui, view, doc, tline, cx);
        let Some(r) = g2.rows.get(trow) else { return (buf.line_to_char(tline), x) };
        let col = row_start(&g2, trow) + r.char_at(x).0;
        let lr = buf.line_range(tline);
        (lr.start + col.min(lr.len()), x)
    }

    // ------------------------------------------------------------ editing helpers

    fn apply_multi(
        view: &mut DocView,
        ws: &mut Workspace,
        kind: EditKind,
        f: impl FnOnce(&Buffer, &[Selection]) -> Option<(Transaction, Vec<Selection>)>,
    ) -> bool {
        let Some(doc) = ws.active_doc_mut() else { return false };
        let all = all_selections(doc.selection(), &view.extra);
        let Some((tx, new)) = f(doc.buffer(), &all) else { return false };
        let head = tx.map_pos(doc.selection().head, Assoc::After);
        let (p, rest) = split_primary(new, head);
        let multi_cursor = !rest.is_empty();
        let kind = if multi_cursor { EditKind::Other } else { kind };
        if doc.apply(tx.with_selection(p), kind).is_ok() {
            view.extra = rest;
            view.own_rev = doc.revision();
            true
        } else {
            false
        }
    }

    fn set_selections(view: &mut DocView, ws: &mut Workspace, sels: Vec<Selection>, prefer_head: usize) {
        let Some(doc) = ws.active_doc_mut() else { return };
        let len = doc.buffer().len_chars();
        let sels = multi::normalize(sels.into_iter().map(|s| s.clamp(len)).collect());
        if sels.is_empty() {
            return;
        }
        let (p, rest) = split_primary(sels, prefer_head);
        doc.set_selection(p);
        view.extra = rest;
    }

    fn selected_text(doc: &Document, extra: &[Selection]) -> String {
        let all = all_selections(doc.selection(), extra);
        all.iter().filter(|s| !s.is_empty()).map(|s| doc.buffer().slice(s.range())).collect::<Vec<_>>().join("\n")
    }

    // ------------------------------------------------------------ commands from the app

    pub(crate) fn command(&mut self, cmd: Command, ws: &mut Workspace) {
        let Some(doc) = ws.active_doc() else { return };
        let id = doc.id;
        let view = self.views.entry(id).or_insert_with(DocView::new);
        let buf = doc.buffer();
        let all = all_selections(doc.selection(), &view.extra);
        let head = doc.selection().head;
        let new = match cmd {
            Command::AddNextOccurrence => multi::add_next_occurrence(buf, &all),
            Command::SelectAllOccurrences => multi::all_occurrences(buf, doc.selection()),
            Command::AddCursorAbove => {
                let top = *all.first().expect("at least the primary");
                let mut v = all.clone();
                v.extend(multi::add_cursor_vertical(buf, top, -1));
                v
            }
            Command::AddCursorBelow => {
                let bottom = *all.last().expect("at least the primary");
                let mut v = all.clone();
                v.extend(multi::add_cursor_vertical(buf, bottom, 1));
                v
            }
            Command::ToggleFold | Command::FoldAll | Command::UnfoldAll => {
                let headings = heading_lines(doc);
                let caret_line = buf.char_to_line(head);
                let row_h = ws.settings.font_size * 1.55;
                match cmd {
                    Command::ToggleFold => {
                        // The heading the caret is under (or on).
                        if let Some(&(h, _)) = headings.iter().rev().find(|(l, _)| *l <= caret_line) {
                            Self::toggle_fold(view, doc, h, row_h);
                            if h != caret_line {
                                let at = buf.line_to_char(h);
                                Self::set_selections(view, ws, vec![Selection::cursor(at)], at);
                            }
                        }
                    }
                    Command::FoldAll => {
                        let n = buf.len_lines();
                        view.folded = headings
                            .iter()
                            .filter(|(l, _)| fold_range(&headings, *l, n, |x| blank_line(buf, x)).is_some())
                            .map(|(l, _)| *l)
                            .collect();
                        view.hidden = None;
                        // The caret moves up to its heading if its line got hidden.
                        if let Some(&(h, _)) = headings.iter().rev().find(|(l, _)| *l <= caret_line)
                            && Self::is_hidden(view, doc, caret_line)
                        {
                            let at = buf.line_to_char(h);
                            Self::set_selections(view, ws, vec![Selection::cursor(at)], at);
                        }
                    }
                    _ => {
                        for h in std::mem::take(&mut view.folded) {
                            for l in fold_range(&headings, h, buf.len_lines(), |x| blank_line(buf, x)).into_iter().flatten() {
                                view.heights.set(l, row_h);
                            }
                        }
                        view.hidden = None;
                    }
                }
                self.scroll_to_caret = true;
                self.request_focus = true;
                return;
            }
            _ => return,
        };
        let prefer = match cmd {
            Command::AddNextOccurrence | Command::AddCursorAbove | Command::AddCursorBelow => {
                // The newest cursor becomes primary so the view follows it.
                new.iter().map(|s| s.head).find(|h| !all.iter().any(|a| a.head == *h)).unwrap_or(head)
            }
            _ => head,
        };
        Self::set_selections(view, ws, new, prefer);
        self.scroll_to_caret = true;
        self.request_focus = true;
    }

    // ------------------------------------------------------------ the widget

    pub(crate) fn show(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette, fonts: FontSupport) {
        self.show_pane(ui, ws, pal, fonts, true);
    }

    /// Draw the active note. `primary` is false for the other half of a split
    /// editor: it doesn't take pending focus or scroll requests.
    pub(crate) fn show_pane(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette, fonts: FontSupport, primary: bool) {
        let Some(doc) = ws.active_doc() else { return };
        let doc_id = doc.id;
        let id = editor_id(doc_id);
        let size = ws.settings.font_size;
        let max_w = ws.settings.line_width as f32 * size * 0.56;
        let inline_images = ws.settings.inline_images;
        let indent = ws.settings.indent_unit.clone();
        let (find_matches, _) = ws.find_matches();
        let find_current = ws.find_current();

        let mut view = self.views.remove(&doc_id).unwrap_or_else(DocView::new);
        self.galleys.clear();
        self.decos.clear();
        self.tables.clear();
        let ctx = ui.ctx().clone();
        let scroll_to_caret = primary && std::mem::take(&mut self.scroll_to_caret);
        let request_focus = primary && std::mem::take(&mut self.request_focus);

        let avail_w = ui.available_width();
        let wrap = (avail_w - 96.0).min(max_w).max(120.0);
        let live = ws.settings.live_preview.then(|| {
            let doc = ws.active_doc().expect("checked");
            let buf = doc.buffer();
            all_selections(doc.selection(), &view.extra)
                .iter()
                .map(|s| buf.char_to_line(s.start())..buf.char_to_line(s.end()) + 1)
                .collect::<Vec<_>>()
        });
        let cx = Cx { pal, fonts, size, wrap, live };
        self.spell = (ws.settings.spell_check && !ws.active_doc().expect("checked").large)
            .then(|| crate::spell_ui::speller(&ws.settings.spell_language, &ctx))
            .flatten();
        Self::sync(&mut view, ws.active_doc().expect("checked"), &cx, inline_images);
        let total = view.heights.total() + TOP_MARGIN + BOTTOM_MARGIN;
        let mut view_action: Option<ViewAction> = None;
        let mut clicked_suggestion: Option<(Suggestions, usize)> = None;
        let mut suggesting = false;

        // Keyed by the note (tab ids restart each launch, and egui remembers
        // scroll offsets across launches), so a note reopens where you left it.
        let scroll_key =
            ws.active_doc().and_then(|d| d.path.as_ref()).map_or_else(|| format!("doc-{}", doc_id.0), |p| p.as_str().to_string());
        if let Some(old) = view.scroll_key.replace(scroll_key.clone())
            && old != scroll_key
        {
            let id_of = |k: &str| ui.make_persistent_id(egui::IdSalt::new(("quire-fast-scroll", k.to_string())));
            if let Some(state) = egui::scroll_area::State::load(&ctx, id_of(&old)) {
                state.store(&ctx, id_of(&scroll_key));
            }
        }
        egui::ScrollArea::vertical().id_salt(("quire-fast-scroll", scroll_key)).auto_shrink([false, false]).show_viewport(
            ui,
            |ui, viewport| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total.max(viewport.height())), Sense::hover());
                let resp = ui.interact(rect, id, Sense::click_and_drag());
                let origin = rect.min;
                let x0 = rect.min.x + ((rect.width() - wrap) / 2.0).max(0.0);
                if request_focus {
                    resp.request_focus();
                }
                if resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::Text);
                }
                // Screen readers: a multi-line text input named after the note,
                // whose value is the caret's line.
                {
                    let doc = ws.active_doc().expect("checked");
                    let buf = doc.buffer();
                    let head = doc.selection().head;
                    let line_no = buf.char_to_line(head);
                    let line_text = buf.line(line_no);
                    let col = head - buf.line_to_char(line_no);
                    let title = doc.title();
                    resp.widget_info(|| egui::WidgetInfo::text_edit(true, &line_text, &line_text, ""));
                    ctx.accesskit_node_builder(id, |node| {
                        node.set_role(egui::accesskit::Role::MultilineTextInput);
                        node.set_label(format!("{title}, line {}", line_no + 1));
                        node.set_value(line_text.trim_end_matches('\n').to_string());
                        node.set_description(format!("Line {} of {}, column {}", line_no + 1, buf.len_lines(), col + 1));
                    });
                }
                let focused = resp.has_focus();
                let mut sugg = if focused { Self::current_suggestions(&mut view, ws) } else { None };
                if focused {
                    // While the suggestion list is open, Escape closes it rather than leaving the editor.
                    let escape = sugg.is_some();
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(id, EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape });
                    });
                }
                let mut moved = false;
                let mut edited = false;

                // ---------------- spelling: right-click a flagged word
                if resp.secondary_clicked() {
                    view.spell_menu = None;
                    if let (Some(p), Some(sp)) = (resp.interact_pointer_pos(), self.spell.clone()) {
                        let doc = ws.active_doc().expect("checked");
                        let at = self.hit(ui, &mut view, doc, &cx, origin, x0, p);
                        let buf = doc.buffer();
                        let line = buf.char_to_line(at);
                        let lr = buf.line_range(line);
                        let col = at - lr.start;
                        // The same words the underlines mark (code, math and front matter excluded).
                        let flagged = self.decos.get(&line).and_then(|ds| {
                            ds.iter().find_map(|d| match *d {
                                Deco::Misspelled { start, end } if col >= start && col <= end => Some((start, end)),
                                _ => None,
                            })
                        });
                        if let Some((a, b)) = flagged {
                            let word = buf.slice(lr.start + a..lr.start + b);
                            view.spell_menu =
                                Some(SpellMenu { range: lr.start + a..lr.start + b, suggestions: sp.suggest(&word, 6), word });
                        }
                    }
                }
                let mut spell_pick: Option<(Range<usize>, String)> = None;
                let menu = view.spell_menu.clone();
                resp.context_menu(|ui| {
                    let Some(m) = &menu else {
                        ui.close();
                        return;
                    };
                    if m.suggestions.is_empty() {
                        ui.label(RichText::new("No suggestions").color(pal.muted));
                    }
                    for s in &m.suggestions {
                        if ui.button(RichText::new(s).strong()).clicked() {
                            spell_pick = Some((m.range.clone(), s.clone()));
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button(format!("Add “{}” to dictionary", m.word)).clicked() {
                        crate::spell_ui::add_word(&m.word);
                        ui.close();
                    }
                    if ui.button("Ignore this session").clicked() {
                        crate::spell_ui::ignore_word(&m.word);
                        ui.close();
                    }
                });
                if let Some((range, with)) = spell_pick
                    && let Some(d) = ws.active_doc_mut()
                    && view
                        .spell_menu
                        .as_ref()
                        .is_some_and(|m| range.end <= d.buffer().len_chars() && d.buffer().slice(range.clone()) == m.word)
                {
                    let caret = range.start + with.chars().count();
                    let _ = d.apply(
                        Transaction::replace(range.start, range.end, with).with_selection(Selection::cursor(caret)),
                        EditKind::Other,
                    );
                    view.spell_menu = None;
                }

                // ---------------- mouse
                let (pressed, pointer, mods, primary_down) =
                    ui.input(|i| (i.pointer.primary_pressed(), i.pointer.interact_pos(), i.modifiers, i.pointer.primary_down()));
                let over_view = pointer.is_some_and(|p| view.view_rects.iter().any(|r| r.contains(p)));
                if over_view && !primary_down {
                    ctx.set_cursor_icon(CursorIcon::Default);
                }
                if let Some(p) = pointer.filter(|_| !over_view || view.drag.is_some()) {
                    let doc = ws.active_doc().expect("checked");
                    let fold_hit = view.fold_toggles.iter().find(|(r, _)| r.contains(p)).map(|(_, l)| *l);
                    let task_hit = view.task_boxes.iter().find(|(r, _, _)| r.contains(p)).copied();
                    let figure_hit = view.figure_boxes.iter().find(|(r, _, _)| r.contains(p)).map(|(_, a, b)| (*a, *b));
                    if figure_hit.is_some() && !primary_down {
                        ctx.set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if pressed
                        && resp.contains_pointer()
                        && !over_view
                        && let Some((first, last)) = figure_hit
                    {
                        // Clicking a picture opens its source for editing: the first line
                        // inside the block, or the line itself for `$$x$$`.
                        resp.request_focus();
                        let line = if first == last { first } else { first + 1 };
                        let at = doc.buffer().line_to_char(line.min(doc.buffer().len_lines().saturating_sub(1)));
                        Self::set_selections(&mut view, ws, vec![Selection::cursor(at)], at);
                        moved = true;
                    } else if pressed
                        && resp.contains_pointer()
                        && let Some(line) = fold_hit
                    {
                        resp.request_focus();
                        Self::toggle_fold(&mut view, doc, line, cx.row_h());
                        // Keep the caret out of what was just folded.
                        let caret_line = doc.buffer().char_to_line(doc.selection().head);
                        if Self::is_hidden(&mut view, doc, caret_line) {
                            let at = doc.buffer().line_to_char(line);
                            Self::set_selections(&mut view, ws, vec![Selection::cursor(at)], at);
                        }
                        moved = true;
                    } else if pressed && resp.contains_pointer() && !over_view && !mods.shift && !mods.alt && task_hit.is_some() {
                        resp.request_focus();
                        if let Some((_, at, done)) = task_hit
                            && let Some(d) = ws.active_doc_mut()
                        {
                            let sel = d.selection();
                            let tx = Transaction::replace(at, at + 1, if done { " " } else { "x" }).with_selection(sel);
                            let _ = d.apply(tx, EditKind::Other);
                        }
                        edited = true;
                    } else if pressed && resp.contains_pointer() && !over_view {
                        resp.request_focus();
                        let at = self.hit(ui, &mut view, doc, &cx, origin, x0, p);
                        let now = Instant::now();
                        let count = match view.last_click {
                            Some((t, lp, c)) if now.duration_since(t) < Duration::from_millis(420) && lp.distance(p) < 6.0 => c % 3 + 1,
                            _ => 1,
                        };
                        view.last_click = Some((now, p, count));
                        let buf = doc.buffer();
                        let cur = doc.selection();
                        let extra = view.extra.clone();
                        let (sels, mode): (Vec<Selection>, DragMode) = match count {
                            2 => {
                                let w = motion::word_at(buf, at);
                                (vec![Selection::new(w.start, w.end)], DragMode::Word(w.start, w.end))
                            }
                            3 => {
                                let l = motion::line_with_break(buf, at);
                                (vec![Selection::new(l.start, l.end)], DragMode::Line(l.start, l.end))
                            }
                            _ if mods.alt => {
                                let mut v = extra;
                                v.push(cur);
                                v.push(Selection::cursor(at));
                                (v, DragMode::Char(at))
                            }
                            _ if mods.shift => (vec![Selection::new(cur.anchor, at)], DragMode::Char(cur.anchor)),
                            _ => (vec![Selection::cursor(at)], DragMode::Char(at)),
                        };
                        view.drag = Some(mode);
                        view.preferred_x = None;
                        Self::set_selections(&mut view, ws, sels, at);
                        moved = true;
                        if mods.command && count == 1 && ws.follow_link_at_caret() {
                            self.request_focus = true;
                            self.scroll_to_caret = true;
                        }
                    } else if primary_down && view.drag.is_some() && resp.dragged() {
                        let at = self.hit(ui, &mut view, doc, &cx, origin, x0, p);
                        let buf = doc.buffer();
                        let sel = match view.drag.expect("checked") {
                            DragMode::Char(a) => Selection::new(a, at),
                            DragMode::Word(s, e) => {
                                let w = motion::word_at(buf, at);
                                if at < s { Selection::new(e, w.start) } else { Selection::new(s, w.end.max(e)) }
                            }
                            DragMode::Line(s, e) => {
                                let l = motion::line_with_break(buf, at);
                                if at < s { Selection::new(e, l.start) } else { Selection::new(s, l.end.max(e)) }
                            }
                        };
                        if let Some(d) = ws.active_doc_mut() {
                            d.set_selection(sel);
                        }
                        view.extra.clear();
                        // Auto-scroll while dragging past the edges.
                        let vis = viewport.translate(origin.to_vec2());
                        if p.y < vis.top() + 8.0 || p.y > vis.bottom() - 8.0 {
                            ui.scroll_to_rect(Rect::from_center_size(p, Vec2::splat(24.0)), None);
                        }
                        moved = true;
                    }
                }
                if !primary_down {
                    view.drag = None;
                }

                // ---------------- keyboard & text
                if focused {
                    let events = ui.input(|i| i.events.clone());
                    let mac = cfg!(target_os = "macos");
                    for ev in events {
                        let Some(doc) = ws.active_doc() else { break };
                        let rev_before = doc.revision();
                        // The suggestion list takes ↑ ↓ Enter Tab and Escape.
                        if let (Some(s), Event::Key { key, pressed: true, modifiers, .. }) = (&sugg, &ev)
                            && modifiers.is_none()
                            && matches!(key, Key::ArrowUp | Key::ArrowDown | Key::Enter | Key::Tab | Key::Escape)
                        {
                            let n = s.items.len();
                            match key {
                                Key::ArrowDown => view.suggest_pick = (view.suggest_pick + 1) % n,
                                Key::ArrowUp => view.suggest_pick = (view.suggest_pick + n - 1) % n,
                                Key::Escape => view.suggest_dismissed = Some((s.context.start, s.context.kind)),
                                _ => edited |= ws.accept_suggestion(s, view.suggest_pick),
                            }
                            sugg = Self::current_suggestions(&mut view, ws);
                            moved = true;
                            if let Some(doc) = ws.active_doc()
                                && doc.revision() != rev_before
                            {
                                self.galleys.clear();
                                self.decos.clear();
                                self.tables.clear();
                                self.decos.clear();
                                self.tables.clear();
                                Self::sync(&mut view, doc, &cx, inline_images);
                            }
                            continue;
                        }
                        match ev {
                            Event::Text(t) => {
                                if t == " " && view.extra.is_empty() && ws.smart_space() {
                                    edited = true;
                                    continue;
                                }
                                let single = t.chars().count() == 1;
                                edited |=
                                    Self::apply_multi(&mut view, ws, if single { EditKind::Insert } else { EditKind::Other }, |_, s| {
                                        Some(multi::insert(s, &t))
                                    });
                            }
                            Event::Ime(ImeEvent::Preedit { text, .. }) => {
                                view.preedit = (!text.is_empty()).then_some(text);
                            }
                            Event::Ime(ImeEvent::Commit(t)) => {
                                view.preedit = None;
                                edited |= Self::apply_multi(&mut view, ws, EditKind::Other, |_, s| Some(multi::insert(s, &t)));
                            }
                            Event::Copy | Event::Cut => {
                                let all = all_selections(doc.selection(), &view.extra);
                                let whole_lines = all.iter().all(Selection::is_empty);
                                let text = if whole_lines {
                                    // VS Code style: copy/cut the current line(s) when nothing is selected.
                                    all.iter()
                                        .map(|s| doc.buffer().slice(motion::line_with_break(doc.buffer(), s.head)))
                                        .collect::<String>()
                                } else {
                                    Self::selected_text(doc, &view.extra)
                                };
                                ctx.copy_text(text);
                                if matches!(ev, Event::Cut) {
                                    edited |= Self::apply_multi(&mut view, ws, EditKind::Other, |b, s| {
                                        let ranges: Vec<Selection> = if whole_lines {
                                            s.iter()
                                                .map(|x| {
                                                    let l = motion::line_with_break(b, x.head);
                                                    Selection::new(l.start, l.end)
                                                })
                                                .collect()
                                        } else {
                                            s.to_vec()
                                        };
                                        Some(multi::insert(&ranges, ""))
                                    });
                                }
                            }
                            Event::Paste(text) => {
                                let all = all_selections(doc.selection(), &view.extra);
                                let is_url = text.starts_with("http://") || text.starts_with("https://");
                                if is_url && all.len() == 1 && !all[0].is_empty() && !text.contains(char::is_whitespace) {
                                    // Paste a URL over a selection → Markdown link.
                                    let label = doc.buffer().slice(all[0].range());
                                    let link = format!("[{label}]({text})");
                                    edited |= Self::apply_multi(&mut view, ws, EditKind::Other, |_, s| Some(multi::insert(s, &link)));
                                } else {
                                    let lines: Vec<String> = text.lines().map(str::to_string).collect();
                                    let texts = if all.len() > 1 && lines.len() == all.len() { lines } else { vec![text.clone()] };
                                    edited |=
                                        Self::apply_multi(&mut view, ws, EditKind::Other, |_, s| Some(multi::replace_each(s, &texts)));
                                }
                            }
                            Event::PasteImage(img) => {
                                let rgba: Vec<u8> = img.pixels.iter().flat_map(Color32::to_srgba_unmultiplied).collect();
                                edited |= ws.paste_image(img.size[0] as u32, img.size[1] as u32, &rgba).is_some();
                            }
                            Event::Key { key, pressed: true, modifiers: m, .. } => {
                                let word = if mac { m.alt } else { m.ctrl };
                                let line_mod = mac && m.mac_cmd;
                                match key {
                                    Key::ArrowLeft | Key::ArrowRight => {
                                        let right = key == Key::ArrowRight;
                                        let buf = doc.buffer();
                                        let all = all_selections(doc.selection(), &view.extra);
                                        let new: Vec<Selection> = all
                                            .iter()
                                            .map(|s| {
                                                if !s.is_empty() && !m.shift && !word && !line_mod {
                                                    return Selection::cursor(if right { s.end() } else { s.start() });
                                                }
                                                let h = s.head;
                                                let to = match (right, word, line_mod) {
                                                    (_, _, true) if right => motion::line_end(buf, h),
                                                    (_, _, true) => motion::smart_home(buf, h),
                                                    (true, true, _) => motion::next_word_end(buf, h),
                                                    (false, true, _) => motion::prev_word_start(buf, h),
                                                    (true, false, _) => (h + 1).min(buf.len_chars()),
                                                    (false, false, _) => h.saturating_sub(1),
                                                };
                                                if m.shift { Selection::new(s.anchor, to) } else { Selection::cursor(to) }
                                            })
                                            .collect();
                                        let primary_head = new[primary_index(&all, doc.selection())].head;
                                        Self::set_selections(&mut view, ws, new, primary_head);
                                        view.preferred_x = None;
                                        moved = true;
                                    }
                                    Key::ArrowUp | Key::ArrowDown => {
                                        let dir = if key == Key::ArrowUp { -1 } else { 1 };
                                        if line_mod || (!mac && m.ctrl) {
                                            // Ctrl+Up/Down scrolls; ⌘+Up/Down jumps to the ends.
                                            if line_mod {
                                                let to = if dir < 0 { 0 } else { doc.buffer().len_chars() };
                                                let a = if m.shift { doc.selection().anchor } else { to };
                                                Self::set_selections(&mut view, ws, vec![Selection::new(a, to)], to);
                                                moved = true;
                                            } else {
                                                ui.scroll_with_delta(Vec2::new(0.0, -dir as f32 * cx.row_h()));
                                            }
                                            continue;
                                        }
                                        let all = all_selections(doc.selection(), &view.extra);
                                        let px = view.preferred_x;
                                        let mut new = Vec::with_capacity(all.len());
                                        let mut px_out = px;
                                        for s in &all {
                                            let (to, x) =
                                                self.vertical(ui, &mut view, doc, &cx, s.head, dir, if all.len() == 1 { px } else { None });
                                            if s.head == doc.selection().head {
                                                px_out = Some(x);
                                            }
                                            new.push(if m.shift { Selection::new(s.anchor, to) } else { Selection::cursor(to) });
                                        }
                                        let ph = {
                                            let p = doc.selection();
                                            all.iter().position(|s| s.head == p.head).map_or(0, |i| new[i].head)
                                        };
                                        Self::set_selections(&mut view, ws, new, ph);
                                        view.preferred_x = px_out;
                                        moved = true;
                                    }
                                    Key::Home | Key::End => {
                                        let buf = doc.buffer();
                                        let all = all_selections(doc.selection(), &view.extra);
                                        let doc_end = buf.len_chars();
                                        let new: Vec<Selection> = all
                                            .iter()
                                            .map(|s| {
                                                let to = match (key == Key::End, m.ctrl || m.mac_cmd) {
                                                    (false, true) => 0,
                                                    (true, true) => doc_end,
                                                    (false, false) => motion::smart_home(buf, s.head),
                                                    (true, false) => motion::line_end(buf, s.head),
                                                };
                                                if m.shift { Selection::new(s.anchor, to) } else { Selection::cursor(to) }
                                            })
                                            .collect();
                                        let ph = new[primary_index(&all, doc.selection())].head;
                                        Self::set_selections(&mut view, ws, new, ph);
                                        view.preferred_x = None;
                                        moved = true;
                                    }
                                    Key::PageUp | Key::PageDown => {
                                        let page = viewport.height() * 0.9 * if key == Key::PageUp { -1.0 } else { 1.0 };
                                        let sel = doc.selection();
                                        let caret = self.caret_rect(ui, &mut view, doc, &cx, origin, x0, sel.head);
                                        let target = Pos2::new(caret.left(), caret.center().y + page);
                                        let to = self.hit(ui, &mut view, doc, &cx, origin, x0, target);
                                        let a = if m.shift { sel.anchor } else { to };
                                        Self::set_selections(&mut view, ws, vec![Selection::new(a, to)], to);
                                        ui.scroll_with_delta(Vec2::new(0.0, -page));
                                        moved = true;
                                    }
                                    Key::Backspace => {
                                        edited |=
                                            Self::apply_multi(&mut view, ws, EditKind::Delete, |b, s| multi::delete_backward(b, s, word));
                                    }
                                    Key::Delete => {
                                        edited |=
                                            Self::apply_multi(&mut view, ws, EditKind::Delete, |b, s| multi::delete_forward(b, s, word));
                                    }
                                    Key::Enter if m.is_none() || m.shift_only() => {
                                        if m.is_none() && view.extra.is_empty() && ws.smart_enter() {
                                            edited = true;
                                        } else {
                                            edited |=
                                                Self::apply_multi(&mut view, ws, EditKind::Other, |_, s| Some(multi::insert(s, "\n")));
                                        }
                                    }
                                    Key::Tab if m.is_none() => {
                                        if view.extra.is_empty() && ws.smart_tab(false) {
                                            edited = true;
                                        } else {
                                            edited |=
                                                Self::apply_multi(&mut view, ws, EditKind::Other, |_, s| Some(multi::insert(s, &indent)));
                                        }
                                    }
                                    Key::Tab if m.shift_only() => {
                                        edited |= ws.smart_tab(true);
                                    }
                                    Key::Escape if !view.extra.is_empty() => {
                                        view.extra.clear();
                                        moved = true;
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                        // Keep geometry in step with each edit so later events in
                        // the same frame (e.g. typing then ↓) see the new text.
                        if let Some(doc) = ws.active_doc()
                            && doc.revision() != rev_before
                        {
                            self.galleys.clear();
                            self.decos.clear();
                            self.tables.clear();
                            Self::sync(&mut view, doc, &cx, inline_images);
                            sugg = Self::current_suggestions(&mut view, ws);
                        }
                    }
                }

                let Some(doc) = ws.active_doc() else { return };
                // The caret never hides inside a fold: going there opens it.
                if moved || edited || scroll_to_caret {
                    let line = doc.buffer().char_to_line(doc.selection().head);
                    Self::reveal_line(&mut view, doc, line, cx.row_h());
                }
                if edited {
                    Self::sync(&mut view, doc, &cx, inline_images);
                    view.preferred_x = None;
                }
                if edited || moved {
                    view.blink_start = Instant::now();
                }

                // ---------------- paint visible lines
                let buf = doc.buffer();
                let vis_top = (viewport.min.y - TOP_MARGIN).max(0.0);
                let vis_bot = viewport.max.y - TOP_MARGIN;
                let first = view.heights.line_at(vis_top);
                let last = view.heights.line_at(vis_bot.max(0.0)).min(buf.len_lines().saturating_sub(1));
                // Make sure the highlight window covers what's on screen.
                let _ = Self::runs_for(&mut view, doc, first..last + 1);

                let view_blocks = match &view.view_blocks {
                    Some((r, b)) if *r == doc.revision() => Rc::clone(b),
                    _ => {
                        let b = Rc::new(if doc.large { Vec::new() } else { qviews::find_blocks(&doc.text()) });
                        view.view_blocks = Some((doc.revision(), Rc::clone(&b)));
                        b
                    }
                };
                let figures = Self::figures_of(&mut view, doc);
                // In live preview a figure shows as its picture unless the caret is in
                // it; while it's being edited (or with live preview off) it's source only,
                // so nothing is rendered per keystroke.
                let figure_at = |line: usize| -> Option<(blocks::Figure, bool)> {
                    if !crate::typeset_ui::AVAILABLE {
                        return None;
                    }
                    let f = figures.iter().find(|f| f.first_line <= line && line <= f.last_line)?;
                    let revealed = cx.live.as_ref().is_none_or(|rv| rv.iter().any(|r| r.start <= f.last_line && f.first_line < r.end));
                    (!revealed).then(|| (f.clone(), true))
                };
                let mut figure_clicks: Vec<(Rect, usize, usize)> = Vec::new();
                let preedit_chars = view.preedit.as_ref().map_or(0, |p| p.chars().count());
                let mut new_view_rects = Vec::new();
                let all = all_selections(doc.selection(), &view.extra);
                let primary = doc.selection();
                let painter = ui.painter_at(viewport.translate(origin.to_vec2()).expand(2.0));
                let sel_color = pal.selection;
                let find_color = pal.mark_bg.gamma_multiply(0.7);
                let find_current_color = pal.mark_bg;
                let mut anchor_shift = 0.0;
                let mut caret_rect_primary = None;
                let mut task_boxes = Vec::new();
                let now = Instant::now();
                // Only blink while the window has focus; in the background the
                // caret stays still and the app stops repainting.
                let window_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
                let blink_on = focused
                    && (!window_focused || (now.duration_since(view.blink_start).as_millis() / BLINK.as_millis()).is_multiple_of(2));

                let hidden = Self::hidden(&mut view, doc);
                let headings = if doc.large { Vec::new() } else { heading_lines(doc) };
                let mut fold_toggles = Vec::new();
                let pointer = ui.input(|i| i.pointer.hover_pos());
                for line in first..=last {
                    if hidden.iter().any(|r| r.contains(&line)) {
                        view.heights.set(line, 0.0);
                        continue;
                    }
                    // A figure shown as a picture: its source lines collapse into the last one.
                    let figure = figure_at(line).map(|(f, collapsed)| {
                        let job = match f.kind {
                            blocks::FigureKind::Math => crate::typeset_ui::Job::Math {
                                latex: f.source.clone(),
                                display: true,
                                size: (cx.size * 1.1).round() as u32,
                                color: crate::typeset_ui::rgba(pal.text),
                            },
                            blocks::FigureKind::Diagram => crate::typeset_ui::Job::Diagram { source: f.source.clone(), dark: pal.dark },
                        };
                        let out = crate::typeset_ui::request(&ctx, &job);
                        (f, collapsed, out)
                    });
                    let collapsed_here = figure.as_ref().is_some_and(|(_, c, out)| *c && !matches!(out, crate::typeset_ui::Out::Failed(_)));
                    if collapsed_here && figure.as_ref().is_some_and(|(f, _, _)| line < f.last_line) {
                        view.heights.set(line, 0.0);
                        continue;
                    }
                    let g = self.galley(ui, &mut view, doc, line, &cx);
                    let top = origin.y + TOP_MARGIN + view.heights.top(line);
                    // Fold arrows beside headings: shown on hover, or while folded.
                    if headings.iter().any(|(l, _)| *l == line) {
                        let folded = view.folded.contains(&line);
                        let row = Rect::from_min_size(Pos2::new(x0 - 40.0, top), Vec2::new(cx.wrap + 40.0, g.size().y));
                        let hovered = pointer.is_some_and(|p| row.contains(p));
                        let first_row = g.rows.first().map_or_else(|| cx.row_h(), |r| r.size.y);
                        let arrow = Rect::from_center_size(Pos2::new(x0 - 26.0, top + first_row / 2.0), Vec2::splat(18.0));
                        if folded || hovered {
                            let over = pointer.is_some_and(|p| arrow.contains(p));
                            if over {
                                painter.rect_filled(arrow, 4.0, pal.hover);
                                ctx.set_cursor_icon(CursorIcon::PointingHand);
                            }
                            let c = arrow.center();
                            let s = 4.5;
                            let pts = if folded {
                                vec![Pos2::new(c.x - s * 0.6, c.y - s), Pos2::new(c.x + s * 0.8, c.y), Pos2::new(c.x - s * 0.6, c.y + s)]
                            } else {
                                vec![Pos2::new(c.x - s, c.y - s * 0.6), Pos2::new(c.x + s, c.y - s * 0.6), Pos2::new(c.x, c.y + s * 0.8)]
                            };
                            let color = if over || folded { pal.muted } else { pal.faint };
                            painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
                            fold_toggles.push((arrow, line));
                        }
                        if folded {
                            // A pill after the heading says there's more inside.
                            let end = g
                                .rows
                                .last()
                                .map_or_else(|| Pos2::new(x0, top), |r| Pos2::new(x0 + r.rect().right(), top + r.rect().center().y));
                            let pill = Rect::from_min_size(Pos2::new(end.x + 10.0, end.y - 9.0), Vec2::new(30.0, 18.0));
                            painter.rect_filled(pill, 9.0, pal.hover);
                            for k in [-1.0, 0.0, 1.0] {
                                painter.circle_filled(pill.center() + Vec2::new(k * 6.0, 0.0), 1.8, pal.muted);
                            }
                            fold_toggles.push((pill, line));
                        }
                    }
                    let lr = buf.line_range(line);
                    let (ls, le) = (lr.start, lr.end);
                    let pos = Pos2::new(x0, top);

                    // Range highlights: find matches, then selections.
                    let paint_range = |s: usize, e: usize, color: Color32, eol: bool| {
                        let (a, b) = (s.max(ls) - ls, e.min(le) - ls);
                        let mut rs = 0;
                        for (k, row) in g.rows.iter().enumerate() {
                            let len = row.glyphs.len();
                            let (ra, rb) = (rs, rs + len);
                            let last_row = k + 1 == g.rows.len();
                            if b >= ra && a <= rb && (a < rb || (a == rb && last_row)) {
                                let xa = if a > ra { row.x_offset(CharIndex(a - ra)) } else { 0.0 };
                                let mut xb = if b < rb { row.x_offset(CharIndex(b - ra)) } else { row.size.x };
                                if last_row && eol {
                                    xb += cx.size * 0.4;
                                }
                                if xb > xa {
                                    let r = Rect::from_min_max(
                                        Pos2::new(pos.x + row.pos.x + xa, pos.y + row.pos.y),
                                        Pos2::new(pos.x + row.pos.x + xb, pos.y + row.pos.y + row.size.y),
                                    );
                                    painter.rect_filled(r, 2.0, color);
                                }
                            }
                            rs += len;
                        }
                    };
                    let mi = find_matches.partition_point(|r| r.end <= ls);
                    for (k, r) in find_matches[mi..].iter().enumerate().take_while(|(_, r)| r.start <= le) {
                        let color = if find_current == Some(mi + k) { find_current_color } else { find_color };
                        paint_range(r.start, r.end, color, false);
                    }
                    for s in all.iter().filter(|s| !s.is_empty() && s.end() >= ls && s.start() <= le) {
                        paint_range(s.start(), s.end(), sel_color, s.end() > le);
                    }

                    // Blocks: one background behind code and front matter, a bar beside quotes.
                    let block = if collapsed_here { Block::None } else { Self::block_of(&mut view, doc, line) };
                    if block != Block::None {
                        let joined_above = line > 0 && Self::block_of(&mut view, doc, line - 1) == block;
                        let joined_below = Self::block_of(&mut view, doc, line + 1) == block;
                        let bottom = if joined_below {
                            origin.y + TOP_MARGIN + view.heights.top(line + 1)
                        } else {
                            top + g.size().y.max(cx.row_h() * 0.9)
                        };
                        let pad = 14.0;
                        if let Block::Callout(k) = block {
                            let kind = &quire_app::markdown::callout::KINDS[k.min(quire_app::markdown::callout::KINDS.len() - 1)];
                            let color = callout_color(kind);
                            let extra_top = if joined_above { 0.0 } else { 4.0 };
                            let extra_bottom = if joined_below { 0.0 } else { 4.0 };
                            let bg = Rect::from_min_max(
                                Pos2::new(x0 - pad, top - extra_top),
                                Pos2::new(x0 + cx.wrap + pad, bottom + extra_bottom),
                            );
                            let c = |joined: bool| if joined { 0 } else { 6 };
                            let r =
                                egui::CornerRadius { nw: c(joined_above), ne: c(joined_above), sw: c(joined_below), se: c(joined_below) };
                            painter.rect_filled(bg, r, color.gamma_multiply(if pal.dark { 0.16 } else { 0.10 }));
                            let bar = Rect::from_min_max(bg.left_top(), Pos2::new(bg.left() + 3.0, bg.bottom()));
                            let rb = egui::CornerRadius { nw: c(joined_above), ne: 0, sw: c(joined_below), se: 0 };
                            painter.rect_filled(bar, rb, color);
                        } else if block == Block::Quote {
                            let bar = Rect::from_min_max(Pos2::new(x0 - pad, top), Pos2::new(x0 - pad + 3.0, bottom));
                            let r = egui::CornerRadius {
                                nw: if joined_above { 0 } else { 2 },
                                ne: if joined_above { 0 } else { 2 },
                                sw: if joined_below { 0 } else { 2 },
                                se: if joined_below { 0 } else { 2 },
                            };
                            painter.rect_filled(bar, r, pal.faint);
                        } else {
                            let extra_top = if joined_above { 0.0 } else { 4.0 };
                            let extra_bottom = if joined_below { 0.0 } else { 4.0 };
                            let bg = Rect::from_min_max(
                                Pos2::new(x0 - pad, top - extra_top),
                                Pos2::new(x0 + cx.wrap + pad, bottom + extra_bottom),
                            );
                            let c = |joined: bool| if joined { 0 } else { 8 };
                            let r =
                                egui::CornerRadius { nw: c(joined_above), ne: c(joined_above), sw: c(joined_below), se: c(joined_below) };
                            let fill = if block == Block::FrontMatter { pal.code_bg.gamma_multiply(0.6) } else { pal.code_bg };
                            painter.rect_filled(bg, r, fill);
                        }
                    }

                    if !collapsed_here {
                        painter.galley(pos, Arc::clone(&g), pal.text);
                    }
                    // Live preview: bullets, checkboxes and rules drawn over hidden syntax.
                    let no_decos: &[Deco] = &[];
                    for d in if collapsed_here { no_decos } else { self.decos.get(&line).map(Vec::as_slice).unwrap_or_default() } {
                        let at = |col: usize| g.pos_from_cursor(CCursor::new(col)).translate(pos.to_vec2());
                        match *d {
                            Deco::Math { col, tex, w, h, baseline } => {
                                let a = at(col);
                                // Sit the formula's baseline on the text's.
                                let text_baseline = a.center().y + cx.size * 0.33;
                                let r = Rect::from_min_size(Pos2::new(a.left() - w - 1.0, text_baseline - baseline), Vec2::new(w, h));
                                painter.image(tex, r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                            }
                            Deco::Misspelled { start, end } => {
                                // Per row, so a word wrapped onto the next row is underlined there.
                                let mut rs = 0;
                                for row in &g.rows {
                                    let len = row.glyphs.len();
                                    let (ra, rb) = (rs, rs + len);
                                    rs += len;
                                    if end <= ra || start >= rb {
                                        continue;
                                    }
                                    let xa = row.x_offset(CharIndex(start.max(ra) - ra));
                                    let xb = row.x_offset(CharIndex(end.min(rb) - ra));
                                    let (x_a, x_b) = (pos.x + row.pos.x + xa, pos.x + row.pos.x + xb);
                                    let y = pos.y + row.pos.y + row.size.y - 3.0;
                                    let steps = ((x_b - x_a) / 2.5).floor().max(0.0) as usize;
                                    let pts: Vec<Pos2> = (0..=steps)
                                        .map(|k| Pos2::new(x_a + k as f32 * 2.5, if k % 2 == 1 { y - 1.6 } else { y + 0.4 }))
                                        .collect();
                                    if pts.len() > 1 {
                                        painter.add(egui::Shape::line(pts, Stroke::new(1.1, pal.danger)));
                                    }
                                }
                            }
                            Deco::Callout { kind, col, label } => {
                                let kind = &quire_app::markdown::callout::KINDS[kind.min(quire_app::markdown::callout::KINDS.len() - 1)];
                                let color = callout_color(kind);
                                let a = at(col);
                                let d = (cx.size * 1.05).round();
                                let left = if label { a.left() } else { a.left() - callout_icon_width(cx.size) };
                                let center = Pos2::new(left + d / 2.0, a.center().y);
                                painter.circle_filled(center, d / 2.0, color);
                                painter.text(
                                    center,
                                    egui::Align2::CENTER_CENTER,
                                    kind.glyph,
                                    egui::FontId::new((cx.size * 0.62).round(), egui::FontFamily::Name(crate::theme::BOLD.into())),
                                    Color32::WHITE,
                                );
                                if label {
                                    painter.text(
                                        Pos2::new(left + callout_icon_width(cx.size), a.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        kind.label,
                                        egui::FontId::new(cx.size, egui::FontFamily::Name(crate::theme::BOLD.into())),
                                        color,
                                    );
                                }
                            }
                            Deco::Bullet(col) => {
                                let (a, b) = (at(col), at(col + 1));
                                let c = Pos2::new(f32::midpoint(a.left(), b.left()), a.center().y);
                                painter.circle_filled(c, (cx.size * 0.16).max(2.5), pal.muted);
                            }
                            Deco::Task { col, done } => {
                                let (a, b) = (at(col), at(col + 3));
                                let side = (cx.size * 0.95).round();
                                let center = Pos2::new(f32::midpoint(a.left(), b.left()), a.center().y);
                                let r = Rect::from_center_size(center, Vec2::splat(side));
                                if done {
                                    painter.rect_filled(r, 4.0, pal.accent);
                                    let s = side;
                                    let pts = [
                                        Pos2::new(r.left() + s * 0.26, r.top() + s * 0.53),
                                        Pos2::new(r.left() + s * 0.43, r.top() + s * 0.70),
                                        Pos2::new(r.left() + s * 0.75, r.top() + s * 0.33),
                                    ];
                                    let st = Stroke::new(1.8, if pal.dark { pal.bg } else { Color32::WHITE });
                                    painter.line_segment([pts[0], pts[1]], st);
                                    painter.line_segment([pts[1], pts[2]], st);
                                } else {
                                    let hovered = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| r.contains(p));
                                    painter.rect_stroke(
                                        r,
                                        4.0,
                                        Stroke::new(1.5, if hovered { pal.accent } else { pal.muted }),
                                        egui::StrokeKind::Inside,
                                    );
                                    if hovered {
                                        ctx.set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                }
                                task_boxes.push((r.expand(3.0), ls + col + 1, done));
                            }
                            Deco::Rule => {
                                let y = at(0).center().y;
                                painter.line_segment([Pos2::new(x0, y), Pos2::new(x0 + cx.wrap, y)], Stroke::new(1.0, pal.border));
                            }
                            Deco::TableLine { width, strong } => {
                                // The separator row is a line; body rows get a hairline below.
                                let y = if strong { top + g.size().y / 2.0 } else { top + g.size().y };
                                let c = if strong { pal.faint } else { pal.border };
                                painter.line_segment([Pos2::new(x0, y), Pos2::new(x0 + width, y)], Stroke::new(1.0, c));
                            }
                        }
                    }
                    // A table's `| --- |` row shrinks to the line drawn in it.
                    let thin = self.decos.get(&line).is_some_and(|d| d.iter().any(|d| matches!(d, Deco::TableLine { strong: true, .. })));
                    let mut h = if thin { g.size().y } else { g.size().y.max(cx.row_h() * 0.9) };
                    // The figure's picture (or why it couldn't be drawn), under its last line.
                    if let Some((f, _, out)) = &figure
                        && line == f.last_line
                    {
                        let base = if collapsed_here { top } else { top + h };
                        match out {
                            crate::typeset_ui::Out::Ready(p) => {
                                let scale = (cx.wrap / p.size.x).min(1.0);
                                let size = p.size * scale;
                                let x = if f.kind == blocks::FigureKind::Math { x0 + (cx.wrap - size.x) / 2.0 } else { x0 };
                                let r = Rect::from_min_size(Pos2::new(x, base + 8.0), size);
                                painter.image(p.texture.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                                figure_clicks.push((r, f.first_line, f.last_line));
                                h = if collapsed_here { size.y + 16.0 } else { h + size.y + 16.0 };
                            }
                            crate::typeset_ui::Out::Pending => {
                                if collapsed_here {
                                    painter.text(
                                        Pos2::new(x0, base + cx.row_h() / 2.0),
                                        egui::Align2::LEFT_CENTER,
                                        if f.kind == blocks::FigureKind::Math { "Rendering math…" } else { "Drawing diagram…" },
                                        egui::FontId::proportional(cx.size * 0.85),
                                        pal.faint,
                                    );
                                    h = cx.row_h() * 1.2;
                                }
                            }
                            crate::typeset_ui::Out::Failed(e) => {
                                let what = if f.kind == blocks::FigureKind::Math { "Math" } else { "Diagram" };
                                painter.text(
                                    Pos2::new(x0, top + h + 4.0),
                                    egui::Align2::LEFT_TOP,
                                    format!("{what} not drawn: {e}"),
                                    egui::FontId::proportional(cx.size * 0.8),
                                    pal.danger,
                                );
                                h += cx.size * 1.4;
                            }
                        }
                    }

                    if let Some(uri) = Self::line_image(ws, doc, line)
                        && let Some(size) = Self::image_size(ui, &uri, cx.wrap)
                    {
                        let r = Rect::from_min_size(Pos2::new(x0, top + h + 4.0), size);
                        egui::Image::new(uri).corner_radius(4.0).paint_at(ui, r);
                        h += size.y + 10.0;
                    }

                    if let Some(vb) = view_blocks.iter().find(|b| b.close_line == line) {
                        let key = Id::unique(("quire-view", doc_id.0, vb.open_line));
                        let prev_h = ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(80.0);
                        let r = Rect::from_min_size(Pos2::new(x0, top + h + 6.0), Vec2::new(cx.wrap, prev_h));
                        // Cover the view so clicks on it don't move the caret.
                        let _ = ui.interact(r, key.with("cover"), Sense::click());
                        let mut child = ui.new_child(
                            UiBuilder::new()
                                .id_salt(key)
                                .max_rect(Rect::from_min_size(r.min, Vec2::new(cx.wrap, 4000.0)))
                                .layout(egui::Layout::top_down(Align::Min)),
                        );
                        if let Some(a) = view_ui::show(&mut child, ws, pal, &vb.spec, doc.path.as_ref(), key) {
                            view_action = Some(a);
                        }
                        let used = child.min_rect().height();
                        new_view_rects.push(Rect::from_min_size(r.min, Vec2::new(cx.wrap, used)));
                        ui.data_mut(|d| d.insert_temp(key, used));
                        if (used - prev_h).abs() > 0.5 {
                            ctx.request_repaint();
                        }
                        h += used + 14.0;
                    }

                    let d = view.heights.set(line, h);
                    if line < first + 1 && top < viewport.min.y + origin.y {
                        anchor_shift += d;
                    }

                    // Carets.
                    for s in all.iter().filter(|s| s.head >= ls && s.head <= le) {
                        let is_primary_head = s.head == primary.head;
                        let col = s.head - ls + if is_primary_head { preedit_chars } else { 0 };
                        let r = g.pos_from_cursor(CCursor::new(col)).translate(pos.to_vec2());
                        let is_primary = s.head == primary.head;
                        if is_primary {
                            caret_rect_primary = Some(r);
                        }
                        if blink_on {
                            let c = if is_primary { pal.accent } else { pal.accent.gamma_multiply(0.7) };
                            painter.line_segment([Pos2::new(r.left(), r.top()), Pos2::new(r.left(), r.bottom())], Stroke::new(2.0, c));
                        }
                    }
                }

                view.view_rects = new_view_rects;
                view.task_boxes = task_boxes;
                view.figure_boxes = figure_clicks;
                view.fold_toggles = fold_toggles;
                // Empty new note: say how it gets its name.
                if doc.auto_name && buf.len_chars() == 0 && view.preedit.is_none() {
                    painter.text(
                        Pos2::new(x0 + 4.0, origin.y + TOP_MARGIN),
                        egui::Align2::LEFT_TOP,
                        "Start with a title — the first line names the note",
                        egui::FontId::proportional(cx.size),
                        pal.faint,
                    );
                }
                if focused {
                    let cr = caret_rect_primary.unwrap_or_else(|| Rect::from_min_size(origin, Vec2::new(2.0, cx.row_h())));
                    ctx.output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput {
                            purpose: egui::viewport::IMEPurpose::default(),
                            rect: viewport.translate(origin.to_vec2()),
                            cursor_rect: cr,
                            should_interrupt_composition: false,
                        });
                    });
                    if window_focused {
                        let phase = now.duration_since(view.blink_start).as_millis() % BLINK.as_millis();
                        ctx.request_repaint_after(Duration::from_millis((BLINK.as_millis() - phase) as u64 + 5));
                    }
                    if let (Some(s), Some(caret)) = (&sugg, caret_rect_primary) {
                        suggesting = true;
                        if let Some(i) = Self::suggestion_popup(&ctx, id, caret, s, view.suggest_pick, pal) {
                            clicked_suggestion = Some((s.clone(), i));
                        }
                    }
                }

                if anchor_shift.abs() > 0.5 {
                    ui.scroll_with_delta(Vec2::new(0.0, -anchor_shift));
                }
                // Keep the caret visible after keyboard work; centre it for jumps.
                if scroll_to_caret || moved || edited {
                    let r = match caret_rect_primary {
                        Some(r) => r,
                        None => self.caret_rect(ui, &mut view, doc, &cx, origin, x0, doc.selection().head),
                    };
                    let align = (scroll_to_caret && caret_rect_primary.is_none()).then_some(Align::Center);
                    ui.scroll_to_rect(r.expand2(Vec2::new(0.0, cx.row_h())), align);
                    if scroll_to_caret && caret_rect_primary.is_none() {
                        // Heights near the target were estimates; settle next frame.
                        self.scroll_to_caret = true;
                    }
                }
                // Measurements changed the total: lay out again next frame.
                if (view.heights.total() + TOP_MARGIN + BOTTOM_MARGIN - total).abs() > 0.5 {
                    ctx.request_repaint();
                }
            },
        );
        self.views.insert(doc_id, view);
        if primary || suggesting {
            self.suggesting = suggesting;
        }
        if let Some((s, i)) = clicked_suggestion
            && ws.accept_suggestion(&s, i)
        {
            self.request_focus = true;
            ctx.request_repaint();
        }
        match view_action {
            Some(ViewAction::Open(p)) => {
                if ws.open_note(&p).is_some() {
                    self.request_focus = true;
                }
            }
            Some(ViewAction::Move { path, field, value })
                if ws.set_note_property(&path, &field, &quire_app::markdown::Value::Str(value.clone())) =>
            {
                ws.notify(quire_app::workspace::NoticeLevel::Info, format!("{} → {field}: {value}", path.stem()));
            }
            Some(ViewAction::Edit { path, field, text }) => {
                // Keep lists as lists.
                let was_list = ws
                    .index()
                    .read()
                    .get(&path)
                    .and_then(|r| {
                        r.properties
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(&field))
                            .map(|(_, v)| matches!(v, quire_app::markdown::Value::List(_)))
                    })
                    .unwrap_or(false);
                let value = quire_app::properties::parse_input(&text, was_list);
                if !ws.set_note_property(&path, &field, &value) {
                    ws.notify(quire_app::workspace::NoticeLevel::Warn, format!("Couldn't change {field} in “{}”", path.stem()));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_preview_hides_syntax_but_keeps_structure() {
        for (m, start) in [("**", false), ("*", false), ("# ", true), ("[[", false), ("]](x)", false), ("`", false), ("==", false)] {
            assert!(hideable_marker(m, start), "{m:?} should hide");
        }
        for (m, start) in [
            ("-", true),
            ("*", true),
            ("12.", true),
            ("[ ]", true),
            ("[x]", true),
            ("> ", true),
            ("```rust", true),
            ("---", true),
            ("<br>", false),
        ] {
            assert!(!hideable_marker(m, start), "{m:?} should stay");
        }
    }

    #[test]
    fn a_fold_hides_its_section_up_to_the_next_heading_of_its_level() {
        let h = [(0, 1), (2, 2), (5, 2), (6, 2), (8, 1), (9, 2)];
        assert_eq!(fold_range(&h, 0, 12, |_| false), Some(1..8), "an H1 hides its H2s too");
        assert_eq!(fold_range(&h, 2, 12, |_| false), Some(3..5));
        assert_eq!(fold_range(&h, 5, 12, |_| false), None, "nothing between it and the next H2");
        assert_eq!(fold_range(&h, 8, 12, |_| false), Some(9..12), "the last section runs to the end");
        assert_eq!(fold_range(&h, 4, 12, |_| false), None, "not a heading");
        // Blank lines before the next heading stay visible, so sections keep their spacing.
        assert_eq!(fold_range(&h, 2, 12, |l| l == 4), Some(3..4));
    }

    #[test]
    fn tables_are_found_and_split_into_cells() {
        let text = "intro\n| Key | Action |\n| --- | :-: |\n| Ctrl+E | Preview |\n|a\\|b| c\nafter";
        let buf = Buffer::from(text);
        assert_eq!(table_range(&buf, 0), None);
        assert_eq!(table_range(&buf, 3), Some((1, 4, 2)));
        let row = "| Ctrl+E |  Preview |";
        let cells: Vec<&str> = table_cells(row).into_iter().map(|r| &row[r]).collect();
        assert_eq!(cells, ["Ctrl+E", "Preview"]);
        let escaped = "|a\\|b| c";
        let cells: Vec<&str> = table_cells(escaped).into_iter().map(|r| &escaped[r]).collect();
        assert_eq!(cells, ["a\\|b", "c"]);
        // Not a table without the | --- | line.
        assert_eq!(table_range(&Buffer::from("| a |\n| b |"), 0), None);
        assert!(hideable_rule("---") && hideable_rule("***") && !hideable_rule("--") && !hideable_rule("-*-"));
    }

    #[test]
    fn primary_is_nearest_to_previous_head() {
        let sels = vec![Selection::cursor(1), Selection::cursor(10), Selection::cursor(20)];
        let (p, rest) = split_primary(sels, 11);
        assert_eq!(p, Selection::cursor(10));
        assert_eq!(rest.len(), 2);
    }
}
