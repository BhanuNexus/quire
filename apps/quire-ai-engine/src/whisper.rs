// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Speech to text with Whisper, for voice notes transcribed on this computer.
//!
//! Candle's `whisper` model, safetensors from `openai/whisper-{tiny,base,small}`:
//! greedy decoding without timestamps, 30-second windows, the language given
//! or detected from the first window.
//!
//! The mel filter bank is computed here (Slaney-style, as librosa makes the
//! `mel_filters.npz` that `OpenAI` ships) rather than downloaded.

use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::{D, DType, Device, IndexOp, Tensor};
use candle_transformers::models::whisper::{self as w, Config, model::Whisper};
use quire_ai::protocol::{TranscribeResult, code};
use tokenizers::Tokenizer;

use crate::Failure;

fn failed(e: impl std::fmt::Display) -> Failure {
    (code::FAILED, e.to_string())
}

/// Whisper's language codes, in its token order.
pub const LANGUAGES: &[&str] = &[
    "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it", "id", "hi", "fi", "vi", "he", "uk",
    "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn",
    "sr", "az", "sl", "kn", "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si", "km", "sn",
    "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo", "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my",
    "bo", "tl", "mg", "as", "tt", "haw", "ln", "ha", "ba", "jw", "su", "yue",
];

pub struct Transcriber {
    model: Whisper,
    tok: Tokenizer,
    device: Device,
    filters: Vec<f32>,
    sot: u32,
    eot: u32,
    transcribe: u32,
    no_timestamps: u32,
    no_speech: Option<u32>,
    /// `(code, token)` for each language the tokenizer knows.
    languages: Vec<(&'static str, u32)>,
    /// Tokens never produced (the config's list, and timestamps).
    suppress: Vec<u32>,
}

/// Slaney-scale mel filters, `n_mels × (n_fft / 2 + 1)`, row-major, as
/// `librosa.filters.mel(sr, n_fft, n_mels)` makes them.
pub fn mel_filters(sr: f32, n_fft: usize, n_mels: usize) -> Vec<f32> {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    let min_log_mel = MIN_LOG_HZ / F_SP;
    let logstep = 6.4f64.ln() / 27.0;
    let hz_to_mel = |f: f64| if f >= MIN_LOG_HZ { min_log_mel + (f / MIN_LOG_HZ).ln() / logstep } else { f / F_SP };
    let mel_to_hz = |m: f64| if m >= min_log_mel { MIN_LOG_HZ * (logstep * (m - min_log_mel)).exp() } else { F_SP * m };
    let bins = n_fft / 2 + 1;
    let fft_freqs: Vec<f64> = (0..bins).map(|i| f64::from(sr) / 2.0 * i as f64 / (bins - 1) as f64).collect();
    let top = hz_to_mel(f64::from(sr) / 2.0);
    let mel_f: Vec<f64> = (0..n_mels + 2).map(|i| mel_to_hz(top * i as f64 / (n_mels + 1) as f64)).collect();
    let mut out = vec![0f32; n_mels * bins];
    for m in 0..n_mels {
        let (lo, mid, hi) = (mel_f[m], mel_f[m + 1], mel_f[m + 2]);
        let enorm = 2.0 / (hi - lo);
        for (j, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - lo) / (mid - lo);
            let upper = (hi - f) / (hi - mid);
            out[m * bins + j] = (lower.min(upper).max(0.0) * enorm) as f32;
        }
    }
    out
}

impl Transcriber {
    pub fn load(dir: &std::path::Path, device: &Device) -> Result<Self, Failure> {
        let load_failed = |e: String| (code::LOAD_FAILED, e);
        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).map_err(|e| load_failed(format!("config.json: {e}")))?)
                .map_err(|e| load_failed(format!("config.json: {e}")))?;
        let weights = std::fs::read(dir.join("model.safetensors")).map_err(|e| load_failed(format!("model.safetensors: {e}")))?;
        let vb = candle_nn::VarBuilder::from_buffered_safetensors(weights, w::DTYPE, device).map_err(|e| load_failed(e.to_string()))?;
        let model = Whisper::load(&vb, config).map_err(|e| load_failed(e.to_string()))?;
        let tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| load_failed(format!("tokenizer.json: {e}")))?;
        Self::new(model, tok, device.clone())
    }

    /// From a loaded model and its tokenizer (tests build both in memory).
    pub fn new(model: Whisper, tok: Tokenizer, device: Device) -> Result<Self, Failure> {
        let id = |t: &str| tok.token_to_id(t).ok_or_else(|| (code::LOAD_FAILED, format!("the tokenizer has no {t}")));
        let (sot, eot, transcribe, no_timestamps) =
            (id(w::SOT_TOKEN)?, id(w::EOT_TOKEN)?, id(w::TRANSCRIBE_TOKEN)?, id(w::NO_TIMESTAMPS_TOKEN)?);
        let no_speech = w::NO_SPEECH_TOKENS.iter().find_map(|t| tok.token_to_id(t));
        let languages = LANGUAGES.iter().filter_map(|l| tok.token_to_id(&format!("<|{l}|>")).map(|t| (*l, t))).collect();
        let mut suppress: Vec<u32> = model.config.suppress_tokens.clone();
        if let Some(ts) = tok.token_to_id("<|0.00|>") {
            suppress.extend(ts..u32::try_from(model.config.vocab_size).unwrap_or(u32::MAX));
        }
        let filters = mel_filters(w::SAMPLE_RATE as f32, w::N_FFT, model.config.num_mel_bins);
        Ok(Self { model, tok, device, filters, sot, eot, transcribe, no_timestamps, no_speech, languages, suppress })
    }

    /// The text of `pcm` (16 kHz mono). `language` is a code or empty.
    pub fn run(&mut self, pcm: &[f32], language: &str, cancel: &AtomicBool) -> Result<TranscribeResult, Failure> {
        let seconds = pcm.len() as f32 / w::SAMPLE_RATE as f32;
        let n_mels = self.model.config.num_mel_bins;
        let mel = w::audio::pcm_to_mel(&self.model.config, pcm, &self.filters);
        let frames = mel.len() / n_mels;
        let mel = Tensor::from_vec(mel, (1, n_mels, frames), &self.device).map_err(failed)?;
        let content = pcm.len() / w::HOP_LENGTH;
        let mut lang = if language.is_empty() { None } else { self.languages.iter().find(|(c, _)| *c == language).copied() };
        if !language.is_empty() && lang.is_none() {
            return Err((code::BAD_PARAMS, format!("Whisper doesn't know the language {language}")));
        }
        let mut text = String::new();
        let mut seek = 0;
        while seek < content.max(1) {
            if cancel.load(Ordering::Acquire) {
                return Err((code::CANCELLED, "cancelled".into()));
            }
            // A whole 30-second window (the mel is padded), as Whisper was trained.
            let size = w::N_FRAMES.min(frames - seek);
            let segment = mel.narrow(2, seek, size).map_err(failed)?;
            let features = self.model.encoder.forward(&segment, true).map_err(failed)?;
            if lang.is_none() {
                lang = self.detect_language(&features)?;
            }
            let piece = self.decode(&features, lang.map(|(_, t)| t), cancel)?;
            let piece = piece.trim();
            if !piece.is_empty() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(piece);
            }
            seek += size;
        }
        Ok(TranscribeResult { text, language: lang.map_or_else(String::new, |(c, _)| c.to_string()), seconds })
    }

    fn logits(&mut self, tokens: &[u32], features: &Tensor, first: bool) -> Result<Tensor, Failure> {
        let t = Tensor::new(tokens, &self.device).and_then(|t| t.unsqueeze(0)).map_err(failed)?;
        let ys = self.model.decoder.forward(&t, features, first).map_err(failed)?;
        let (_, len, _) = ys.dims3().map_err(failed)?;
        self.model.decoder.final_linear(&ys.i((..1, len - 1..)).map_err(failed)?).and_then(|l| l.i(0)?.i(0)).map_err(failed)
    }

    fn detect_language(&mut self, features: &Tensor) -> Result<Option<(&'static str, u32)>, Failure> {
        if self.languages.is_empty() {
            return Ok(None);
        }
        let logits = self.logits(&[self.sot], features, true)?;
        let ids: Vec<u32> = self.languages.iter().map(|(_, t)| *t).collect();
        let picked = logits.index_select(&Tensor::new(ids.as_slice(), &self.device).map_err(failed)?, 0).map_err(failed)?;
        let best = picked.argmax(D::Minus1).and_then(|t| t.to_scalar::<u32>()).map_err(failed)? as usize;
        Ok(self.languages.get(best).copied())
    }

    /// Greedy decoding of one window; empty when it is silence.
    fn decode(&mut self, features: &Tensor, lang: Option<u32>, cancel: &AtomicBool) -> Result<String, Failure> {
        let mut tokens = vec![self.sot];
        tokens.extend(lang);
        tokens.extend([self.transcribe, self.no_timestamps]);
        let start = tokens.len();
        let vocab = self.model.config.vocab_size;
        let mut mask = vec![0f32; vocab];
        for &t in &self.suppress {
            if let Some(m) = mask.get_mut(t as usize) {
                *m = f32::NEG_INFINITY;
            }
        }
        let mask = Tensor::new(mask.as_slice(), &self.device).map_err(failed)?;
        let max_len = self.model.config.max_target_positions / 2;
        for i in 0..max_len {
            if cancel.load(Ordering::Acquire) {
                return Err((code::CANCELLED, "cancelled".into()));
            }
            let logits = self.logits(&tokens, features, i == 0)?.to_dtype(DType::F32).map_err(failed)?;
            if i == 0
                && let Some(ns) = self.no_speech
            {
                let probs = candle_nn::ops::softmax_last_dim(&logits).map_err(failed)?;
                let p = probs.i(ns as usize).and_then(|t| t.to_scalar::<f32>()).map_err(failed)?;
                if f64::from(p) > w::NO_SPEECH_THRESHOLD {
                    return Ok(String::new());
                }
            }
            let next = (logits + &mask).and_then(|l| l.argmax(D::Minus1)).and_then(|t| t.to_scalar::<u32>()).map_err(failed)?;
            if next == self.eot {
                break;
            }
            tokens.push(next);
        }
        self.tok.decode(&tokens[start..], true).map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mel_filters_match_librosa() {
        // Values from librosa 0.10.2's own `filters.mel(sr=16000, n_fft=400, n_mels=80)`.
        let f = mel_filters(16_000.0, 400, 80);
        assert_eq!(f.len(), 80 * 201);
        assert!((f[1] - 0.024_862_595).abs() < 1e-6, "{}", f[1]);
        assert!((f[79 * 201 + 195] - 0.002_243_795).abs() < 1e-6, "{}", f[79 * 201 + 195]);
        assert!(f[0].abs() < f32::EPSILON);
        // Every filter has some weight, and none beyond Nyquist.
        for m in 0..80 {
            assert!(f[m * 201..(m + 1) * 201].iter().any(|&v| v > 0.0), "filter {m}");
        }
        // Slaney normalization (librosa: 1.0665944).
        let area: f32 = f[40 * 201..41 * 201].iter().sum::<f32>() * 40.0;
        assert!((area - 1.066_594_4).abs() < 1e-4, "{area}");
    }
}
