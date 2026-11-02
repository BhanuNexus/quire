// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Periodic notes and the calendar (design §E.4).
//!
//! `[periodic]` in `.quire/shared/vault.toml` says where daily, weekly,
//! monthly and yearly notes go, how they're named and which template they
//! use; the daily settings drive "Open today's daily note" too. The
//! calendar marks each day: a daily note exists, tasks are due, and how
//! many words that day's note has.
//!
//! Owned by WS-E; kept in `ws.features.periodic`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{Datelike, NaiveDate};
use quire_plan::periodic::{Period, PeriodicPrefs};
use quire_vault::{NotePath, VaultPrefs};

use crate::document::DocId;
use crate::workspace::{NoticeLevel, Workspace};

/// What the calendar shows for one day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DayMark {
    /// A daily note exists.
    pub note: bool,
    /// Open tasks due that day.
    pub due: usize,
    /// Words in that day's daily note.
    pub words: usize,
}

/// Day marks per month, for the index version they were worked out at.
type MonthCache = RefCell<HashMap<(i32, u32), (u64, Rc<Vec<DayMark>>)>>;

/// State for this feature.
#[derive(Debug, Default)]
pub struct PeriodicState {
    pub prefs: PeriodicPrefs,
    loaded: bool,
    /// The file was made by a newer Quire or can't be read: show, don't save.
    pub read_only: Option<String>,
    month_cache: MonthCache,
}

/// Read `[periodic]` and point "today's daily note" at it.
pub(crate) fn load(ws: &mut Workspace) {
    let Some(vault) = ws.vault.as_ref() else { return };
    let prefs = VaultPrefs::load(vault);
    let st = &mut ws.features.periodic;
    st.read_only = prefs.is_newer().then(|| "made by a newer Quire".to_string());
    st.prefs = prefs.section("periodic");
    st.loaded = true;
    apply_daily(ws);
}

fn apply_daily(ws: &mut Workspace) {
    let d = ws.features.periodic.prefs.resolve(Period::Daily);
    if let Some(v) = ws.vault.as_mut() {
        v.config.daily_folder = d.folder;
        v.config.daily_format = d.format;
        v.config.daily_template = d.template;
    }
}

impl Workspace {
    pub fn periodic_prefs(&self) -> &PeriodicPrefs {
        &self.features.periodic.prefs
    }

    /// Save `[periodic]` (comments and other sections in vault.toml stay).
    pub fn set_periodic_prefs(&mut self, prefs: PeriodicPrefs) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("Open a vault first")?;
        let mut file = VaultPrefs::load(&vault);
        file.set_section("periodic", &prefs).map_err(|e| e.to_string())?;
        file.save(&vault).map_err(|e| e.to_string())?;
        self.features.periodic.prefs = prefs;
        apply_daily(self);
        Ok(())
    }

    /// Open (or make) the note for the period holding `date`.
    pub fn open_period(&mut self, p: Period, date: NaiveDate) -> Option<DocId> {
        let prefs = &self.features.periodic.prefs;
        let path = NotePath::new(prefs.path_for(p, date));
        let r = prefs.resolve(p);
        let vault = self.vault()?.clone();
        if vault.exists(&path) {
            return self.open_note(&path);
        }
        let start = p.start_of(date);
        let tpl = (!r.template.trim().is_empty()).then(|| NotePath::join(&vault.config.templates_folder, r.template.trim()));
        let id = match tpl.filter(|t| vault.exists(t)) {
            Some(t) => self.new_note_at_from_template(&path, &t, start),
            None => self.create_note_at(&path, &format!("# {}\n\n", path.stem()), None),
        };
        if id.is_some() {
            self.notify(NoticeLevel::Info, format!("Created {}", path.as_str()));
        }
        id
    }

    /// Open the next (`+1`) or previous (`-1`) note of the active periodic note's kind.
    pub fn step_period(&mut self, n: i32) -> bool {
        let Some(path) = self.active_doc().and_then(|d| d.path.clone()) else { return false };
        let Some((p, date)) = self.features.periodic.prefs.identify(path.as_str()) else { return false };
        let target = self.period_path(p, p.step(date, n));
        let existed = self.vault().is_some_and(|v| v.exists(&target));
        let ok = self.open_period(p, p.step(date, n)).is_some();
        if ok && existed {
            // Say where we are: the note changed under the caret.
            self.notify(NoticeLevel::Info, target.stem().to_string());
        }
        ok
    }

    /// Marks for every day of a month (index 0 is the 1st). Cached until the index changes.
    pub fn calendar_month(&self, year: i32, month: u32) -> Rc<Vec<DayMark>> {
        let version = self.index.version();
        if let Some((v, m)) = self.features.periodic.month_cache.borrow().get(&(year, month))
            && *v == version
        {
            return Rc::clone(m);
        }
        let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else { return Rc::default() };
        let days = Period::Monthly.step(first, 1).signed_duration_since(first).num_days().max(0) as usize;
        let mut marks = vec![DayMark::default(); days];
        let prefs = &self.features.periodic.prefs;
        let ix = self.index.read();
        for (i, m) in marks.iter_mut().enumerate() {
            let d = first + chrono::Duration::days(i as i64);
            if let Some(r) = ix.get(&NotePath::new(prefs.path_for(Period::Daily, d))) {
                m.note = true;
                m.words = r.stats.words;
            }
        }
        let prefix = format!("{year:04}-{month:02}-");
        for r in ix.records() {
            for t in r.tasks.iter().filter(|t| !t.done) {
                if let Some(day) =
                    t.due.as_deref().and_then(|d| d.strip_prefix(&prefix)).and_then(|d| d.get(..2)).and_then(|d| d.parse::<usize>().ok())
                    && let Some(m) = marks.get_mut(day.saturating_sub(1))
                {
                    m.due += 1;
                }
            }
        }
        drop(ix);
        let marks = Rc::new(marks);
        let mut cache = self.features.periodic.month_cache.borrow_mut();
        if cache.len() > 24 {
            cache.clear();
        }
        cache.insert((year, month), (version, Rc::clone(&marks)));
        marks
    }

    /// The note for `p` holding today: its path.
    pub fn period_path(&self, p: Period, date: NaiveDate) -> NotePath {
        NotePath::new(self.features.periodic.prefs.path_for(p, date))
    }
}

/// Year and month `n` months after `(y, m)`.
pub fn add_months(y: i32, m: u32, n: i32) -> (i32, u32) {
    let k = y * 12 + m as i32 - 1 + n;
    (k.div_euclid(12), k.rem_euclid(12) as u32 + 1)
}

/// Monday-first weekday of the 1st of a month, 0–6.
pub fn first_weekday(y: i32, m: u32) -> u32 {
    NaiveDate::from_ymd_opt(y, m, 1).map_or(0, |d| d.weekday().num_days_from_monday())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(&[
            (".quire/shared/vault.toml", "# mine\nformat = 1\n[periodic.weekly]\nfolder = \"Weeks\"\ntemplate = \"Week.md\"\n"),
            ("Templates/Week.md", "# Week of {{date:%d %b}}\n"),
            ("Daily/2026-11-02.md", "# Mon\none two three\n- [ ] x 📅 2026-11-05\n"),
        ]));
        load(&mut w);
        w.index().flush();
        w
    }

    #[test]
    fn opens_periods_and_steps() {
        let mut w = ws();
        let d = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let id = w.open_period(Period::Weekly, d).unwrap();
        assert_eq!(w.doc(id).unwrap().path.as_ref().unwrap().as_str(), "Weeks/2026-W45.md");
        assert_eq!(&*w.doc(id).unwrap().text(), "# Week of 02 Nov\n", "the template's date is the week's first day");
        assert!(w.step_period(1));
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Weeks/2026-W46.md");
        assert!(w.step_period(-2));
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Weeks/2026-W44.md");
        w.open_note(&"Daily/2026-11-02.md".into());
        assert!(w.step_period(1));
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Daily/2026-11-03.md");
        let m = w.calendar_month(2026, 11);
        assert_eq!(m.len(), 30);
        assert_eq!(m[1], DayMark { note: true, due: 0, words: m[1].words });
        assert!(m[1].words >= 4);
        assert_eq!(m[4].due, 1);
        assert_eq!(add_months(2026, 12, 1), (2027, 1));
        assert_eq!(add_months(2026, 1, -1), (2025, 12));
        assert_eq!(first_weekday(2026, 11), 6);
    }

    #[test]
    fn saving_keeps_comments_and_moves_daily_notes() {
        let mut w = ws();
        let mut p = w.periodic_prefs().clone();
        p.daily.folder = Some("Journal".into());
        w.set_periodic_prefs(p).unwrap();
        let text = w.vault().unwrap().read(&".quire/shared/vault.toml".into()).unwrap();
        assert!(text.starts_with("# mine\n") && text.contains("folder = \"Journal\"") && text.contains("folder = \"Weeks\""), "{text}");
        let id = w.open_daily().unwrap();
        assert!(w.doc(id).unwrap().path.as_ref().unwrap().as_str().starts_with("Journal/"));
    }
}
