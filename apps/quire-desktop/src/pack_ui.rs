// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Turn on on-device AI…" (`docs/design/0.14-local-ai.md` §3.8): the pack
//! dialog, opened from Settings → AI and from the first-run AI choice.
//!
//! It lists the pack's rows this build can download ([`PACK_ROWS`]): the
//! writing model (Qwen 3.5 4B by default, 9B from 16 GB) and OCR, each with
//! its size, then the total download, the free space in the models folder and
//! the memory writing uses. Nothing is written or downloaded until "Download
//! and turn on"; Cancel changes nothing.
//!
//! [`PACK_ROWS`]: quire_app::ai::pack::PACK_ROWS

use crossbeam_channel::Receiver;
use egui::RichText;
use quire_app::Workspace;
use quire_app::ai::pack::{self, PackChoice, PackKind, PackOffer};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// The dialog.
pub(crate) struct PackOverlay {
    offer: PackOffer,
    choice: PackChoice,
    /// Free bytes in the models folder: read once, off the UI thread.
    free: Option<u64>,
    free_rx: Option<Receiver<Option<u64>>>,
    /// Opened from the first-run choice: record it there too.
    onboarding: bool,
}

impl PackOverlay {
    /// For `registry::UiTarget::Overlay` (`Command::AiPack`).
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let _ = ws;
        Box::new(Self::new(false))
    }

    /// From the first-run AI choice.
    pub(crate) fn from_onboarding() -> Self {
        Self::new(true)
    }

    fn new(onboarding: bool) -> Self {
        let offer = pack::offer(quire_app::ai::catalog(), quire_app::onboarding::memory_gb(), quire_app::ai::engine_installed());
        let choice = offer.default_choice();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let dir = quire_app::ai::models_dir();
        let started = std::thread::Builder::new().name("quire-free-disk".into()).spawn(move || {
            let _ = tx.send(dir.as_deref().and_then(pack::free_disk_bytes));
        });
        Self { offer, choice, free: None, free_rx: started.is_ok().then_some(rx), onboarding }
    }

    /// A fixed offer for `layout_check` (rows as a release will have them).
    #[cfg(test)]
    pub(crate) fn sample(memory_gb: Option<u64>, lite: bool, free: Option<u64>, nothing: bool) -> Self {
        use quire_app::ai::pack::{Offered, PACK_ROWS};
        let rows = if nothing {
            Vec::new()
        } else {
            PACK_ROWS
                .iter()
                .map(|row| {
                    let (size, ram_mb, min) = match row.id {
                        "qwen3.5-4b-q4km" => (2_740_000_000, 3300, 8),
                        "qwen3.5-9b-q4km" => (5_680_000_000, 6300, 16),
                        _ => (13_000_000, 150, 0),
                    };
                    let blocked = match (row.kind, memory_gb) {
                        (PackKind::Writing, Some(gb)) if gb < 8 => Some("needs a computer with 8 GB of memory".into()),
                        (PackKind::Writing, Some(gb)) if gb < min => Some(format!("needs {min} GB")),
                        _ => None,
                    };
                    Offered { row: *row, size, blocked, ram_mb }
                })
                .collect()
        };
        let default_llm = rows.iter().find(|o: &&Offered| o.row.kind == PackKind::Writing && o.blocked.is_none()).map(|o| o.row.id);
        let offer = PackOffer { rows, default_llm, engine_size: lite.then_some(24_000_000) };
        let choice = offer.default_choice();
        Self { offer, choice, free, free_rx: None, onboarding: false }
    }
}

/// Sizes as the model pages show them: decimal GB, or MB below one GB.
fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 { quire_ai::memory::gb(bytes) } else { format!("{} MB", bytes.div_ceil(1_000_000)) }
}

impl OverlayView for PackOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        if let Some(rx) = &self.free_rx {
            if let Ok(f) = rx.try_recv() {
                self.free = f;
                self.free_rx = None;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        egui::Modal::new(egui::Id::unique("quire-ai-pack")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(
            ctx,
            |ui| {
                let w = 560.0_f32.min(ctx.content_rect().width() - 48.0);
                ui.set_width(w);
                ui.set_max_width(w);
                ui.label(RichText::new("On-device AI").size(20.0).strong().color(pal.strong));
                ui.add(egui::Label::new(RichText::new("Runs on this computer, nothing is sent anywhere.").color(pal.muted)).wrap());
                ui.add_space(10.0);
                if self.offer.rows.is_empty() {
                    ui.add(
                        egui::Label::new(
                            RichText::new("This build of Quire can't download on-device models. A release build can.").color(pal.text),
                        )
                        .wrap(),
                    );
                }
                for o in &self.offer.rows {
                    ui.horizontal(|ui| {
                        let label = match &o.blocked {
                            Some(why) => format!("{} ({why})", o.row.label),
                            None => o.row.label.to_string(),
                        };
                        ui.add_enabled_ui(o.blocked.is_none(), |ui| match o.row.kind {
                            PackKind::Writing => {
                                let on = self.choice.llm.as_deref() == Some(o.row.id);
                                if ui.radio(on, label).clicked() {
                                    self.choice.llm = if on { None } else { Some(o.row.id.to_string()) };
                                }
                            }
                            PackKind::Ocr => {
                                ui.checkbox(&mut self.choice.ocr, label);
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(size(o.size)).color(pal.muted));
                        });
                    });
                }
                let bytes = self.offer.download_bytes(&self.choice);
                if let Some(engine) = self.offer.engine_size.filter(|_| bytes > 0) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("The AI engine (Quire Lite only)").color(pal.text));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(size(engine)).color(pal.muted));
                        });
                    });
                }
                ui.add_space(8.0);
                let free = self.free.map_or_else(String::new, |f| format!(" Free space here: {}.", size(f)));
                if bytes > 0 {
                    ui.label(RichText::new(format!("Download: {}.{free}", size(bytes))).color(pal.text));
                }
                if let Some(line) = self.offer.memory_line(&self.choice, cfg!(target_os = "macos")) {
                    ui.label(RichText::new(line).color(pal.muted));
                }
                let space = pack::check_space(bytes, self.free);
                if let Err(why) = &space {
                    ui.label(RichText::new(why).color(pal.danger));
                }
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let go =
                            ui.add_enabled(bytes > 0 && space.is_ok(), egui::Button::new(RichText::new("Download and turn on").strong()));
                        if go.clicked() {
                            if self.onboarding {
                                ws.onboarding_pack(&self.choice);
                            } else {
                                ws.pack_turn_on(&self.choice);
                            }
                            res.close = true;
                        }
                    });
                });
            },
        );
        res
    }
}
