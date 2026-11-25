// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Periodic notes: the `[periodic]` section of `.quire/shared/vault.toml`
//! (`docs/design/0.9.md` §E.4). Read and written with `VaultPrefs::section("periodic")`.
//!
//! Every key is optional; [`PeriodicPrefs::resolve`] fills in the defaults,
//! so a file that sets only `weekly.folder` keeps the other weekly defaults.

use quire_base::dates::{self, Date};
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
    pub fn start_of(self, d: Date) -> Date {
        match self {
            Self::Daily => d,
            Self::Weekly => dates::plus_days(d, -dates::from_monday(d.weekday())),
            Self::Monthly => d.first_of_month(),
            Self::Yearly => d.first_of_year(),
        }
    }

    /// The start of the period `n` periods after the one holding `d`.
    pub fn step(self, d: Date, n: i32) -> Date {
        let s = self.start_of(d);
        match self {
            Self::Daily => dates::plus_days(s, i64::from(n)),
            Self::Weekly => dates::plus_days(s, 7 * i64::from(n)),
            Self::Monthly => {
                let m = i32::from(s.year()) * 12 + i32::from(s.month() - 1) + n;
                dates::ymd_opt(m.div_euclid(12), m.rem_euclid(12) as u32 + 1, 1).unwrap_or(s)
            }
            Self::Yearly => dates::ymd_opt(i32::from(s.year()) + n, 1, 1).unwrap_or(s),
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
    /// A `strftime` format for the note name (as chrono read it: `quire_base::dates`).
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
        // A format that isn't one falls back to the default.
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
    pub fn path_for(&self, p: Period, d: Date) -> String {
        let r = self.resolve(p);
        let start = p.start_of(d);
        // A format that can't write a date (`%H`) names it by the default, where chrono panicked.
        let name = dates::format(&r.format, &dates::Moment::date(start))
            .unwrap_or_else(|_| dates::fmt(p.defaults().1, &dates::Moment::date(start)))
            .replace(['/', '\\'], "-");
        if r.folder.is_empty() { format!("{name}.md") } else { format!("{}/{name}.md", r.folder) }
    }

    /// Which periodic note `path` is, and the first day of its period.
    pub fn identify(&self, path: &str) -> Option<(Period, Date)> {
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
    dates::is_valid(f)
}

/// Read a note name back into a date, for `format`.
fn parse_name(p: Period, stem: &str, format: &str) -> Option<Date> {
    let direct = match p {
        Period::Daily => dates::parse_date(stem, format).ok(),
        Period::Weekly => dates::parse_date(&format!("{stem} 1"), &format!("{format} %u")).ok(),
        Period::Monthly => dates::parse_date(&format!("{stem} 1"), &format!("{format} %d")).ok(),
        Period::Yearly => dates::parse_date(&format!("{stem} 1 1"), &format!("{format} %m %d")).ok(),
    };
    if let Some(d) = direct.map(|d| p.start_of(d))
        && dates::format(format, &dates::Moment::date(d)).is_ok_and(|s| s == stem)
    {
        return Some(d);
    }
    // Formats that can't be read back: try the periods of the last and next 30 years.
    let today = dates::today();
    let span = match p {
        Period::Daily => return None,
        Period::Weekly => 52 * 30,
        Period::Monthly => 12 * 30,
        Period::Yearly => 30,
    };
    (-span..=span).map(|n| p.step(today, n)).find(|d| dates::format(format, &dates::Moment::date(*d)).is_ok_and(|s| s == stem))
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
        let d = dates::ymd_opt(2026, 11, 4).unwrap();
        let mut p = PeriodicPrefs::default();
        assert_eq!(p.path_for(Period::Daily, d), "Daily/2026-11-04.md");
        assert_eq!(p.path_for(Period::Weekly, d), "Weekly/2026-W45.md");
        assert_eq!(p.path_for(Period::Monthly, d), "Monthly/2026-11.md");
        assert_eq!(p.path_for(Period::Yearly, d), "Yearly/2026.md");
        assert_eq!(p.identify("Weekly/2026-W45.md"), Some((Period::Weekly, dates::ymd_opt(2026, 11, 2).unwrap())));
        assert_eq!(p.identify("Monthly/2026-11.md"), Some((Period::Monthly, dates::ymd_opt(2026, 11, 1).unwrap())));
        assert_eq!(p.identify("Yearly/2026.md"), Some((Period::Yearly, dates::ymd_opt(2026, 1, 1).unwrap())));
        assert_eq!(p.identify("Daily/2026-11-04.md"), Some((Period::Daily, d)));
        assert_eq!(p.identify("Notes/2026-11-04.md"), None);
        assert_eq!(p.identify("Weekly/Plan.md"), None);
        assert_eq!(Period::Monthly.step(dates::ymd_opt(2026, 1, 31).unwrap(), -1), dates::ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(Period::Weekly.step(d, 1), dates::ymd_opt(2026, 11, 9).unwrap());
        // A custom format that chrono can't read back directly, and a broken one.
        p.weekly.format = Some("Week %V of %G".into());
        p.monthly.format = Some("%B %Y".into());
        p.yearly.format = Some("%Q".into());
        assert_eq!(p.path_for(Period::Weekly, d), "Weekly/Week 45 of 2026.md");
        assert_eq!(p.identify("Weekly/Week 45 of 2026.md").map(|x| x.1), Some(dates::ymd_opt(2026, 11, 2).unwrap()));
        assert_eq!(p.identify("Monthly/November 2026.md").map(|x| x.1), Some(dates::ymd_opt(2026, 11, 1).unwrap()));
        assert_eq!(p.path_for(Period::Yearly, d), "Yearly/2026.md");
    }
}
