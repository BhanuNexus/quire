// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.8 and 0.9 share one settings.toml: a file written by either loads in
//! the other, and a save by either keeps what the other wrote.
//!
//! `Settings08` is the 0.8.0 `Settings` struct, copied unchanged, standing in
//! for the old binary.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Settings, Theme};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Settings08 {
    theme: Theme,
    font_size: f32,
    line_width: u32,
    indent_unit: String,
    autosave_ms: u64,
    large_file_kb: usize,
    show_preview: bool,
    show_sidebar: bool,
    show_insights: bool,
    dates_as_links: bool,
    inline_calculator: bool,
    last_vault: Option<PathBuf>,
    #[serde(alias = "assistant")]
    ai_provider: String,
    ai_base_url: String,
    #[serde(alias = "assistant_model")]
    ai_model: String,
    embeddings: String,
    embed_model: String,
    embed_base_url: String,
    editor: String,
    inline_images: bool,
    live_preview: bool,
    tour_done: bool,
    check_updates: bool,
    last_update_check: i64,
    skip_update_version: String,
    capture_shortcut: bool,
    spell_check: bool,
    spell_language: String,
    #[serde(flatten)]
    other: toml::Table,
}

impl Default for Settings08 {
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
            other: toml::Table::new(),
        }
    }
}

impl Settings08 {
    /// What 0.8's `load_from` did, failing loudly instead of falling back.
    fn load(path: &Path) -> Self {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).expect("0.8 reads the file")
    }

    fn save(&self, path: &Path) {
        std::fs::write(path, toml::to_string_pretty(self).unwrap()).unwrap();
    }
}

const FILE_08: &str = include_str!("../../tests/fixtures/settings-0.8.toml");
const FILE_09: &str = include_str!("../../tests/fixtures/settings-0.9.toml");

#[test]
fn a_0_8_file_survives_a_0_9_save_and_reads_back_in_0_8() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.toml");
    std::fs::write(&p, FILE_08).unwrap();
    let before = Settings08::load(&p);
    let s = Settings::load_from(&p);
    assert_eq!((s.ai_provider.as_str(), s.embeddings.as_str(), s.spell_language.as_str()), ("anthropic", "endpoint", "en_GB"));
    assert_eq!(s.ai_local, super::AiLocalSettings::default(), "new tables start at their defaults");
    assert!(s.other.is_empty(), "every 0.8 key is known: {:?}", s.other);
    s.save_to(&p).unwrap();
    let mut after = Settings08::load(&p);
    // 0.8 sees the new tables as keys it doesn't know, and keeps them.
    for key in ["onboarding", "ai_local", "agent_panel", "trust", "mcp_http", "editing", "tasks", "appearance", "jobs"] {
        assert!(after.other.remove(key).is_some(), "{key} written as a table");
    }
    assert_eq!(after, before, "0.8 reads exactly what it wrote");
}

#[test]
fn a_0_9_file_survives_a_0_8_save_and_reads_back_in_0_9() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.toml");
    std::fs::write(&p, FILE_09).unwrap();
    let before = Settings::load_from(&p);
    assert!(before.ai_local.embeddings && before.mcp_http.port == 7400 && before.editing.vim, "fixture sets 0.9 values");
    assert_eq!(before.editing.other.get("future_editing_key").and_then(toml::Value::as_bool), Some(true));
    assert!(before.other.contains_key("future_table"));
    // 0.8 opens it (no error, no `.unreadable` copy), changes a setting, saves.
    let mut old = Settings08::load(&p);
    assert!(old.other.contains_key("ai_local") && old.other.contains_key("future_table"));
    old.font_size = 20.0;
    old.save(&p);
    let back = Settings::load_from(&p);
    assert!(!dir.path().join("settings.toml.unreadable").exists());
    assert_eq!(back, Settings { font_size: 20.0, ..before });
    // And a 0.9 save of that keeps everything again.
    back.save_to(&p).unwrap();
    assert_eq!(Settings::load_from(&p), back);
}

#[test]
fn new_tables_never_collide_with_0_8_keys() {
    let old: toml::Table = toml::Table::try_from(Settings08::default()).unwrap();
    let new: toml::Table = toml::Table::try_from(Settings::default()).unwrap();
    let aliases = ["assistant", "assistant_model"];
    for (k, v) in &new {
        if v.is_table() {
            assert!(!old.contains_key(k) && !aliases.contains(&k.as_str()), "table [{k}] collides with a 0.8 key");
        } else {
            assert!(old.contains_key(k), "0.9 added top-level key {k}; new settings go in a table");
        }
    }
}
