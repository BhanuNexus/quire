// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Periodic notes: the `[periodic]` section of `.quire/shared/vault.toml`
//! (design §E.4). Read and written with `VaultPrefs::section("periodic")`.
//!
//! Every key is optional; [`PeriodicPrefs::resolve`] fills in the defaults,
//! so a file that sets only `weekly.folder` keeps the other weekly defaults.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Period {
    /// (folder, name format, template) used when vault.toml doesn't say.
    /// The daily ones are 0.8's daily-note defaults.
    pub const fn defaults(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Daily => ("Daily", "%Y-%m-%d", "Daily.md"),
            Self::Weekly => ("Weekly", "%G-W%V", ""),
            Self::Monthly => ("Monthly", "%Y-%m", ""),
            Self::Yearly => ("Yearly", "%Y", ""),
        }
    }
}

/// One kind of periodic note, as written in the file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeriodConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// chrono `strftime` format for the note name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Template path relative to the templates folder; empty for none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// A period's settings with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub folder: String,
    pub format: String,
    pub template: String,
}

/// `[periodic]` in vault.toml.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeriodicPrefs {
    pub daily: PeriodConfig,
    pub weekly: PeriodConfig,
    pub monthly: PeriodConfig,
    pub yearly: PeriodConfig,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl PeriodicPrefs {
    pub const fn get(&self, p: Period) -> &PeriodConfig {
        match p {
            Period::Daily => &self.daily,
            Period::Weekly => &self.weekly,
            Period::Monthly => &self.monthly,
            Period::Yearly => &self.yearly,
        }
    }

    pub fn resolve(&self, p: Period) -> Resolved {
        let c = self.get(p);
        let (folder, format, template) = p.defaults();
        Resolved {
            folder: c.folder.clone().unwrap_or_else(|| folder.into()),
            format: c.format.clone().unwrap_or_else(|| format.into()),
            template: c.template.clone().unwrap_or_else(|| template.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_sections_keep_defaults() {
        let p: PeriodicPrefs = toml::from_str("week_start = \"monday\"\n[weekly]\nfolder = \"Weeks\"\n").unwrap();
        assert_eq!(p.resolve(Period::Weekly), Resolved { folder: "Weeks".into(), format: "%G-W%V".into(), template: String::new() });
        assert_eq!(p.resolve(Period::Daily).folder, "Daily");
        assert_eq!(p.other["week_start"].as_str(), Some("monday"));
        let back = toml::to_string(&p).unwrap();
        assert!(back.contains("week_start") && back.contains("[weekly]") && !back.contains("[daily]\nfolder"), "{back}");
    }
}
