// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hunk review in the editor (design §7.3): the widget under each
//! suggested change, the bar under a note's title when suggestions wait,
//! and the review card at the top of the Inbox tab.
//!
//! The lines a hunk replaces are tinted and struck through by the editor
//! (decorations from `quire_app::agent_state`); this file draws what goes
//! under them: the agent's lines, **✓ Accept · ✕ Reject · ✎ Edit**, the
//! conflict warning, and the inline editor. Owned by WS-A.

use egui::{Align, Color32, Key, KeyboardShortcut, Layout, Modifiers, RichText, Stroke};
use quire_app::Owner;
use quire_app::Workspace;
use quire_app::agent::{ChangeOp, Proposal};
use quire_app::agent_state::review::{HunkAct, ReviewSession, decode, display_lines, encode};
use quire_app::agent_state::{TITLE_WIDGET, inbox};
use quire_app::decor::{ActionId, WidgetId};

use crate::panels::PanelOut;
use crate::theme::Palette;

/// Green for proposed text (the theme has no "added" colour of its own).
pub(crate) fn added(pal: &Palette) -> Color32 {
    if pal.dark { Color32::from_rgb(0x5f, 0xc8, 0x7a) } else { Color32::from_rgb(0x1f, 0x80, 0x3a) }
}

fn added_bg(pal: &Palette) -> Color32 {
    added(pal).gamma_multiply(if pal.dark { 0.16 } else { 0.10 })
}

fn conflict_bg(pal: &Palette) -> Color32 {
    pal.warn.gamma_multiply(if pal.dark { 0.18 } else { 0.12 })
}

fn act(gen_: u64, k: usize, a: HunkAct) -> ActionId {
    ActionId { owner: Owner::Agent, id: encode(gen_, k, a) }
}

/// A small text button with a hover hint (and its keys).
fn tool_button(ui: &mut egui::Ui, label: &str, color: Color32, hint: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).size(12.5).color(color)).corner_radius(4.0)).on_hover_text(hint).clicked()
}

/// Draw WS-A's widget `id` under a line. Returns its height and a clicked action.
pub(crate) fn paint_widget(ui: &mut egui::Ui, ws: &Workspace, id: WidgetId, pal: &Palette) -> (f32, Option<ActionId>) {
    if id.id & 0xFFFF_0000 == TITLE_WIDGET && id.id >> 32 == 0 {
        return title_bar(ui, ws, id.id, pal);
    }
    let Some((gen_, k, _)) = decode(id.id) else { return (0.0, None) };
    let Some(r) = ws.features.agent.review.as_ref() else { return (0.0, None) };
    if r.hunk_for(gen_, k).is_none() {
        return (0.0, None);
    }
    hunk_widget(ui, ws, r, gen_, k, pal)
}

/// The bar under a note's first line: an agent suggested changes here.
fn title_bar(ui: &mut egui::Ui, ws: &Workspace, id: u64, pal: &Palette) -> (f32, Option<ActionId>) {
    let i = usize::try_from(id & 0xFFFF).unwrap_or(usize::MAX);
    let Some(p) = ws.features.agent.inbox.pending.get(i) else { return (0.0, None) };
    let mut clicked = None;
    let r = egui::Frame::NONE
        .fill(added_bg(pal))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .outer_margin(egui::Margin { top: 4, bottom: 6, ..egui::Margin::ZERO })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} suggested changes to this note", p.actor.label)).color(pal.text).size(13.0));
                ui.label(RichText::new(format!("· {}", p.title)).color(pal.muted).size(13.0));
                if ui.button(RichText::new("Review").size(12.5)).on_hover_text("Show the changes here, one by one").clicked() {
                    clicked = Some(ActionId { owner: Owner::Agent, id });
                }
            });
        })
        .response;
    (r.rect.height() + 10.0, clicked)
}

/// One hunk: the agent's lines and the toolbar (or the inline editor).
fn hunk_widget(ui: &mut egui::Ui, ws: &Workspace, r: &ReviewSession, gen_: u64, k: usize, pal: &Palette) -> (f32, Option<ActionId>) {
    let Some(h) = r.hunks.get(k) else { return (0.0, None) };
    let current = r.current == k;
    let editing = r.editing == Some(k);
    let size = (ws.settings.font_size * 0.92).clamp(11.0, 22.0);
    let mut clicked = None;
    let stripe = if h.conflict { pal.warn } else { added(pal) };
    let frame = egui::Frame::NONE
        .fill(if h.conflict { conflict_bg(pal) } else { added_bg(pal) })
        .stroke(if current { Stroke::new(1.5, stripe) } else { Stroke::new(1.0, stripe.gamma_multiply(0.35)) })
        .corner_radius(4.0)
        .inner_margin(egui::Margin { left: 10, right: 8, top: 5, bottom: 5 })
        .outer_margin(egui::Margin { top: 2, bottom: 6, ..egui::Margin::ZERO });
    let resp = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if h.conflict {
            ui.label(
                RichText::new("⚠ You changed this after the agent read it. Accepting replaces your version of these lines.")
                    .size(12.0)
                    .color(pal.warn),
            );
        }
        let old_empty = h.old_lines.is_empty();
        if old_empty && h.range_in_current.start == 0 {
            ui.label(RichText::new("Add at the top of the note:").size(12.0).color(pal.muted));
        }
        if editing {
            edit_box(ui, r, gen_, k, size, pal, &mut clicked);
            return;
        }
        if h.new_lines.is_empty() {
            let n = h.old_lines.len();
            ui.label(
                RichText::new(if n == 1 { "Remove this line".to_string() } else { format!("Remove these {n} lines") })
                    .size(12.5)
                    .color(pal.muted),
            );
        } else {
            for line in display_lines(&h.new_lines) {
                let shown = if line.is_empty() { " ".to_string() } else { line };
                ui.add(egui::Label::new(RichText::new(shown).size(size).color(pal.text)).wrap());
            }
        }
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let total = r.hunks.len();
            ui.label(RichText::new(format!("{} · {} of {total}", r.proposal.actor.label, k + 1)).size(11.5).color(pal.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if tool_button(ui, "✎ Edit", pal.text, "Change the suggested text before accepting it") {
                    clicked = Some(act(gen_, k, HunkAct::Edit));
                }
                if tool_button(ui, "✕ Reject", pal.danger, "Keep the note as it is (Ctrl+Alt+Backspace)") {
                    clicked = Some(act(gen_, k, HunkAct::Reject));
                }
                let hint = if h.conflict {
                    "Use the agent's version; your edits to these lines are replaced (Ctrl+Alt+Y). Undo brings them back."
                } else {
                    "Put this change into the note (Ctrl+Alt+Y). Undo takes it out again."
                };
                if tool_button(ui, "✓ Accept", added(pal), hint) {
                    clicked = Some(act(gen_, k, HunkAct::Accept));
                }
            });
        });
    });
    if !current && clicked.is_none() && resp.response.interact(egui::Sense::click()).clicked() {
        clicked = Some(act(gen_, k, HunkAct::Select));
    }
    (resp.response.rect.height() + 8.0, clicked)
}

/// The inline editor: Enter accepts the edited text, Shift+Enter starts a
/// new line, Escape cancels.
fn edit_box(ui: &mut egui::Ui, r: &ReviewSession, gen_: u64, k: usize, size: f32, pal: &Palette, clicked: &mut Option<ActionId>) {
    let rows = r.edit_text.borrow().lines().count().clamp(1, 12);
    let te_id = egui::Id::unique(("quire-hunk-edit", gen_, k));
    let resp = {
        let mut text = r.edit_text.borrow_mut();
        ui.add(
            egui::TextEdit::multiline(&mut *text)
                .id(te_id)
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .font(egui::FontId::proportional(size))
                .return_key(Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))),
        )
    };
    if r.edit_focus.replace(false) {
        resp.request_focus();
    }
    let (enter, esc) = ui.input(|i| (i.key_pressed(Key::Enter) && !i.modifiers.shift, i.key_pressed(Key::Escape)));
    if resp.has_focus() && enter {
        *clicked = Some(act(gen_, k, HunkAct::ConfirmEdit));
    } else if (resp.has_focus() || resp.lost_focus()) && esc {
        *clicked = Some(act(gen_, k, HunkAct::CancelEdit));
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Enter accepts · Shift+Enter new line · Esc cancels").size(11.5).color(pal.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if tool_button(ui, "Cancel", pal.text, "Back to the suggestion (Esc)") {
                *clicked = Some(act(gen_, k, HunkAct::CancelEdit));
            }
            if tool_button(ui, "✓ Accept edited", added(pal), "Put your version into the note (Enter)") {
                *clicked = Some(act(gen_, k, HunkAct::ConfirmEdit));
            }
        });
    });
}

/// What one change of a proposal is, in words.
pub(crate) fn change_words(c: &quire_app::agent::proposal::ProposalChange) -> String {
    let name = |p: Option<&quire_app::vault::NotePath>| p.map_or_else(String::new, |p| p.as_str().to_string());
    match c.op {
        ChangeOp::Edit => format!("Edit {}", name(c.path.as_ref())),
        ChangeOp::Create => format!("New note {}", name(c.path.as_ref())),
        ChangeOp::Rename => format!("Rename {} → {}", name(c.from.as_ref()), name(c.to.as_ref())),
        ChangeOp::Delete => format!("Delete {}", name(c.path.as_ref())),
    }
}

/// The agent's reason, shown as plain text, never interpreted (P8).
pub(crate) fn rationale(ui: &mut egui::Ui, p: &Proposal, pal: &Palette) {
    if p.rationale.trim().is_empty() {
        return;
    }
    let mut text: String = p.rationale.trim().chars().take(600).collect();
    if p.rationale.trim().chars().count() > 600 {
        text.push('…');
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("Why:").size(12.5).color(pal.muted).italics());
        ui.add(egui::Label::new(RichText::new(text).size(12.5).color(pal.text)).wrap().selectable(true));
    });
}

/// The red chip on suggestions based on untrusted content (§A.3).
pub(crate) fn untrusted_chip(ui: &mut egui::Ui, p: &Proposal, pal: &Palette) {
    let Some(src) = &p.untrusted_source else { return };
    let instructions = p.changes.iter().any(|c| {
        c.paths().iter().any(|x| quire_app::agent::instructions::is_instructions_name(x) || x.file_name().eq_ignore_ascii_case("SKILL.md"))
    });
    let text = if instructions {
        format!("⚠ Changes agent instructions and is based on untrusted content ({src})")
    } else {
        format!("Based on untrusted content ({src})")
    };
    egui::Frame::NONE.fill(pal.danger.gamma_multiply(0.15)).corner_radius(10.0).inner_margin(egui::Margin::symmetric(8, 2)).show(
        ui,
        |ui| {
            ui.label(RichText::new(text).size(11.5).color(pal.danger));
        },
    );
}

/// Something to do with the workspace after drawing.
type Later = Box<dyn FnOnce(&mut Workspace)>;

/// The review card at the top of the Inbox tab while a proposal is under
/// review: who and why, where you are, Accept all / Reject all / Done, and
/// the proposal's other notes (new notes, renames, deletions).
pub(crate) fn review_card(ui: &mut egui::Ui, ws: &mut Workspace, preview: &mut Option<String>, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(r) = ws.features.agent.review.as_ref() else { return out };
    if r.reveal.replace(false) {
        out.reveal_only = true;
    }
    let p = r.proposal.clone();
    let (n, current, conflicts, path) = (r.hunks.len(), r.current, r.conflicts(), r.path.clone());
    let pending = p.pending_changes().count();
    let mut action: Option<Later> = None;
    egui::Frame::NONE.fill(pal.raised).stroke(Stroke::new(1.0, added(pal).gamma_multiply(0.6))).corner_radius(8.0).inner_margin(10.0).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            let total: usize = n + p.pending_changes().filter(|c| c.path != path || !matches!(c.op, ChangeOp::Edit)).count();
            ui.label(
                RichText::new(format!("{} suggests {} change{}", p.actor.label, total.max(1), if total == 1 { "" } else { "s" }))
                    .strong()
                    .color(pal.strong),
            );
            ui.label(RichText::new(&p.title).color(pal.text));
            rationale(ui, &p, pal);
            untrusted_chip(ui, &p, pal);
            ui.add_space(4.0);
            if n > 0 {
                ui.horizontal(|ui| {
                    if ui.button("◀ Prev").on_hover_text("Previous change (Alt+[)").clicked() {
                        action = Some(Box::new(|ws: &mut Workspace| ws.review_step(-1)));
                    }
                    ui.label(RichText::new(format!("{} of {n}", current + 1)).color(pal.muted));
                    if ui.button("Next ▶").on_hover_text("Next change (Alt+])").clicked() {
                        action = Some(Box::new(|ws: &mut Workspace| ws.review_step(1)));
                    }
                });
                if conflicts > 0 {
                    ui.label(
                        RichText::new(format!("{conflicts} of them conflict with your edits (amber); Accept all leaves those for you"))
                            .size(12.0)
                            .color(pal.warn),
                    );
                }
            }
            ui.horizontal_wrapped(|ui| {
                if n > 0 {
                    if ui
                        .button("Accept all in note")
                        .on_hover_text("Every change in this note except conflicts (Ctrl+Alt+Shift+Y)")
                        .clicked()
                    {
                        action = Some(Box::new(|ws: &mut Workspace| ws.review_all_in_file(true)));
                    }
                    if ui.button("Reject all in note").clicked() {
                        action = Some(Box::new(|ws: &mut Workspace| ws.review_all_in_file(false)));
                    }
                }
                if ui.button("Done").on_hover_text("Stop reviewing; what's left stays in the Inbox").clicked() {
                    action = Some(Box::new(Workspace::end_review));
                }
            });
            if pending > 1 || path.is_none() {
                ui.add_space(6.0);
                ui.label(RichText::new("IN THIS SUGGESTION").size(11.0).color(pal.muted).strong());
                for c in p.pending_changes() {
                    let Some(subject) = c.subject().cloned() else { continue };
                    let here = path.as_ref() == Some(&subject);
                    let shown = subject.clone();
                    ui.horizontal_wrapped(|ui| {
                        let words = change_words(c);
                        let label =
                            if here { RichText::new(format!("• {words}")).strong() } else { RichText::new(format!("• {words}")) };
                        ui.label(label.size(12.5).color(pal.text));
                        let id = p.id.clone();
                        match c.op {
                            ChangeOp::Edit if !here => {
                                if ui.small_button("Review").clicked() {
                                    action = Some(Box::new(move |ws: &mut Workspace| {
                                        ws.start_review(&id, Some(&subject));
                                    }));
                                }
                            }
                            ChangeOp::Edit => {}
                            _ => {
                                if c.op == ChangeOp::Create
                                    && ui
                                        .small_button(if preview.as_deref() == Some(subject.as_str()) { "Hide" } else { "Preview" })
                                        .clicked()
                                {
                                    *preview = if preview.as_deref() == Some(subject.as_str()) { None } else { Some(subject.to_string()) };
                                }
                                let (s1, s2, id2) = (subject.clone(), subject.clone(), id.clone());
                                if ui.small_button("Accept").clicked() {
                                    action = Some(Box::new(move |ws: &mut Workspace| ws.resolve_one_change(&id, &s1, true)));
                                }
                                if ui.small_button("Reject").clicked() {
                                    action = Some(Box::new(move |ws: &mut Workspace| ws.resolve_one_change(&id2, &s2, false)));
                                }
                            }
                        }
                    });
                    if c.op == ChangeOp::Create && preview.as_deref() == Some(shown.as_str()) {
                        create_preview(ui, c.text.as_deref().unwrap_or_default(), pal);
                    }
                }
                ui.horizontal(|ui| {
                    let (a, b) = (p.id.clone(), p.id.clone());
                    if ui.button("Accept everything").on_hover_text("Every change of this suggestion, except conflicts").clicked() {
                        action = Some(Box::new(move |ws: &mut Workspace| ws.resolve_whole_proposal(&a, true)));
                    }
                    if ui.button("Reject everything").clicked() {
                        action = Some(Box::new(move |ws: &mut Workspace| ws.resolve_whole_proposal(&b, false)));
                    }
                });
            }
        },
    );
    if let Some(f) = action {
        f(ws);
        out.reveal_only = true;
    }
    ui.add_space(8.0);
    out
}

/// A new note's text, read-only.
pub(crate) fn create_preview(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    egui::Frame::NONE.fill(pal.bg).stroke(Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(("quire-create-preview", text.len())).max_height(220.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).monospace().size(12.0).color(pal.text)).wrap().selectable(true));
        });
    });
}

/// How a group of suggestions is introduced: "Claude Code · 14:02".
pub(crate) fn group_line(g: &inbox::Group) -> String {
    format!("{} · {}", g.actor_label, short_time(g.at_ms))
}

/// "14:02" today, "Mon 14:02" this week, else the date.
pub(crate) fn short_time(ms: i64) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(ms) else { return String::new() };
    let t = t.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    let days = (now.date_naive() - t.date_naive()).num_days();
    if days == 0 {
        t.format("%H:%M").to_string()
    } else if (1..7).contains(&days) {
        t.format("%a %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use quire_app::agent::{Actor, Change, ChangeSet, Via};
    use quire_app::vault::{NotePath, Vault};

    use super::*;

    #[test]
    fn widgets_draw_in_both_themes() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Rope.md", "# Rope\nOne.\nTwo.\n")]));
        let store = ws.agent_store().unwrap().clone();
        let agent = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d");
        let cs = ChangeSet {
            rationale: "Because.".into(),
            ..ChangeSet::one(Change::Edit { path: "Rope.md".into(), new_text: "# Rope\nOne!\nTwo.\nThree.\n".into() })
        };
        let quire_app::agent::SubmitOutcome::Proposed { id, .. } = store.submit(&agent, cs).unwrap() else { panic!() };
        assert!(ws.start_review(&id, Some(&NotePath::new("Rope.md"))));
        let r = ws.features.agent.review.as_ref().unwrap();
        assert_eq!(r.hunks.len(), 2);
        let gen_ = r.generation & 0xFF_FFFF;
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        for pal in [&crate::theme::DARK, &crate::theme::LIGHT] {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                for k in 0..2 {
                    let (h, c) = paint_widget(ui, &ws, WidgetId { owner: Owner::Agent, id: encode(gen_, k, HunkAct::Select) }, pal);
                    assert!(h > 20.0 && c.is_none());
                }
                let (h, _) = paint_widget(ui, &ws, WidgetId { owner: Owner::Agent, id: encode(gen_ + 1, 0, HunkAct::Select) }, pal);
                assert!(h < 0.01, "an old generation draws nothing");
            });
            out.textures_delta.clear();
        }
    }
}
