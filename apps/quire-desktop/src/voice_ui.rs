// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Record voice note…" (0.10): record from the microphone, save the
//! recording in the vault, and insert it with a transcript when one is set
//! up (`quire_app::voice`).
//!
//! The microphone is open only between Record and Stop (or Discard, or the
//! dialog closing). Capture uses `cpal` (feature `voice`, the full edition;
//! ALSA on Linux); without it the command isn't offered.

use std::time::{Duration, Instant};

use egui::RichText;
use quire_app::Workspace;
use quire_app::vault::NotePath;
use quire_app::voice::{self, Place, TranscribeJob};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

/// The longest recording (memory stays bounded: about 110 MB at 48 kHz).
pub(crate) const MAX_SECONDS: u64 = 10 * 60;

#[cfg(feature = "voice")]
mod mic {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    /// An open microphone: mono samples collect until it is dropped.
    pub(crate) struct Mic {
        _stream: cpal::Stream,
        buf: Arc<Mutex<Vec<f32>>>,
        level: Arc<AtomicU32>,
        error: Arc<Mutex<Option<String>>>,
        pub(crate) rate: u32,
        pub(crate) device: String,
    }

    impl Mic {
        pub(crate) fn start() -> Result<Self, String> {
            let host = cpal::default_host();
            let dev = host.default_input_device().ok_or("No microphone was found.")?;
            let device = dev.description().map_or_else(|_| "Microphone".into(), |d| d.to_string());
            let cfg = dev.default_input_config().map_err(|e| format!("The microphone can't be used: {e}"))?;
            let channels = usize::from(cfg.channels()).max(1);
            let rate = cfg.sample_rate();
            let max = (rate as usize) * (super::MAX_SECONDS as usize);
            let buf = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 30)));
            let level = Arc::new(AtomicU32::new(0));
            let error = Arc::new(Mutex::new(None));
            let parts = (Arc::clone(&buf), Arc::clone(&level), Arc::clone(&error));
            let config = cfg.config();
            let stream = match cfg.sample_format() {
                SampleFormat::F32 => build::<f32>(&dev, config, channels, max, parts),
                SampleFormat::I16 => build::<i16>(&dev, config, channels, max, parts),
                SampleFormat::U16 => build::<u16>(&dev, config, channels, max, parts),
                SampleFormat::I32 => build::<i32>(&dev, config, channels, max, parts),
                SampleFormat::U8 => build::<u8>(&dev, config, channels, max, parts),
                other => return Err(format!("The microphone's sample format ({other}) isn't supported.")),
            }?;
            stream.play().map_err(|e| format!("The microphone didn't start: {e}"))?;
            Ok(Self { _stream: stream, buf, level, error, rate, device })
        }

        /// The loudest sample of the last buffer (0–1).
        pub(crate) fn level(&self) -> f32 {
            f32::from_bits(self.level.load(Ordering::Relaxed))
        }

        pub(crate) fn seconds(&self) -> f32 {
            self.buf.lock().unwrap_or_else(PoisonError::into_inner).len() as f32 / self.rate.max(1) as f32
        }

        pub(crate) fn error(&self) -> Option<String> {
            self.error.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }

        /// Close the microphone and take what it recorded.
        pub(crate) fn finish(self) -> (Vec<f32>, u32) {
            let rate = self.rate;
            let buf = std::mem::take(&mut *self.buf.lock().unwrap_or_else(PoisonError::into_inner));
            drop(self);
            (buf, rate)
        }
    }

    type Parts = (Arc<Mutex<Vec<f32>>>, Arc<AtomicU32>, Arc<Mutex<Option<String>>>);

    fn build<T>(
        dev: &cpal::Device,
        config: cpal::StreamConfig,
        channels: usize,
        max: usize,
        (buf, level, error): Parts,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample + Send + 'static,
        f32: FromSample<T>,
    {
        dev.build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                let mut b = buf.lock().unwrap_or_else(PoisonError::into_inner);
                let mut peak = 0f32;
                for frame in data.chunks(channels) {
                    if b.len() >= max {
                        break;
                    }
                    let v = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32;
                    peak = peak.max(v.abs());
                    b.push(v);
                }
                level.store(peak.min(1.0).to_bits(), Ordering::Relaxed);
            },
            move |e| *error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string()),
            None,
        )
        .map_err(|e| format!("The microphone can't be used: {e}"))
    }
}

/// Whether this build can record (the `voice` feature).
pub(crate) const CAN_RECORD: bool = cfg!(feature = "voice");

enum State {
    Ready,
    #[cfg_attr(not(any(feature = "voice", test)), expect(dead_code, reason = "Quire Lite can't record; the command isn't offered there"))]
    Recording {
        #[cfg(feature = "voice")]
        mic: Option<mic::Mic>,
        started: Instant,
        /// Layout samples have no microphone.
        sample_seconds: f32,
    },
    Transcribing {
        path: NotePath,
        seconds: f32,
        job: TranscribeJob,
    },
    Failed(String),
}

pub(crate) struct VoiceDialog {
    state: State,
    /// Started from quick capture: the note goes to today's note.
    to_capture: bool,
}

impl VoiceDialog {
    /// For `registry::UiTarget::Overlay` (`Command::RecordVoiceNote`).
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let to_capture = std::mem::take(&mut ws.features.ai.voice.to_capture);
        Box::new(Self { state: State::Ready, to_capture })
    }

    #[cfg(test)]
    pub(crate) fn sample(state: u8) -> Self {
        let state = match state {
            0 => State::Ready,
            1 => State::Recording {
                #[cfg(feature = "voice")]
                mic: None,
                started: Instant::now(),
                sample_seconds: 83.0,
            },
            2 => State::Transcribing {
                path: NotePath::new("attachments/Voice note 2026-11-05 153012.wav"),
                seconds: 83.0,
                job: voice::Transcription::custom(Place::Device, "Whisper base on this computer", |_, _| {
                    std::thread::sleep(Duration::from_hours(1));
                    Ok(String::new())
                })
                .start("/nowhere.wav".into()),
            },
            _ => State::Failed("No microphone was found.".into()),
        };
        Self { state, to_capture: false }
    }

    /// Add the note (once: the dialog is done afterwards, so closing it
    /// doesn't add it again).
    fn insert(&mut self, ws: &mut Workspace, path: &NotePath, seconds: f32, transcript: Option<(&str, &str)>) -> bool {
        let md = voice::voice_note_markdown(path.file_name(), seconds, transcript);
        let done = if self.to_capture { ws.capture(&md) } else { ws.insert_voice_note(&md) };
        self.state = State::Ready;
        if let Some(note) = done {
            ws.notify(quire_app::workspace::NoticeLevel::Info, format!("Added a voice note to {note}"));
            true
        } else {
            false
        }
    }

    /// Stop recording: save, then transcribe or insert.
    fn stop(&mut self, ws: &mut Workspace) -> bool {
        let State::Recording { .. } = &self.state else { return false };
        let State::Recording {
            #[cfg(feature = "voice")]
            mic,
            ..
        } = std::mem::replace(&mut self.state, State::Ready)
        else {
            return false;
        };
        #[cfg(feature = "voice")]
        let (samples, rate) = mic.map(mic::Mic::finish).unwrap_or_default();
        #[cfg(not(feature = "voice"))]
        let (samples, rate): (Vec<f32>, u32) = (Vec::new(), 0);
        if samples.len() < (rate as usize) / 4 {
            self.state = State::Failed("Nothing was recorded.".into());
            return false;
        }
        let seconds = samples.len() as f32 / rate.max(1) as f32;
        let Some(path) = ws.save_recording(&samples, rate) else {
            self.state = State::Failed("The recording couldn't be saved.".into());
            return false;
        };
        match ws.transcribe_attachment(&path) {
            Some(job) => {
                self.state = State::Transcribing { path, seconds, job };
                false
            }
            None => self.insert(ws, &path, seconds, None),
        }
    }
}

fn level_bar(ui: &mut egui::Ui, level: f32, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(240.0), 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, pal.border);
    let mut fill = rect;
    fill.set_width(rect.width() * level.sqrt().clamp(0.0, 1.0));
    ui.painter().rect_filled(fill, 4.0, pal.accent);
}

impl OverlayView for VoiceDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        let mut start = false;
        let mut stop = false;
        let mut discard = false;
        let mut skip_transcript = false;
        // A transcription that finished.
        if let State::Transcribing { path, seconds, job } = &self.state {
            if let Some(r) = job.poll() {
                let (path, seconds, label) = (path.clone(), *seconds, job.label.clone());
                match r {
                    Ok(text) => res.close = self.insert(ws, &path, seconds, Some((&text, &label))),
                    Err(e) => {
                        ws.notify(
                            quire_app::workspace::NoticeLevel::Warn,
                            format!("Transcription failed ({e}); the recording was added without one."),
                        );
                        res.close = self.insert(ws, &path, seconds, None);
                    }
                }
                if res.close {
                    return res;
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if matches!(self.state, State::Recording { .. }) {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        // 0.11: a vault with AI off sends no recording anywhere.
        let transcription = if quire_app::secure_state::ai_off(ws) { None } else { voice::transcription(&ws.settings) };
        egui::Modal::new(egui::Id::unique("quire-voice")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 460.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.label(RichText::new("Voice note").size(20.0).strong().color(pal.strong));
            ui.add_space(2.0);
            let where_to = if self.to_capture { "today's note" } else { "this note, at the cursor" };
            match &self.state {
                State::Ready => {
                    ui.label(
                        RichText::new(format!(
                            "The microphone records only after you press Record, until you press Stop. The recording is saved in this vault as a WAV file and added to {where_to}."
                        ))
                        .color(pal.muted),
                    );
                    ui.add_space(6.0);
                    match (&transcription, voice::unavailable_reason(&ws.settings)) {
                        (Some(t), _) if t.place == Place::Provider => {
                            ui.label(RichText::new(format!("Transcript: the recording will be sent to {} to be transcribed.", t.label)).color(pal.warn));
                        }
                        (Some(t), _) => {
                            ui.label(RichText::new(format!("Transcript: {}. The recording doesn't leave this computer.", t.label)).color(pal.text));
                        }
                        (None, Some(why)) => {
                            ui.label(RichText::new(format!("No transcript: {why}")).color(pal.muted));
                        }
                        (None, None) => {
                            ui.label(RichText::new("No transcript: choose where transcription runs in Settings → AI → Voice notes.").color(pal.muted));
                        }
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("●  Record").color(pal.danger).strong())).clicked() {
                            start = true;
                        }
                        if ui.button("Cancel").clicked() {
                            res.close = true;
                        }
                    });
                }
                State::Recording {
                    #[cfg(feature = "voice")]
                    mic,
                    started,
                    sample_seconds,
                } => {
                    #[cfg(feature = "voice")]
                    let (secs, level, device, err) = mic.as_ref().map_or_else(
                        || (*sample_seconds, 0.35, "Microphone".to_string(), None),
                        |m| (m.seconds(), m.level(), m.device.clone(), m.error()),
                    );
                    #[cfg(not(feature = "voice"))]
                    let (secs, level, device, err): (f32, f32, String, Option<String>) = (*sample_seconds, 0.35, "Microphone".into(), None);
                    let _ = started;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(pal.danger).size(16.0));
                        ui.label(RichText::new(format!("Recording · {}", voice::duration_label(secs))).strong().color(pal.text));
                    });
                    ui.label(RichText::new(format!("From {device}. Stops by itself after {} minutes.", MAX_SECONDS / 60)).size(12.0).color(pal.muted));
                    ui.add_space(4.0);
                    level_bar(ui, level, pal);
                    if let Some(e) = err {
                        ui.label(RichText::new(e).color(pal.danger));
                    }
                    if secs >= MAX_SECONDS as f32 {
                        stop = true;
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("■  Stop").strong())).clicked() {
                            stop = true;
                        }
                        if ui.button("Discard").on_hover_text("Stop and keep nothing").clicked() {
                            discard = true;
                        }
                    });
                }
                State::Transcribing { seconds, job, .. } => {
                    ui.label(RichText::new(format!("Saved the {} recording.", voice::duration_label(*seconds))).color(pal.text));
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new(format!("Transcribing with {}…", job.label)).color(pal.text));
                    });
                    ui.add_space(10.0);
                    if ui.button("Add without a transcript").on_hover_text("Stop transcribing; the recording is added on its own").clicked() {
                        skip_transcript = true;
                    }
                }
                State::Failed(e) => {
                    ui.label(RichText::new(e).color(pal.danger));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            start = true;
                        }
                        if ui.button("Close").clicked() {
                            res.close = true;
                        }
                    });
                }
            }
        });
        if start {
            #[cfg(feature = "voice")]
            {
                self.state = match mic::Mic::start() {
                    Ok(m) => State::Recording { mic: Some(m), started: Instant::now(), sample_seconds: 0.0 },
                    Err(e) => State::Failed(e),
                };
            }
            #[cfg(not(feature = "voice"))]
            {
                self.state = State::Failed("This edition of Quire can't record audio.".into());
            }
        }
        if stop {
            res.close |= self.stop(ws);
        }
        if discard {
            // Dropping the microphone closes it; nothing is saved.
            self.state = State::Ready;
            res.close = true;
        }
        if skip_transcript && let State::Transcribing { path, seconds, job } = &self.state {
            job.cancel();
            let (path, seconds) = (path.clone(), *seconds);
            res.close = self.insert(ws, &path, seconds, None);
        }
        res
    }

    fn closed(&mut self, ws: &mut Workspace) {
        // Closing never leaves the microphone open; a recording in progress
        // is saved and added (Discard is the way to drop it).
        if matches!(self.state, State::Recording { .. }) {
            self.stop(ws);
        }
        if let State::Transcribing { path, seconds, job } = &self.state {
            job.cancel();
            let (path, seconds) = (path.clone(), *seconds);
            self.insert(ws, &path, seconds, None);
        }
        self.state = State::Ready;
    }
}
