// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Periodic notes: the `[periodic]` section of `.quire/shared/vault.toml`
//! (design §E.4). Read and written with `VaultPrefs::section("periodic")`.
//!
//! Every key is optional; [`PeriodicPrefs::resolve`] fills in the defaults,
//! so a file that sets only `weekly.folder` keeps the other weekly defaults.

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Period {
    pub const ALL: [Self; 4] = [Self::Daily, Self::Weekly, Self::Monthly, Self::Yearly];

    /// "week", for "this week's note".
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Daily => "day",
            Self::Weekly => "week",
            Self::Monthly => "month",
            Self::Yearly => "year",
        }
    }

    /// The first day of the period holding `d` (weeks start on Monday, ISO 8601).
    pub fn start_of(self, d: NaiveDate) -> NaiveDate {
        match self {
            Self::Daily => d,
            Self::Weekly => d - Duration::days(i64::from(d.weekday().num_days_from_monday())),
            Self::Monthly => d.with_day(1).unwrap_or(d),
            Self::Yearly => NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d),
        }
    }

    /// The start of the period `n` periods after the one holding `d`.
    pub fn step(self, d: NaiveDate, n: i32) -> NaiveDate {
        let s = self.start_of(d);
        match self {
            Self::Daily => s + Duration::days(i64::from(n)),
            Self::Weekly => s + Duration::weeks(i64::from(n)),
            Self::Monthly => {
                let m = s.year() * 12 + s.month0() as i32 + n;
                NaiveDate::from_ymd_opt(m.div_euclid(12), m.rem_euclid(12) as u32 + 1, 1).unwrap_or(s)
            }
            Self::Yearly => NaiveDate::from_ymd_opt(s.year() + n, 1, 1).unwrap_or(s),
        }
    }

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
        // A format chrono can't print falls back to the default (it would panic).
        let fmt = c.format.clone().filter(|f| !f.trim().is_empty() && valid_format(f)).unwrap_or_else(|| format.into());
        Resolved {
            folder: c.folder.as_deref().map_or_else(|| folder.into(), |f| f.trim().trim_matches('/').to_string()),
            format: fmt,
            template: c.template.clone().unwrap_or_else(|| template.into()),
        }
    }

    pub fn get_mut(&mut self, p: Period) -> &mut PeriodConfig {
        match p {
            Period::Daily => &mut self.daily,
            Period::Weekly => &mut self.weekly,
            Period::Monthly => &mut self.monthly,
            Period::Yearly => &mut self.yearly,
        }
    }

    /// The note for the period holding `d`: `Weekly/2026-W45.md`.
    pub fn path_for(&self, p: Period, d: NaiveDate) -> String {
        let r = self.resolve(p);
        let name = p.start_of(d).format(&r.format).to_string().replace(['/', '\\'], "-");
        if r.folder.is_empty() { format!("{name}.md") } else { format!("{}/{name}.md", r.folder) }
    }

    /// Which periodic note `path` is, and the first day of its period.
    pub fn identify(&self, path: &str) -> Option<(Period, NaiveDate)> {
        let path = path.strip_suffix(".md")?;
        let (folder, stem) = path.rsplit_once('/').unwrap_or(("", path));
        for p in Period::ALL {
            let r = self.resolve(p);
            if !r.folder.eq_ignore_ascii_case(folder) {
                continue;
            }
            if let Some(d) = parse_name(p, stem, &r.format)
                && self.path_for(p, d).strip_suffix(".md").is_some_and(|x| x.eq_ignore_ascii_case(path))
            {
                return Some((p, d));
            }
        }
        None
    }
}

fn valid_format(f: &str) -> bool {
    !chrono::format::StrftimeItems::new(f).any(|i| matches!(i, chrono::format::Item::Error))
}

/// Read a note name back into a date, for `format`.
fn parse_name(p: Period, stem: &str, format: &str) -> Option<NaiveDate> {
    let direct = match p {
        Period::Daily => NaiveDate::parse_from_str(stem, format).ok(),
        Period::Weekly => NaiveDate::parse_from_str(&format!("{stem} 1"), &format!("{format} %u")).ok(),
        Period::Monthly => NaiveDate::parse_from_str(&format!("{stem} 1"), &format!("{format} %d")).ok(),
        Period::Yearly => NaiveDate::parse_from_str(&format!("{stem} 1 1"), &format!("{format} %m %d")).ok(),
    };
    if let Some(d) = direct.map(|d| p.start_of(d))
        && d.format(format).to_string() == stem
    {
        return Some(d);
    }
    // Formats chrono can't read back: try the periods of the last and next 30 years.
    let today = chrono::Local::now().date_naive();
    let span = match p {
        Period::Daily => return None,
        Period::Weekly => 52 * 30,
        Period::Monthly => 12 * 30,
        Period::Yearly => 30,
    };
    (-span..=span).map(|n| p.step(today, n)).find(|d| d.format(format).to_string() == stem)
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

    #[test]
    fn names_and_periods() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let mut p = PeriodicPrefs::default();
        assert_eq!(p.path_for(Period::Daily, d), "Daily/2026-11-04.md");
        assert_eq!(p.path_for(Period::Weekly, d), "Weekly/2026-W45.md");
        assert_eq!(p.path_for(Period::Monthly, d), "Monthly/2026-11.md");
        assert_eq!(p.path_for(Period::Yearly, d), "Yearly/2026.md");
        assert_eq!(p.identify("Weekly/2026-W45.md"), Some((Period::Weekly, NaiveDate::from_ymd_opt(2026, 11, 2).unwrap())));
        assert_eq!(p.identify("Monthly/2026-11.md"), Some((Period::Monthly, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap())));
        assert_eq!(p.identify("Yearly/2026.md"), Some((Period::Yearly, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap())));
        assert_eq!(p.identify("Daily/2026-11-04.md"), Some((Period::Daily, d)));
        assert_eq!(p.identify("Notes/2026-11-04.md"), None);
        assert_eq!(p.identify("Weekly/Plan.md"), None);
        assert_eq!(Period::Monthly.step(NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(), -1), NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(Period::Weekly.step(d, 1), NaiveDate::from_ymd_opt(2026, 11, 9).unwrap());
        // A custom format that chrono can't read back directly, and a broken one.
        p.weekly.format = Some("Week %V of %G".into());
        p.monthly.format = Some("%B %Y".into());
        p.yearly.format = Some("%Q".into());
        assert_eq!(p.path_for(Period::Weekly, d), "Weekly/Week 45 of 2026.md");
        assert_eq!(p.identify("Weekly/Week 45 of 2026.md").map(|x| x.1), Some(NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()));
        assert_eq!(p.identify("Monthly/November 2026.md").map(|x| x.1), Some(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()));
        assert_eq!(p.path_for(Period::Yearly, d), "Yearly/2026.md");
    }
}
