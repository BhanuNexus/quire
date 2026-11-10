// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-F's entry point into the workspace.
//!
//! Editor and navigation: Vim mode, tab modes, positions, sorting, the homepage, themes and git commands.
//!
//! `features.rs` calls the functions below for every Editor-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-F. State lives in
//! `ws.features.nav` (and `ws.features.git`, `ws.features.themes`).

mod browse;
mod embed;
mod tabs;
mod vim_doc;

use std::time::{Duration, Instant};

use quire_text::vim::VimState;
use quire_vault::{VaultEvent, VaultPrefs};

pub use browse::{
    FolderLink, SortMode, display_name, folder_link, folder_note, move_note_in_order, note_times, set_sort_mode, sort_mode, sorted,
};
pub use embed::{EMBED_DOC_BIT, embed_doc, embed_doc_mut, embed_doc_ref, save_embedded};
pub use tabs::{pin, remember_position, restore_position};
pub use vim_doc::{VimHost, collapse_insert, vim_ex, vim_undo};

use crate::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::{DocId, Document};
use crate::session::Positions;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// How often vault.toml is checked for changes made elsewhere.
const PREFS_CHECK: Duration = Duration::from_secs(5);
/// How long a Vim message stays in the status bar.
const VIM_MESSAGE: Duration = Duration::from_secs(6);

/// WS-F's state, kept in `ws.features.nav`.
#[derive(Default)]
pub struct NavState {
    /// Vim keys (when `editing.vim` is on).
    pub vim: VimState,
    /// The note the Vim state belongs to; switching notes returns to Normal mode.
    vim_doc: Option<DocId>,
    /// Vim marks of open notes other than the active one.
    vim_marks: std::collections::HashMap<DocId, quire_text::vim::Marks>,
    /// A message from Vim (an error, "Pattern not found") and when it came.
    pub vim_message: Option<(String, Instant)>,
    /// Reuse mode: the tab the next opened note replaces (shown in italics).
    pub preview: Option<DocId>,
    /// The preview tab's revision when it opened; editing it pins it.
    preview_rev: u64,
    /// Open notes seen last frame, to notice new ones.
    known: Vec<DocId>,
    /// Set after the first frame of a vault (restored tabs are kept).
    started: bool,
    /// The next note opened gets a tab of its own (Ctrl+click, middle-click,
    /// double-click), even in reuse mode.
    pub open_pinned: bool,
    positions: Positions,
    positions_dirty: bool,
    last_stash: Option<Instant>,
    /// Vault preferences WS-F reads: manual order and sort modes.
    pub(crate) order: std::collections::BTreeMap<String, Vec<String>>,
    pub(crate) sort: std::collections::BTreeMap<String, String>,
    /// Bumped when `order` or `sort` change, so a sorted file list can be
    /// kept between frames.
    sort_generation: u64,
    prefs_seen: Option<(Instant, Option<std::time::SystemTime>)>,
    /// A folder link was followed: the sidebar shows this folder.
    pub reveal_folder: Option<String>,
    /// Notes being edited in an embed that aren't open in a tab.
    pub(crate) hidden: Vec<Document>,
    /// When an embed last showed each hidden note.
    pub(crate) hidden_seen: Vec<(DocId, Instant)>,
    next_hidden: u64,
    /// Modified and created times for sorting, per index version.
    pub(crate) times: browse::TimesCache,
    /// 0.11: a macro chosen in the palette, for the app to run.
    pub run_macro: Option<String>,
}

impl NavState {
    /// Changes whenever a folder's sort mode or manual order changes.
    pub const fn sort_generation(&self) -> u64 {
        self.sort_generation
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let mut changed = crate::themes::tick(ws);
    if ws.vault().is_some() && !ws.features.nav.started {
        changed |= start(ws);
    }
    changed |= check_prefs(ws, now);
    tabs::track_vim_doc(ws);
    changed |= tabs::track_tabs(ws);
    tabs::track_positions(ws, now);
    changed |= embed::tick(ws, now);
    changed |= crate::git::tick(ws, now);
    if ws.features.nav.vim_message.as_ref().is_some_and(|(_, at)| now.duration_since(*at) > VIM_MESSAGE) {
        ws.features.nav.vim_message = None;
        changed = true;
    }
    changed
}

/// The first frame with a vault: what was restored stays open; with nothing
/// restored, the homepage opens.
fn start(ws: &mut Workspace) -> bool {
    let st = &mut ws.features.nav;
    st.started = true;
    st.known = ws.docs.iter().map(|d| d.id).collect();
    tabs::load_positions(ws);
    read_prefs(ws);
    crate::git::start(ws);
    if ws.docs.iter().all(|d| d.path.is_none()) && !ws.settings.editing.homepage.trim().is_empty() {
        open_homepage(ws, false);
        ws.features.nav.known = ws.docs.iter().map(|d| d.id).collect();
        return true;
    }
    false
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let st = &ws.features.nav;
    [
        st.vim_message.as_ref().map(|(_, at)| *at + VIM_MESSAGE),
        embed::next_deadline(ws),
        crate::git::next_deadline(ws),
        st.positions_dirty.then(|| st.last_stash.map_or_else(Instant::now, |t| t + tabs::STASH_EVERY)),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// Whether a Editor-owned command is ready to offer. Commands that aren't
/// are hidden from the palette, the shortcut sheet and the keyboard, so a
/// build looks like 0.8 until a feature lands.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        // Git commands only for a vault that is a git repository.
        Command::GitCommitNow | Command::GitPull | Command::GitPush => ws.features.git.repo,
        _ => true,
    }
}

/// A Editor-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::ToggleVim => {
            ws.settings.editing.vim = !ws.settings.editing.vim;
            ws.features.nav.vim.reset();
            let _ = ws.settings.save();
            let msg = if ws.settings.editing.vim { "Vim keys on (Esc for Normal mode, i to type)" } else { "Vim keys off" };
            ws.notify(NoticeLevel::Info, msg);
        }
        Command::ToggleTwoPaneBrowser => {
            ws.settings.editing.two_pane = !ws.settings.editing.two_pane;
            ws.settings.show_sidebar = true;
            ws.focus_mode = false;
            let _ = ws.settings.save();
        }
        Command::OpenHomepage => open_homepage(ws, true),
        // 0.11 editor: Settings → Commands, the attachment picker and the toolbar's keyboard focus are UI.
        Command::ChooseTheme | Command::CustomizeCommands | Command::InsertAttachment | Command::FocusToolbar => {
            return Some(UiRequest::Feature(cmd));
        }
        Command::GitCommitNow => crate::git::commit_now(ws),
        Command::GitPull => crate::git::pull(ws),
        Command::GitPush => crate::git::push(ws),
        // 0.11 editor (formatting commands run in `format_cmds` before this).
        Command::ToggleToolbar => {
            ws.settings.toolbar.show = !ws.settings.toolbar.show;
            let _ = ws.settings.save();
        }
        _ => ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…'))),
    }
    None
}

/// Open `editing.homepage` (a note path or name).
fn open_homepage(ws: &mut Workspace, asked: bool) {
    let home = ws.settings.editing.homepage.trim().to_string();
    if home.is_empty() {
        if asked {
            ws.notify(NoticeLevel::Info, "Choose a homepage note in Settings → Editing");
        }
        return;
    }
    let target = home.trim_end_matches(".md");
    let found = ws.index().read().resolve(target, None);
    match found {
        Some(p) => {
            ws.features.nav.open_pinned = true;
            ws.open_note(&p);
        }
        None => ws.notify(NoticeLevel::Warn, format!("The homepage note “{home}” wasn't found")),
    }
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let _ = (ws, doc, out);
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let _ = (ws, ev);
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    embed::vault_event(ws, ev);
    crate::git::vault_event(ws, ev);
}

/// A vault was opened: state starts fresh (start jobs here: `ws.jobs.add(...)`).
/// Save the notes edited through embeds (not open in a tab).
pub(crate) fn save_hidden_docs(ws: &mut Workspace) {
    embed::save_all_hidden(ws);
}

pub(crate) fn vault_opened(ws: &mut Workspace) {
    let vim = std::mem::take(&mut ws.features.nav.vim);
    ws.features.nav = NavState { vim, ..NavState::default() };
    ws.features.nav.vim.reset();
    ws.features.git = crate::git::GitState::default();
    ws.features.themes.tag_colors.clear();
    ws.features.themes.folder_icons.clear();
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    use crate::commands::Owner;
    if ws.settings.editing.vim && ws.active_doc().is_some() {
        let st = &ws.features.nav;
        let text = match &st.vim_message {
            Some((m, _)) if st.vim.cmdline().is_none() => m.clone(),
            _ => st.vim.status(),
        };
        let text = if text.is_empty() { "NORMAL".to_string() } else { text };
        let warn = st.vim_message.is_some() && st.vim.cmdline().is_none();
        out.push(StatusItem {
            owner: Owner::Editor,
            text,
            tooltip: "Vim keys are on. Click to turn them off.".into(),
            command: Some(Command::ToggleVim),
            warn,
        });
    }
    crate::git::status(ws, out);
}

// ------------------------------------------------------------ vault preferences

/// Re-read vault.toml when it changed on disk (another device, a hand edit).
fn check_prefs(ws: &mut Workspace, now: Instant) -> bool {
    if !ws.features.nav.started || ws.features.nav.prefs_seen.is_some_and(|(at, _)| now.duration_since(at) < PREFS_CHECK) {
        return false;
    }
    let mtime = ws.vault().and_then(|v| v.storage().modified(&quire_vault::NotePath::new(quire_vault::prefs::VAULT_PREFS_PATH)).ok());
    let before = ws.features.nav.prefs_seen.and_then(|(_, m)| m);
    ws.features.nav.prefs_seen = Some((now, mtime));
    if mtime != before {
        read_prefs(ws);
        return true;
    }
    false
}

/// Load the sections WS-F uses from `.quire/shared/vault.toml`.
fn read_prefs(ws: &mut Workspace) {
    let Some(vault) = ws.vault().cloned() else { return };
    let prefs = VaultPrefs::load(&vault);
    browse::read_prefs(ws, &prefs);
    crate::themes::read_prefs(ws, &prefs);
    crate::git::read_prefs(ws, &prefs);
    crate::links_state::read_prefs(ws, &prefs);
    let mtime = vault.storage().modified(&quire_vault::NotePath::new(quire_vault::prefs::VAULT_PREFS_PATH)).ok();
    ws.features.nav.prefs_seen = Some((Instant::now(), mtime));
}

/// Change vault.toml: read it fresh, let `f` edit it, save it, and use it.
pub fn edit_prefs(ws: &mut Workspace, f: impl FnOnce(&mut VaultPrefs) -> Result<(), quire_vault::PrefsError>) -> Result<(), String> {
    let Some(vault) = ws.vault().cloned() else { return Err("Open a vault first".into()) };
    let mut prefs = VaultPrefs::load(&vault);
    f(&mut prefs).map_err(|e| e.to_string())?;
    prefs.save(&vault).map_err(|e| e.to_string())?;
    read_prefs(ws);
    Ok(())
}

/// Show a message from Vim in the status bar.
pub fn vim_say(ws: &mut Workspace, msg: impl Into<String>) {
    ws.features.nav.vim_message = Some((msg.into(), Instant::now()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;

    pub(super) fn ws_with(files: &[(&str, &str)]) -> Workspace {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(files));
        ws.index().flush();
        ws
    }

    #[test]
    fn editor_commands_are_offered_and_do_something() {
        let mut ws = ws_with(&[("A.md", "# A\n")]);
        for c in [Command::ToggleVim, Command::ChooseTheme, Command::ToggleTwoPaneBrowser, Command::OpenHomepage] {
            assert!(ws.command_available(c), "{c:?}");
        }
        assert!(!ws.command_available(Command::GitCommitNow), "not a git repository");
        ws.run(Command::ToggleVim);
        assert!(ws.settings.editing.vim);
        ws.open_note(&"A.md".into());
        assert_eq!(ws.status_items().first().map(|s| s.text.as_str()), Some("NORMAL"));
        ws.run(Command::ToggleVim);
        assert!(!ws.settings.editing.vim && ws.status_items().is_empty());
        assert_eq!(ws.run(Command::ChooseTheme), Some(UiRequest::Feature(Command::ChooseTheme)));
        let n = ws.notices.len();
        ws.run(Command::OpenHomepage);
        assert!(ws.notices.len() > n, "says where to set one");
    }

    #[test]
    fn the_homepage_opens_when_nothing_was_restored() {
        let mut s = Settings::default();
        s.editing.homepage = "Home".into();
        let mut ws = Workspace::new(s);
        ws.attach_vault(Vault::in_memory(&[("Home.md", "# Home\n"), ("B.md", "b")]));
        ws.index().flush();
        ws.tick(Instant::now());
        assert_eq!(ws.active_doc().and_then(|d| d.path.as_ref()).map(|p| p.as_str().to_string()), Some("Home.md".into()));
        // Not when a session was restored.
        let mut s = Settings::default();
        s.editing.homepage = "Home".into();
        let mut ws = Workspace::new(s);
        ws.attach_vault(Vault::in_memory(&[("Home.md", "# Home\n"), ("B.md", "b")]));
        ws.index().flush();
        ws.open_note(&"B.md".into());
        ws.tick(Instant::now());
        assert_eq!(ws.docs().len(), 1);
    }
}
