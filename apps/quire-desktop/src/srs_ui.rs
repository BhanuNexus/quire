// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Flashcard review (design §E.6): the question, Space for the answer, then
//! 1–4 (Again, Hard, Good, Easy), each showing when the card comes back.
//!
//! Owned by WS-E. Opened by "Review flashcards" (Ctrl+Alt+R) and "Review
//! this note's flashcards"; shown from the Plan panel tab.

use egui::{Key, Modifiers, RichText, Ui};
use quire_app::Workspace;
use quire_app::markdown::srs::CardKind;
use quire_app::plan::srs::{Rating, span_label};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// Review state kept in `FeatureUi` (the session itself is in the workspace).
#[derive(Default)]
pub(crate) struct ReviewUi {
    focused: bool,
}

const KEYS: [Key; 4] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4];

impl ReviewUi {
    /// Draw as a modal; true when it should close.
    pub(crate) fn show_modal(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> bool {
        let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let has_card = ws.features.srs.session.as_ref().is_some_and(|s| s.current().is_some());
        let revealed = ws.features.srs.session.as_ref().is_some_and(|s| s.revealed);
        // Keys: Space shows the answer; 1–4 answer; Enter = Good, or closes when done.
        let (space, enter, rating) = ctx.input_mut(|i| {
            let space = i.consume_key(Modifiers::NONE, Key::Space);
            let enter = i.consume_key(Modifiers::NONE, Key::Enter);
            let rating = KEYS.iter().position(|k| i.consume_key(Modifiers::NONE, *k));
            // The same keys as typed text must not reach the note either.
            i.events.retain(|e| !matches!(e, egui::Event::Text(t) if matches!(t.as_str(), " " | "1" | "2" | "3" | "4")));
            (space, enter, rating)
        });
        if has_card && !revealed && (space || enter) {
            ws.srs_reveal();
        } else if has_card && revealed {
            if let Some(r) = rating {
                ws.srs_answer(Rating::ALL[r]);
            } else if space || enter {
                ws.srs_answer(Rating::Good);
            }
        } else if !has_card && (enter || space) {
            close = true;
        }
        egui::Modal::new(egui::Id::unique("quire-review")).show(ctx, |ui| {
            let w = 620.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            // Keep the keyboard here, away from the note.
            let id = egui::Id::unique("quire-review-keys");
            let r = ui.interact(egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::ZERO), id, egui::Sense::focusable_noninteractive());
            if !self.focused || !r.has_focus() {
                r.request_focus();
                self.focused = true;
            }
            close |= body(ui, ws, pal, ctx.content_rect().height());
        });
        close
    }
}

fn body(ui: &mut Ui, ws: &mut Workspace, pal: &Palette, screen_h: f32) -> bool {
    let mut close = false;
    let Some(s) = ws.features.srs.session.clone() else {
        ui.heading("Flashcards");
        ui.label(RichText::new("Nothing to review.").color(pal.muted));
        return ui.button("Close").clicked();
    };
    ui.horizontal(|ui| {
        ui.heading(&s.title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close |= ui.button("End").on_hover_text("Escape — your answers so far are kept").clicked();
            let left = s.queue.len();
            ui.label(RichText::new(format!("{} reviewed · {left} to go", s.reviewed)).size(12.0).color(pal.muted));
        });
    });
    // Progress.
    let done = s.reviewed as f32 / (s.reviewed as f32 + s.queue.len() as f32).max(1.0);
    ui.add(egui::ProgressBar::new(done).desired_height(4.0).fill(pal.accent));
    ui.add_space(10.0);
    let card_h = (screen_h * 0.42).clamp(150.0, 360.0);
    let Some(card) = s.current().cloned() else {
        // Finished.
        egui::Frame::NONE.fill(pal.panel).corner_radius(8.0).inner_margin(18.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(card_h);
            ui.vertical_centered(|ui| {
                ui.add_space(card_h * 0.25);
                ui.label(RichText::new("Done for now").size(20.0).color(pal.strong));
                let again = if s.again > 0 { format!(" ({} answered Again)", s.again) } else { String::new() };
                ui.label(
                    RichText::new(format!("You reviewed {} card{}{again}.", s.reviewed, if s.reviewed == 1 { "" } else { "s" }))
                        .color(pal.muted),
                );
            });
        });
        ui.add_space(10.0);
        ui.vertical_centered(|ui| close |= ui.button(RichText::new("Close").strong()).clicked());
        return close;
    };
    let state = ws.features.srs.states.get(&card.card.id).copied();
    egui::Frame::NONE.fill(pal.panel).stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(8.0).inner_margin(18.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(card_h);
        egui::ScrollArea::vertical().id_salt("quire-review-card").max_height(card_h).auto_shrink([false, true]).show(ui, |ui| {
            let kind = match card.card.kind {
                CardKind::Basic | CardKind::MultiLine => "Question",
                CardKind::Cloze => "Fill in the blank",
            };
            ui.label(RichText::new(kind.to_uppercase()).size(10.5).color(pal.faint).strong());
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(&card.card.question).size(17.0).color(pal.strong)).wrap());
            if s.revealed {
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("ANSWER").size(10.5).color(pal.faint).strong());
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(&card.card.answer).size(17.0).color(pal.accent)).wrap());
                if card.card.kind == CardKind::Cloze {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(&card.card.context).size(13.0).color(pal.muted)).wrap());
                }
            }
        });
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let seen = state.map_or_else(
            || "New card".to_string(),
            |st| {
                format!(
                    "Reviewed {} time{} · last {}",
                    st.reviews,
                    if st.reviews == 1 { "" } else { "s" },
                    quire_app::chrono_fmt(st.last_review)
                )
            },
        );
        ui.label(RichText::new(format!("{} · {seen}", card.path.as_str().trim_end_matches(".md"))).size(11.5).color(pal.faint));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Open note").clicked() {
                let at = card.card.id_at;
                if ws.open_note(&card.path).is_some() {
                    ws.jump_to_byte(at);
                }
                close = true;
            }
        });
    });
    ui.add_space(8.0);
    if s.revealed {
        let preview = ws.srs_preview().unwrap_or_default();
        ui.columns(4, |cols| {
            for (i, r) in Rating::ALL.iter().enumerate() {
                let color = match r {
                    Rating::Again => pal.danger,
                    Rating::Hard => pal.warn,
                    Rating::Good => pal.accent,
                    Rating::Easy => pal.strong,
                };
                let label = RichText::new(format!("{}\n{}", r.label(), span_label(preview[i]))).color(color);
                let b = cols[i].add_sized([cols[i].available_width(), 40.0], egui::Button::new(label));
                if b.on_hover_text(format!("Key {}", i + 1)).clicked() {
                    ws.srs_answer(*r);
                }
            }
        });
        ui.label(RichText::new("1 Again · 2 Hard · 3 Good · 4 Easy").size(11.0).color(pal.faint));
    } else {
        ui.vertical_centered(|ui| {
            if ui.add_sized([220.0, 40.0], egui::Button::new(RichText::new("Show answer").strong())).clicked() {
                ws.srs_reveal();
            }
        });
        ui.vertical_centered(|ui| ui.label(RichText::new("Space").size(11.0).color(pal.faint)));
    }
    close
}

/// The flashcard review (`Command::StartReview`, `Command::ReviewThisNote`,
/// a note's "Review" button). The session is in the workspace.
pub(crate) struct ReviewOverlay(pub ReviewUi, pub Option<quire_app::srs_state::Session>);

impl OverlayView for ReviewOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        if let Some(s) = self.1.take() {
            ws.features.srs.session = Some(s);
        }
        if ws.features.srs.session.is_none() {
            return OverlayResult { close: true, ..Default::default() };
        }
        OverlayResult { close: self.0.show_modal(ctx, ws, pal), ..Default::default() }
    }

    fn closed(&mut self, ws: &mut Workspace) {
        ws.srs_end();
    }
}

impl ReviewOverlay {
    /// For `registry::UiTarget::Overlay`: the command started the session.
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self(ReviewUi::default(), None))
    }

    /// A session with a long question, at the question, the answer or the end.
    #[cfg(test)]
    pub(crate) fn sample(revealed: bool, finished: bool) -> Self {
        use quire_app::markdown::srs::Card;
        use quire_app::srs_state::{DeckCard, Session};
        use quire_app::vault::NotePath;
        let card = |i: usize| DeckCard {
            path: NotePath::new(format!("Biology/Cells {i}.md")),
            card: Card {
                id: format!("k_{i:012}"),
                kind: if i.is_multiple_of(2) { CardKind::Basic } else { CardKind::Cloze },
                question: "Which organelle makes most of the cell's ATP, and what is the name of the process it uses? ".repeat(3),
                answer: "The mitochondrion, by oxidative phosphorylation.".into(),
                context: "The mitochondrion makes ATP by oxidative phosphorylation.".into(),
                lines: 3..4,
                block_id: None,
                id_at: 0,
            },
        };
        let cards: Vec<DeckCard> = (0..12).map(card).collect();
        let queue = if finished { std::collections::VecDeque::new() } else { (0..12).collect() };
        let s = Session { title: "Biology".into(), cards, queue, revealed, reviewed: if finished { 12 } else { 3 }, again: 1, total: 12 };
        Self(ReviewUi::default(), Some(s))
    }
}
