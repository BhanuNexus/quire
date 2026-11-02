// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[ai_local]` in settings.toml: on-device AI (design §D.1–§D.3).
//!
//! Nothing runs or downloads unless turned on.
//!
//! Owned by WS-D. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiLocalSettings {
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
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for AiLocalSettings {
    fn default() -> Self {
        Self {
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
            other: toml::Table::new(),
        }
    }
}
