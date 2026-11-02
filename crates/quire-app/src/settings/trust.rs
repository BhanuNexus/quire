// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[trust]` in settings.toml: trust tools (design §C).
//!
//! Owned by WS-C. Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustSettings {
    /// Find contradictions (preview; needs an AI model).
    pub contradictions: bool,
    /// Cloud requests per day for claim checks.
    pub claims_daily_cap: u32,
    /// Ask what you expect before showing an answer.
    pub ask_before_tell: bool,
    /// Weekly digest of notes that look out of date.
    pub staleness: bool,
    /// Check that cited passages still say what was quoted.
    pub verify_citations: bool,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for TrustSettings {
    fn default() -> Self {
        Self {
            contradictions: false,
            claims_daily_cap: 200,
            ask_before_tell: false,
            staleness: true,
            verify_citations: true,
            other: toml::Table::new(),
        }
    }
}
