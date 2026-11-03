// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Device-local settings: `<config>/settings.toml`.
//!
//! [`Settings`] holds the 0.8 keys at the top level. Each 0.9 feature area
//! has its own table in its own file (`[onboarding]` in `onboarding.rs`, and
//! so on), so features never edit a shared struct. Table names must not
//! collide with top-level keys or their aliases (`editor`, `assistant`): a
//! collision would make the file unreadable to older versions. Keys a
//! version doesn't know are kept (`other`), so 0.9 and later can share one file.
//!
//! Quire 0.8 reads the file but drops every key it doesn't know when it
//! saves. So 0.9 also writes what 0.8 doesn't know to a sidecar,
//! `settings.v09.toml` next to it, and marks the main file `written_by =
//! "0.9"`. A file without that mark was last saved by 0.8: tables it lacks
//! are taken back from the sidecar (docs/formats/settings.md).

mod agent_panel;
mod ai_local;
mod appearance;
mod editing;
mod jobs;
mod mcp_http;
mod onboarding;
mod tasks;
mod trust;

use std::path::{Path, PathBuf};

pub use agent_panel::AgentPanelSettings;
pub use ai_local::{AiLocalSettings, EXPERIMENTAL_ENV};
pub use appearance::AppearanceSettings;
pub use editing::EditingSettings;
pub use jobs::JobSettings;
pub use mcp_http::McpHttpSettings;
pub use onboarding::OnboardingSettings;
use serde::{Deserialize, Serialize};
pub use tasks::TaskSettings;
pub use trust::TrustSettings;

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
    /// Hide Markdown syntax (`**`, `#`, `[[ ]]`…) except on the line being edited.
    pub live_preview: bool,
    /// The first-run tour has been shown (or skipped).
    pub tour_done: bool,
    /// Ask GitHub once a day whether a newer release exists (opt-in).
    pub check_updates: bool,
    /// Unix time of the last update check.
    pub last_update_check: i64,
    /// A release the user chose to skip.
    pub skip_update_version: String,
    /// The quick-capture shortcut works from any app while Quire runs.
    pub capture_shortcut: bool,
    /// Underline misspelled words.
    pub spell_check: bool,
    /// Dictionary for spell checking (`en_US`, `en_GB`, or one installed on the system).
    pub spell_language: String,
    // ---- 0.9 tables, one per feature area (see the module docs).
    pub onboarding: OnboardingSettings,
    pub ai_local: AiLocalSettings,
    pub agent_panel: AgentPanelSettings,
    pub trust: TrustSettings,
    pub mcp_http: McpHttpSettings,
    pub editing: EditingSettings,
    pub tasks: TaskSettings,
    pub appearance: AppearanceSettings,
    pub jobs: JobSettings,
    /// Settings this version doesn't know (written by a newer version, or
    /// the other edition), kept as they are so saving doesn't drop them.
    #[serde(flatten)]
    pub other: toml::Table,
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
            live_preview: true,
            tour_done: false,
            check_updates: false,
            last_update_check: 0,
            skip_update_version: String::new(),
            capture_shortcut: true,
            spell_check: true,
            spell_language: "en_US".into(),
            onboarding: OnboardingSettings::default(),
            ai_local: AiLocalSettings::default(),
            agent_panel: AgentPanelSettings::default(),
            trust: TrustSettings::default(),
            mcp_http: McpHttpSettings::default(),
            editing: EditingSettings::default(),
            tasks: TaskSettings::default(),
            appearance: AppearanceSettings::default(),
            jobs: JobSettings::default(),
            other: toml::Table::new(),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "Quire", "Quire").map(|d| d.config_dir().to_path_buf())
}

/// The settings generation written as `written_by`.
const WRITTEN_BY: &str = "0.9";

/// The keys Quire 0.8.0 knows (and so keeps when it saves), aliases included.
const KEYS_08: &[&str] = &[
    "theme",
    "font_size",
    "line_width",
    "indent_unit",
    "autosave_ms",
    "large_file_kb",
    "show_preview",
    "show_sidebar",
    "show_insights",
    "dates_as_links",
    "inline_calculator",
    "last_vault",
    "ai_provider",
    "assistant",
    "ai_base_url",
    "ai_model",
    "assistant_model",
    "embeddings",
    "embed_model",
    "embed_base_url",
    "editor",
    "inline_images",
    "live_preview",
    "tour_done",
    "check_updates",
    "last_update_check",
    "skip_update_version",
    "capture_shortcut",
    "spell_check",
    "spell_language",
];

/// The sidecar of `path`: what only 0.9 and later know.
pub fn sidecar_path(path: &Path) -> PathBuf {
    path.with_file_name("settings.v09.toml")
}

/// Whether `written_by` names a version before 0.9 (or is missing).
fn older_than_09(written_by: Option<&toml::Value>) -> bool {
    let Some(v) = written_by.and_then(toml::Value::as_str) else { return true };
    let mut parts = v.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) < (0, 9)
}

impl Settings {
    /// The settings file. Unit tests get none, so they never touch real settings.
    pub fn path() -> Option<PathBuf> {
        if cfg!(test) {
            return None;
        }
        config_dir().map(|d| d.join("settings.toml"))
    }

    /// Settings from `path`; defaults if it's missing. A file that can't be
    /// read as settings is copied to `settings.toml.unreadable` first, so the
    /// next save doesn't silently replace what was in it.
    ///
    /// When the file was last saved by Quire 0.8 (no `written_by`), the
    /// tables and keys it dropped are restored from the sidecar.
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else { return Self::default() };
        let parsed = toml::from_str::<toml::Table>(&text).and_then(|mut t| {
            if older_than_09(t.get("written_by"))
                && let Ok(side) = std::fs::read_to_string(sidecar_path(path))
                && let Ok(side) = toml::from_str::<toml::Table>(&side)
            {
                for (k, v) in side {
                    if !KEYS_08.contains(&k.as_str()) && k != "written_by" && !t.contains_key(&k) {
                        t.insert(k, v);
                    }
                }
            }
            t.remove("written_by");
            toml::Value::Table(t).try_into::<Self>()
        });
        parsed.unwrap_or_else(|e| {
            let kept = crate::atomic::keep_unreadable(path);
            eprintln!("quire: couldn't read {} ({e}); using defaults, a copy is at {kept:?}", path.display());
            Self::default()
        })
    }

    pub fn load() -> Self {
        Self::path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    /// Write the settings, and the sidecar with what Quire 0.8 would drop.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let mut newer: toml::Table = toml::Table::try_from(self).map_err(std::io::Error::other)?;
        newer.retain(|k, _| !KEYS_08.contains(&k));
        newer.remove("written_by");
        let side = format!(
            "# Settings Quire 0.8 doesn't know, kept here in case it saves settings.toml.\n{}",
            toml::to_string_pretty(&newer).map_err(std::io::Error::other)?
        );
        crate::atomic::write(&sidecar_path(path), side.as_bytes(), false)?;
        crate::atomic::write(path, format!("written_by = \"{WRITTEN_BY}\"\n{body}").as_bytes(), false)
    }

    pub fn save(&self) -> std::io::Result<()> {
        match Self::path() {
            Some(p) => self.save_to(&p),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod compat;

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values round-trip exactly through TOML")]
mod tests {
    use super::*;

    #[test]
    fn settings_from_another_version_or_edition_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        // Written by a newer Quire: a key and a table this one doesn't know.
        std::fs::write(&p, "font_size = 18.0\nfuture_option = true\n\n[future_section]\nlevel = 3\n").unwrap();
        let mut s = Settings::load_from(&p);
        assert_eq!(s.font_size, 18.0);
        s.font_size = 19.0;
        s.save_to(&p).unwrap();
        let back = std::fs::read_to_string(&p).unwrap();
        assert!(back.contains("future_option = true") && back.contains("[future_section]") && back.contains("level = 3"), "{back}");
        assert_eq!(Settings::load_from(&p).font_size, 19.0);
    }

    #[test]
    fn an_unreadable_settings_file_is_kept_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(&p, "font_size = \"not a number\"\n").unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(s.font_size, Settings::default().font_size);
        s.save_to(&p).unwrap();
        let kept = std::fs::read_to_string(dir.path().join("settings.toml.unreadable")).unwrap();
        assert!(kept.contains("not a number"));
    }

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
