// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.8 and 0.9 share one settings.toml: a file written by either loads in
//! the other, and a save by either keeps what the other wrote.
//!
//! `Settings08` is the 0.8.0 `Settings` struct, copied unchanged from the
//! `v0.8.0` tag, standing in for the old binary. It has no catch-all field:
//! 0.8 drops every key it doesn't know when it saves, and 0.9 gets them back
//! from its sidecar.

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
    let text = std::fs::read_to_string(&p).unwrap();
    for key in ["onboarding", "ai_local", "agent_panel", "trust", "mcp_http", "editing", "tasks", "appearance", "jobs"] {
        assert!(text.contains(&format!("[{key}]")), "{key} written as a table");
    }
    assert!(text.starts_with("written_by = \"0.9\"\n"), "{text}");
    // 0.8 ignores what it doesn't know, and reads exactly what it wrote.
    assert_eq!(Settings08::load(&p), before);
}

#[test]
fn a_0_9_file_survives_a_0_8_save_and_reads_back_in_0_9() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.toml");
    std::fs::write(&p, FILE_09).unwrap();
    let before = Settings::load_from(&p);
    assert!(before.ai_local.embeddings && before.mcp_http.port == 7400 && before.editing.vim, "fixture sets 0.9 values");
    assert_eq!(before.editing.other.get("future_editing_key").and_then(toml::Value::as_bool), Some(true));
    assert!(before.other.contains_key("future_table") && before.other.contains_key("future_flag"));
    // 0.9 saves it (and its sidecar).
    before.save_to(&p).unwrap();
    // 0.8 opens it (no error, no `.unreadable` copy), changes a setting, saves:
    // every table it doesn't know is gone from settings.toml.
    let mut old = Settings08::load(&p);
    old.font_size = 20.0;
    old.save(&p);
    assert!(!std::fs::read_to_string(&p).unwrap().contains("[editing]"), "0.8 drops what it doesn't know");
    // 0.9 takes them back from the sidecar.
    let back = Settings::load_from(&p);
    assert!(!dir.path().join("settings.toml.unreadable").exists());
    assert_eq!(back, Settings { font_size: 20.0, ..before });
    // And a 0.9 save of that keeps everything again.
    back.save_to(&p).unwrap();
    assert_eq!(Settings::load_from(&p), back);
}

#[test]
fn review_real_0_8_save_keeps_0_9_tables() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.toml");
    // 0.9 → 0.8 (change any setting) → 0.9.
    let mut s = Settings::default();
    s.editing.vim = true;
    s.onboarding.ai_done = true;
    s.mcp_http.port = 7411;
    s.save_to(&p).unwrap();
    let mut old = Settings08::load(&p);
    old.spell_check = false;
    old.save(&p);
    let back = Settings::load_from(&p);
    assert!(back.editing.vim && back.onboarding.ai_done && back.mcp_http.port == 7411, "{back:?}");
    assert!(!back.spell_check, "0.8's own change is kept too");
    // A table 0.9 writes wins over the sidecar's older copy.
    let mut again = back;
    again.editing.vim = false;
    again.save_to(&p).unwrap();
    assert!(!Settings::load_from(&p).editing.vim);
    // A file 0.9 wrote is never mixed with the sidecar.
    std::fs::write(super::sidecar_path(&p), "[editing]\nvim = true\n").unwrap();
    assert!(!Settings::load_from(&p).editing.vim);
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
