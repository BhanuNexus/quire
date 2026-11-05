// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[ai_local]` in settings.toml: on-device AI (design §D.1–§D.3).
//!
//! Nothing runs or downloads unless turned on.
//!
//! On-device AI is experimental in 0.9.0 (its models aren't pinned yet):
//! everything here is ignored unless `experimental` is on, or
//! `QUIRE_EXPERIMENTAL_ON_DEVICE=1` is set for testing. The capability keys
//! keep their values either way, so turning it on later restores them.
//!
//! Owned by WS-D. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiLocalSettings {
    /// Show and use on-device AI (Settings → AI → Advanced). Off in 0.9.0.
    pub experimental: bool,
    /// The engine may run at all.
    pub engine: bool,
    /// Meaning search on this computer.
    pub embeddings: bool,
    /// Writing and agent model on this computer.
    pub llm: bool,
    /// Read text in images and scanned PDFs.
    pub ocr: bool,
    /// Catalog id of the embedder.
    pub embed_model: String,
    /// Catalog id of the language model.
    pub llm_model: String,
    /// 0 = automatic.
    pub threads: u32,
    /// `"auto"`, `"on"` or `"off"` (Metal on macOS).
    pub gpu: String,
    /// Unload an idle model after this many seconds.
    pub idle_unload_s: u64,
    /// Replaces the download host; empty for the catalog URLs.
    pub mirror_url: String,
    /// 0.10: expand every model weight into the engine's faster x86 tiles
    /// (prompts read about twice as fast; the Balanced model then uses about
    /// 4 GB instead of 1.7 GB). Off: only the Small model's are.
    pub fast_prompts: bool,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for AiLocalSettings {
    fn default() -> Self {
        Self {
            experimental: false,
            engine: false,
            embeddings: false,
            llm: false,
            ocr: false,
            embed_model: "bge-small-en-v1.5".into(),
            llm_model: "qwen3-1.7b-q4km".into(),
            threads: 0,
            gpu: "auto".into(),
            idle_unload_s: 300,
            mirror_url: String::new(),
            fast_prompts: false,
            other: toml::Table::new(),
        }
    }
}

/// Set to `1` to turn on on-device AI for this process (testing).
pub const EXPERIMENTAL_ENV: &str = "QUIRE_EXPERIMENTAL_ON_DEVICE";

fn env_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(EXPERIMENTAL_ENV).is_some_and(|v| v == "1"))
}

impl AiLocalSettings {
    /// On-device AI is offered at all: the experimental switch or
    /// [`EXPERIMENTAL_ENV`]. When false nothing on-device is shown, spawned
    /// or downloaded, whatever the other keys say.
    pub fn on_device(&self) -> bool {
        self.experimental || env_on()
    }

    /// Meaning search on this computer is on (and on-device AI is offered).
    pub fn embeddings_on(&self) -> bool {
        self.embeddings && self.on_device()
    }

    /// The on-device language model is on (and on-device AI is offered).
    pub fn llm_on(&self) -> bool {
        self.llm && self.on_device()
    }

    /// OCR is on (and on-device AI is offered).
    pub fn ocr_on(&self) -> bool {
        self.ocr && self.on_device()
    }
}
