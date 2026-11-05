// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Voice notes in the editor (0.10): a line that only embeds a recording
//! (`![[Voice note 2026-11-05 153012.wav]]`) gets a small player under it
//! instead of looking like a link.
//!
//! The full edition plays WAV recordings in place through `cpal` (already
//! there for recording): the file is decoded with `quire_ai::wav`,
//! resampled to the speaker's rate and played once. Every edition offers
//! "Open in system player", which also plays formats Quire doesn't decode
//! (MP3, M4A, Ogg).

use std::path::Path;

use egui::{Rect, RichText, Stroke, UiBuilder};
use quire_app::Workspace;

use crate::theme::Palette;

/// Audio files a note may embed.
const AUDIO: &[&str] = &["wav", "mp3", "m4a", "ogg", "oga", "opus", "flac", "aac", "webm"];

/// The target of a line that only embeds an audio file (`![[x.wav]]`,
/// `![](x.mp3)`), if it is one.
pub(crate) fn audio_embed(line: &str) -> Option<&str> {
    let t = line.trim();
    let target = match t.strip_prefix("![[").and_then(|r| r.strip_suffix("]]")) {
        Some(inner) => inner.split('|').next().unwrap_or(inner),
        None => t.strip_prefix("![")?.split_once("](")?.1.strip_suffix(')')?.split_whitespace().next()?,
    }
    .trim();
    let ext = target.rsplit_once('.')?.1;
    (AUDIO.iter().any(|a| a.eq_ignore_ascii_case(ext)) && !target.contains("://")).then_some(target)
}

/// "0:07", "1:02:03".
pub(crate) fn clock(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// A WAV file's length from its header (no decoding), for the label.
pub(crate) fn wav_seconds(bytes: &[u8]) -> Option<f32> {
    if bytes.get(0..4) != Some(b"RIFF".as_slice()) || bytes.get(8..12) != Some(b"WAVE".as_slice()) {
        return None;
    }
    let u16_at = |i: usize| Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    let (mut i, mut per_second, mut data) = (12_usize, None, None);
    while i + 8 <= bytes.len() {
        let len = u32_at(i + 4)? as usize;
        match &bytes[i..i + 4] {
            b"fmt " => per_second = u32_at(i + 8 + 8).filter(|n| *n > 0).or_else(|| Some(u32::from(u16_at(i + 8 + 2)?))),
            b"data" => data = Some(len.min(bytes.len().saturating_sub(i + 8))),
            _ => {}
        }
        i = i + 8 + len + (len & 1);
    }
    Some(data? as f32 / per_second? as f32)
}

/// The player under one embed line, in `rect` (one row). Returns its height.
pub(crate) fn row(ui: &mut egui::Ui, ws: &Workspace, target: &str, rect: Rect, pal: &Palette, key: egui::Id) -> f32 {
    let path = ws.attachment_path(target);
    let _ = ui.interact(rect, key.with("cover"), egui::Sense::click());
    let mut child = ui.new_child(UiBuilder::new().id_salt(key).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    egui::Frame::NONE
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(&mut child, |ui| {
            ui.horizontal(|ui| {
                let name = target.rsplit('/').next().unwrap_or(target);
                let Some(path) = path else {
                    ui.label(RichText::new(format!("{name} · not found in the vault")).size(12.0).color(pal.muted));
                    return;
                };
                player::controls(ui, &path, pal);
                let length = length_label(ui, &path);
                ui.label(RichText::new(format!("{name}{length}")).size(12.0).color(pal.muted));
                if ui.small_button("Open in system player").on_hover_text("Play it in the app your system uses for audio").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(format!("file://{}", path.display())));
                }
            });
        });
    rect.height()
}

/// " · 0:12" for a WAV file (cached per file and size), else "".
fn length_label(ui: &egui::Ui, path: &Path) -> String {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or_default();
    let key = egui::Id::unique(("quire-audio-len", path.to_path_buf(), size));
    if let Some(s) = ui.data(|d| d.get_temp::<String>(key)) {
        return s;
    }
    let s = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")) {
        std::fs::read(path).ok().and_then(|b| wav_seconds(&b)).map_or_else(String::new, |s| format!(" · {}", clock(s)))
    } else {
        String::new()
    };
    ui.data_mut(|d| d.insert_temp(key, s.clone()));
    s
}

#[cfg(feature = "voice")]
mod player {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};
    use egui::RichText;

    use crate::theme::Palette;

    /// The recording playing now (one at a time, on the UI thread).
    struct Playing {
        path: PathBuf,
        _stream: cpal::Stream,
        pos: Arc<AtomicUsize>,
        len: usize,
        rate: u32,
        done: Arc<AtomicBool>,
    }

    thread_local! {
        static NOW: RefCell<Option<Playing>> = const { RefCell::new(None) };
        static FAILED: RefCell<Option<(PathBuf, String)>> = const { RefCell::new(None) };
    }

    fn start(path: &Path) -> Result<Playing, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("can't read it: {e}"))?;
        let audio = quire_ai::wav::decode(&bytes)?;
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or("no speaker or headphones were found")?;
        let cfg = dev.default_output_config().map_err(|e| format!("the speaker can't be used: {e}"))?;
        let rate = cfg.sample_rate();
        let samples: Arc<Vec<f32>> = Arc::new(quire_ai::wav::resample(&audio.samples, audio.rate, rate));
        let channels = usize::from(cfg.channels()).max(1);
        let pos = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let parts = (Arc::clone(&samples), Arc::clone(&pos), Arc::clone(&done));
        let config = cfg.config();
        let stream = match cfg.sample_format() {
            SampleFormat::F32 => build::<f32>(&dev, &config, channels, parts),
            SampleFormat::I16 => build::<i16>(&dev, &config, channels, parts),
            SampleFormat::U16 => build::<u16>(&dev, &config, channels, parts),
            SampleFormat::I32 => build::<i32>(&dev, &config, channels, parts),
            other => return Err(format!("the speaker's sample format ({other}) isn't supported")),
        }?;
        stream.play().map_err(|e| format!("playback didn't start: {e}"))?;
        Ok(Playing { path: path.to_path_buf(), _stream: stream, pos, len: samples.len(), rate, done })
    }

    type Parts = (Arc<Vec<f32>>, Arc<AtomicUsize>, Arc<AtomicBool>);

    fn build<T>(
        dev: &cpal::Device,
        config: &cpal::StreamConfig,
        channels: usize,
        (samples, pos, done): Parts,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample + FromSample<f32> + Send + 'static,
    {
        let failed = Arc::clone(&done);
        dev.build_output_stream::<T, _, _>(
            *config,
            move |out: &mut [T], _| {
                let mut i = pos.load(Ordering::Relaxed);
                for frame in out.chunks_mut(channels) {
                    let v = samples.get(i).copied().unwrap_or(0.0);
                    for s in frame {
                        *s = T::from_sample(v);
                    }
                    i += 1;
                }
                pos.store(i.min(samples.len()), Ordering::Relaxed);
                if i >= samples.len() {
                    done.store(true, Ordering::Relaxed);
                }
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(|e| format!("the speaker can't be used: {e}"))
    }

    /// ▶ Play / ■ Stop for a WAV file, and where it is while playing.
    pub(super) fn controls(ui: &mut egui::Ui, path: &Path, pal: &Palette) {
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")) {
            return;
        }
        NOW.with_borrow_mut(|now| {
            if now.as_ref().is_some_and(|p| p.done.load(Ordering::Relaxed)) {
                *now = None;
            }
            let here = now.as_ref().filter(|p| p.path == path);
            if let Some(p) = here {
                let at = p.pos.load(Ordering::Relaxed) as f32 / p.rate.max(1) as f32;
                let total = p.len as f32 / p.rate.max(1) as f32;
                if ui.small_button("■ Stop").clicked() {
                    *now = None;
                } else {
                    ui.label(
                        RichText::new(format!("{} / {}", super::clock(at), super::clock(total))).size(12.0).monospace().color(pal.text),
                    );
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                }
            } else if ui.small_button("▶ Play").on_hover_text("Play the recording here").clicked() {
                match start(path) {
                    Ok(p) => {
                        *now = Some(p);
                        FAILED.with_borrow_mut(|f| *f = None);
                    }
                    Err(e) => FAILED.with_borrow_mut(|f| *f = Some((path.to_path_buf(), e))),
                }
            }
        });
        FAILED.with_borrow(|f| {
            if let Some((p, e)) = f
                && p == path
            {
                // Short, so the row keeps its name and "Open in system player".
                ui.label(RichText::new("Can't play here").size(11.5).color(pal.warn)).on_hover_text(e);
            }
        });
    }
}

#[cfg(not(feature = "voice"))]
mod player {
    use std::path::Path;

    use crate::theme::Palette;

    /// Quire Lite plays recordings in the system's player only.
    pub(super) fn controls(_ui: &mut egui::Ui, _path: &Path, _pal: &Palette) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_embeds_and_lengths() {
        assert_eq!(audio_embed("![[Voice note 2026-11-05 153012.wav]]"), Some("Voice note 2026-11-05 153012.wav"));
        assert_eq!(audio_embed("  ![[talk.MP3|my talk]] "), Some("talk.MP3"));
        assert_eq!(audio_embed("![](rec/a.m4a)"), Some("rec/a.m4a"));
        assert_eq!(audio_embed("![[photo.png]]"), None);
        assert_eq!(audio_embed("see ![[a.wav]] here"), None, "only a line that is just the embed");
        assert_eq!(audio_embed("![](https://x/a.mp3)"), None);
        let wav = quire_ai::wav::encode_pcm16(&vec![0.0; 16_000 * 7], 16_000);
        assert_eq!(wav_seconds(&wav).map(f32::round), Some(7.0));
        assert_eq!(wav_seconds(b"not a wav"), None);
        assert_eq!(clock(7.4), "0:07");
        assert_eq!(clock(3_723.0), "1:02:03");
    }
}
