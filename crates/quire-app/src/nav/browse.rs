// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browsing the vault: sort order per folder (with a manual order you drag
//! into shape), links to folders, and display titles (design §F.3, §E.1).
//!
//! Sort modes and manual order live in the vault, so they travel with it:
//! `[sort] "Projects/" = "manual"` and `[order."Projects/"] = ["Plan.md", …]`
//! in `.quire/shared/vault.toml` (`"/"` is the vault's top folder).

use std::collections::{BTreeMap, HashMap};

use quire_vault::{NotePath, VaultPrefs};

use crate::workspace::{NoticeLevel, Workspace};

/// How a folder's notes are ordered in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Name,
    /// Most recently changed first.
    Modified,
    /// Newest first.
    Created,
    /// The order you dragged them into.
    Manual,
}

impl SortMode {
    pub const ALL: [Self; 4] = [Self::Name, Self::Modified, Self::Created, Self::Manual];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Created => "created",
            Self::Manual => "manual",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Last changed",
            Self::Created => "Newest",
            Self::Manual => "Manual (drag to arrange)",
        }
    }

    pub fn from_key(s: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.key().eq_ignore_ascii_case(s.trim())).unwrap_or_default()
    }
}

/// The vault.toml key for a folder: `"Projects/"`, or `"/"` for the top.
fn folder_key(folder: &str) -> String {
    let f = folder.trim_matches('/');
    if f.is_empty() { "/".into() } else { format!("{f}/") }
}

/// Modified and created times (Unix ms) of notes, for one index version.
#[derive(Debug, Default)]
pub(crate) struct TimesCache {
    version: u64,
    map: HashMap<NotePath, (i64, i64)>,
}

pub(super) fn read_prefs(ws: &mut Workspace, prefs: &VaultPrefs) {
    let order: BTreeMap<String, Vec<String>> = prefs.section("order");
    let sort: BTreeMap<String, String> = prefs.section("sort");
    let st = &mut ws.features.nav;
    let order = order.into_iter().map(|(k, v)| (folder_key(&k), v)).collect();
    let sort = sort.into_iter().map(|(k, v)| (folder_key(&k), v)).collect();
    if st.order != order || st.sort != sort {
        st.order = order;
        st.sort = sort;
        st.sort_generation += 1;
    }
}

pub fn sort_mode(ws: &Workspace, folder: &str) -> SortMode {
    ws.features.nav.sort.get(&folder_key(folder)).map(|s| SortMode::from_key(s)).unwrap_or_default()
}

/// Choose how a folder is sorted. Choosing manual keeps the order shown now.
pub fn set_sort_mode(ws: &mut Workspace, folder: &str, mode: SortMode, shown: &[NotePath]) -> Result<(), String> {
    let key = folder_key(folder);
    let order: Vec<String> = shown.iter().map(|p| p.file_name().to_string()).collect();
    let has_order = ws.features.nav.order.contains_key(&key);
    super::edit_prefs(ws, |p| {
        let mut sort: BTreeMap<String, String> = p.section("sort");
        if mode == SortMode::Name {
            sort.remove(&key);
        } else {
            sort.insert(key.clone(), mode.key().to_string());
        }
        p.set_section("sort", &sort)?;
        if mode == SortMode::Manual && !has_order {
            let mut o: BTreeMap<String, Vec<String>> = p.section("order");
            o.insert(key.clone(), order);
            p.set_section("order", &o)?;
        }
        Ok(())
    })
}

/// Modified and created times (Unix ms) of `notes`, cached per index version.
pub fn note_times(ws: &mut Workspace, notes: &[NotePath]) -> HashMap<NotePath, (i64, i64)> {
    let version = ws.index().version();
    let Some(vault) = ws.vault().cloned() else { return HashMap::new() };
    let cache = &mut ws.features.nav.times;
    if cache.version != version {
        cache.version = version;
        cache.map.clear();
    }
    let ms = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    for p in notes {
        cache.map.entry(p.clone()).or_insert_with(|| {
            let modified = vault.storage().modified(p).map_or(0, ms);
            let created =
                vault.storage().absolute(p).and_then(|a| std::fs::metadata(a).ok()).and_then(|m| m.created().ok()).map_or(modified, ms);
            (modified, created)
        });
    }
    notes.iter().filter_map(|p| cache.map.get(p).map(|t| (p.clone(), *t))).collect()
}

/// Sort a folder's notes as the folder asks.
pub fn sorted(ws: &mut Workspace, folder: &str, notes: &mut [NotePath]) {
    let by_name = |a: &NotePath, b: &NotePath| a.stem().to_lowercase().cmp(&b.stem().to_lowercase()).then_with(|| a.cmp(b));
    match sort_mode(ws, folder) {
        SortMode::Name => notes.sort_by(by_name),
        mode @ (SortMode::Modified | SortMode::Created) => {
            let t = note_times(ws, notes);
            let pick = |p: &NotePath| t.get(p).map_or(0, |(m, c)| if mode == SortMode::Modified { *m } else { *c });
            notes.sort_by(|a, b| pick(b).cmp(&pick(a)).then_with(|| by_name(a, b)));
        }
        SortMode::Manual => {
            let order = ws.features.nav.order.get(&folder_key(folder)).cloned().unwrap_or_default();
            let rank = |p: &NotePath| order.iter().position(|n| n == p.file_name()).unwrap_or(usize::MAX);
            notes.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| by_name(a, b)));
        }
    }
}

/// Manual order: put `note` just before `before` (or last), in the order
/// `shown` (the folder's notes as listed now). The folder becomes manual.
pub fn move_note_in_order(
    ws: &mut Workspace,
    folder: &str,
    shown: &[NotePath],
    note: &NotePath,
    before: Option<&NotePath>,
) -> Result<(), String> {
    let key = folder_key(folder);
    let mut order: Vec<String> = shown.iter().map(|p| p.file_name().to_string()).collect();
    let name = note.file_name().to_string();
    order.retain(|n| *n != name);
    let at = before.and_then(|b| order.iter().position(|n| n == b.file_name())).unwrap_or(order.len());
    order.insert(at, name);
    super::edit_prefs(ws, |p| {
        let mut o: BTreeMap<String, Vec<String>> = p.section("order");
        o.insert(key.clone(), order);
        p.set_section("order", &o)?;
        let mut sort: BTreeMap<String, String> = p.section("sort");
        sort.insert(key.clone(), SortMode::Manual.key().to_string());
        p.set_section("sort", &sort)
    })
}

/// What a note is called in lists and tabs: its title (`title:` or first
/// heading) with display titles on, else its file name.
pub fn display_name(ws: &Workspace, path: &NotePath) -> String {
    // 0.11: a locked folder's note, by its plain name, with its lock.
    if let Some(plain) = quire_vault::lock::plain_name(path).filter(NotePath::is_markdown) {
        let open = ws.locked_folder_cached(&plain).is_some_and(|f| ws.is_unlocked(&f));
        return format!("{} {}", if open { "🔓" } else { "🔒" }, plain.stem());
    }
    if !path.is_markdown() {
        // A `.base`, canvas or PDF: the file name tells it from a note of the same name.
        return path.file_name().to_string();
    }
    if ws.settings.editing.display_titles
        && let Some(r) = ws.index().read().get(path)
        && !r.title.trim().is_empty()
    {
        return r.title.trim().to_string();
    }
    path.stem().to_string()
}

/// The note that stands for a folder: `Projects/Projects.md` or `Projects/index.md`.
pub fn folder_note(ws: &Workspace, folder: &str) -> Option<NotePath> {
    let f = folder.trim_matches('/');
    let vault = ws.vault()?;
    let last = f.rsplit('/').next().unwrap_or(f);
    [format!("{f}/{last}.md"), format!("{f}/index.md"), format!("{f}/Index.md")].into_iter().map(NotePath::new).find(|p| vault.exists(p))
}

/// Where a `[[Projects/]]` link goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderLink {
    /// The folder's own note.
    Note(NotePath),
    /// No folder note: show the folder in the sidebar.
    Folder(String),
}

/// Follow a link to a folder (`target` ends in `/`). `None` when `target`
/// isn't a folder link; a folder that doesn't exist says so.
pub fn folder_link(ws: &mut Workspace, target: &str) -> Option<FolderLink> {
    let t = target.trim();
    if !t.ends_with('/') || t.trim_matches('/').is_empty() {
        return None;
    }
    let folder = t.trim_matches('/').to_string();
    if let Some(p) = folder_note(ws, &folder) {
        ws.open_note(&p);
        return Some(FolderLink::Note(p));
    }
    let exists = ws.vault().is_some_and(|v| v.folders().iter().any(|f| f.trim_matches('/').eq_ignore_ascii_case(&folder)));
    if !exists {
        ws.notify(NoticeLevel::Warn, format!("There is no folder “{folder}”"));
        return Some(FolderLink::Folder(folder));
    }
    ws.features.nav.reveal_folder = Some(folder.clone());
    Some(FolderLink::Folder(folder))
}

#[cfg(test)]
mod tests {
    use super::super::tests::ws_with;
    use super::*;

    #[test]
    fn folders_sort_by_name_or_by_hand() {
        let mut ws = ws_with(&[("P/b.md", "b"), ("P/a.md", "a"), ("P/C.md", "c")]);
        ws.tick(std::time::Instant::now());
        let mut notes: Vec<NotePath> = ["P/b.md", "P/a.md", "P/C.md"].map(NotePath::new).to_vec();
        sorted(&mut ws, "P", &mut notes);
        assert_eq!(notes.iter().map(NotePath::stem).collect::<Vec<_>>(), ["a", "b", "C"]);
        let shown = notes.clone();
        move_note_in_order(&mut ws, "P", &shown, &NotePath::new("P/C.md"), Some(&NotePath::new("P/a.md"))).unwrap();
        assert_eq!(sort_mode(&ws, "P/"), SortMode::Manual);
        sorted(&mut ws, "P", &mut notes);
        assert_eq!(notes.iter().map(NotePath::stem).collect::<Vec<_>>(), ["C", "a", "b"]);
        // It's in the vault's preferences, readable by hand.
        let text = ws.vault().unwrap().storage().read(&NotePath::new(quire_vault::prefs::VAULT_PREFS_PATH)).unwrap();
        assert!(text.contains("[order]") && text.contains("\"P/\" = [\"C.md\", \"a.md\", \"b.md\"]"), "{text}");
        assert!(text.contains("[sort]") && text.contains("\"P/\" = \"manual\""), "{text}");
        set_sort_mode(&mut ws, "P", SortMode::Name, &notes).unwrap();
        sorted(&mut ws, "P", &mut notes);
        assert_eq!(notes.iter().map(NotePath::stem).collect::<Vec<_>>(), ["a", "b", "C"]);
    }

    #[test]
    fn links_to_folders() {
        let mut ws = ws_with(&[("Projects/Projects.md", "# P"), ("Areas/x.md", "x"), ("Docs/index.md", "i")]);
        assert_eq!(folder_link(&mut ws, "Projects/"), Some(FolderLink::Note(NotePath::new("Projects/Projects.md"))));
        assert_eq!(ws.active_doc().map(crate::Document::title).as_deref(), Some("Projects"));
        assert_eq!(folder_link(&mut ws, "Docs/"), Some(FolderLink::Note(NotePath::new("Docs/index.md"))));
        assert_eq!(folder_link(&mut ws, "Areas/"), Some(FolderLink::Folder("Areas".into())));
        assert_eq!(ws.features.nav.reveal_folder.as_deref(), Some("Areas"));
        assert_eq!(folder_link(&mut ws, "Areas"), None, "not a folder link");
        let n = ws.notices.len();
        assert_eq!(folder_link(&mut ws, "Nope/"), Some(FolderLink::Folder("Nope".into())));
        assert!(ws.notices.len() > n);
    }

    #[test]
    fn display_titles() {
        let mut ws = ws_with(&[("2026-01-01 x.md", "---\ntitle: Kick-off\n---\nbody"), ("plain.md", "no heading")]);
        assert_eq!(display_name(&ws, &NotePath::new("2026-01-01 x.md")), "2026-01-01 x");
        ws.settings.editing.display_titles = true;
        assert_eq!(display_name(&ws, &NotePath::new("2026-01-01 x.md")), "Kick-off");
        assert_eq!(display_name(&ws, &NotePath::new("plain.md")), "plain");
    }
}
