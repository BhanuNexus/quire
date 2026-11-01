// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub font_size: f32,
    /// Maximum text column width in characters (readable line length).
    pub line_width: u32,
    pub indent_unit: String,
    /// Autosave after this many ms of idle typing.
    pub autosave_ms: u64,
    /// Files bigger than this open in plain mode.
    pub large_file_kb: usize,
    pub show_preview: bool,
    pub show_sidebar: bool,
    pub show_insights: bool,
    /// `@tomorrow` becomes `[[2026-11-02]]` (true) or `2026-11-02` (false).
    pub dates_as_links: bool,
    /// Evaluate `expr =` lines on Enter.
    pub inline_calculator: bool,
    pub last_vault: Option<PathBuf>,
    /// `"local"` (offline, default), `"anthropic"`, or `"openai"` — any
    /// OpenAI-compatible server, including Ollama and LM Studio.
    #[serde(alias = "assistant")]
    pub ai_provider: String,
    /// Empty means the provider's default (see `ai::default_base_url`).
    pub ai_base_url: String,
    /// Empty means the provider's default model.
    #[serde(alias = "assistant_model")]
    pub ai_model: String,
    /// `"lsa"` (built-in, no download) or `"endpoint"` (neural, via `/v1/embeddings`).
    pub embeddings: String,
    pub embed_model: String,
    /// Empty means: the AI base URL for OpenAI-compatible providers, else local Ollama.
    pub embed_base_url: String,
    /// `"fast"` (virtualized, multi-cursor) or `"classic"` (egui `TextEdit`).
    pub editor: String,
    /// Show images under lines that embed them.
    pub inline_images: bool,
    /// The first-run tour has been shown (or skipped).
    pub tour_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            font_size: 16.0,
            line_width: 84,
            indent_unit: "\t".into(),
            autosave_ms: 800,
            large_file_kb: 512,
            show_preview: false,
            show_sidebar: true,
            show_insights: true,
            dates_as_links: true,
            inline_calculator: true,
            last_vault: None,
            ai_provider: "local".into(),
            ai_base_url: String::new(),
            ai_model: String::new(),
            embeddings: "lsa".into(),
            embed_model: "nomic-embed-text".into(),
            embed_base_url: String::new(),
            editor: "fast".into(),
            inline_images: true,
            tour_done: false,
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "Quire", "Quire").map(|d| d.config_dir().to_path_buf())
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("settings.toml"))
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn load() -> Self {
        Self::path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, s)
    }

    pub fn save(&self) -> std::io::Result<()> {
        match Self::path() {
            Some(p) => self.save_to(&p),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values round-trip exactly through TOML")]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.toml");
        let mut s = Settings { font_size: 18.0, ..Default::default() };
        s.last_vault = Some("/notes".into());
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p), s);
        std::fs::write(&p, "font_size = 20.0\n").unwrap();
        let partial = Settings::load_from(&p);
        assert_eq!(partial.font_size, 20.0);
        assert_eq!(partial.autosave_ms, 800);
        // Settings written by earlier versions keep working.
        std::fs::write(&p, "assistant = \"anthropic\"\nassistant_model = \"claude-x\"\n").unwrap();
        let old = Settings::load_from(&p);
        assert_eq!((old.ai_provider.as_str(), old.ai_model.as_str()), ("anthropic", "claude-x"));
    }
}
