// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → AI (design §7.2), replacing 0.8's "AI & search" tab:
//!
//! * **Provider** — the cloud or server model (0.8's form, with Test connection);
//! * **On this computer** — one row per capability with its state, progress,
//!   Pause, Cancel, Delete, "Install from file…", advanced options and the
//!   engine's status;
//! * **Features** — which model the agent panel uses, meaning search, and
//!   the trust features (Ask style, contradictions, staleness, citations);
//! * **Background work** — what the jobs are doing.
//!
//! Owned by WS-D. Registered as the `"ai"` Settings tab.

use crossbeam_channel::Receiver;
use egui::{RichText, Stroke};
use quire_app::ai::{self, ModelState, catalog};
use quire_app::workspace::NoticeLevel;
use quire_app::{Command, Workspace};

use crate::onboarding_ui::size_text;
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct AiSettingsUi {
    key_input: String,
    test: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    confirm_delete: Option<String>,
}

/// Always offered: it replaces the built-in AI tab.
pub(crate) const fn available(_ws: &Workspace) -> bool {
    true
}

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    section_badge(ui, pal, title, None);
}

fn section_badge(ui: &mut egui::Ui, pal: &Palette, title: &str, badge: Option<&str>) {
    ui.add_space(14.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(title.to_uppercase()).size(11.0).color(pal.muted).strong());
        if let Some(b) = badge {
            experimental_label(ui, pal, b);
        }
    });
    ui.add_space(4.0);
}

/// A small amber label such as "Experimental".
pub(crate) fn experimental_label(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    egui::Frame::NONE.stroke(Stroke::new(1.0, pal.warn)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(5, 1)).show(ui, |ui| {
        ui.add(egui::Label::new(RichText::new(text).size(10.5).color(pal.warn)).extend());
    });
}

/// The experimental switch (Settings → AI → On this computer → Advanced).
fn experimental_switch(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
    ui.checkbox(&mut ws.settings.ai_local.experimental, "Experimental: show on-device AI");
    if !ws.settings.ai_local.experimental && ws.settings.ai_local.on_device() {
        ui.label(RichText::new(format!("On for now: {}=1 is set.", quire_app::settings::EXPERIMENTAL_ENV)).size(11.5).color(pal.faint));
    }
}

/// While on-device AI is off: one line, and the switch under Advanced.
fn device_later(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
    ui.add(egui::Label::new(RichText::new(ai::ON_DEVICE_LATER).size(12.5).color(pal.text)).wrap());
    egui::CollapsingHeader::new(RichText::new("Advanced").color(pal.text)).id_salt("quire-ai-advanced").show(ui, |ui| {
        experimental_switch(ui, ws, pal);
    });
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let s = &mut st.ai_settings;
    if let Some(rx) = &s.test
        && let Ok(r) = rx.try_recv()
    {
        s.test_result = Some(r);
        s.test = None;
    }
    if s.test.is_some() || ws.features.ai.busy() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
    ui.label(
        RichText::new(
            "Quire works without AI. Nothing leaves this computer unless you choose a provider, and nothing downloads unless you ask.",
        )
        .size(12.5)
        .color(pal.muted),
    );
    let before = ws.settings.ai_local.clone();
    let before_embed = ws.settings.embeddings.clone();
    let before_panel = ws.settings.agent_panel.model.clone();
    section(ui, pal, "Provider");
    provider(ui, ws, s, pal);
    if ws.settings.ai_local.on_device() {
        section_badge(ui, pal, "On this computer", Some("Experimental"));
        device(ui, ws, s, pal);
    } else {
        section(ui, pal, "On this computer");
        device_later(ui, ws, pal);
    }
    section(ui, pal, "Features");
    features(ui, ws, pal, &mut out);
    // Ask, citations, staleness and contradictions (WS-C's settings).
    if crate::trust_ui::settings_available(ws) {
        let trust = crate::trust_ui::settings_tab(ui, ws, st, pal);
        out.run = out.run.or(trust.run);
    }
    section(ui, pal, "Voice notes");
    voice(ui, ws, &mut st.ai_settings, pal);
    section(ui, pal, "Background work");
    jobs(ui, ws, pal);
    if ws.settings.ai_local != before || ws.settings.embeddings != before_embed || ws.settings.agent_panel.model != before_panel {
        ws.apply_ai_settings();
    }
    out
}

/// A label above its control, so the tab works at any width.
fn field(ui: &mut egui::Ui, pal: &Palette, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.label(RichText::new(label).size(12.5).color(pal.muted));
    add(ui);
}

fn text_width(ui: &egui::Ui, want: f32) -> f32 {
    ui.available_width().min(want)
}

fn provider(ui: &mut egui::Ui, ws: &mut Workspace, s: &mut AiSettingsUi, pal: &Palette) {
    let provider = ws.settings.ai_provider.clone();
    {
        let st = &mut ws.settings;
        ui.radio_value(&mut st.ai_provider, "local".to_string(), "No provider");
        ui.radio_value(&mut st.ai_provider, "anthropic".to_string(), "Anthropic (Claude)");
        ui.radio_value(&mut st.ai_provider, "openai".to_string(), "OpenAI-compatible (OpenAI, Ollama, LM Studio…)");
    }
    if provider == "local" {
        return;
    }
    let w = text_width(ui, 320.0);
    field(ui, pal, "Server", |ui| {
        ui.add(egui::TextEdit::singleline(&mut ws.settings.ai_base_url).hint_text(ai::default_base_url(&provider)).desired_width(w));
    });
    field(ui, pal, "Model", |ui| {
        ui.add(egui::TextEdit::singleline(&mut ws.settings.ai_model).hint_text(ai::default_model(&provider)).desired_width(w));
    });
    field(ui, pal, "API key", |ui| {
        let has = ai::has_api_key(&provider);
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut s.key_input)
                    .password(true)
                    .hint_text(if has { "•••••• saved" } else { "paste key" })
                    .desired_width(text_width(ui, 200.0)),
            );
            if ui.add_enabled(!s.key_input.trim().is_empty(), egui::Button::new("Save key")).clicked() {
                match ws.save_api_key(&provider, &s.key_input) {
                    Ok(()) => ws.notify(NoticeLevel::Info, "Key saved"),
                    Err(e) => ws.notify(NoticeLevel::Error, e),
                }
                s.key_input.clear();
            }
            if has && ui.button("Remove").clicked() {
                let _ = ws.save_api_key(&provider, "");
            }
        });
        let note = if provider == "openai" { " · not needed for local servers" } else { "" };
        ui.add(egui::Label::new(RichText::new(format!("Stored in {}{note}", ai::secret_store_kind())).size(11.5).color(pal.muted)).wrap());
    });
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(s.test.is_none(), egui::Button::new("Test connection")).clicked() {
            s.test = Some(ws.test_ai_connection());
            s.test_result = None;
        }
        if s.test.is_some() {
            ui.add(egui::Spinner::new());
        }
    });
    match &s.test_result {
        Some(Ok(m)) => {
            ui.add(egui::Label::new(RichText::new(format!("✓ {m}")).color(pal.accent)).wrap());
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
        }
        None => {}
    }
    ui.add(
        egui::Label::new(
            RichText::new("Your notes go to this provider only when you use an AI feature (Ask with a model, the agent panel, summaries).")
                .size(11.5)
                .color(pal.faint),
        )
        .wrap(),
    );
}

fn eta(remaining: u64, rate: f64) -> String {
    if rate < 1.0 {
        return String::new();
    }
    let secs = (remaining as f64 / rate).round() as u64;
    if secs < 60 { format!(" · {secs} s left") } else { format!(" · {} min left", secs.div_ceil(60)) }
}

/// One downloadable model: state, progress and actions.
fn model_row(ui: &mut egui::Ui, ws: &mut Workspace, s: &mut AiSettingsUi, pal: &Palette, id: &str) {
    let Some(entry) = catalog().get(id).cloned() else { return };
    let state = ws.model_state(id);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(entry.display_name()).color(pal.strong));
        // A bundled engine downloads nothing, so its size says nothing.
        let bundled = entry.purpose == ai::Purpose::Engine && ai::engine_source() == Some(ai::EngineSource::Bundled);
        if !bundled {
            ui.label(RichText::new(size_text(id)).size(12.0).color(pal.muted));
        }
        if entry.ram_mb > 0 && entry.purpose != ai::Purpose::Engine {
            ui.label(
                RichText::new(format!("· about {} of memory in use", ai::human_size(u64::from(entry.ram_mb) * 1_000_000)))
                    .size(12.0)
                    .color(pal.faint),
            );
        }
    });
    ui.horizontal_wrapped(|ui| match &state {
        ModelState::Unavailable => {
            ui.label(RichText::new("Not available in this build").color(pal.faint))
                .on_hover_text("This build of Quire has no verified hash for it, so it won't download it. Release builds do.");
        }
        ModelState::NotInstalled => {
            ui.label(RichText::new("Not installed").color(pal.muted));
            if ui.button("Download").clicked() {
                ws.ai_download(id);
            }
        }
        ModelState::Queued => {
            ui.label(RichText::new("Waiting to download").color(pal.muted));
            if ui.button("Cancel").clicked() {
                ws.ai_cancel(id);
            }
        }
        ModelState::Downloading { done, total, bytes_per_s } => {
            let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).desired_width(text_width(ui, 140.0)).desired_height(8.0).fill(pal.accent));
            ui.label(
                RichText::new(format!(
                    "Downloading {:.0}% · {}/s{}",
                    frac * 100.0,
                    ai::human_size(*bytes_per_s as u64),
                    eta(total.saturating_sub(*done), *bytes_per_s)
                ))
                .size(12.0)
                .color(pal.text),
            );
            if ui.button("Pause").clicked() {
                ws.ai_pause(id);
            }
            if ui.button("Cancel").clicked() {
                ws.ai_cancel(id);
            }
        }
        ModelState::Verifying => {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Verifying").color(pal.text));
        }
        ModelState::Paused { done, total } => {
            let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).desired_width(text_width(ui, 140.0)).desired_height(8.0).fill(pal.muted));
            ui.label(RichText::new(format!("Paused at {:.0}%", frac * 100.0)).size(12.0).color(pal.muted));
            if ui.button("Resume").clicked() {
                ws.ai_download(id);
            }
            if ui.button("Cancel").clicked() {
                ws.ai_cancel(id);
            }
        }
        ModelState::Ready => {
            ui.label(RichText::new("✓ Ready").color(pal.accent));
            // An engine packaged with Quire (the full edition) or set with
            // QUIRE_AI_ENGINE isn't Quire's to delete.
            let source = if id == ai::ENGINE_ID { ai::engine_source() } else { None };
            if source == Some(ai::EngineSource::Bundled) {
                ui.label(RichText::new("· Included with Quire").size(12.0).color(pal.faint));
            } else if source == Some(ai::EngineSource::Override) {
                ui.label(RichText::new("· from QUIRE_AI_ENGINE").size(12.0).color(pal.faint));
            } else if s.confirm_delete.as_deref() == Some(id) {
                ui.label(RichText::new("Delete its files?").color(pal.warn));
                if ui.button("Delete").clicked() {
                    ws.ai_delete(id);
                    s.confirm_delete = None;
                }
                if ui.button("Keep").clicked() {
                    s.confirm_delete = None;
                }
            } else if ui.button("Delete").on_hover_text("Delete the downloaded files from this computer").clicked() {
                s.confirm_delete = Some(id.to_string());
            }
        }
        ModelState::Failed(e) => {
            ui.label(RichText::new("Error").color(pal.danger)).on_hover_text(e);
            ui.add(egui::Label::new(RichText::new(e).size(12.0).color(pal.danger)).truncate());
            if ui.button("Retry").clicked() {
                ws.ai_download(id);
            }
        }
    });
    if !entry.attribution.is_empty() {
        ui.label(RichText::new(&entry.attribution).size(11.0).color(pal.faint));
    }
}

fn capability_frame(ui: &mut egui::Ui, pal: &Palette, on: bool, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE.stroke(Stroke::new(1.0, if on { pal.accent } else { pal.border })).corner_radius(8.0).inner_margin(10.0).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        },
    );
    ui.add_space(6.0);
}

fn device(ui: &mut egui::Ui, ws: &mut Workspace, s: &mut AiSettingsUi, pal: &Palette) {
    ui.label(RichText::new("Small models that run here, in a separate engine process. They download once (from Hugging Face; the engine from Quire's GitHub releases), are checked against hashes built into Quire, and then work offline.").size(12.0).color(pal.muted));
    ui.add_space(6.0);
    // Meaning search.
    let on = ws.settings.ai_local.embeddings;
    capability_frame(ui, pal, on, |ui| {
        let mut v = ws.settings.ai_local.embeddings;
        if ui.checkbox(&mut v, RichText::new("Meaning search").strong()).changed() {
            ws.settings.ai_local.embeddings = v;
            ws.settings.ai_local.engine |= v;
            ws.settings.embeddings = if v {
                "local".into()
            } else if ws.settings.embeddings == "local" {
                "lsa".into()
            } else {
                ws.settings.embeddings.clone()
            };
        }
        ui.label(RichText::new("Better “find by meaning” and related notes.").size(12.0).color(pal.muted));
        if v {
            ui.horizontal_wrapped(|ui| {
                let m = &mut ws.settings.ai_local.embed_model;
                ui.radio_value(m, "bge-small-en-v1.5".to_string(), "English");
                ui.radio_value(m, "multilingual-e5-small".to_string(), "Many languages");
            });
            let id = ws.settings.ai_local.embed_model.clone();
            model_row(ui, ws, s, pal, &id);
        }
    });
    // Writing and agent.
    let on = ws.settings.ai_local.llm;
    capability_frame(ui, pal, on, |ui| {
        let mut v = ws.settings.ai_local.llm;
        if ui.checkbox(&mut v, RichText::new("Writing and agent").strong()).changed() {
            ws.settings.ai_local.llm = v;
            ws.settings.ai_local.engine |= v;
        }
        ui.label(RichText::new("Summaries, tags, the agent panel and claim checks.").size(12.0).color(pal.muted));
        if v {
            ui.horizontal_wrapped(|ui| {
                let m = &mut ws.settings.ai_local.llm_model;
                for (id, name) in [("qwen3-0.6b-q8", "Small"), ("qwen3-1.7b-q4km", "Balanced"), ("qwen3-4b-q4km", "Better")] {
                    ui.radio_value(m, id.to_string(), name).on_hover_text(crate::onboarding_ui::ram_text(id));
                }
            });
            let id = ws.settings.ai_local.llm_model.clone();
            model_row(ui, ws, s, pal, &id);
        }
    });
    // OCR.
    let on = ws.settings.ai_local.ocr;
    capability_frame(ui, pal, on, |ui| {
        let mut v = ws.settings.ai_local.ocr;
        if ui.checkbox(&mut v, RichText::new("Read text in images and scanned PDFs").strong()).changed() {
            ws.settings.ai_local.ocr = v;
            ws.settings.ai_local.engine |= v;
        }
        ui.label(
            RichText::new("Makes their text searchable. Latin script only. Runs while you're not typing.").size(12.0).color(pal.muted),
        );
        if v {
            model_row(ui, ws, s, pal, "ocrs");
        }
    });
    // The engine.
    capability_frame(ui, pal, false, |ui| {
        ui.label(RichText::new("Engine").strong());
        model_row(ui, ws, s, pal, "engine");
        engine_status(ui, ws, pal);
    });
    if ui
        .button("Install from file…")
        .on_hover_text("For computers without internet: pick model files downloaded elsewhere. Each is checked by its SHA-256.")
        .clicked()
        && let Some(paths) = rfd::FileDialog::new().set_title("Install model files").pick_files()
    {
        ws.ai_install_files(&paths);
    }
    if let Some(dir) = ai::models_dir() {
        ui.add(egui::Label::new(RichText::new(format!("Stored in {}", dir.display())).size(11.5).color(pal.faint)).wrap());
    }
    egui::CollapsingHeader::new(RichText::new("Advanced").color(pal.text)).id_salt("quire-ai-advanced").show(ui, |ui| {
        experimental_switch(ui, ws, pal);
        let a = &mut ws.settings.ai_local;
        field(ui, pal, "Download mirror (keeps each file's path and hash)", |ui| {
            ui.add(egui::TextEdit::singleline(&mut a.mirror_url).hint_text("https://…").desired_width(text_width(ui, 300.0)));
        });
        field(ui, pal, "Threads", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::DragValue::new(&mut a.threads).range(0..=64));
                ui.label(
                    RichText::new(if a.threads == 0 { "automatic" } else { "used when the engine next starts" })
                        .size(12.0)
                        .color(pal.muted),
                );
            });
        });
        if cfg!(target_os = "macos") {
            field(ui, pal, "GPU (Metal)", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut a.gpu, "auto".to_string(), "Automatic");
                    ui.radio_value(&mut a.gpu, "off".to_string(), "Off");
                });
            });
        }
        ui.checkbox(&mut a.fast_prompts, "Faster prompts, more memory")
            .on_hover_text("On Intel and AMD processors, the writing model reads long prompts about twice as fast, but Balanced uses about 4 GB of memory instead of 1.7 GB. Takes effect when the model next loads.");
        field(ui, pal, "Free the memory of a model unused for", |ui| {
            let mut mins = (a.idle_unload_s / 60).max(1);
            if ui.add(egui::DragValue::new(&mut mins).range(1..=240).suffix(" min")).changed() {
                a.idle_unload_s = mins * 60;
            }
        });
    });
}

/// Voice notes (0.10): where recordings are transcribed, if anywhere.
fn voice(ui: &mut egui::Ui, ws: &mut Workspace, s: &mut AiSettingsUi, pal: &Palette) {
    let muted = |t: &str| RichText::new(t.to_string()).size(12.0).color(pal.muted);
    if crate::voice_ui::CAN_RECORD {
        ui.add(
            egui::Label::new(muted(
                "“Record voice note…” records only between Record and Stop, and saves the recording in the vault as a WAV file.",
            ))
            .wrap(),
        );
    } else {
        ui.add(egui::Label::new(muted("This edition can't record. Recordings already in a vault can still be played elsewhere.")).wrap());
        return;
    }
    field(ui, pal, "Transcripts", |ui| {
        let t = &mut ws.settings.voice.transcribe;
        ui.radio_value(t, String::new(), "None (keep the recording only)");
        if ws.settings.ai_local.on_device() {
            ui.radio_value(t, "device".to_string(), "On this computer (Whisper; experimental)");
        }
        ui.radio_value(t, "provider".to_string(), "Through my provider (sends the recording)");
    });
    match ws.settings.voice.transcribe.as_str() {
        "device" => {
            ui.horizontal_wrapped(|ui| {
                let m = &mut ws.settings.voice.device_model;
                for (id, name) in [("whisper-tiny", "Fast"), ("whisper-base", "Balanced"), ("whisper-small", "Better")] {
                    ui.radio_value(m, id.to_string(), name);
                }
            });
            let id = ws.settings.voice.device_model.clone();
            model_row(ui, ws, s, pal, &id);
            ui.add(egui::Label::new(muted("The recording doesn't leave this computer. It needs the engine too (above).")).wrap());
        }
        "provider" => {
            let (base, _) = ai::resolved(&ws.settings);
            field(ui, pal, "Transcription model", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut ws.settings.voice.provider_model)
                        .hint_text("whisper-1")
                        .desired_width(text_width(ui, 200.0)),
                );
            });
            if ws.settings.ai_provider == "openai" {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Each recording you transcribe is sent to {base} (/v1/audio/transcriptions)."))
                            .size(12.0)
                            .color(pal.warn),
                    )
                    .wrap(),
                );
            }
        }
        _ => {}
    }
    if let Some(why) = quire_app::voice::unavailable_reason(&ws.settings) {
        ui.add(egui::Label::new(RichText::new(why).size(12.0).color(pal.warn)).wrap());
    }
    if !ws.settings.voice.transcribe.is_empty() {
        field(ui, pal, "Language (empty: detect it)", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut ws.settings.voice.language).hint_text("en, de, fr…").desired_width(text_width(ui, 120.0)),
            );
        });
    }
}

fn engine_status(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
    let snap = ws.ai_engine_status();
    ui.horizontal_wrapped(|ui| {
        if let Some(why) = &snap.gave_up {
            ui.add(egui::Label::new(RichText::new(why).size(12.0).color(pal.danger)).wrap());
            if ui.button("Restart").clicked() {
                ws.ai_stop_engine();
            }
            return;
        }
        if snap.running {
            let names: Vec<String> = snap
                .loaded
                .iter()
                .map(|m| catalog().get(&m.model).map_or_else(|| m.model.clone(), |e| e.display_name().to_string()))
                .collect();
            let mem = if snap.rss_bytes > 0 { format!(", {} in use", ai::human_size(snap.rss_bytes)) } else { String::new() };
            let what = if names.is_empty() { "nothing loaded".to_string() } else { names.join(", ") };
            ui.label(RichText::new(format!("● Running{mem} · {what}")).size(12.0).color(pal.accent));
            if ui.button("Stop").on_hover_text("Free its memory now; it starts again when needed").clicked() {
                ws.ai_stop_engine();
            }
        } else if snap.installed {
            ui.label(RichText::new("Not running · starts when a feature needs it").size(12.0).color(pal.muted));
        }
    });
}

fn features(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, out: &mut PanelOut) {
    // The cached model (provider first), not a keychain read every frame.
    let has_provider = ws.trust_chat().is_some_and(|c| c.locality() != quire_app::intel::chat::Locality::Device);
    let has_device = ai::device_chat(&ws.settings).is_some();
    field(ui, pal, "Agent panel", |ui| {
        ui.checkbox(&mut ws.settings.agent_panel.enabled, "Show the Agent tab (Ctrl+Shift+J)");
        if has_provider && has_device {
            ui.horizontal_wrapped(|ui| {
                ui.label("Use");
                let m = &mut ws.settings.agent_panel.model;
                ui.radio_value(m, String::new(), "Your provider");
                ui.radio_value(m, "device".to_string(), "On this computer");
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Steps per message");
            ui.add(egui::DragValue::new(&mut ws.settings.agent_panel.max_steps).range(1..=50));
        });
        if ws.settings.agent_panel.backend == "acp" && ws.settings.agent_panel.acp_risk_ack {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("External agent: {}", ws.settings.agent_panel.acp_command)).color(pal.warn));
                if ui.button("Disconnect").clicked() {
                    ws.settings.agent_panel.backend = "provider".into();
                }
            });
        } else if ui.button("Connect an external agent (ACP)…").clicked() {
            out.run = Some(Command::ConnectExternalAgent);
        }
    });
    field(ui, pal, "Meaning search", |ui| {
        let s = &mut ws.settings;
        ui.radio_value(&mut s.embeddings, "lsa".to_string(), "Built in (offline, instant)");
        if s.ai_local.on_device() {
            if ui.radio_value(&mut s.embeddings, "local".to_string(), "On this computer (the model above)").changed() {
                s.ai_local.embeddings = true;
                s.ai_local.engine = true;
            }
        } else if s.embeddings == "local" {
            // Kept for when on-device AI is on; the built-in index is used meanwhile.
            ui.label(RichText::new("Using the built-in index until on-device AI is available.").size(12.0).color(pal.muted));
        }
        ui.radio_value(&mut s.embeddings, "endpoint".to_string(), "From a server (/v1/embeddings)");
        if s.embeddings == "endpoint" {
            let w = text_width(ui, 240.0);
            ui.label(RichText::new("Model").size(12.0).color(pal.muted));
            ui.add(egui::TextEdit::singleline(&mut s.embed_model).desired_width(w));
            ui.label(RichText::new("Server").size(12.0).color(pal.muted));
            ui.add(egui::TextEdit::singleline(&mut s.embed_base_url).hint_text("same as the provider").desired_width(w));
        }
        if let Some(e) = ws.index().semantic_error() {
            ui.add(
                egui::Label::new(RichText::new(format!("Not available ({e}); using the built-in index.")).size(12.0).color(pal.warn))
                    .wrap(),
            );
        }
    });
}

fn jobs(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette) {
    let statuses = ws.jobs.statuses();
    if statuses.is_empty() {
        ui.label(RichText::new("Nothing running.").size(12.0).color(pal.muted));
        return;
    }
    for j in statuses {
        let state = match j.state {
            quire_app::jobs::JobState::Waiting => "waiting".to_string(),
            quire_app::jobs::JobState::Running => "running".to_string(),
            quire_app::jobs::JobState::Paused(why) => format!("paused: {why}"),
            quire_app::jobs::JobState::Done => "done".to_string(),
        };
        let last = j.last_step.map_or_else(|| "not yet".to_string(), |t| format!("{} s ago", t.elapsed().as_secs()));
        ui.add(
            egui::Label::new(RichText::new(format!("{} · {state} · {} steps · last {last}", j.id, j.steps)).size(12.0).color(pal.muted))
                .wrap(),
        );
    }
}
