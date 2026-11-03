// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Thinking timeline tab: claims on a time axis for a topic (design §C.7).
//!
//! Owned by WS-C. A topic is a tag, a note or words; each claim found in
//! the notes' saved versions sits on a vertical axis by day: ● when it
//! appeared, ○ when it was taken out, ⚡ when another note contradicted it.
//! Clicking a claim opens its note there. A Preview: claims come from the
//! AI model's cache where it has seen the text, else from sentences
//! ("approximate").

use egui::{Color32, RichText, Sense, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::intel::timeline::{Event, Item, by_day};
use quire_app::trust_jobs::Topic;
use quire_app::vault::NotePath;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct TimelineUi {
    focus: bool,
}

/// Whether the tab is offered.
pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.agent_store().is_some()
}

fn glyph(e: Event) -> &'static str {
    match e {
        Event::Added => "●",
        Event::Removed => "○",
        Event::Contradicted => "⚡",
    }
}

fn color(e: Event, pal: &Palette) -> Color32 {
    match e {
        Event::Added => pal.accent,
        Event::Removed => pal.muted,
        Event::Contradicted => pal.warn,
    }
}

fn preview_chip(ui: &mut egui::Ui, pal: &Palette) {
    egui::Frame::NONE.stroke(Stroke::new(1.0, pal.warn)).corner_radius(8.0).inner_margin(egui::Margin::symmetric(6, 1)).show(ui, |ui| {
        ui.label(RichText::new("Preview").size(10.5).color(pal.warn));
    });
}

fn start(ws: &mut Workspace, backfill: bool) {
    let text = ws.features.trust.timeline.topic_text.clone();
    let topic = {
        let ix = ws.index().read();
        Topic::parse(&text, &|n| ix.resolve(n, None))
    };
    let Some(topic) = topic else { return };
    let Some(vault) = ws.vault().cloned() else { return };
    let model = if backfill { ws.trust_chat() } else { None };
    let cache_model = ws.trust_chat().map(|m| m.name().to_string());
    // The state borrows the index only to pick the notes; the build runs on its own thread.
    let mut state = std::mem::take(&mut ws.features.trust.timeline);
    state.start(vault, ws.index(), topic, model, cache_model);
    ws.features.trust.timeline = state;
}

/// Where a claim's text is in its note, to open it there.
fn find_in(ws: &Workspace, item: &Item) -> Option<usize> {
    let ix = ws.index().read();
    let src = ix.source(&item.path)?;
    let words: Vec<&str> = item.text.split_whitespace().take(6).collect();
    src.find(item.text.trim_end_matches('.')).or_else(|| src.find(&words.join(" ")))
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let ui_st = &mut st.timeline;
    let mut go = false;
    let mut backfill = false;
    let mut open: Option<(NotePath, Option<usize>)> = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("THINKING TIMELINE").size(11.0).color(pal.muted).strong());
        preview_chip(ui, pal);
    });
    ui.add(
        egui::Label::new(
            RichText::new("When your notes started saying something, and when they changed their mind.").size(12.0).color(pal.muted),
        )
        .wrap(),
    );
    ui.add_space(4.0);
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 24.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.add_enabled(!ws.features.trust.timeline.topic_text.trim().is_empty(), egui::Button::new("Show")).clicked() {
            go = true;
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut ws.features.trust.timeline.topic_text)
                .hint_text("#tag, [[Note]] or words")
                .desired_width(ui.available_width()),
        );
        if std::mem::take(&mut ui_st.focus) {
            r.request_focus();
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
    });
    if let Some(p) = ws.active_doc().and_then(|d| d.path.clone())
        && ui.add(egui::Button::new(format!("This note: {}", p.stem())).small().truncate()).on_hover_text(p.as_str()).clicked()
    {
        ws.features.trust.timeline.topic_text = format!("[[{}]]", p.without_ext());
        go = true;
    }
    let tl = &ws.features.trust.timeline;
    if tl.busy() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(
                RichText::new(if tl.backfilling() {
                    "Asking the AI model about older versions…"
                } else {
                    "Reading the notes' history…"
                })
                .size(12.0)
                .color(pal.muted),
            );
        });
        if ui.small_button("Stop").clicked() {
            ws.features.trust.timeline.cancel();
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
    }
    if let Some(e) = &ws.features.trust.timeline.error {
        ui.label(RichText::new(e).size(12.0).color(pal.danger));
    }
    let result = ws.features.trust.timeline.result.clone();
    let topic = ws.features.trust.timeline.topic.clone();
    match (&result, &topic) {
        (Some(r), Some(t)) => {
            ui.add_space(4.0);
            let n = r.items.len();
            let mut line = format!(
                "{} · {n} event{} in {} note{}",
                t.label(),
                if n == 1 { "" } else { "s" },
                r.notes,
                if r.notes == 1 { "" } else { "s" }
            );
            if r.approximate {
                line.push_str(" · approximate");
            }
            ui.label(RichText::new(line).size(12.0).color(pal.text)).on_hover_text(if r.approximate {
                "Approximate: claims are read as plain sentences where the AI model hasn't seen the text. Notes keep up to 20 versions."
            } else {
                "Claims as the AI model read them, from up to 20 saved versions of each note."
            });
            ui.horizontal_wrapped(|ui| {
                for e in [Event::Added, Event::Removed, Event::Contradicted] {
                    let word = match e {
                        Event::Added => "said",
                        Event::Removed => "taken out",
                        Event::Contradicted => "contradicted",
                    };
                    ui.label(RichText::new(glyph(e)).color(color(e, pal)).size(12.0));
                    ui.label(RichText::new(word).size(11.5).color(pal.muted));
                }
            });
            if r.approximate
                && !ws.features.trust.timeline.busy()
                && let Some(m) = ws.trust_chat()
            {
                let allowed = quire_app::trust_jobs::timeline::backfill_allowed(m.as_ref(), ws.settings.trust.contradictions);
                let tip = if allowed {
                    format!(
                        "Ask {} for the claims in older versions (at most {} requests; your notes' text is sent to it)",
                        m.name(),
                        quire_app::trust_jobs::timeline::BACKFILL_REQUESTS
                    )
                } else {
                    "Turn on “Find contradictions” in Settings → AI to let the cloud model read your notes".to_string()
                };
                if ui
                    .add_enabled(allowed, egui::Button::new("Fill in with AI").small())
                    .on_hover_text(tip)
                    .on_disabled_hover_text("Turn on “Find contradictions” in Settings → AI first")
                    .clicked()
                {
                    backfill = true;
                }
            }
            if r.capped {
                ui.label(RichText::new(format!("Stopped after {} requests.", r.requests)).size(11.5).color(pal.warn));
            }
            ui.separator();
            if r.items.is_empty() {
                ui.add(egui::Label::new(RichText::new("No claims found. Statements like “The launch moved to June.” show up here; questions, tasks and headings don't.").size(12.0).color(pal.muted)).wrap());
            }
            egui::ScrollArea::vertical().id_salt("quire-timeline").auto_shrink([false, false]).show(ui, |ui| {
                for (day, items) in by_day(&r.items).iter().rev() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(day).size(11.5).color(pal.muted).strong());
                    for it in items.iter().rev() {
                        let resp = ui
                            .horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
                                let p = ui.painter();
                                p.line_segment(
                                    [rect.center_top() - Vec2::Y * 3.0, rect.center_bottom() + Vec2::Y * 3.0],
                                    Stroke::new(1.0, pal.border),
                                );
                                p.text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    glyph(it.event),
                                    egui::FontId::proportional(12.0),
                                    color(it.event, pal),
                                );
                                ui.vertical(|ui| {
                                    let text = RichText::new(&it.text).size(12.5).color(if it.event == Event::Removed {
                                        pal.muted
                                    } else {
                                        pal.text
                                    });
                                    let text = if it.event == Event::Removed { text.strikethrough() } else { text };
                                    ui.add(egui::Label::new(text).wrap().sense(Sense::click()))
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(format!("{} · open the note", it.path))
                                        .clicked()
                                        .then(|| open = Some((it.path.clone(), find_in(ws, it))));
                                    ui.label(RichText::new(it.path.stem()).size(11.0).color(pal.faint));
                                });
                            })
                            .response;
                        let _ = resp;
                    }
                }
                ui.add_space(12.0);
            });
        }
        _ if !ws.features.trust.timeline.busy() => {
            ui.add_space(10.0);
            ui.add(
                egui::Label::new(
                    RichText::new("Pick a topic: a tag like #project, a note like [[Launch plan]], or a few words. Quire reads the saved versions of the notes it covers and shows what they said over time.")
                        .size(12.0)
                        .color(pal.muted),
                )
                .wrap(),
            );
        }
        _ => {}
    }
    if go {
        start(ws, false);
    }
    if backfill {
        start(ws, true);
    }
    if let Some((p, at)) = open
        && ws.open_note(&p).is_some()
    {
        if let Some(b) = at {
            ws.jump_to_byte(b);
        }
        out.jumped = true;
    }
    out
}
