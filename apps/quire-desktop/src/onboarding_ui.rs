// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The first-run AI choice (design §7.1): a two-step dialog after the tour
//! of a new install (and from "Set up AI…"), and a dismissible card above
//! the note for people upgrading from 0.8.
//!
//! Step 1 offers No AI, a provider, or models on this computer, with nothing
//! preselected. Step 2 (on this computer) lists each capability with its
//! download size and memory, all unticked.
//!
//! Owned by WS-D.

use egui::{RichText, Stroke};
use quire_app::ai::{catalog, installed};
use quire_app::onboarding::{DeviceChoice, LlmSize};
use quire_app::{Command, Workspace};

use crate::overlays::{Overlay, OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Off,
    Provider,
    Device,
}

/// The dialog.
pub(crate) struct OnboardingOverlay {
    step: u8,
    choice: Option<Choice>,
    device: DeviceChoice,
    /// The person chose (or skipped); closing doesn't record it again.
    decided: bool,
}

impl OnboardingOverlay {
    /// For `registry::UiTarget::Overlay` (`Command::AiSetup`).
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self { step: 1, choice: None, device: DeviceChoice::default(), decided: false })
    }

    /// Step 1 with a card chosen, or step 2 with some boxes ticked.
    #[cfg(test)]
    pub(crate) fn sample(step: u8) -> Self {
        let device =
            if step == 2 { DeviceChoice { embeddings: true, llm: true, ..DeviceChoice::default() } } else { DeviceChoice::default() };
        Self { step, choice: Some(Choice::Device), device, decided: false }
    }
}

/// Approximate sizes (bytes) for models whose exact size isn't pinned in
/// this build, from the design's table.
fn approx_size(id: &str) -> u64 {
    match id {
        "engine" => 18_000_000,
        "bge-small-en-v1.5" => 130_000_000,
        "multilingual-e5-small" => 470_000_000,
        "qwen3-0.6b-q8" => 640_000_000,
        "qwen3-1.7b-q4km" => 1_100_000_000,
        "qwen3-4b-q4km" => 2_500_000_000,
        "ocrs" => 13_000_000,
        _ => 0,
    }
}

/// A model's download size and whether it is exact.
pub(crate) fn size_of(id: &str) -> (u64, bool) {
    match catalog().get(id) {
        Some(e) if e.is_pinned() => (e.size(), true),
        _ => (approx_size(id), false),
    }
}

/// "for 16 GB of memory or more", from the catalog.
pub(crate) fn ram_text(id: &str) -> String {
    catalog().get(id).filter(|e| e.min_ram_gb > 0).map_or_else(String::new, |e| format!("for {} GB of memory or more", e.min_ram_gb))
}

pub(crate) fn size_text(id: &str) -> String {
    let (n, exact) = size_of(id);
    let s = quire_app::ai::human_size(n);
    if exact { s } else { format!("about {s}") }
}

/// What a row of step two says after the model's name: its size, and
/// "not available in this build" when this build can't download it.
fn option_text(id: &str, more: &str) -> String {
    let mut t = size_text(id);
    if !more.is_empty() {
        t = format!("{t} · {more}");
    }
    if catalog().get(id).is_none_or(|e| !e.is_pinned() && !installed(e)) {
        t.push_str(" · not available in this build");
    }
    t
}

fn card(ui: &mut egui::Ui, pal: &Palette, selected: bool, w: f32, h: f32, title: &str, body: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let fill = if selected {
        pal.active_bg
    } else if resp.hovered() {
        pal.hover
    } else {
        pal.bg
    };
    let stroke = if selected { Stroke::new(2.0, pal.accent) } else { Stroke::new(1.0, pal.border) };
    ui.painter().rect(rect, 10.0, fill, stroke, egui::StrokeKind::Inside);
    let inner = rect.shrink2(egui::vec2(16.0, 14.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
    child.label(RichText::new(title).size(16.0).strong().color(pal.strong));
    child.add_space(6.0);
    child.add(egui::Label::new(RichText::new(body).size(13.0).color(pal.text)).wrap());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

const OFF_BODY: &str = "Everything stays as it is: search, links and suggestions work on this computer without AI models.";
const PROVIDER_BODY: &str =
    "Claude, OpenAI, or a server you run (Ollama, LM Studio). Your notes are sent to it only when you use an AI feature.";
const DEVICE_BODY: &str = "Download small models once; after that they work offline. Nothing leaves this computer.";

impl OverlayView for OnboardingOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(egui::Id::unique("quire-onboarding-ai")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(
            ctx,
            |ui| {
                let w = 700.0_f32.min(ctx.content_rect().width() - 48.0);
                ui.set_width(w);
                ui.set_max_width(w);
                if self.step == 1 {
                    self.step_one(ui, ws, pal, w, &mut res);
                } else {
                    self.step_two(ui, ws, pal, &mut res);
                }
            },
        );
        res
    }

    fn closed(&mut self, ws: &mut Workspace) {
        // Escape or Skip: don't ask again; Settings → AI is always there.
        if !self.decided && !ws.settings.onboarding.ai_done {
            ws.settings.onboarding.ai_done = true;
            ws.settings.onboarding.upgrade_card_done = true;
            let _ = ws.settings.save();
        }
    }
}

impl OnboardingOverlay {
    fn step_one(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, w: f32, res: &mut OverlayResult) {
        ui.label(RichText::new("Would you like AI in Quire?").size(22.0).strong().color(pal.strong));
        ui.add_space(2.0);
        ui.label(RichText::new("Pick one. You can change it any time in Settings → AI.").color(pal.muted));
        ui.add_space(14.0);
        let narrow = w < 560.0;
        let cards = [
            (Choice::Off, "No AI", OFF_BODY),
            (Choice::Provider, "Use a provider", PROVIDER_BODY),
            (Choice::Device, "On this computer", DEVICE_BODY),
        ];
        if narrow {
            for (c, t, b) in cards {
                if card(ui, pal, self.choice == Some(c), w, 92.0, t, b) {
                    self.choice = Some(c);
                }
                ui.add_space(8.0);
            }
        } else {
            let cw = (w - 2.0 * 10.0) / 3.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                for (c, t, b) in cards {
                    if card(ui, pal, self.choice == Some(c), cw, 150.0, t, b) {
                        self.choice = Some(c);
                    }
                }
            });
            ui.add_space(14.0);
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Skip").color(pal.muted)).on_hover_text("Decide later in Settings → AI").clicked() {
                res.close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match self.choice {
                    Some(Choice::Provider) => "Set up a provider",
                    Some(Choice::Device) => "Next",
                    _ => "Done",
                };
                let go = ui.add_enabled(self.choice.is_some(), egui::Button::new(RichText::new(label).strong()));
                if go.clicked() {
                    match self.choice {
                        Some(Choice::Off) => {
                            ws.onboarding_no_ai();
                            self.decided = true;
                            res.close = true;
                        }
                        Some(Choice::Provider) => {
                            ws.onboarding_provider();
                            self.decided = true;
                            if ws.settings.ai_provider == "local" {
                                ws.settings.ai_provider = "anthropic".into();
                                ws.apply_ai_settings();
                            }
                            res.then = Some(Overlay::Settings(Box::new(crate::dialogs::SettingsState::feature_tab("ai"))));
                        }
                        Some(Choice::Device) => self.step = 2,
                        None => {}
                    }
                }
            });
        });
    }

    fn step_two(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, res: &mut OverlayResult) {
        ui.label(RichText::new("What should run on this computer?").size(22.0).strong().color(pal.strong));
        ui.add_space(2.0);
        ui.label(RichText::new("Each model downloads once and then works offline. Your notes stay on this computer.").color(pal.muted));
        ui.add_space(12.0);
        let d = &mut self.device;
        // The list scrolls on short windows; the totals and buttons stay put.
        let list_h = (ui.ctx().content_rect().height() - 330.0).max(120.0);
        egui::ScrollArea::vertical().id_salt("quire-onboarding-capabilities").max_height(list_h).auto_shrink([false, true]).show(
            ui,
            |ui| {
                capability(ui, pal, &mut d.embeddings, "Meaning search", "Better “find by meaning” and related notes. Light.", |ui| {
                    ui.radio_value(&mut d.multilingual, false, format!("English · {}", option_text("bge-small-en-v1.5", "")));
                    ui.radio_value(&mut d.multilingual, true, format!("Many languages · {}", option_text("multilingual-e5-small", "")));
                });
                capability(ui, pal, &mut d.llm, "Writing and agent", "Summaries, tags, the agent panel and claim checks.", |ui| {
                    for (size, name) in [(LlmSize::Small, "Small"), (LlmSize::Balanced, "Balanced"), (LlmSize::Better, "Better")] {
                        ui.radio_value(&mut d.llm_size, size, format!("{name} · {}", option_text(size.id(), &ram_text(size.id()))));
                    }
                });
                capability(
                    ui,
                    pal,
                    &mut d.ocr,
                    "Read text in images and scanned PDFs",
                    "Makes their text searchable. Latin script only.",
                    |ui| {
                        ui.label(RichText::new(option_text("ocrs", "")).color(pal.muted));
                    },
                );
            },
        );
        ui.add_space(6.0);
        let models = d.models();
        let engine_needed = !models.is_empty() && !quire_app::ai::engine_installed();
        let mut total: u64 = models.iter().map(|id| size_of(id).0).sum();
        if engine_needed {
            total += size_of("engine").0;
        }
        let exact = models.iter().all(|id| size_of(id).1) && (!engine_needed || size_of("engine").1);
        let total_s = quire_app::ai::human_size(total);
        let engine_s = if engine_needed {
            format!(" including the {} engine", size_text("engine").trim_start_matches("about "))
        } else {
            String::new()
        };
        ui.label(
            RichText::new(if models.is_empty() {
                "Tick what you want. You can change this any time in Settings → AI.".to_string()
            } else {
                format!("Total: {}{total_s}{engine_s} · You can change this any time in Settings → AI", if exact { "" } else { "about " })
            })
            .color(pal.text),
        );
        let all_available = models.iter().all(|id| catalog().get(id).is_some_and(|e| e.is_pinned() || installed(e)));
        let can_download = !models.is_empty()
            && all_available
            && (!engine_needed || catalog().get("engine").is_some_and(quire_ai::catalog::CatalogEntry::is_pinned));
        if !models.is_empty() && !can_download {
            ui.label(
                RichText::new("This build of Quire can't download these models yet. Your choices are saved; install the files with “Install from file…” in Settings → AI, or download them from a release build.")
                    .size(12.0)
                    .color(pal.warn),
            );
        }
        ui.label(RichText::new("Models download from Hugging Face, and the engine from Quire's releases on GitHub. Each file is checked against the hash built into Quire.").size(11.5).color(pal.faint));
        ui.add_space(8.0);
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                self.step = 1;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dl = ui.add_enabled(can_download, egui::Button::new(RichText::new("Download now").strong()));
                if dl.clicked() {
                    ws.onboarding_device(self.device, true);
                    self.decided = true;
                    res.close = true;
                }
                if ui
                    .add_enabled(!models.is_empty(), egui::Button::new("Later"))
                    .on_hover_text("Save these choices; start the downloads from Settings → AI")
                    .clicked()
                {
                    ws.onboarding_device(self.device, false);
                    self.decided = true;
                    res.close = true;
                }
            });
        });
    }
}

/// A capability row: a checkbox with a title and a line, and its options
/// shown under it while ticked.
fn capability(ui: &mut egui::Ui, pal: &Palette, on: &mut bool, title: &str, what: &str, options: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, if *on { pal.accent } else { pal.border }))
        .corner_radius(8.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.checkbox(on, RichText::new(title).size(15.0).strong().color(pal.strong));
            ui.indent(title, |ui| {
                ui.label(RichText::new(what).color(pal.text));
                ui.add_enabled_ui(*on, |ui| {
                    ui.add_space(2.0);
                    options(ui);
                });
            });
        });
    ui.add_space(8.0);
}

// ------------------------------------------------------------------ the upgrade card

/// Whether the card shows (people upgrading from 0.8 who haven't chosen).
pub(crate) fn banner_available(ws: &Workspace) -> bool {
    quire_app::onboarding::show_upgrade_card(ws)
}

/// "New in 0.9: AI is opt-in…" with Choose… and ×.
pub(crate) fn banner(ui: &mut egui::Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    egui::Frame::NONE.fill(pal.panel).stroke(Stroke::new(1.0, pal.border)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("New in Quire 0.9").strong().color(pal.accent));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("Don't show this again").clicked() {
                    ws.dismiss_upgrade_card();
                }
                if ui.button("Choose…").clicked() {
                    out.run = Some(Command::AiSetup);
                }
                // The message takes the space left of the buttons.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("AI is your choice: keep it off, use a provider, or run small models on this computer.")
                                .color(pal.text),
                        )
                        .truncate(),
                    );
                });
            });
        });
    });
    out
}
