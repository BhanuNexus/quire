// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Ingest into wiki…" (design §D.7): pick a source in `raw/` (or a web
//! page to capture first), see what it will cost on a cloud model, run it
//! in the background with Stop, and see what changed, with Revert run.
//!
//! Owned by WS-D (included by `agent_panel_ui.rs`).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use egui::RichText;
use quire_app::agent::{Actor, RevertSelector, Via};
use quire_app::intel::chat::{CancelFlag, ChatModel, Locality};
use quire_app::vault::NotePath;
use quire_app::workspace::UiRequest;
use quire_app::{Command, Workspace};
use quire_assistant::wiki::{IngestEvent, IngestReport, estimate, ingest};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

enum State {
    Choose,
    Running { rx: Receiver<Result<IngestEvent, String>>, cancel: CancelFlag, step: String },
    Done(Result<IngestReport, String>),
}

pub(crate) struct IngestDialog {
    sources: Vec<NotePath>,
    chosen: Option<NotePath>,
    state: State,
    /// The model, chosen when the dialog opens.
    model: Option<Arc<dyn ChatModel>>,
}

impl IngestDialog {
    /// For `registry::UiTarget::Overlay` (`Command::IngestIntoWiki`).
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let mut sources: Vec<NotePath> =
            ws.vault().and_then(|v| v.notes().ok()).unwrap_or_default().into_iter().filter(|p| p.as_str().starts_with("raw/")).collect();
        sources.sort();
        let chosen = ws.features.assistant.ingest_source.take().or_else(|| sources.first().cloned());
        let model = quire_app::ai::panel_chat(&ws.settings).map(|(m, _, _)| m);
        Box::new(Self { sources, chosen, state: State::Choose, model })
    }

    #[cfg(test)]
    pub(crate) fn sample(state: u8) -> Self {
        let sources =
            (0..30).map(|i| NotePath::from(format!("raw/Paper {i} with a fairly long title about ropes.md").as_str())).collect::<Vec<_>>();
        let st = match state {
            0 => State::Choose,
            1 => {
                let (_tx, rx) = channel();
                State::Running { rx, cancel: CancelFlag::new(), step: "Concept 3 of 8: Structural sharing".into() }
            }
            _ => State::Done(Ok(IngestReport {
                created: vec!["wiki/sources/Paper.md".into(), "wiki/concepts/Ropes.md".into()],
                updated: vec!["wiki/concepts/Trees.md".into()],
                dropped: 2,
                ..IngestReport::default()
            })),
        };
        Self { chosen: sources.first().cloned(), sources, state: st, model: None }
    }
}

impl OverlayView for IngestDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        if let State::Running { rx, step, .. } = &mut self.state {
            let mut done = None;
            while let Ok(m) = rx.try_recv() {
                match m {
                    Ok(IngestEvent::Step(s)) => *step = s,
                    Ok(IngestEvent::Done(r)) => done = Some(Ok(r)),
                    Err(e) => done = Some(Err(e)),
                }
            }
            if let Some(d) = done {
                self.state = State::Done(d);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        egui::Modal::new(egui::Id::unique("quire-ingest")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 560.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.label(RichText::new("Ingest into wiki").size(20.0).strong().color(pal.strong));
            ui.add_space(2.0);
            ui.label(RichText::new("A source becomes a summary page and concept pages in wiki/, each point quoting the source. Links from your own notes arrive as suggestions.").color(pal.muted));
            ui.add_space(10.0);
            match &mut self.state {
                State::Choose => self.choose(ui, ws, pal, &mut res),
                State::Running { step, cancel, .. } => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new(step.as_str()).color(pal.text));
                    });
                    ui.add_space(10.0);
                    if ui.button("Stop").clicked() {
                        cancel.cancel();
                    }
                }
                State::Done(r) => {
                    let r = r.clone();
                    done(ui, ws, pal, &r, &mut res);
                }
            }
        });
        res
    }

    fn closed(&mut self, _ws: &mut Workspace) {
        if let State::Running { cancel, .. } = &self.state {
            cancel.cancel();
        }
    }
}

impl IngestDialog {
    fn choose(&mut self, ui: &mut egui::Ui, ws: &Workspace, pal: &Palette, res: &mut OverlayResult) {
        let model = self.model.clone();
        ui.label(RichText::new("Source").strong());
        egui::Frame::NONE.stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(4.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("quire-ingest-sources").max_height(170.0).auto_shrink([false, false]).show(ui, |ui| {
                if self.sources.is_empty() {
                    ui.label(RichText::new("No notes in raw/ yet. Capture a web page, or move a source note into raw/.").color(pal.muted));
                }
                for s in &self.sources {
                    let label = s.as_str().trim_start_matches("raw/").trim_end_matches(".md");
                    if crate::theme::row(ui, self.chosen.as_ref() == Some(s), 26.0, pal, |painter, rect| {
                        painter.text(
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            label,
                            egui::FontId::proportional(13.5),
                            pal.text,
                        );
                    })
                    .clicked()
                    {
                        self.chosen = Some(s.clone());
                    }
                }
            });
        });
        if let Some(c) = &self.chosen
            && !c.as_str().starts_with("raw/")
        {
            ui.label(RichText::new(format!("Using the open note, {}.", c.stem())).size(12.0).color(pal.muted));
        }
        ui.horizontal(|ui| {
            if ui.button("Capture a web page…").on_hover_text("Save a page into raw/ first").clicked() {
                res.run = Some(Command::CaptureWebPage);
            }
        });
        ui.add_space(8.0);
        let Some(model) = model else {
            ui.label(RichText::new("Ingest needs an AI model.").color(pal.warn));
            ui.horizontal(|ui| {
                if ui.button("Set up AI…").clicked() {
                    res.run = Some(Command::AiSetup);
                }
                if ui.button("Close").clicked() {
                    res.close = true;
                }
            });
            return;
        };
        let text = self.chosen.as_ref().and_then(|p| ws.vault().and_then(|v| v.read(p).ok())).unwrap_or_default();
        let e = estimate(&text);
        let where_ = quire_app::ai::locality_words(model.locality());
        ui.label(RichText::new(format!("Model: {} ({where_})", model.name())).color(pal.text));
        if model.locality() == Locality::Cloud {
            ui.label(RichText::new(format!("This sends {} to your provider.", e.describe())).color(pal.warn));
        } else {
            ui.label(RichText::new(format!("{} on {where_}.", e.describe().replace("about", "About"))).size(12.0).color(pal.muted));
        }
        ui.add_space(8.0);
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                res.close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(self.chosen.is_some(), egui::Button::new(RichText::new("Ingest").strong())).clicked()
                    && let Some(src) = self.chosen.clone()
                {
                    self.start(ws, model, src);
                }
            });
        });
    }

    fn start(&mut self, ws: &Workspace, model: Arc<dyn ChatModel>, source: NotePath) {
        let Some(agent) = ws.agent_store().cloned() else { return };
        let (tx, rx) = channel();
        let cancel = CancelFlag::new();
        let c2 = cancel.clone();
        let device = ws.device().id.clone();
        let provider = ws.settings.ai_provider.clone();
        std::thread::spawn(move || {
            let actor = Actor::agent(format!("panel:{provider}"), format!("Ingest ({})", model.name()), Via::App, &device);
            let now = chrono::Local::now();
            let job = quire_assistant::wiki::Ingest {
                model: model.as_ref(),
                agent: &agent,
                actor: &actor,
                source,
                today: now.format("%Y-%m-%d").to_string(),
                now: now.format("%Y-%m-%d %H:%M").to_string(),
            };
            let tx2 = tx.clone();
            if let Err(e) = ingest(&job, &c2, &mut |ev| {
                let _ = tx2.send(Ok(ev));
            }) {
                let _ = tx.send(Err(e));
            }
        });
        self.state = State::Running { rx, cancel, step: "Starting".into() };
    }
}

fn done(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, r: &Result<IngestReport, String>, res: &mut OverlayResult) {
    match r {
        Err(e) => {
            ui.label(RichText::new("The ingest stopped. Nothing was written.").strong().color(pal.warn));
            ui.add(egui::Label::new(RichText::new(e).color(pal.muted)).wrap());
        }
        Ok(rep) => {
            ui.label(RichText::new(format!("✓ {}", rep.describe())).strong().color(pal.accent));
            ui.add_space(4.0);
            let mut open = None;
            for (p, what) in rep.created.iter().map(|p| (p, "new")).chain(rep.updated.iter().map(|p| (p, "updated"))) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(what).size(11.0).color(pal.faint));
                    if ui.link(p.stem()).clicked() {
                        open = Some(p.clone());
                    }
                });
            }
            if rep.dropped > 0 {
                ui.label(
                    RichText::new(format!("{} point(s) left out: their quotes weren't in the source.", rep.dropped))
                        .size(12.0)
                        .color(pal.muted),
                );
            }
            if let Some(p) = open
                && ws.open_note(&p).is_some()
            {
                res.close = true;
                res.jumped = true;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if !rep.proposals.is_empty() && ui.button("Review suggestions").clicked() {
                    res.close = true;
                    res.run = Some(Command::OpenInbox);
                }
                if let Some(run) = &rep.run
                    && ui.button("Revert run…").on_hover_text("See what undoing this ingest would do, then decide").clicked()
                {
                    ws.features.agent.revert_preset =
                        Some((RevertSelector { run: Some(run.clone()), ..RevertSelector::default() }, "Undo the ingest".into()));
                    ws.request_ui(UiRequest::Feature(Command::UndoAgentChanges));
                    res.close = true;
                }
            });
        }
    }
    ui.add_space(6.0);
    if ui.button("Close").clicked() {
        res.close = true;
    }
}
