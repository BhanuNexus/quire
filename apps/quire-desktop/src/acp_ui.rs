// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Connecting an external agent over ACP (design §D.6): the warning dialog,
//! which must be acknowledged before anything is started, and the
//! permission dialog shown when the agent asks to do something.
//!
//! Owned by WS-D (included by `agent_panel_ui.rs`).

use egui::{RichText, Stroke};
use quire_app::{Command, Workspace};
use quire_assistant::acp::{PRESETS, PermissionRequest, WARNING, may_allow_always};

use super::Pending;
use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// "Connect an external agent (ACP)…".
pub(crate) struct ConnectOverlay {
    preset: Option<usize>,
    command: String,
    args: String,
    understood: bool,
}

impl ConnectOverlay {
    /// For `registry::UiTarget::Overlay` (`Command::ConnectExternalAgent`).
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let p = &ws.settings.agent_panel;
        let preset = PRESETS.iter().position(|(_, c)| *c == p.acp_command);
        Box::new(Self { preset, command: p.acp_command.clone(), args: p.acp_args.join(" "), understood: false })
    }

    #[cfg(test)]
    pub(crate) fn sample(understood: bool) -> Self {
        Self {
            preset: None,
            command: "/usr/local/bin/my-agent --with a long command line that wraps".into(),
            args: "--acp --verbose".into(),
            understood,
        }
    }
}

impl OverlayView for ConnectOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(egui::Id::unique("quire-acp-connect")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 560.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.label(RichText::new("Connect an external agent").size(20.0).strong().color(pal.strong));
            ui.add_space(4.0);
            ui.label(RichText::new("The agent panel can talk to a coding agent you have installed, over the Agent Client Protocol.").color(pal.muted));
            ui.add_space(10.0);
            egui::Grid::new("quire-acp-form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Agent");
                ui.horizontal_wrapped(|ui| {
                    for (i, (label, cmd)) in PRESETS.iter().enumerate() {
                        if ui.selectable_label(self.preset == Some(i), *label).clicked() {
                            self.preset = Some(i);
                            self.command = (*cmd).to_string();
                            self.args.clear();
                        }
                    }
                    if ui.selectable_label(self.preset.is_none(), "Other command").clicked() {
                        self.preset = None;
                    }
                });
                ui.end_row();
                ui.label("Command");
                ui.add(egui::TextEdit::singleline(&mut self.command).hint_text("my-agent-acp").desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Arguments");
                ui.add(egui::TextEdit::singleline(&mut self.args).hint_text("optional").desired_width(f32::INFINITY));
                ui.end_row();
            });
            ui.add_space(10.0);
            egui::Frame::NONE.fill(pal.warn.gamma_multiply(0.12)).stroke(Stroke::new(1.0, pal.warn)).corner_radius(8.0).inner_margin(12.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("⚠ Before you connect").strong().color(pal.warn));
                ui.add_space(2.0);
                ui.add(egui::Label::new(RichText::new(WARNING).color(pal.text)).wrap());
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(
                        RichText::new("Its edits to your notes through Quire arrive as suggestions. When it asks to run commands, fetch from the web or delete, Quire's answer starts on Deny.")
                            .size(12.0)
                            .color(pal.muted),
                    )
                    .wrap(),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.understood, "I understand that this agent runs with my permissions");
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = self.understood && !self.command.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new(RichText::new("Connect").strong())).clicked() {
                        let p = &mut ws.settings.agent_panel;
                        p.backend = "acp".into();
                        p.acp_command = self.command.trim().to_string();
                        p.acp_args = self.args.split_whitespace().map(str::to_string).collect();
                        p.acp_risk_ack = true;
                        p.enabled = true;
                        let _ = ws.settings.save();
                        res.close = true;
                        res.run = Some(Command::ToggleAgentPanel);
                    }
                });
            });
        });
        res
    }
}

fn kind_words(kind: &str) -> &'static str {
    match kind {
        "execute" => "run a command",
        "fetch" => "fetch from the web",
        "delete" => "delete files",
        "edit" => "edit files",
        "move" => "move files",
        "read" => "read files",
        "search" => "search",
        "think" => "think",
        _ => "do something",
    }
}

/// The permission question as a dialog; true when answered (or stopped).
pub(crate) fn permission_modal(ctx: &egui::Context, p: &mut Pending, pal: &Palette) -> bool {
    let mut answer: Option<Choice> = None;
    let default = p.req.default_option().map(|o| o.id.clone());
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
        answer = Some(default.clone().map_or(Choice::Cancelled, Choice::Option));
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        answer = Some(Choice::Cancelled);
    }
    egui::Modal::new(egui::Id::unique("quire-acp-permission")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(
        ctx,
        |ui| {
            if let Some(a) = permission_body(ui, ctx, &p.req, default.as_deref(), pal) {
                answer = Some(a);
            }
        },
    );
    if let Some(a) = answer {
        if let Some(tx) = p.reply.take() {
            let _ = tx.send(match a {
                Choice::Option(id) => Some(id),
                Choice::Cancelled => None,
            });
        }
        return true;
    }
    false
}

/// What the person answered in the permission dialog.
enum Choice {
    /// One of the agent's options, by id.
    Option(String),
    /// Escape, or no option to choose: the agent is told the request was cancelled.
    Cancelled,
}

/// The dialog's content; returns the person's answer once they give one.
fn permission_body(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    req: &PermissionRequest,
    default: Option<&str>,
    pal: &Palette,
) -> Option<Choice> {
    let mut answer = None;
    let w = 520.0_f32.min(ctx.content_rect().width() - 48.0);
    ui.set_width(w);
    ui.set_max_width(w);
    let risky = quire_assistant::acp::denied_by_default(&req.kind);
    ui.label(RichText::new(format!("The external agent wants to {}", kind_words(&req.kind))).size(18.0).strong().color(if risky {
        pal.warn
    } else {
        pal.strong
    }));
    ui.add_space(4.0);
    ui.add(egui::Label::new(RichText::new(&req.title).color(pal.text)).wrap());
    if !req.detail.trim().is_empty() {
        ui.add_space(6.0);
        egui::Frame::NONE.fill(pal.bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("quire-acp-perm-detail").max_height(160.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(req.detail.trim()).monospace().size(12.0).color(pal.text)).wrap());
            });
        });
    }
    if risky {
        ui.add_space(6.0);
        ui.label(RichText::new("Quire can't see or undo what this does. Deny unless you expected it.").size(12.0).color(pal.warn));
    }
    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        for o in req.offered() {
            let is_default = default == Some(o.id.as_str());
            let mut text = RichText::new(&o.name);
            if is_default {
                text = text.strong();
            }
            let b = ui.add(egui::Button::new(text).stroke(if is_default { Stroke::new(1.5, pal.accent) } else { Stroke::NONE }));
            let b = if o.kind.ends_with("_always") && may_allow_always(&req.kind) {
                b.on_hover_text("For the rest of this session")
            } else {
                b
            };
            if b.clicked() {
                answer = Some(Choice::Option(o.id.clone()));
            }
        }
    });
    ui.label(RichText::new("Enter chooses the highlighted answer · Escape cancels").size(11.0).color(pal.faint));
    answer
}

/// The permission dialog as a stand-alone overlay (for `layout_check`).
#[cfg(test)]
pub(crate) struct PermissionOverlay(pub(crate) PermissionRequest);

#[cfg(test)]
impl PermissionOverlay {
    pub(crate) fn sample(kind: &str) -> Self {
        use quire_assistant::acp::PermissionOption;
        let opt = |id: &str, name: &str, kind: &str| PermissionOption { id: id.into(), name: name.into(), kind: kind.into() };
        Self(PermissionRequest {
            title: "Run `cargo test --workspace` in the vault folder".into(),
            kind: kind.into(),
            detail: "cargo test --workspace --all-targets -- --nocapture\n".repeat(12),
            options: vec![opt("a1", "Allow", "allow_once"), opt("a2", "Always allow", "allow_always"), opt("r1", "Deny", "reject_once")],
        })
    }
}

#[cfg(test)]
impl OverlayView for PermissionOverlay {
    fn show(&mut self, ctx: &egui::Context, _ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let default = self.0.default_option().map(|o| o.id.clone());
        egui::Modal::new(egui::Id::unique("quire-acp-permission")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(
            ctx,
            |ui| {
                res.close = permission_body(ui, ctx, &self.0, default.as_deref(), pal).is_some();
            },
        );
        res
    }
}
