// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The model catalog, compiled in from `catalog.toml` (design §D.2).

use serde::{Deserialize, Serialize};

pub const CATALOG_FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Engine,
    Embed,
    Llm,
    Ocr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogFile {
    /// File name under `<data_dir>/models/<id>/`.
    pub name: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub purpose: Purpose,
    /// Shown in Settings: "Balanced".
    pub label: String,
    pub files: Vec<CatalogFile>,
    pub licence: String,
    /// Memory in use when loaded.
    pub ram_mb: u32,
    /// Embedding size, for embedders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<u32>,
    #[serde(flatten)]
    pub other: toml::Table,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub format: u32,
    #[serde(default, rename = "model")]
    pub models: Vec<CatalogEntry>,
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Self, String> {
        let c: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        if c.format > CATALOG_FORMAT {
            return Err(format!("catalog format {} is newer than {CATALOG_FORMAT}", c.format));
        }
        Ok(c)
    }

    /// The catalog built into this version.
    pub fn builtin() -> Self {
        Self::parse(include_str!("catalog.toml")).unwrap_or(Self { format: CATALOG_FORMAT, models: Vec::new() })
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Total download size of a model.
    pub fn size_of(&self, id: &str) -> u64 {
        self.get(id).map_or(0, |m| m.files.iter().map(|f| f.size).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses_and_entries_have_the_documented_shape() {
        assert!(Catalog::builtin().models.is_empty());
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
[[model.files]]
name = "model.safetensors"
url = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/5c38ec7c/model.safetensors"
sha256 = "00"
size = 133466304
"#,
        )
        .unwrap();
        assert_eq!(c.size_of("bge-small-en-v1.5"), 133_466_304);
        assert_eq!(c.get("bge-small-en-v1.5").unwrap().other["revision"].as_str(), Some("5c38ec7c"));
        assert!(Catalog::parse("format = 2").is_err());
    }
}
