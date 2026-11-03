// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Ask your notes" (design §C.4): a question, optionally what you expect,
//! the evidence grouped by topic, then the answer with its sources, or
//! "Not found in your notes" with the nearest passages. An answer can be
//! saved as a note whose citations Quire keeps checking.
//!
//! Owned by WS-C (moved out of `overlays.rs`); opened by `UiRequest::Ask`.

use std::collections::HashSet;

use egui::{Key, RichText, Stroke};
use quire_app::Workspace;
use quire_app::ask_state::AskStep;
use quire_app::intel::Source;
use quire_app::vault::NotePath;

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// Puts a sample state into the workspace (layout checks).
type Preset = fn(&mut Workspace);

pub(crate) struct AskOverlay {
    focused: bool,
    /// Evidence groups opened up.
    expanded: HashSet<usize>,
    preset: Option<Preset>,
    /// Where Save as note saves (read once: it looks at the agent policy).
    folder: &'static str,
}

impl AskOverlay {
    /// The dialog for Ask (Ctrl+Shift+A): a fresh question, keeping the last
    /// one's text so it can be refined.
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let before = ws.settings.trust.ask_before_tell;
        if ws.ask.answer.is_some() || ws.ask.step != AskStep::Question {
            let q = std::mem::take(&mut ws.ask.question);
            ws.ask.reset(before);
            ws.ask.question = q;
        } else {
            ws.ask.before_tell = before;
        }
        ws.ask.open = true;
        let folder = ws.answer_folder();
        Box::new(Self { focused: false, expanded: HashSet::new(), preset: None, folder })
    }

    #[cfg(test)]
    pub(crate) fn sample(preset: Preset) -> Self {
        Self { focused: true, expanded: HashSet::from([0]), preset: Some(preset), folder: "Answers" }
    }
}

/// `Note › Heading`.
fn place(s: &Source) -> String {
    let stem = s.path.stem();
    match &s.heading {
        Some(h) if h != stem => format!("{stem} › {h}"),
        _ => stem.to_string(),
    }
}

/// One line of a passage, for lists.
fn excerpt(s: &Source, n: usize) -> String {
    let mut body: String = s.body().split_whitespace().collect::<Vec<_>>().join(" ");
    // The passage starts with its heading, already shown above it.
    for h in s.heading.iter().map(String::as_str).chain([s.path.stem()]) {
        if let Some(rest) = body.strip_prefix(h).filter(|r| r.starts_with(' ')) {
            body = rest.trim_start().to_string();
            break;
        }
    }
    let short: String = body.chars().take(n).collect();
    if body.chars().count() > n { format!("{short}…") } else { short }
}

fn small(text: impl Into<String>, pal: &Palette) -> RichText {
    RichText::new(text).size(12.0).color(pal.muted)
}

/// A passage the person can click to open.
fn source_row(ui: &mut egui::Ui, s: &Source, quote: Option<&str>, pal: &Palette, open: &mut Option<(NotePath, usize)>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{}.", s.n)).size(12.5).color(pal.muted));
        if ui.link(RichText::new(place(s)).size(12.5)).on_hover_text(s.path.as_str()).clicked() {
            *open = Some((s.path.clone(), s.offset));
        }
    });
    let text = quote.map_or_else(|| excerpt(s, 180), |q| format!("“{}”", q.split_whitespace().collect::<Vec<_>>().join(" ")));
    ui.horizontal(|ui| {
        ui.add_space(18.0);
        ui.add(egui::Label::new(RichText::new(text).size(12.0).color(pal.muted)).wrap());
    });
}

impl OverlayView for AskOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        if let Some(p) = self.preset.take() {
            p(ws);
        }
        let mut res = OverlayResult::default();
        let mut go = false;
        let mut open_at: Option<(NotePath, usize)> = None;
        let mut action: Option<fn(&mut Workspace)> = None;
        let mut insert = false;
        let mut save = false;
        if ws.ask.pending() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        egui::Modal::new(egui::Id::unique("quire-ask")).show(ctx, |ui| {
            let w = 640.0_f32.min(ctx.content_rect().width() - 48.0);
            let max_h = (ctx.content_rect().height() - 260.0).clamp(160.0, 420.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.heading("Ask your notes");
            ui.label(small(format!("Answers come only from your vault · {}", ws.assistant_name()), pal));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let r =
                    ui.add(egui::TextEdit::singleline(&mut ws.ask.question).hint_text("What did I decide about…?").desired_width(w - 70.0));
                if !self.focused {
                    r.request_focus();
                    self.focused = true;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = true;
                }
                if ui.add_enabled(!ws.ask.pending() && !ws.ask.question.trim().is_empty(), egui::Button::new("Ask")).clicked() {
                    go = true;
                }
            });
            ui.checkbox(&mut ws.ask.before_tell, RichText::new("Say what I expect first, then show the evidence").size(12.5))
                .on_hover_text(
                    "Think it through before the answer: you write what you expect, see the passages grouped by topic, then the answer.",
                );
            match ws.ask.step {
                AskStep::Question => {}
                AskStep::Expect => {
                    ui.separator();
                    ui.label(RichText::new("What do you expect the answer to be?").strong());
                    ui.label(small("Optional. The answer will say where your notes agree or disagree.", pal));
                    ui.add(egui::TextEdit::multiline(&mut ws.ask.hypothesis).desired_rows(2).desired_width(w).hint_text("I think…"));
                    ui.horizontal(|ui| {
                        if ui.button("Show the evidence").clicked() {
                            action = Some(Workspace::ask_evidence);
                        }
                        if ui.button("Skip").on_hover_text("Go straight to the evidence").clicked() {
                            ws.ask.hypothesis.clear();
                            action = Some(Workspace::ask_evidence);
                        }
                    });
                }
                AskStep::Evidence => {
                    ui.separator();
                    let n = ws.ask.sources.len();
                    ui.label(RichText::new(format!("What your notes say · {n} passage{}", if n == 1 { "" } else { "s" })).strong());
                    if !ws.ask.hypothesis.trim().is_empty() {
                        ui.label(small(format!("You expect: {}", ws.ask.hypothesis.trim()), pal));
                    }
                    egui::ScrollArea::vertical().id_salt("quire-ask-evidence").max_height(max_h).auto_shrink([false, true]).show(
                        ui,
                        |ui| {
                            for (i, c) in ws.ask.clusters.iter().enumerate() {
                                let open = self.expanded.contains(&i);
                                let label = if c.label.is_empty() { "Other passages".to_string() } else { c.label.join(" · ") };
                                egui::Frame::NONE
                                    .stroke(Stroke::new(1.0, pal.border))
                                    .corner_radius(6.0)
                                    .inner_margin(6.0)
                                    .outer_margin(egui::Margin { bottom: 4, ..egui::Margin::ZERO })
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        let head = format!(
                                            "{} {label}  ({} passage{})",
                                            if open { "▼" } else { "▶" },
                                            c.members.len(),
                                            if c.members.len() == 1 { "" } else { "s" }
                                        );
                                        if ui
                                            .add(egui::Label::new(RichText::new(head).color(pal.strong)).sense(egui::Sense::click()))
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            if open {
                                                self.expanded.remove(&i);
                                            } else {
                                                self.expanded.insert(i);
                                            }
                                        }
                                        if open {
                                            for &m in &c.members {
                                                if let Some(s) = ws.ask.sources.get(m) {
                                                    source_row(ui, s, None, pal, &mut open_at);
                                                }
                                            }
                                        }
                                    });
                            }
                        },
                    );
                    if ui.button("Show the answer").clicked() {
                        action = Some(Workspace::ask_answer);
                    }
                }
                AskStep::Answer => {
                    ui.separator();
                    if ws.ask.pending() {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new());
                            ui.label("Thinking…");
                        });
                    }
                    if let Some(e) = &ws.ask.error {
                        ui.label(
                            RichText::new(format!("The AI model failed ({e}); this answer quotes your notes instead."))
                                .color(pal.warn)
                                .size(12.0),
                        );
                    }
                    if let Some(a) = ws.ask.answer.clone() {
                        egui::ScrollArea::vertical().id_salt("quire-ask-answer").max_height(max_h).auto_shrink([false, true]).show(
                            ui,
                            |ui| {
                                if a.abstained {
                                    ui.label(RichText::new(quire_app::intel::ask::NOT_FOUND_TEXT).size(15.0).color(pal.warn).strong());
                                    ui.label(small("Nothing in your notes answers this. These passages came closest:", pal));
                                    ui.add_space(4.0);
                                    for s in a.sources.iter().take(3) {
                                        source_row(ui, s, None, pal, &mut open_at);
                                    }
                                } else {
                                    ui.add(
                                        egui::Label::new(RichText::new(a.markdown.replace("[[", "").replace("]]", "")).size(14.0)).wrap(),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(RichText::new(format!("SOURCES · {}", a.by)).size(11.0).color(pal.muted).strong());
                                    let cited: Vec<&Source> =
                                        a.sources.iter().filter(|s| a.citations.iter().any(|(n, _)| *n == s.n)).collect();
                                    let list = if cited.is_empty() { a.sources.iter().collect() } else { cited };
                                    for s in list {
                                        let quote = a.citations.iter().find(|(n, _)| *n == s.n).map(|(_, an)| an.quote.as_str());
                                        source_row(ui, s, quote, pal, &mut open_at);
                                    }
                                }
                            },
                        );
                        ui.add_space(6.0);
                        if let Some(p) = &ws.ask.saved {
                            ui.label(small(format!("Saved to {p}"), pal));
                        }
                        ui.horizontal(|ui| {
                            let can_save = !a.abstained && ws.ask.saved.is_none() && ws.agent_store().is_some();
                            let folder = self.folder;
                            if ui
                                .add_enabled(can_save, egui::Button::new("Save as note"))
                                .on_hover_text(format!(
                                    "A note in {folder}/ with the citations as footnotes; Quire tells you if a cited passage changes later"
                                ))
                                .clicked()
                            {
                                save = true;
                            }
                            if !a.abstained && ws.active_doc().is_some() && ui.button("Insert into note").clicked() {
                                insert = true;
                            }
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(a.markdown.clone());
                            }
                            if ui.button("Close").clicked() {
                                res.close = true;
                            }
                        });
                    }
                }
            }
        });
        if go {
            ws.ask();
            self.expanded = HashSet::from([0]);
        }
        if let Some(f) = action {
            f(ws);
        }
        if insert && ws.insert_answer() {
            res.close = true;
            res.jumped = true;
        }
        if save && ws.save_answer_note().is_some() {
            res.close = true;
            res.jumped = true;
        }
        if let Some((p, at)) = open_at
            && ws.open_note(&p).is_some()
        {
            ws.jump_to_byte(at);
            res.close = true;
            res.jumped = true;
        }
        res
    }

    fn closed(&mut self, ws: &mut Workspace) {
        ws.ask.open = false;
    }
}

/// Sample states for `layout_check`.
#[cfg(test)]
pub(crate) mod samples {
    use quire_app::Workspace;
    use quire_app::ask_state::AskStep;
    use quire_app::intel::cluster::Cluster;
    use quire_app::intel::{Answer, Source};
    use quire_app::vault::NotePath;

    fn sources() -> Vec<Source> {
        (1..=6)
            .map(|n| Source {
                n,
                path: NotePath::new(format!("Folder {}/Note {n}.md", n % 5)),
                heading: Some(format!("A heading that is fairly long, number {n}")),
                offset: 0,
                end: 10,
                text: format!("Note {n}\n{}", "A passage with enough words to wrap over more than one line in the dialog. ".repeat(4)),
                score: 0.5,
                sim: 0.4,
                coverage: 0.8,
            })
            .collect()
    }

    fn base(ws: &mut Workspace, step: AskStep) {
        ws.ask.reset(true);
        ws.ask.question = "What did we decide about the launch date of the new app, and why?".into();
        ws.ask.step = step;
        ws.ask.sources = sources();
    }

    pub(crate) fn empty(ws: &mut Workspace) {
        ws.ask.reset(false);
    }

    pub(crate) fn expect(ws: &mut Workspace) {
        base(ws, AskStep::Expect);
        ws.ask.hypothesis = "I think it moved to June because of the review.".into();
    }

    pub(crate) fn evidence(ws: &mut Workspace) {
        expect(ws);
        ws.ask.step = AskStep::Evidence;
        ws.ask.clusters = vec![
            Cluster { label: vec!["launch".into(), "june".into(), "review".into()], members: vec![0, 1, 2] },
            Cluster { label: vec!["budget".into()], members: vec![3, 4] },
            Cluster { label: vec![], members: vec![5] },
        ];
    }

    pub(crate) fn answer(ws: &mut Workspace) {
        base(ws, AskStep::Answer);
        let markdown = "The launch moved to June 2027 after the design review found two blocking issues [1][2]. The budget stayed the same [4].\n\nYour expectation: the notes agree that the review caused it [2].".to_string();
        let src = sources();
        let citations =
            src.iter().take(2).map(|s| (s.n, quire_app::intel::anchor::make_anchor(&s.path, &s.text, 0..s.text.len()))).collect();
        ws.ask.answer = Some(Answer { markdown, sources: src, by: "Claude Sonnet".into(), abstained: false, citations });
    }

    pub(crate) fn not_found(ws: &mut Workspace) {
        base(ws, AskStep::Answer);
        let src = sources();
        ws.ask.answer = Some(Answer {
            markdown: quire_app::intel::ask::abstain_markdown(&src),
            sources: src,
            by: "extractive".into(),
            abstained: true,
            citations: vec![],
        });
    }
}
