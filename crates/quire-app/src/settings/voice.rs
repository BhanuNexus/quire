// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[voice]` in settings.toml: voice notes (0.10).
//!
//! Recording happens only when the person presses Record. Transcription is
//! off until they choose where it runs: on this computer (Whisper in the
//! on-device engine, while on-device AI is experimental) or through their
//! OpenAI-compatible provider, which receives the recording.
//!
//! Every field defaults to off or unchanged; keys this version doesn't know
//! are kept in `other`, so older and newer Quire versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Where recordings are transcribed: `""` (not at all), `"device"` or
    /// `"provider"`.
    pub transcribe: String,
    /// Catalog id of the on-device Whisper model.
    pub device_model: String,
    /// The provider's transcription model (`/v1/audio/transcriptions`).
    pub provider_model: String,
    /// A language code (`en`, `de`), or empty to let the model detect it.
    pub language: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            transcribe: String::new(),
            device_model: "whisper-base".into(),
            provider_model: "whisper-1".into(),
            language: String::new(),
            other: toml::Table::new(),
        }
    }
}
