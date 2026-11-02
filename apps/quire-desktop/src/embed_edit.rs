// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Editing embeds in place (design §F.2).
//!
//! A line that is only `![[Note]]`, `![[Note#Heading]]` or `![[Note#^id]]`
//! gets the embedded text under it, editable. Edits go to the embedded note
//! itself (its tab, or a hidden document that autosaves), as edits of the
//! section's range, so the note is saved and undo inside the embed undoes
//! that note. One level only: an embed's own embeds are plain text there, and
//! a note embedding itself (or a note that embeds it back) shows a link.

use std::collections::HashMap;
use std::ops::Range;

use egui::{Key, Modifiers, RichText, TextBuffer};
use quire_app::text::{EditKind, Selection, Transaction};
use quire_app::vault::NotePath;
use quire_app::{DocId, Workspace, nav};

use crate::highlight::HighlightCache;
use crate::theme::{FontSupport, Palette};

/// Sections longer than this are shown with a link instead of an editor.
const MAX_EDIT_BYTES: usize = 64 * 1024;
/// The embedded text scrolls past this height.
const MAX_HEIGHT: f32 = 520.0;

/// An embed on a line: the note and the part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Embed {
    pub target: String,
    /// `Heading` or `^id`.
    pub section: Option<String>,
}

/// `![[Note]]` (with `#Heading`, `#^id` or `|alias`) alone on a line, not an image.
pub(crate) fn embed_on_line(line: &str) -> Option<Embed> {
    let t = line.trim();
    let inner = t.strip_prefix("![[")?.strip_suffix("]]")?;
    if inner.contains("]]") || inner.contains("[[") || quire_app::attach::image_embed(t).is_some() {
        return None;
    }
    let inner = inner.split('|').next().unwrap_or(inner).trim();
    let (target, section) = match inner.split_once('#') {
        Some((t, s)) => (t.trim(), Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        None => (inner, None),
    };
    // Other files (PDFs, canvases) aren't edited here.
    if target.is_empty()
        || target.rsplit_once('.').is_some_and(|(_, ext)| !ext.eq_ignore_ascii_case("md") && ext.len() <= 5 && !ext.contains(' '))
    {
        return None;
    }
    Some(Embed { target: target.to_string(), section })
}

/// The byte range of a section of `text`: the whole note, a heading and
/// what's under it (to the next heading of its level or higher), or a
/// paragraph with a `^id`.
pub(crate) fn section_range(text: &str, section: Option<&str>) -> Option<Range<usize>> {
    let Some(sec) = section else { return Some(0..text.len()) };
    if let Some(id) = sec.strip_prefix('^') {
        let mut r = quire_app::markdown::callout::find_block(text, id)?;
        // A heading right above the paragraph is a block of its own.
        while let Some(nl) = text[r.clone()].find('\n') {
            let first = text[r.start..r.start + nl].trim_start();
            if !(first.starts_with('#') && first.trim_start_matches('#').starts_with(' ')) {
                break;
            }
            r.start += nl + 1;
        }
        return Some(r);
    }
    let headings = quire_app::markdown::extract(text).headings;
    let slug = quire_app::markdown::outline::slugify(sec);
    let i = headings.iter().position(|h| h.anchor == slug || h.text.eq_ignore_ascii_case(sec))?;
    let level = headings[i].level;
    let end = headings[i + 1..].iter().find(|h| h.level <= level).map_or(text.len(), |h| h.offset);
    // Drop the blank lines before the next heading, keep one line break.
    let body = &text[headings[i].offset..end];
    let trimmed = body.trim_end_matches(['\n', '\r', ' ', '\t']);
    let keep = if trimmed.len() < body.len() { trimmed.len() + 1 } else { trimmed.len() };
    Some(headings[i].offset..headings[i].offset + keep.min(body.len()))
}

/// Common prefix and suffix in chars: (start, old end, new end).
fn diff(old: &str, new: &str) -> (usize, usize, usize) {
    let p = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let max = old.chars().count().min(new.chars().count()) - p;
    let s = old.chars().rev().zip(new.chars().rev()).take(max).take_while(|(a, b)| a == b).count();
    (p, old.chars().count() - s, new.chars().count() - s)
}

/// The widget-side copy of one embed.
struct Mirror {
    doc: DocId,
    rev: u64,
    /// Char range of the section in the note.
    range: Range<usize>,
    text: String,
    highlight: HighlightCache,
}

/// Every embed on screen, by (host note, line).
#[derive(Default)]
pub(crate) struct EmbedEditors {
    mirrors: HashMap<(DocId, usize), Mirror>,
}

/// Where an embed is drawn and how big it was last time.
pub(crate) struct Place {
    pub host: DocId,
    pub host_path: Option<NotePath>,
    pub line: usize,
    pub width: f32,
    pub size: f32,
}

/// What happened in an embed this frame.
#[derive(Default)]
pub(crate) struct EmbedOut {
    /// Height used under the line.
    pub height: f32,
    /// Open this note (the label was clicked, or it can't be edited here).
    pub open: Option<NotePath>,
    /// The embedded editor has the keyboard.
    pub focused: bool,
    /// Escape: give the keyboard back to the note.
    pub leave: bool,
}

impl EmbedEditors {
    /// Drop embeds of notes that are gone, and of lines no longer shown.
    pub(crate) fn retain(&mut self, keep: impl Fn(DocId, usize) -> bool) {
        self.mirrors.retain(|(d, l), _| keep(*d, *l));
    }

    /// Draw the embed `e` under a line of the host note.
    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        ws: &mut Workspace,
        e: &Embed,
        at: &Place,
        pal: &Palette,
        fonts: FontSupport,
    ) -> EmbedOut {
        let mut out = EmbedOut::default();
        let resolved = ws.index().read().resolve(&e.target, at.host_path.as_ref());
        let label = match &e.section {
            Some(s) => format!("{} › {}", e.target, s.trim_start_matches('^')),
            None => e.target.clone(),
        };
        let frame = egui::Frame::NONE
            .fill(pal.code_bg.gamma_multiply(0.55))
            .stroke(egui::Stroke::new(1.0, pal.border))
            .corner_radius(6.0)
            .inner_margin(egui::Margin { left: 12, right: 12, top: 6, bottom: 8 });
        let r = frame.show(ui, |ui| {
            ui.set_width(at.width - 24.0);
            let Some(path) = resolved else {
                ui.label(RichText::new(format!("“{}” doesn't exist yet", e.target)).size(at.size * 0.8).color(pal.muted));
                return;
            };
            // A note embedding itself, or one that embeds it back: a link.
            let cycle = at.host_path.as_ref() == Some(&path) || at.host_path.as_ref().is_some_and(|h| embeds(ws, &path, h));
            let header = ui.horizontal(|ui| {
                let verb = if cycle { "Embeds" } else { "Editing" };
                ui.label(RichText::new(verb).size(at.size * 0.72).color(pal.muted));
                let link = ui
                    .add(egui::Label::new(RichText::new(&label).size(at.size * 0.72).italics().color(pal.link)).sense(egui::Sense::click()))
                    .on_hover_text("Open this note")
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                link.clicked()
            });
            if header.inner {
                out.open = Some(path.clone());
            }
            if cycle {
                ui.label(RichText::new("It embeds this note, so it isn't shown here again.").size(at.size * 0.72).color(pal.faint));
                return;
            }
            let Some(doc_id) = nav::embed_doc(ws, &path) else { return };
            let Some(doc) = nav::embed_doc_ref(ws, doc_id) else { return };
            let text = doc.text();
            let Some(bytes) = section_range(&text, e.section.as_deref()) else {
                ui.label(RichText::new("That part of the note isn't there any more.").size(at.size * 0.8).color(pal.warn));
                return;
            };
            if bytes.len() > MAX_EDIT_BYTES || doc.large {
                ui.label(RichText::new("Too long to edit here; open the note to edit it.").size(at.size * 0.8).color(pal.muted));
                return;
            }
            let range = doc.byte_to_char(bytes.start)..doc.byte_to_char(bytes.end);
            let key = (at.host, at.line);
            let rev = doc.revision();
            let m = self.mirrors.entry(key).or_insert_with(|| Mirror {
                doc: doc_id,
                rev: u64::MAX,
                range: range.clone(),
                text: String::new(),
                highlight: HighlightCache::default(),
            });
            // The note changed (here, elsewhere, undo): show it as it is now.
            if m.rev != rev || m.doc != doc_id || m.range != range {
                m.text = text[bytes].to_string();
                m.rev = rev;
                m.doc = doc_id;
                m.range = range;
            }
            let id = egui::Id::unique(("quire-embed", at.host.0, at.line));
            let focused = ui.ctx().memory(|mem| mem.has_focus(id));
            out.focused = focused;
            // Undo and redo here are the embedded note's.
            if focused {
                let (undo, redo, esc) = ui.ctx().input_mut(|i| {
                    let mac = cfg!(target_os = "macos");
                    let redo =
                        i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || (!mac && i.consume_key(Modifiers::COMMAND, Key::Y));
                    let undo = i.consume_key(Modifiers::COMMAND, Key::Z);
                    let esc = i.consume_key(Modifiers::NONE, Key::Escape);
                    (undo, redo, esc)
                });
                out.leave = esc;
                if undo || redo {
                    if let Some(d) = nav::embed_doc_mut(ws, doc_id) {
                        let done = if undo { d.undo() } else { d.redo() };
                        if !done {
                            ws.notify(quire_app::NoticeLevel::Info, if undo { "Nothing to undo in this embed" } else { "Nothing to redo" });
                        }
                    }
                    // Next frame shows the note as it is after undo.
                    ui.ctx().request_repaint();
                }
            }
            let size = at.size;
            let mut layouter = |ui: &egui::Ui, buf: &dyn TextBuffer, wrap: f32| {
                let mut job = m.highlight.job(buf.as_str(), false, pal, size, fonts);
                job.wrap.max_width = wrap;
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let resp = egui::ScrollArea::vertical()
                .id_salt(id.with("scroll"))
                .max_height(MAX_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::TextEdit::multiline(&mut m.text)
                        .id(id)
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(1)
                        .lock_focus(true)
                        .layouter(&mut layouter)
                        .show(ui)
                })
                .inner;
            if resp.response.changed() {
                let old = &text[section_range(&text, e.section.as_deref()).unwrap_or(0..0)];
                let (s, oe, ne) = diff(old, &m.text);
                let inserted: String = m.text.chars().skip(s).take(ne - s).collect();
                let start = m.range.start;
                let caret = resp.cursor_range.map_or(start + ne, |c| start + c.primary.index.0);
                let kind = if oe == s && inserted.chars().count() == 1 && inserted != "\n" { EditKind::Insert } else { EditKind::Other };
                let tx = Transaction::replace(start + s, start + oe, inserted).with_selection(Selection::cursor(caret));
                if let Some(d) = nav::embed_doc_mut(ws, doc_id)
                    && d.apply(tx, kind).is_ok()
                {
                    m.rev = d.revision();
                    m.range = start..start + m.text.chars().count();
                }
            }
        });
        out.height = r.response.rect.height();
        out
    }
}

/// Whether the note at `from` embeds `to` (one level: enough to stop a loop).
fn embeds(ws: &Workspace, from: &NotePath, to: &NotePath) -> bool {
    let ix = ws.index().read();
    let Some(src) = ix.source(from) else { return false };
    src.lines().filter_map(embed_on_line).any(|e| ix.resolve(&e.target, Some(from)).as_ref() == Some(to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_lines() {
        assert_eq!(embed_on_line("![[Note]]"), Some(Embed { target: "Note".into(), section: None }));
        assert_eq!(embed_on_line("  ![[Note#Plan B|shown]] "), Some(Embed { target: "Note".into(), section: Some("Plan B".into()) }));
        assert_eq!(embed_on_line("![[Note#^abc123]]").and_then(|e| e.section), Some("^abc123".into()));
        assert_eq!(embed_on_line("![[pic.png]]"), None, "images are drawn, not edited");
        assert_eq!(embed_on_line("![[paper.pdf]]"), None);
        assert_eq!(embed_on_line("![[v1.2 notes]]").map(|e| e.target), Some("v1.2 notes".into()));
        assert_eq!(embed_on_line("text ![[Note]]"), None, "only a line of its own");
        assert_eq!(embed_on_line("[[Note]]"), None);
    }

    #[test]
    fn sections() {
        let t = "# A\nintro\n## B\nbee\n\n## C\nsea ^x1\n\n# D\n";
        assert_eq!(&t[section_range(t, Some("B")).unwrap()], "## B\nbee\n");
        assert_eq!(&t[section_range(t, Some("A")).unwrap()], "# A\nintro\n## B\nbee\n\n## C\nsea ^x1\n");
        assert_eq!(&t[section_range(t, Some("^x1")).unwrap()], "sea ^x1");
        assert_eq!(section_range(t, None), Some(0..t.len()));
        assert_eq!(section_range(t, Some("Nope")), None);
        assert_eq!(diff("abc", "abXc"), (2, 2, 3));
    }
}
