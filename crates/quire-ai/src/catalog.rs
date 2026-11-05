// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The model catalog, compiled in from `catalog.toml` (design §D.2,
//! docs/formats/model-catalog.md), plus the engine binary for this target,
//! whose SHA-256 the release build embeds (`build.rs`).
//!
//! A file is downloaded only when its SHA-256 is pinned here. An entry with
//! an unpinned file (empty `sha256`) is listed but can't be downloaded or
//! installed: Settings says it isn't available in this build.

use serde::{Deserialize, Serialize};

pub const CATALOG_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Engine,
    Embed,
    Llm,
    Ocr,
    /// Speech to text (Whisper, voice notes).
    Transcribe,
}

/// How capable a language model is, which decides what the agent panel lets
/// it do (design §D.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Chat about the open note; no tools.
    Small,
    /// Read tools and one change per message.
    #[default]
    Balanced,
    /// Every tool.
    Better,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogFile {
    /// File name under `<data_dir>/models/<id>/`.
    pub name: String,
    pub url: String,
    /// Lowercase hex; empty while not pinned (then nothing downloads).
    #[serde(default)]
    pub sha256: String,
    /// Bytes; 0 while not pinned.
    #[serde(default)]
    pub size: u64,
}

impl CatalogFile {
    pub fn is_pinned(&self) -> bool {
        self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) && self.size > 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub purpose: Purpose,
    /// Shown in Settings: "Balanced".
    pub label: String,
    /// The model's name: "Qwen3 1.7B".
    #[serde(default)]
    pub model_name: String,
    pub files: Vec<CatalogFile>,
    pub licence: String,
    /// Shown next to the licence (CC BY-SA asks for it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub attribution: String,
    /// Memory in use when loaded.
    pub ram_mb: u32,
    /// Recommended computer memory, for language models.
    #[serde(default)]
    pub min_ram_gb: u32,
    /// Embedding size, for embedders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<u32>,
    /// Language models: what the agent panel allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// Embedders: `cls` or `mean` pooling.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pooling: String,
    /// Embedders: prepended to search queries and to passages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query_prefix: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub passage_prefix: String,
    /// The upstream repository and the revision the URLs are pinned to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revision: String,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl CatalogEntry {
    /// Total download size.
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Every file has a pinned hash and size, so it may be downloaded.
    pub fn is_pinned(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(CatalogFile::is_pinned)
    }

    /// The model's display name, falling back to the label.
    pub fn display_name(&self) -> &str {
        if self.model_name.is_empty() { &self.label } else { &self.model_name }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub format: u32,
    #[serde(default, rename = "model")]
    pub models: Vec<CatalogEntry>,
}

/// Id of the engine entry.
pub const ENGINE_ID: &str = "engine";

/// The target triple this app was built for (the engine must match it).
pub const TARGET: &str = env!("QUIRE_AI_TARGET");

/// The release asset of the engine for this target and its pinned hash and
/// size, written by the release workflow before the app is built.
const ENGINE_PIN: &str = env!("QUIRE_AI_ENGINE_PIN");

/// The engine entry's key for the unpacked binary's SHA-256.
pub const BINARY_SHA256: &str = "binary_sha256";

/// SHA-256 of the engine packaged next to the app (the full edition's
/// packages; empty when this build ships without one). Written by the
/// packaging scripts after `lipo` and signing, so it is the hash of the
/// exact file installed.
const BUNDLED_ENGINE_SHA256: &str = env!("QUIRE_AI_BUNDLED_ENGINE_SHA256");

/// The hash a bundled engine must have, if this build ships one.
pub fn bundled_engine_sha256() -> Option<&'static str> {
    is_sha256(BUNDLED_ENGINE_SHA256).then_some(BUNDLED_ENGINE_SHA256)
}

/// 64 hex digits.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where release assets are downloaded from.
pub const RELEASES: &str = "https://github.com/BhanuNexus/quire/releases/download";

impl Catalog {
    pub fn parse(text: &str) -> Result<Self, String> {
        let c: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        if c.format > CATALOG_FORMAT {
            return Err(format!("catalog format {} is newer than {CATALOG_FORMAT}", c.format));
        }
        Ok(c)
    }

    /// The catalog built into this version, with the engine for this target first.
    pub fn builtin() -> Self {
        let mut c = Self::parse(include_str!("catalog.toml")).unwrap_or(Self { format: CATALOG_FORMAT, models: Vec::new() });
        c.models.insert(0, engine_entry(env!("CARGO_PKG_VERSION"), TARGET, ENGINE_PIN));
        c
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Total download size of a model.
    pub fn size_of(&self, id: &str) -> u64 {
        self.get(id).map_or(0, CatalogEntry::size)
    }

    /// Entries for one purpose, in catalog order.
    pub fn of(&self, purpose: Purpose) -> impl Iterator<Item = &CatalogEntry> {
        self.models.iter().filter(move |m| m.purpose == purpose)
    }
}

/// The engine as a catalog entry.
///
/// The file is `quire-ai-engine-<target>.gz` from this version's GitHub
/// release. `pin` is `"<sha256> <size> <binary-sha256>"` (empty in
/// development builds, where the engine comes from `QUIRE_AI_ENGINE` or
/// next to the app instead).
pub fn engine_entry(version: &str, target: &str, pin: &str) -> CatalogEntry {
    let mut parts = pin.split_whitespace();
    let sha256 = parts.next().unwrap_or_default().to_ascii_lowercase();
    let size = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut other = toml::Table::new();
    if let Some(bin) = parts.next() {
        other.insert(BINARY_SHA256.into(), bin.to_ascii_lowercase().into());
    }
    let asset = format!("quire-ai-engine-{target}.gz");
    CatalogEntry {
        id: ENGINE_ID.into(),
        purpose: Purpose::Engine,
        label: "Engine".into(),
        model_name: "Quire AI engine".into(),
        files: vec![CatalogFile { name: asset.clone(), url: format!("{RELEASES}/v{version}/{asset}"), sha256, size }],
        licence: "AGPL-3.0-or-later".into(),
        attribution: String::new(),
        ram_mb: 30,
        min_ram_gb: 0,
        dims: None,
        tier: None,
        pooling: String::new(),
        query_prefix: String::new(),
        passage_prefix: String::new(),
        repo: "BhanuNexus/quire".into(),
        revision: format!("v{version}"),
        other,
    }
}

/// `"1.1 GB"`, `"130 MB"`, `"13 MB"`.
pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    let mb = bytes as f64 / MB;
    if mb >= 1000.0 {
        let gb = mb / 1000.0;
        if gb >= 10.0 {
            format!("{gb:.0} GB")
        } else {
            let s = format!("{gb:.2}");
            format!("{} GB", s.trim_end_matches('0').trim_end_matches('.'))
        }
    } else if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{:.0} KB", (bytes as f64 / 1000.0).max(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_every_documented_model() {
        let c = Catalog::builtin();
        assert_eq!(c.models[0].id, ENGINE_ID);
        for id in [
            "bge-small-en-v1.5",
            "multilingual-e5-small",
            "qwen3-0.6b-q8",
            "qwen3-1.7b-q4km",
            "qwen3-4b-q4km",
            "ocrs",
            "whisper-tiny",
            "whisper-base",
            "whisper-small",
        ] {
            let m = c.get(id).unwrap_or_else(|| panic!("{id} missing"));
            assert!(!m.files.is_empty(), "{id}");
            assert!(m.files.iter().all(|f| f.url.starts_with("https://")), "{id}");
            assert!(!m.licence.is_empty(), "{id}");
        }
        assert_eq!(c.get("qwen3-0.6b-q8").unwrap().tier, Some(Tier::Small));
        assert_eq!(c.get("qwen3-4b-q4km").unwrap().tier, Some(Tier::Better));
        assert_eq!(c.get("bge-small-en-v1.5").unwrap().dims, Some(384));
        assert!(c.get("ocrs").unwrap().attribution.contains("HierText"));
        assert_eq!(c.of(Purpose::Transcribe).count(), 3);
        for m in c.of(Purpose::Transcribe) {
            let names: Vec<&str> = m.files.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(names, ["model.safetensors", "config.json", "tokenizer.json"], "{}", m.id);
        }
        // Hugging Face URLs name the revision they are pinned to (the first
        // file comes from the entry's own repository).
        for m in c.models.iter().filter(|m| m.files[0].url.starts_with("https://huggingface.co/")) {
            assert!(m.files[0].url.contains(&format!("{}/resolve/{}/", m.repo, m.revision)), "{}", m.id);
        }
    }

    #[test]
    fn entries_parse_and_pins_are_checked() {
        let c = Catalog::parse(
            r#"format = 1
[[model]]
id = "bge-small-en-v1.5"
purpose = "embed"
label = "English"
licence = "MIT"
ram_mb = 250
dims = 384
revision = "5c38ec7c"
custom = "kept"
[[model.files]]
name = "model.safetensors"
url = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/5c38ec7c/model.safetensors"
sha256 = "00"
size = 133466304
"#,
        )
        .unwrap();
        assert_eq!(c.size_of("bge-small-en-v1.5"), 133_466_304);
        let m = c.get("bge-small-en-v1.5").unwrap();
        assert_eq!(m.other["custom"].as_str(), Some("kept"));
        assert!(!m.is_pinned(), "a short hash is not a pin");
        assert!(Catalog::parse("format = 2").is_err());
        let e = engine_entry("0.9.0", "x86_64-unknown-linux-gnu", &format!("{} 1234 {}", "A".repeat(64), "b".repeat(64)));
        assert!(e.is_pinned());
        assert_eq!(e.other[BINARY_SHA256].as_str(), Some("b".repeat(64).as_str()));
        assert_eq!(
            e.files[0].url,
            "https://github.com/BhanuNexus/quire/releases/download/v0.9.0/quire-ai-engine-x86_64-unknown-linux-gnu.gz"
        );
        assert!(!engine_entry("0.9.0", "t", "").is_pinned());
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(133_466_304), "133 MB");
        assert_eq!(human_size(1_100_000_000), "1.1 GB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
        assert_eq!(human_size(1_000_000_000), "1 GB");
        assert_eq!(human_size(13_000_000), "13 MB");
        assert_eq!(human_size(12_345_678_900), "12 GB");
    }
}
