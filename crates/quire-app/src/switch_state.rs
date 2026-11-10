// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 switching tools: the entry module.
//!
//! * **Tidy (the Markdown linter).** The options live in `vault.toml`
//!   `[lint.format]` (`docs/formats/lint-format.md`). "Tidy this note" is
//!   one undo step in the editor; "Tidy notes in a folder" plans off the UI
//!   thread ([`plan_folder`]), shows a diff, and writes each note through
//!   the safe write path with history, as one undoable batch
//!   ([`Workspace::lint_apply_step`]).
//! * **Home.** The sections come from `[home]` (`docs/formats/home.md`). The
//!   data ([`HomeData`]) is worked out only while Home is on screen, from
//!   queries the app already keeps (recent notes, the agenda, the inbox),
//!   and cached; the page opens at start only when the setting says so, and
//!   only after the first frame.
//! * **Import records.** "Undo the last import" ([`crate::import_run`]).
//!
//! Nothing here runs before the first frame, and nothing walks the vault on
//! the UI thread.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use chrono::{Local, NaiveDate};
pub use quire_markdown::linter;
use quire_markdown::linter::{Cx, Options, Outcome};
use quire_vault::{NotePath, Vault, VaultEvent, VaultPrefs};
use serde::{Deserialize, Serialize};

use crate::commands::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

// ------------------------------------------------------------------ settings

/// `vault.toml` `[lint]`: the format rules are in `[lint.format]`; the keys
/// the vault check owns (`exclude`, a level per check) pass through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct LintSection {
    format: Options,
    #[serde(flatten)]
    other: toml::Table,
}

/// The Home page's sections, in the order they appear unless `[home] sections` says otherwise.
pub const HOME_SECTIONS: &[(&str, &str)] = &[
    ("recent", "Recent notes"),
    ("pinned", "Pinned"),
    ("tasks", "Today"),
    ("view", "View"),
    ("inbox", "Inbox"),
    ("capture", "Quick capture"),
    ("daily", "Daily note"),
    ("random", "Random note"),
    ("stats", "Writing"),
];

/// `vault.toml` `[home]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct HomeConfig {
    /// Show Home when the app starts.
    pub open_on_start: bool,
    /// Section ids, in order (see [`HOME_SECTIONS`]); unknown ids are ignored.
    pub sections: Vec<String>,
    /// Rows in "Recent notes".
    pub recent: usize,
    /// Pinned notes, as vault paths.
    pub pinned: Vec<String>,
    /// For the "view" section: a `quire-view` spec (`from: #project`…), or the path of a `.base` file.
    pub view: String,
    /// Which view of the `.base` file (0 is the first).
    pub base_view: usize,
    /// Words a day to aim for; 0 for no goal.
    pub word_goal: usize,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            open_on_start: false,
            sections: HOME_SECTIONS.iter().map(|(id, _)| (*id).to_string()).filter(|id| id != "view").collect(),
            recent: 8,
            pinned: Vec::new(),
            view: String::new(),
            base_view: 0,
            word_goal: 0,
            other: toml::Table::new(),
        }
    }
}

impl HomeConfig {
    /// The sections to draw, known ids only, once each.
    pub fn active_sections(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for s in &self.sections {
            if let Some((id, _)) = HOME_SECTIONS.iter().find(|(id, _)| id == s)
                && !out.contains(id)
            {
                out.push(id);
            }
        }
        out
    }

    pub fn shows(&self, id: &str) -> bool {
        self.sections.iter().any(|s| s == id)
    }
}

/// What the app reads from `vault.toml` for these features, once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SwitchPrefs {
    pub lint: Options,
    pub home: HomeConfig,
}

fn read_prefs(vault: &Vault) -> SwitchPrefs {
    let p = VaultPrefs::load(vault);
    SwitchPrefs { lint: p.section::<LintSection>("lint").format, home: p.section::<HomeConfig>("home") }
}

// --------------------------------------------------------------------- state

type StatsCache = RefCell<Option<(((u64, u64), NaiveDate), HomeStats)>>;
type HomeCache = RefCell<Option<(HomeKey, Rc<HomeData>)>>;

/// This feature's state, in [`crate::features::Features`].
#[derive(Default)]
pub struct SwitchState {
    /// Frames since the vault opened; nothing is read before the second.
    frames: u8,
    start_checked: bool,
    prefs: RefCell<Option<Rc<SwitchPrefs>>>,
    stats: StatsCache,
    home: HomeCache,
    /// A folder tidy being written, one chunk per frame.
    pub apply: Option<LintApply>,
}

/// A note that tidying would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLint {
    pub path: NotePath,
    pub before: String,
    pub after: String,
    /// Rule ids that changed it.
    pub rules: Vec<&'static str>,
}

/// What a folder plan found.
#[derive(Debug, Clone, Default)]
pub struct LintPlan {
    pub files: Vec<FileLint>,
    /// Notes looked at.
    pub checked: usize,
    /// Left alone, with why (excluded, `lint: false`, unsaved edits).
    pub skipped: usize,
    /// Rules that were undone because they would have changed more than whitespace and markup.
    pub reverted: Vec<(NotePath, &'static str)>,
    /// Notes whose line endings or front matter the linter could not work with.
    pub notes: Vec<(NotePath, String)>,
    pub cancelled: bool,
}

/// A folder tidy in progress.
#[derive(Debug, Clone)]
pub struct LintApply {
    pub id: String,
    pub title: String,
    pub at_ms: i64,
    pub next: usize,
    pub done: Vec<(NotePath, String, String)>,
    pub failed: usize,
}

/// Today's date as `[lint.format] date-format` writes it.
fn stamp_now(format: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    // A format with a line break or another control character, or one that opens
    // with `:`, could write more than a value into the front matter: not used.
    if !Options::date_format_ok(format) || write!(s, "{}", Local::now().format(format)).is_err() || s.is_empty() {
        s = Local::now().format("%Y-%m-%dT%H:%M").to_string();
    }
    s
}

/// Tidy `text` for the note at `path` (excluded or opted-out notes come back unchanged).
pub fn lint_text(opts: &Options, path: &NotePath, text: &str, edited: bool) -> Outcome {
    let unchanged = || Outcome { text: text.to_string(), ..Outcome::default() };
    if skip_reason(opts, path, text).is_some() {
        return unchanged();
    }
    let stamp = if opts.date_modified { stamp_now(&opts.date_format) } else { String::new() };
    linter::lint(text, opts, &Cx { stamp: &stamp, edited })
}

/// Why the note isn't tidied, if it isn't.
fn skip_reason(opts: &Options, path: &NotePath, text: &str) -> Option<&'static str> {
    if opts.exclude.iter().any(|z| quire_agent::policy::path_in_zone(z, path.as_str())) {
        return Some("in an excluded folder");
    }
    let off = quire_markdown::frontmatter::parse(text).and_then(|fm| fm.get("lint").and_then(|v| v.as_str().map(str::to_ascii_lowercase)));
    off.filter(|v| matches!(v.as_str(), "false" | "no" | "off")).map(|_| "has `lint: false`")
}

/// Which notes under `folder` (all of them for `""`) tidying would change.
///
/// Runs on a thread of the caller's: it reads each note once and holds nothing.
/// `skip` are notes with unsaved edits open (their text isn't on disk yet).
pub fn plan_folder<S: std::hash::BuildHasher>(
    vault: &Vault,
    folder: &str,
    opts: &Options,
    skip: &HashSet<NotePath, S>,
    cancel: &AtomicBool,
) -> LintPlan {
    let mut plan = LintPlan::default();
    let prefix = if folder.is_empty() { String::new() } else { format!("{}/", folder.trim_end_matches('/')) };
    let mut all = vault.notes().unwrap_or_default();
    all.sort();
    for path in all {
        if cancel.load(Ordering::Relaxed) {
            plan.cancelled = true;
            break;
        }
        if !path.as_str().starts_with(&prefix) || vault.is_template(&path) {
            continue;
        }
        if skip.contains(&path) {
            plan.skipped += 1;
            continue;
        }
        let Ok(before) = vault.read(&path) else { continue };
        plan.checked += 1;
        if !is_utf8_note(vault, &path) {
            plan.skipped += 1;
            plan.notes.push((path, "The file isn't UTF-8 text; it was left alone".into()));
            continue;
        }
        if skip_reason(opts, &path, &before).is_some() {
            plan.skipped += 1;
            continue;
        }
        let out = lint_text(opts, &path, &before, false);
        plan.reverted.extend(out.reverted.iter().map(|r| (path.clone(), *r)));
        plan.notes.extend(out.notes.iter().map(|n| (path.clone(), n.clone())));
        if out.text != before {
            plan.files.push(FileLint { path, before, after: out.text, rules: out.fixes.iter().map(|f| f.rule).collect() });
        }
    }
    plan
}

/// Whether the note's bytes are UTF-8 (a BOM aside). Reading others replaces
/// what it can't decode, and writing that back would lose it. Notes in a
/// locked folder aren't tidied in batches, so only plain files are looked at.
fn is_utf8_note(vault: &Vault, path: &NotePath) -> bool {
    if vault.locked_folder(path).is_some() {
        return true;
    }
    vault.storage().read_bytes(path).is_ok_and(|b| std::str::from_utf8(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b)).is_ok())
}

/// "3 fixes (trailing spaces, blank lines)".
pub fn fixes_text(rules: &[&str]) -> String {
    let names: Vec<&str> = rules.iter().map(|id| linter::RULES.iter().find(|r| r.id == *id).map_or(*id, |r| r.title)).collect();
    format!(
        "{} ({})",
        if rules.len() == 1 { "1 fix".to_string() } else { format!("{} fixes", rules.len()) },
        names.join(", ").to_lowercase()
    )
}

/// A unified diff of the change, as `git diff` prints it (two lines of context).
pub fn unified_diff(path: &NotePath, before: &str, after: &str) -> String {
    let name = path.as_str();
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(2)
        .header(&format!("a/{name}"), &format!("b/{name}"))
        .to_string()
}

/// One line of a diff as the preview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// `' '` for context, `'-'` removed, `'+'` added, `'~'` for a gap between changes.
    pub tag: char,
    pub text: String,
}

/// The changed lines with two lines of context, and a gap row between changes far apart.
/// At most `max` rows (the preview shows a note's start of change, not all of it).
pub fn diff_rows(before: &str, after: &str, max: usize) -> Vec<DiffRow> {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(before, after);
    let mut rows = Vec::new();
    for (i, group) in diff.grouped_ops(2).iter().enumerate() {
        if i > 0 {
            rows.push(DiffRow { tag: '~', text: String::new() });
        }
        for op in group {
            for change in diff.iter_changes(op) {
                if rows.len() >= max {
                    return rows;
                }
                let tag = match change.tag() {
                    ChangeTag::Equal => ' ',
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                };
                rows.push(DiffRow { tag, text: change.value().trim_end_matches(['\n', '\r']).to_string() });
            }
        }
    }
    rows
}

/// `[lint.format]` for the vault at hand (the CLI has no workspace).
pub fn lint_options(vault: &Vault) -> Options {
    read_prefs(vault).lint
}

/// Write a tidied note that isn't open anywhere.
///
/// History before and after
/// (the new version with provenance), and only over the text the change was
/// made from. `Ok(false)` when the note changed meanwhile and nothing was written.
pub fn write_tidied(vault: &Vault, path: &NotePath, before: &str, after: &str, device: &str, now_ms: i64) -> Result<bool, String> {
    if !is_utf8_note(vault, path) {
        return Err(format!("{} isn't UTF-8 text; it was left alone", path.stem()));
    }
    let prov = quire_agent::Provenance::new(quire_agent::Actor::human(device), quire_agent::Op::Write, after, now_ms).to_json();
    let _ = vault.snapshot(path, before, now_ms - 1, true);
    if !vault.write_if(path, Some(before), after).map_err(|e| format!("couldn't write {}: {e}", path.stem()))? {
        return Ok(false);
    }
    let _ = vault.snapshot_with(path, after, now_ms, true, Some(&prov));
    Ok(true)
}

// ------------------------------------------------------------------ Home data

/// A note in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeNote {
    pub path: NotePath,
    pub title: String,
}

/// A task for today's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeTask {
    pub path: NotePath,
    pub text: String,
    /// `overdue since 2026-11-01`, `today`, `14:00–15:30`.
    pub when: String,
}

/// Counts for "Writing".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeStats {
    pub notes: usize,
    pub words: usize,
    pub open_tasks: usize,
    /// Words now minus words when Home was first drawn today on this device (can be negative).
    pub words_today: i64,
}

/// What Home shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeData {
    pub recent: Vec<HomeNote>,
    pub pinned: Vec<HomeNote>,
    /// Pinned paths that no longer exist.
    pub missing_pins: Vec<String>,
    pub overdue: Vec<HomeTask>,
    pub today: Vec<HomeTask>,
    /// Suggested changes waiting for a decision, by title.
    pub suggestions: Vec<String>,
    /// Checks and other inbox items waiting.
    pub other_waiting: usize,
    pub daily: Option<NotePath>,
    pub daily_exists: bool,
    pub stats: HomeStats,
}

/// What a cached [`HomeData`] was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HomeKey {
    version: (u64, u64),
    today: NaiveDate,
    prefs: usize,
    recent: Vec<NotePath>,
    inbox: (usize, usize, i64),
    active: Option<NotePath>,
}

/// Caps so a huge agenda can't make Home heavy.
const MAX_TASK_ROWS: usize = 200;
const MAX_RECENT: usize = 50;

const BASELINE_FILE: &str = "home-stats.json";

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    day: String,
    words: usize,
}

impl Workspace {
    // ------------------------------------------------------------- settings

    /// The vault's `[lint.format]` and `[home]`, read once per vault.
    pub fn switch_prefs(&self) -> Rc<SwitchPrefs> {
        if let Some(p) = self.features.switch.prefs.borrow().as_ref() {
            return Rc::clone(p);
        }
        let p = Rc::new(self.vault.as_ref().map(read_prefs).unwrap_or_default());
        *self.features.switch.prefs.borrow_mut() = Some(Rc::clone(&p));
        p
    }

    fn save_switch_prefs(&mut self, next: &SwitchPrefs) -> bool {
        let Some(vault) = self.vault.clone() else { return false };
        let mut file = VaultPrefs::load(&vault);
        let mut lint: LintSection = file.section("lint");
        lint.format = next.lint.clone();
        let saved = file.set_section("lint", &lint).and_then(|()| file.set_section("home", &next.home)).and_then(|()| file.save(&vault));
        match saved {
            Ok(()) => {
                *self.features.switch.prefs.borrow_mut() = Some(Rc::new(next.clone()));
                self.features.switch.home.borrow_mut().take();
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save vault.toml: {e}"));
                false
            }
        }
    }

    pub fn set_lint_options(&mut self, lint: Options) -> bool {
        let mut p = (*self.switch_prefs()).clone();
        p.lint = lint;
        self.save_switch_prefs(&p)
    }

    pub fn set_home_config(&mut self, home: HomeConfig) -> bool {
        let mut p = (*self.switch_prefs()).clone();
        p.home = home;
        self.save_switch_prefs(&p)
    }

    // ----------------------------------------------------------------- tidy

    /// Notes the linter never touches, however it is asked: templates (they are
    /// patterns, not notes) and notes in locked folders.
    fn lint_blocked(&self, path: &NotePath) -> Option<&'static str> {
        let vault = self.vault.as_ref()?;
        if vault.is_template(path) {
            Some("a template")
        } else if vault.locked_folder(path).is_some() {
            Some("in a locked folder")
        } else {
            None
        }
    }

    /// Tidy the active note: one undo step.
    pub fn lint_active_note(&mut self) {
        let opts = self.switch_prefs().lint.clone();
        let Some(doc) = self.active_doc() else { return };
        let Some(path) = doc.path.clone().filter(NotePath::is_markdown) else {
            self.notify(NoticeLevel::Info, "Only Markdown notes can be tidied");
            return;
        };
        let (text, dirty) = (doc.text(), doc.is_dirty());
        if let Some(why) = self.lint_blocked(&path) {
            self.notify(NoticeLevel::Info, format!("“{}” isn't tidied: it is {why}", path.stem()));
            return;
        }
        if let Some(why) = skip_reason(&opts, &path, &text) {
            self.notify(NoticeLevel::Info, format!("“{}” isn't tidied: it is {why}", path.stem()));
            return;
        }
        let out = lint_text(&opts, &path, &text, dirty);
        for (_, msg) in out.notes.iter().map(|n| (&path, n)) {
            self.notify(NoticeLevel::Info, msg.clone());
        }
        if out.text == *text {
            let msg = if out.reverted.is_empty() { "Nothing to tidy".to_string() } else { "Nothing could be tidied safely".to_string() };
            self.notify(NoticeLevel::Info, msg);
            return;
        }
        if let Some(d) = self.active_doc_mut() {
            d.replace_minimally(&out.text);
        }
        let rules: Vec<&str> = out.fixes.iter().map(|f| f.rule).collect();
        self.notify(NoticeLevel::Info, format!("Tidied: {}. Undo with Ctrl+Z", fixes_text(&rules)));
    }

    /// Before an explicit Save: tidy the note when `[lint.format] on-save` is on.
    pub(crate) fn lint_before_save(&mut self, id: crate::document::DocId) {
        let Some(doc) = self.doc(id) else { return };
        if !doc.is_dirty() || !doc.path.as_ref().is_some_and(NotePath::is_markdown) {
            return;
        }
        let opts = self.switch_prefs().lint.clone();
        if !opts.on_save {
            return;
        }
        let (Some(path), text) = (doc.path.clone(), doc.text()) else { return };
        if self.lint_blocked(&path).is_some() {
            return;
        }
        let out = lint_text(&opts, &path, &text, true);
        if out.text != *text
            && let Some(i) = self.doc_pos(id)
        {
            self.docs[i].replace_minimally(&out.text);
        }
    }

    /// Notes with unsaved edits (a folder tidy leaves them alone).
    pub fn unsaved_notes(&self) -> HashSet<NotePath> {
        self.docs.iter().filter(|d| d.is_dirty()).filter_map(|d| d.path.clone()).collect()
    }

    /// Start writing a plan: the title is what the batch is called in the activity list.
    pub fn lint_apply_begin(&mut self, folder: &str) {
        let now = crate::history_ops::now_ms();
        let title = if folder.is_empty() { "Tidy notes".to_string() } else { format!("Tidy notes in {folder}") };
        self.features.switch.apply = Some(LintApply {
            id: quire_agent::BatchId::new(now).as_str().to_string(),
            title,
            at_ms: now,
            next: 0,
            done: Vec::new(),
            failed: 0,
        });
    }

    /// Write up to `limit` more notes of `files`; true when all are written (the batch is then
    /// in the undo list).
    pub fn lint_apply_step(&mut self, files: &[FileLint], limit: usize) -> bool {
        let Some(mut st) = self.features.switch.apply.take() else { return true };
        let end = (st.next + limit).min(files.len());
        for f in &files[st.next..end] {
            // Only over what the plan was made from; a note changed since is left as it is.
            let Some(current) = self.note_text(&f.path) else { continue };
            if current != f.before {
                st.failed += 1;
                continue;
            }
            match self.write_note_with_history(&f.path, &f.before, &f.after, st.at_ms) {
                Ok(snaps) => {
                    self.record_batch_entry(&st.id, &st.title, &f.path, &f.before, &f.after, snaps, st.at_ms);
                    st.done.push((f.path.clone(), f.before.clone(), f.after.clone()));
                }
                Err(msg) => {
                    st.failed += 1;
                    self.notify(NoticeLevel::Error, msg);
                }
            }
        }
        st.next = end;
        if end < files.len() {
            self.features.switch.apply = Some(st);
            return false;
        }
        self.index.flush();
        let n = st.done.len();
        let extra = if st.failed > 0 { format!(" ({} changed meanwhile or couldn't be written)", st.failed) } else { String::new() };
        self.notify(NoticeLevel::Info, format!("{}: {n} note{} changed{extra}", st.title, if n == 1 { "" } else { "s" }));
        if n > 0 {
            self.features.meta.batches.push(crate::meta_refactor::Batch { id: st.id, title: st.title, at_ms: st.at_ms, files: st.done });
        }
        true
    }

    /// The id of the newest tidy batch of this session, for "Undo".
    pub fn last_lint_batch(&self) -> Option<(String, String)> {
        self.features.meta.batches.iter().rev().find(|b| b.title.starts_with("Tidy notes")).map(|b| (b.id.clone(), b.title.clone()))
    }

    // --------------------------------------------------------------- import

    /// Undo the newest import that wasn't undone (what it created and nobody edited goes to the trash).
    pub fn undo_last_import(&mut self) {
        let Some(vault) = self.vault.clone() else { return };
        let Some(rec) = crate::import_run::list(&vault).into_iter().find(|r| r.undone_ms.is_none()) else {
            self.notify(NoticeLevel::Info, "There is no import to undo");
            return;
        };
        self.undo_import(&rec.id);
    }

    /// Undo one import by id. Returns what was trashed and what stayed (edited since).
    pub fn undo_import(&mut self, id: &str) -> Option<crate::import_run::UndoPlan> {
        let vault = self.vault.clone()?;
        // Typing that autosave hasn't written yet must reach the file first: then
        // the note differs from what the import wrote, and stays.
        self.save_open_notes();
        match crate::import_run::undo(&vault, id, crate::history_ops::now_ms()) {
            Ok((rec, plan)) => {
                // Close tabs of notes that went away (never one with unsaved text); the index follows.
                for p in &plan.trash {
                    if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(p) && !d.is_dirty()) {
                        self.docs.remove(i);
                        self.active = if self.docs.is_empty() { None } else { Some(self.active.map_or(0, |a| a.min(self.docs.len() - 1))) };
                    }
                    self.index.remove(p.clone());
                }
                self.index.flush();
                let kept = if plan.edited.is_empty() { String::new() } else { format!("; {} edited since stay", plan.edited.len()) };
                let lost = match plan.missing.as_slice() {
                    [] => String::new(),
                    [one] => format!("; {one} isn't where the import put it (renamed, moved or deleted), so the import stays open"),
                    many => format!(
                        "; {} notes or files aren't where the import put them (renamed, moved or deleted), so the import stays open",
                        many.len()
                    ),
                };
                self.notify(NoticeLevel::Info, format!("Undid {}: {} moved to .trash{kept}{lost}", rec.title(), plan.trash.len()));
                Some(plan)
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't undo the import: {e}"));
                None
            }
        }
    }

    // ----------------------------------------------------------------- home

    pub fn is_pinned(&self, path: &NotePath) -> bool {
        self.switch_prefs().home.pinned.iter().any(|p| p == path.as_str())
    }

    /// Pin or unpin a note on Home.
    pub fn set_pinned(&mut self, path: &NotePath, pin: bool) {
        let mut home = self.switch_prefs().home.clone();
        home.pinned.retain(|p| p != path.as_str());
        if pin {
            home.pinned.push(path.as_str().to_string());
        }
        if self.set_home_config(home) {
            self.notify(NoticeLevel::Info, format!("{} “{}” on Home", if pin { "Pinned" } else { "Unpinned" }, path.stem()));
        }
    }

    /// A note picked at random (not the open one, not a template).
    pub fn random_note(&self) -> Option<NotePath> {
        let active = self.active_doc().and_then(|d| d.path.clone());
        let vault = self.vault.as_ref()?;
        let pool: Vec<NotePath> =
            self.index.read().paths().into_iter().filter(|p| Some(p) != active.as_ref() && !vault.is_template(p)).collect();
        if pool.is_empty() {
            return None;
        }
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let seed = t.as_nanos() ^ (t.as_millis() << 17);
        pool.get(usize::try_from(seed % pool.len() as u128).unwrap_or(0)).cloned()
    }

    fn home_titles(&self, paths: impl Iterator<Item = NotePath>) -> Vec<HomeNote> {
        let ix = self.index.read();
        paths.filter_map(|p| Some(HomeNote { title: ix.get(&p)?.title.clone(), path: p })).collect()
    }

    fn baseline_path(&self) -> Option<std::path::PathBuf> {
        if cfg!(test) {
            return None;
        }
        Some(quire_agent::approval::default_state_dir(self.vault_root()?)?.join(BASELINE_FILE))
    }

    /// Notes, words and open tasks; cached per notes version and day.
    fn home_stats(&self, today: NaiveDate) -> HomeStats {
        let key = (self.notes_version(), today);
        if let Some((k, s)) = self.features.switch.stats.borrow().as_ref()
            && *k == key
        {
            return s.clone();
        }
        let (mut notes, mut words, mut open_tasks, mut plain_words) = (0, 0, 0, 0);
        self.with_notes(|n| {
            for r in n.records() {
                notes += 1;
                words += r.stats.words;
                open_tasks += r.tasks.iter().filter(|t| !t.done).count();
            }
            // The baseline is written to disk and compared across sessions, so it counts only
            // the notes of the vault index: a locked folder's words never reach the file, and
            // unlocking or locking a folder doesn't move "today".
            plain_words = n.main().records().map(|r| r.stats.words).sum();
        });
        // This device's count from the first time Home was drawn today.
        let day = today.format("%Y-%m-%d").to_string();
        let file = self.baseline_path();
        let known = file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Baseline>(&t).ok())
            .filter(|b| b.day == day);
        let base = known.map_or_else(
            || {
                if let Some(p) = &file {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let json = serde_json::to_string(&Baseline { day: day.clone(), words: plain_words }).unwrap_or_default();
                    let _ = crate::atomic::write(p, json.as_bytes(), false);
                }
                plain_words
            },
            |b| b.words,
        );
        let stats =
            HomeStats { notes, words, open_tasks, words_today: i64::try_from(plain_words).unwrap_or(0) - i64::try_from(base).unwrap_or(0) };
        *self.features.switch.stats.borrow_mut() = Some((key, stats.clone()));
        stats
    }

    /// What Home shows now. Cheap when nothing changed (it is called every frame while Home is open).
    pub fn home_data(&self) -> Rc<HomeData> {
        let prefs = self.switch_prefs();
        let cfg = &prefs.home;
        let today = Local::now().date_naive();
        // Never wait for the index worker on a frame: while it applies a batch, keep what Home showed.
        let Some(recent) = self.recent_with_active(cfg.recent.clamp(1, MAX_RECENT)) else {
            return self.features.switch.home.borrow().as_ref().map(|(_, d)| Rc::clone(d)).unwrap_or_default();
        };
        let agent = &self.features.agent.inbox;
        let pending: Vec<&quire_agent::Proposal> =
            agent.pending.iter().filter(|p| !agent.snoozed(&crate::agent_state::inbox::proposal_fingerprint(p))).collect();
        let key = HomeKey {
            version: self.notes_version(),
            today,
            prefs: Rc::as_ptr(&prefs) as usize,
            recent,
            inbox: (pending.len(), agent.findings.len(), agent.loaded_at_ms),
            active: self.active_doc().and_then(|d| d.path.clone()),
        };
        if let Some((k, d)) = self.features.switch.home.borrow().as_ref()
            && *k == key
        {
            return Rc::clone(d);
        }
        let vault = self.vault.as_ref();
        let mut data = HomeData { recent: self.home_titles(key.recent.iter().cloned()), ..HomeData::default() };
        if cfg.shows("pinned") {
            let (mut found, mut missing) = (Vec::new(), Vec::new());
            for p in &cfg.pinned {
                let path = NotePath::new(p);
                if self.index.read().get(&path).is_some() { found.push(path) } else { missing.push(p.clone()) }
            }
            data.pinned = self.home_titles(found.into_iter());
            data.missing_pins = missing;
        }
        if cfg.shows("tasks") {
            let agenda = self.agenda();
            let row = |i: &quire_plan::agenda::AgendaItem, when: String| HomeTask {
                path: NotePath::new(&i.path),
                text: i.task.text.clone(),
                when,
            };
            data.overdue =
                agenda.overdue.iter().take(MAX_TASK_ROWS).map(|i| row(i, format!("overdue since {}", i.date.format("%Y-%m-%d")))).collect();
            data.today = agenda.today.iter().take(MAX_TASK_ROWS).map(|i| row(i, "today".into())).collect();
            for b in agenda.blocks.iter().take(MAX_TASK_ROWS) {
                data.today.push(HomeTask {
                    path: NotePath::new(&b.path),
                    text: b.task.text.clone(),
                    when: format!("{}–{}", b.start.format("%H:%M"), b.end.format("%H:%M")),
                });
            }
        }
        if cfg.shows("inbox") {
            data.suggestions = pending.iter().map(|p| p.summary().title).take(5).collect();
            data.other_waiting = agent.findings.iter().filter(|f| !agent.snoozed(&f.fingerprint)).count();
        }
        if cfg.shows("daily")
            && let Some(v) = vault
        {
            let p = v.daily_path(today);
            data.daily_exists = v.exists(&p);
            data.daily = Some(p);
        }
        if cfg.shows("stats") {
            data.stats = self.home_stats(today);
        }
        let data = Rc::new(data);
        *self.features.switch.home.borrow_mut() = Some((key, Rc::clone(&data)));
        data
    }
}

// ---------------------------------------------------------- feature plumbing

/// After the first frame: open Home if `[home] open-on-start` says so.
pub(crate) fn tick(ws: &mut Workspace, _now: Instant) -> bool {
    if ws.features.switch.start_checked || ws.vault.is_none() {
        return false;
    }
    ws.features.switch.frames = ws.features.switch.frames.saturating_add(1);
    if ws.features.switch.frames < 3 {
        return true;
    }
    ws.features.switch.start_checked = true;
    if ws.switch_prefs().home.open_on_start {
        ws.request_ui(UiRequest::Feature(Command::OpenHome));
        return true;
    }
    false
}

pub(crate) const fn next_deadline(_ws: &Workspace) -> Option<Instant> {
    None
}

pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::LintNote => ws.lint_active_note(),
        Command::PinToHome => {
            let path = ws.active_doc().and_then(|d| d.path.clone())?;
            let pinned = ws.is_pinned(&path);
            ws.set_pinned(&path, !pinned);
        }
        Command::UndoImport => ws.undo_last_import(),
        _ => return Some(UiRequest::Feature(cmd)),
    }
    None
}

pub(crate) fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        Command::LintNote | Command::PinToHome => ws.active_doc().is_some_and(|d| d.path.is_some()),
        _ => ws.vault().is_some(),
    }
}

pub(crate) const fn decorations(_ws: &Workspace, _doc: &Document, _out: &mut Vec<Decoration>) {}

pub(crate) const fn decoration_event(_ws: &mut Workspace, _ev: &DecorationEvent) {}

pub(crate) const fn vault_event(_ws: &mut Workspace, _ev: &VaultEvent) {}

pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.switch = SwitchState::default();
}

pub(crate) const fn status(_ws: &Workspace, _out: &mut Vec<StatusItem>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(files));
        ws.index().flush();
        ws
    }

    #[test]
    fn diffs_show_the_change_with_context() {
        let rows = diff_rows("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nb\nC\nd\ne\nf\ng\nH\n", 100);
        let tags: String = rows.iter().map(|r| r.tag).collect();
        assert_eq!(tags, "  -+    -+", "{rows:?}");
        let u = unified_diff(&"N.md".into(), "x   \n", "x\n");
        assert!(u.contains("--- a/N.md") && u.contains("-x   ") && u.contains("+x"), "{u}");
    }

    #[test]
    fn tidying_a_note_is_one_undo_step() {
        let mut w = ws(&[("A.md", "# A\ntext   \n* a\n")]);
        w.open_note(&"A.md".into());
        w.lint_active_note();
        assert_eq!(&*w.active_doc().expect("open").text(), "# A\n\ntext\n\n- a\n");
        w.run(Command::Undo);
        assert_eq!(&*w.active_doc().expect("open").text(), "# A\ntext   \n* a\n", "one undo restores it");
    }

    #[test]
    fn templates_are_never_tidied_by_hand_or_on_save() {
        let mut w = ws(&[("Templates/T.md", "# T\ntext   \n"), ("A.md", "a\n")]);
        let t = NotePath::new("Templates/T.md");
        w.open_note(&t);
        w.lint_active_note();
        assert_eq!(&*w.active_doc().expect("open").text(), "# T\ntext   \n");
        let mut p = (*w.switch_prefs()).clone();
        p.lint.on_save = true;
        assert!(w.save_switch_prefs(&p));
        w.active_doc_mut().expect("open").replace_minimally("# T\ntext   \nmore   \n");
        let id = w.active_doc().expect("open").id;
        assert!(w.save(id));
        assert_eq!(w.vault().expect("vault").read(&t).expect("read"), "# T\ntext   \nmore   \n");
    }

    #[test]
    fn a_note_that_is_not_utf8_is_not_rewritten() {
        let w = ws(&[("A.md", "a   \n")]);
        let vault = w.vault().expect("vault").clone();
        let bad = NotePath::new("Latin.md");
        let bytes = b"na\xEFve   \n";
        vault.storage().write_bytes_atomic(&bad, bytes).expect("write");
        let plan = plan_folder(&vault, "", &Options::default(), &HashSet::new(), &AtomicBool::new(false));
        assert!(plan.files.iter().all(|f| f.path != bad), "{plan:?}");
        assert!(plan.notes.iter().any(|(p, _)| *p == bad));
        // Nor through a write with a plan made some other way.
        let before = vault.read(&bad).expect("read");
        assert!(write_tidied(&vault, &bad, &before, "na\u{FFFD}ve\n", "dev", 1).is_err());
        assert_eq!(vault.storage().read_bytes(&bad).expect("bytes"), bytes);
        // A plain UTF-8 note, with a BOM, still goes through.
        assert!(plan.files.iter().any(|f| f.path == NotePath::new("A.md")));
    }

    #[test]
    fn tidying_leaves_alone_notes_that_opt_out_or_are_excluded() {
        let o = Options { exclude: vec!["Private".into()], ..Options::default() };
        let p = NotePath::new("Private/A.md");
        assert_eq!(lint_text(&o, &p, "a   \n", false).text, "a   \n");
        let q = NotePath::new("B.md");
        assert_eq!(lint_text(&o, &q, "---\nlint: false\n---\na   \n", false).text, "---\nlint: false\n---\na   \n");
        assert_eq!(lint_text(&o, &q, "a   \n", false).text, "a\n");
    }

    #[test]
    fn options_are_kept_beside_the_vault_checks_keys() {
        let mut w = ws(&[]);
        let vault = w.vault().expect("vault").clone();
        std::fs::create_dir_all(std::env::temp_dir()).expect("tmp");
        let mut file = VaultPrefs::load(&vault);
        let mut raw: LintSection = file.section("lint");
        raw.other.insert("exclude".into(), toml::Value::Array(vec![toml::Value::String("Archive".into())]));
        file.set_section("lint", &raw).expect("set");
        file.save(&vault).expect("save");
        let o = Options { list_marker: linter::ListMarker::Star, ..Options::default() };
        assert!(w.set_lint_options(o.clone()));
        let again = VaultPrefs::load(&vault);
        let lint: LintSection = again.section("lint");
        assert_eq!(lint.format.list_marker, linter::ListMarker::Star);
        assert!(lint.other.contains_key("exclude"), "the vault check's keys stay");
        assert_eq!(read_prefs(&vault).lint, o);
    }

    #[test]
    fn a_folder_tidy_is_planned_applied_and_undone() {
        let mut w = ws(&[("N/A.md", "# A\ntext   \n"), ("N/B.md", "# B\n\nclean\n"), ("Other/C.md", "c   \n")]);
        let vault = w.vault().expect("vault").clone();
        let plan = plan_folder(&vault, "N", &Options::default(), &HashSet::new(), &AtomicBool::new(false));
        assert_eq!((plan.checked, plan.files.len()), (2, 1));
        assert_eq!(plan.files[0].after, "# A\n\ntext\n");
        w.lint_apply_begin("N");
        assert!(w.lint_apply_step(&plan.files, 10));
        assert_eq!(vault.read(&"N/A.md".into()).expect("read"), "# A\n\ntext\n");
        assert_eq!(vault.read(&"Other/C.md".into()).expect("read"), "c   \n", "other folders are untouched");
        let (id, _) = w.last_lint_batch().expect("a batch");
        assert_eq!(w.meta_undo(&id).restored, 1);
        assert_eq!(vault.read(&"N/A.md".into()).expect("read"), "# A\ntext   \n");
    }

    #[test]
    fn a_note_changed_since_the_plan_is_left_alone() {
        let mut w = ws(&[("A.md", "a   \n")]);
        let vault = w.vault().expect("vault").clone();
        let plan = plan_folder(&vault, "", &Options::default(), &HashSet::new(), &AtomicBool::new(false));
        vault.write(&"A.md".into(), "a   \nnew line\n").expect("write");
        w.lint_apply_begin("");
        assert!(w.lint_apply_step(&plan.files, 10));
        assert_eq!(vault.read(&"A.md".into()).expect("read"), "a   \nnew line\n");
    }

    #[test]
    fn home_lists_what_the_app_already_knows() {
        let mut w = ws(&[("A.md", "# A\n- [ ] ship it 📅 2000-01-01\nwords here\n"), ("B.md", "# B\n")]);
        w.open_note(&"A.md".into());
        w.track_recent();
        w.open_note(&"B.md".into());
        w.track_recent();
        w.index().flush();
        let home = HomeConfig { pinned: vec!["A.md".into(), "Gone.md".into()], ..HomeConfig::default() };
        assert!(w.set_home_config(home));
        let d = w.home_data();
        assert_eq!(d.recent.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["B", "A"], "newest first, the open note included");
        assert_eq!(d.pinned.len(), 1);
        assert_eq!(d.missing_pins, ["Gone.md"]);
        assert_eq!(d.overdue.len(), 1);
        assert_eq!((d.stats.notes, d.stats.open_tasks), (2, 1));
        assert!(Rc::ptr_eq(&d, &w.home_data()), "cached until something changes");
        assert!(w.random_note().is_some());
    }

    #[test]
    fn pinning_toggles_and_sections_are_checked() {
        let mut w = ws(&[("A.md", "# A\n")]);
        let p = NotePath::new("A.md");
        w.set_pinned(&p, true);
        assert!(w.is_pinned(&p));
        w.set_pinned(&p, false);
        assert!(!w.is_pinned(&p));
        let cfg = HomeConfig { sections: vec!["stats".into(), "nope".into(), "stats".into(), "recent".into()], ..HomeConfig::default() };
        assert_eq!(cfg.active_sections(), ["stats", "recent"]);
    }

    #[test]
    fn undoing_an_import_trashes_only_what_it_made() {
        let mut w = ws(&[("Keep.md", "# keep\n")]);
        let vault = w.vault().expect("vault").clone();
        let plan = quire_import::ImportPlan { notes: vec![quire_import::PlannedNote::new("N.md", "# N\n")], ..Default::default() };
        let (_, rec) = crate::import_run::run(
            &vault,
            &plan,
            "Imported",
            std::path::Path::new("x"),
            "dev",
            1_000,
            Some(&AtomicBool::new(false)),
            &mut |_| {},
        )
        .expect("run");
        w.reload_index();
        let undone = w.undo_import(&rec.id).expect("undone");
        assert_eq!(undone.trash.len(), 1);
        assert!(!vault.exists(&"Imported/N.md".into()));
        assert!(vault.exists(&"Keep.md".into()));
    }

    #[test]
    fn undo_keeps_a_note_with_unsaved_typing_and_its_tab() {
        let mut w = ws(&[]);
        let vault = w.vault().expect("vault").clone();
        let plan = quire_import::ImportPlan {
            notes: vec![quire_import::PlannedNote::new("N.md", "# N\n"), quire_import::PlannedNote::new("M.md", "# M\n")],
            ..Default::default()
        };
        let (_, rec) =
            crate::import_run::run(&vault, &plan, "Imported", std::path::Path::new("x"), "dev", 1_000, None, &mut |_| {}).expect("run");
        w.reload_index();
        w.open_note(&"Imported/M.md".into()).expect("opens");
        w.open_note(&"Imported/N.md".into()).expect("opens");
        w.insert_text("typed, not saved yet ");
        assert!(w.docs.iter().any(Document::is_dirty));
        let undone = w.undo_import(&rec.id).expect("undone");
        assert_eq!((undone.trash.len(), undone.edited.len()), (1, 1), "{undone:?}");
        assert!(vault.read(&"Imported/N.md".into()).unwrap().contains("typed, not saved yet"), "the typing reached the file");
        assert!(w.docs.iter().any(|d| d.path.as_ref().is_some_and(|p| p.as_str() == "Imported/N.md")), "its tab stays");
        assert!(!w.docs.iter().any(|d| d.path.as_ref().is_some_and(|p| p.as_str() == "Imported/M.md")), "the trashed note's tab closes");
    }
}
