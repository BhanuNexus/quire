// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Voice notes (0.10): a recording saved as a WAV attachment, embedded in
//! the note, with a transcript when the person chose where transcription
//! runs (`[voice] transcribe`).
//!
//! * Nothing records until the person presses Record (the desktop app owns
//!   the microphone; this module only sees the samples it hands over).
//! * Nothing is transcribed unless `[voice] transcribe` is `device` or
//!   `provider`. `device` runs Whisper in the on-device engine and is
//!   offered only while on-device AI is (experimental, as models are
//!   unpinned). `provider` sends the recording to the OpenAI-compatible
//!   provider set in Settings → AI (`/v1/audio/transcriptions`).
//! * The recording stays in the vault either way; the transcript is text the
//!   person can edit like any other.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::Receiver;
use quire_intel::chat::CancelFlag;
use quire_vault::NotePath;

use crate::settings::Settings;
use crate::workspace::{NoticeLevel, Workspace};

/// What the voice note command needs to know (in `ws.features.ai.voice`).
#[derive(Debug, Default)]
pub struct VoiceState {
    /// This build can record (the desktop's `voice` feature); the command is
    /// offered only then.
    pub can_record: bool,
    /// The next recording goes to today's note (started from quick capture).
    pub to_capture: bool,
}

/// Where a transcription runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Device,
    Provider,
}

/// Turns a WAV file into text (or says why it couldn't).
type RunFn = dyn Fn(&Path, &CancelFlag) -> Result<String, String> + Send + Sync;

/// A way to turn a WAV file into text, and what to tell the person about it.
#[derive(Clone)]
pub struct Transcription {
    pub place: Place,
    /// "Whisper base on this computer", "whisper-1 at api.openai.com".
    pub label: String,
    run: Arc<RunFn>,
}

impl std::fmt::Debug for Transcription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcription").field("place", &self.place).field("label", &self.label).finish_non_exhaustive()
    }
}

impl Transcription {
    /// A stand-in (tests and UI checks).
    pub fn custom(
        place: Place,
        label: impl Into<String>,
        run: impl Fn(&Path, &CancelFlag) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self { place, label: label.into(), run: Arc::new(run) }
    }

    /// Transcribe on a background thread.
    pub fn start(&self, wav: PathBuf) -> TranscribeJob {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let cancel = CancelFlag::new();
        let (run, c) = (Arc::clone(&self.run), cancel.clone());
        std::thread::spawn(move || {
            let _ = tx.send(run(&wav, &c));
        });
        TranscribeJob { rx, cancel, label: self.label.clone() }
    }
}

/// A transcription in progress.
#[derive(Debug)]
pub struct TranscribeJob {
    rx: Receiver<Result<String, String>>,
    cancel: CancelFlag,
    pub label: String,
}

impl TranscribeJob {
    /// The result once it is there.
    pub fn poll(&self) -> Option<Result<String, String>> {
        self.rx.try_recv().ok()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// The host of a base URL, for "at api.openai.com".
#[cfg_attr(not(any(feature = "remote-llm", test)), expect(dead_code, reason = "only providers have a host"))]
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Why transcription isn't available with these settings (`None`: it is,
/// or it is off by choice).
pub fn unavailable_reason(settings: &Settings) -> Option<&'static str> {
    match settings.voice.transcribe.as_str() {
        "device" if !settings.ai_local.on_device() => Some("Transcription on this computer needs on-device AI, which is experimental."),
        "device" => {
            let model = crate::ai::catalog().get(&settings.voice.device_model);
            (!model.is_some_and(crate::ai::installed) || !crate::ai::engine_installed())
                .then_some("Download the speech model in Settings → AI → On this computer first.")
        }
        "provider" if settings.ai_provider != "openai" => {
            Some("Transcription through a provider needs an OpenAI-compatible provider in Settings → AI.")
        }
        _ => None,
    }
}

/// How recordings are transcribed with these settings, or `None` (off, or
/// not available: see [`unavailable_reason`]).
pub fn transcription(settings: &Settings) -> Option<Transcription> {
    if unavailable_reason(settings).is_some() {
        return None;
    }
    let language = settings.voice.language.trim().to_string();
    match settings.voice.transcribe.as_str() {
        "device" => {
            let entry = crate::ai::catalog().get(&settings.voice.device_model)?.clone();
            let setup = crate::ai::local_setup(settings)?;
            let engine = crate::ai::engine(settings);
            let local = quire_ai::local::LocalTranscriber::new(engine, &entry, &setup);
            let label = format!("{} on this computer", entry.display_name());
            Some(Transcription::custom(Place::Device, label, move |wav, cancel| {
                local.transcribe(wav, &language, cancel).map(|r| r.text).map_err(|e| e.to_string())
            }))
        }
        "provider" => provider_transcription(settings, &language),
        _ => None,
    }
}

#[cfg(feature = "remote-llm")]
#[expect(clippy::unnecessary_wraps, reason = "matches the build without providers, which has none")]
fn provider_transcription(settings: &Settings, language: &str) -> Option<Transcription> {
    let language = language.to_string();
    let (base, _) = crate::ai::resolved(settings);
    let key = crate::secrets::api_key(crate::secrets::store().as_ref(), "openai");
    let model = if settings.voice.provider_model.trim().is_empty() {
        "whisper-1".to_string()
    } else {
        settings.voice.provider_model.trim().to_string()
    };
    let label = format!("{model} at {}", host(&base));
    let client = quire_intel::llm::OpenAiCompatible::new(base, model.clone(), key);
    Some(Transcription::custom(Place::Provider, label, move |wav, cancel| {
        let bytes = std::fs::read(wav).map_err(|e| e.to_string())?;
        let name = wav.file_name().map_or_else(|| "voice-note.wav".into(), |n| n.to_string_lossy().into_owned());
        if cancel.is_cancelled() {
            return Err("stopped".into());
        }
        client.transcribe(&model, &bytes, &name, &language).map_err(|e| e.to_string())
    }))
}

#[cfg(not(feature = "remote-llm"))]
fn provider_transcription(_settings: &Settings, _language: &str) -> Option<Transcription> {
    None
}

/// `0:42`, `12:05`.
pub fn duration_label(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// What goes into the note: the recording's embed, then the transcript (if
/// any) in an `ai` callout that says what wrote it.
pub fn voice_note_markdown(file_name: &str, seconds: f32, transcript: Option<(&str, &str)>) -> String {
    let mut out = format!("![[{file_name}]]\n");
    if let Some((text, by)) = transcript.filter(|(t, _)| !t.trim().is_empty()) {
        let _ = write!(out, "\n> [!ai] Transcript of the {} voice note · {by}\n", duration_label(seconds));
        for line in text.trim().lines() {
            out.push_str("> ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

/// The file name for a recording made now.
pub fn recording_name(now: chrono::DateTime<chrono::Local>) -> String {
    format!("Voice note {}.wav", now.format("%Y-%m-%d %H%M%S"))
}

impl Workspace {
    /// Save a recording (mono samples at `rate`) as a 16 kHz WAV attachment.
    pub fn save_recording(&mut self, samples: &[f32], rate: u32) -> Option<NotePath> {
        let vault = self.vault.clone()?;
        let pcm = quire_ai::wav::resample(samples, rate, quire_ai::wav::VOICE_RATE);
        let bytes = quire_ai::wav::encode_pcm16(&pcm, quire_ai::wav::VOICE_RATE);
        match vault.save_attachment(&recording_name(chrono::Local::now()), &bytes) {
            Ok(p) => {
                self.file_cache.clear();
                Some(p)
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save the recording: {e}"));
                None
            }
        }
    }

    /// Transcribe a saved attachment with the configured transcription.
    pub fn transcribe_attachment(&self, path: &NotePath) -> Option<TranscribeJob> {
        let t = transcription(&self.settings)?;
        let file = self.attachment_path(path.file_name())?;
        Some(t.start(file))
    }

    /// Put a voice note at the caret of the active note, or append it to the
    /// daily capture when no note is open. Returns where it went.
    pub fn insert_voice_note(&mut self, markdown: &str) -> Option<NotePath> {
        if let Some(p) = self.active_doc().and_then(|d| d.path.clone()) {
            let needs_break = self.active_doc().is_some_and(|d| {
                let s = d.selection().start();
                s > 0 && d.buffer().char_at(s - 1).is_some_and(|c| c != '\n')
            });
            self.insert_text(&if needs_break { format!("\n{markdown}") } else { markdown.to_string() });
            return Some(p);
        }
        self.capture(markdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::Vault;

    #[test]
    fn markdown_embeds_the_recording_and_labels_the_transcript() {
        let md =
            voice_note_markdown("Voice note 2026-11-05 153012.wav", 42.4, Some(("Buy milk.\nCall Sam.", "Whisper base on this computer")));
        assert_eq!(
            md,
            "![[Voice note 2026-11-05 153012.wav]]\n\n> [!ai] Transcript of the 0:42 voice note · Whisper base on this computer\n> Buy milk.\n> Call Sam.\n"
        );
        assert_eq!(voice_note_markdown("a.wav", 3.0, Some(("  ", "x"))), "![[a.wav]]\n");
        assert_eq!(voice_note_markdown("a.wav", 3.0, None), "![[a.wav]]\n");
        assert_eq!(duration_label(725.0), "12:05");
        assert_eq!(host("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(host("http://localhost:8080"), "localhost:8080");
    }

    #[test]
    fn transcription_is_off_until_chosen_and_device_needs_the_switch() {
        let mut s = Settings::default();
        assert!(transcription(&s).is_none(), "off by default");
        assert_eq!(unavailable_reason(&s), None);
        s.voice.transcribe = "device".into();
        s.ai_local.experimental = false;
        if !s.ai_local.on_device() {
            assert!(unavailable_reason(&s).is_some_and(|r| r.contains("experimental")));
            assert!(transcription(&s).is_none());
        }
        s.voice.transcribe = "provider".into();
        s.ai_provider = "anthropic".into();
        assert!(unavailable_reason(&s).is_some_and(|r| r.contains("OpenAI-compatible")));
        s.ai_provider = "openai".into();
        s.ai_base_url = "https://api.example.com".into();
        assert_eq!(unavailable_reason(&s), None);
        #[cfg(feature = "remote-llm")]
        assert_eq!(transcription(&s).map(|t| (t.place, t.label)), Some((Place::Provider, "whisper-1 at api.example.com".to_string())));
    }

    #[test]
    fn a_recording_is_saved_transcribed_and_inserted() {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[("Note.md", "# Note\nText")]));
        let samples = vec![0.1f32; 48_000];
        let p = w.save_recording(&samples, 48_000).unwrap();
        assert!(p.file_name().starts_with("Voice note ") && p.file_name().rsplit_once('.').is_some_and(|(_, e)| e == "wav"), "{p}");
        let bytes = w.vault().unwrap().storage().read_bytes(&p).unwrap();
        let audio = quire_ai::wav::decode(&bytes).unwrap();
        assert_eq!((audio.rate, audio.samples.len()), (16_000, 16_000), "one second at 16 kHz");
        let t = Transcription::custom(Place::Provider, "test", |_, _| Ok("Hello there.".into()));
        let job = t.start(PathBuf::from("/nowhere.wav"));
        let text = loop {
            if let Some(r) = job.poll() {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        w.open_note(&NotePath::new("Note.md")).unwrap();
        let md = voice_note_markdown(p.file_name(), audio.seconds(), Some((&text, &job.label)));
        assert_eq!(w.insert_voice_note(&md), Some(NotePath::new("Note.md")));
        let doc = w.active_doc().unwrap().text().to_string();
        assert!(doc.contains(&format!("![[{}]]", p.file_name())), "{doc}");
        assert!(doc.contains("> Hello there."));
    }
}
