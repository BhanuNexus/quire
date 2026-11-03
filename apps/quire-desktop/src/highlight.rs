// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown → egui `LayoutJob`, memoized by content hash.

use std::hash::{Hash, Hasher};

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Align, Color32, FontFamily, FontId, Stroke};
use quire_app::markdown::{Style, StyleRun, highlight};

use crate::theme::{BOLD, FontSupport, ITALIC, Palette};

#[derive(Default)]
pub(crate) struct HighlightCache {
    key: u64,
    job: Option<LayoutJob>,
}

const HEADING_SCALE: [f32; 6] = [1.7, 1.42, 1.22, 1.1, 1.0, 1.0];

pub(crate) fn format_for(style: &Style, p: &Palette, size: f32, fonts: FontSupport) -> TextFormat {
    let mut f = TextFormat {
        font_id: FontId::new(size, FontFamily::Proportional),
        color: p.text,
        line_height: Some(size * 1.55),
        ..Default::default()
    };
    if style.heading > 0 {
        let s = size * HEADING_SCALE[(style.heading as usize - 1).min(5)];
        f.font_id = FontId::new(s, if fonts.bold { FontFamily::Name(BOLD.into()) } else { FontFamily::Proportional });
        f.color = p.h[(style.heading as usize - 1).min(5)];
        f.line_height = Some(s * 1.45);
    }
    if style.strong {
        if fonts.bold {
            f.font_id.family = FontFamily::Name(BOLD.into());
        }
        if style.heading == 0 {
            f.color = p.strong;
        }
    }
    if style.emphasis {
        // A true italic face, not a slanted regular.
        if fonts.bold && !style.strong && style.heading == 0 {
            f.font_id.family = FontFamily::Name(ITALIC.into());
        } else {
            f.italics = true;
        }
    }
    if style.quote {
        // The editor draws a bar beside quotes; the text just recedes a little.
        f.color = p.quote;
    }
    if style.code_block || style.front_matter {
        // Blocks get one background behind the whole block (drawn by the editor).
        f.font_id = FontId::new(size * 0.88, FontFamily::Monospace);
    } else if style.code || style.math {
        // Inline code: a tight chip around the text, not the whole line height.
        f.font_id = FontId::new(size * 0.88, FontFamily::Monospace);
        f.background = p.code_bg;
        f.line_height = None;
        f.valign = Align::Center;
    }
    if style.front_matter {
        f.color = p.muted;
    }
    if style.math {
        f.color = p.tag;
    }
    if style.link || style.wikilink {
        f.color = p.link;
    }
    if style.wikilink {
        f.underline = Stroke::new(1.0, p.link.gamma_multiply(0.35));
    }
    if style.tag {
        f.color = p.tag;
        f.background = p.tag_bg;
        f.line_height = None;
        f.valign = Align::Center;
    }
    if style.highlight {
        f.background = p.mark_bg;
    }
    if style.strike || style.task_done {
        f.strikethrough = Stroke::new(1.0, p.muted);
        f.color = p.muted;
    }
    if style.marker {
        f.color = p.faint;
        if !style.code_block {
            f.background = Color32::TRANSPARENT;
        }
        f.underline = Stroke::NONE;
    }
    if style.conflict {
        f.font_id = FontId::new(size * 0.9, FontFamily::Monospace);
        f.color = p.conflict;
        f.background = p.mark_bg;
    }
    f
}

pub(crate) fn build_job(text: &str, runs: &[StyleRun], p: &Palette, size: f32, fonts: FontSupport) -> LayoutJob {
    let plain = format_for(&Style::default(), p, size, fonts);
    let mut job = LayoutJob { text: text.to_owned(), ..Default::default() };
    let mut pos = 0;
    let mut push = |range: std::ops::Range<usize>, format: TextFormat| {
        if range.start < range.end {
            job.sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(range.start)..ByteIndex(range.end), format });
        }
    };
    for run in runs {
        if run.range.start > pos {
            push(pos..run.range.start, plain.clone());
        }
        push(run.range.clone(), format_for(&run.style, p, size, fonts));
        pos = run.range.end;
    }
    if pos < text.len() || text.is_empty() {
        push(pos..text.len(), plain.clone());
        if text.is_empty() {
            job.sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(0)..ByteIndex(0), format: plain });
        }
    }
    job
}

impl HighlightCache {
    /// Highlighted job for `text`; recomputed only when text or style inputs change.
    pub(crate) fn job(&mut self, text: &str, plain_mode: bool, p: &Palette, size: f32, fonts: FontSupport) -> LayoutJob {
        let mut h = std::hash::DefaultHasher::new();
        text.hash(&mut h);
        plain_mode.hash(&mut h);
        // The whole palette: another theme recolours without clearing caches.
        p.hash(&mut h);
        size.to_bits().hash(&mut h);
        let key = h.finish();
        if self.key != key || self.job.is_none() {
            let runs = if plain_mode { Vec::new() } else { highlight(text) };
            self.job = Some(build_job(text, &runs, p, size, fonts));
            self.key = key;
        }
        self.job.clone().expect("set above")
    }
}
