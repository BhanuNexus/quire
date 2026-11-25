// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.11 security in the workspace: locked folders (docs/design/0.11-secure.md)
//! and the vault's "AI off" switch.
//!
//! Locked notes never enter the vault index (`Vault::notes` leaves them out
//! and the index service drops them). While a folder is unlocked its notes
//! are searchable through a second, in-memory index kept here and dropped
//! on lock. Keys live in the vault's key ring (memory only), and in the OS
//! keychain only when the person asks for "unlock automatically on this
//! computer".
//!
//! State lives in `ws.features.secure`.

use quire_base::dates;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use quire_index::{Index, QuickOpenHit, SearchHit};
use quire_vault::lock::{self, FolderKey, Kdf};
use quire_vault::{LockedFolderState, NotePath, Vault, VaultError, VaultEvent};

use crate::Command;
use crate::decor::StatusItem;
use crate::document::Document;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// Argon2id settings for new locked folders (fast ones in unit tests).
const KDF: Kdf = if cfg!(test) { Kdf::TEST } else { Kdf::DEFAULT };

/// What a background conversion finished with.
enum Done {
    Locked {
        folder: String,
        recovery: Option<String>,
        remember: bool,
        conflicts: usize,
    },
    Unlocked {
        folder: String,
        id: String,
    },
    Rekeyed {
        folder: String,
        recovery: Option<String>,
        remembered: bool,
    },
    Failed {
        folder: String,
        error: String,
        recovery: Option<String>,
        what: Conversion,
    },
    /// Not the end: a key change wrote its key file, and its new recovery
    /// key is shown now, before any file is encrypted again.
    KeyReady {
        folder: String,
        recovery: String,
        remembered: bool,
    },
}

/// Which conversion a job runs (for what to say when it stops).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conversion {
    Lock,
    UnlockForGood,
    /// A key change; `begun` once the key file has the new key.
    ChangeKey {
        begun: bool,
    },
}

struct Job {
    label: String,
    /// The folder being converted. While it runs, this process's own
    /// rewrites of its files are not read back as changes made elsewhere.
    folder: String,
    what: Conversion,
    rx: Receiver<Done>,
}

/// The workspace's security state.
#[derive(Default)]
pub struct SecureState {
    /// Search over the notes of unlocked locked folders. In memory only.
    locked_index: Option<Index>,
    /// Bumped when `locked_index` changes (UI caches).
    pub locked_version: u64,
    building: Option<Receiver<Index>>,
    /// Open locked notes as last put in `locked_index` (path → revision).
    indexed: HashMap<NotePath, u64>,
    /// The person's last input (keys, mouse); with typing, what idle is measured from.
    last_input: Option<Instant>,
    job: Option<Job>,
    /// Shown once after a folder is locked: (folder, recovery key).
    recovery: Option<(String, lock::zeroize::Zeroizing<String>)>,
    /// AI-off as last applied to the chat model and embedder.
    ai_off_applied: bool,
    /// Idle locking waits until then (a folder kept unlocked because a note
    /// in it couldn't be saved).
    idle_retry: Option<Instant>,
    /// Folders asked to lock while a drawing in them had edits not yet
    /// written: they lock once the editors have saved (and since when).
    lock_waiting: Vec<(String, Instant)>,
    /// The person chose to quit although notes in locked folders can't be
    /// saved; and the window should close now.
    quit_confirmed: bool,
    quit_now: bool,
    /// The locked folders, read at most once a second (sidebar rows).
    /// The pass over a newly opened vault (one listing, off the UI thread).
    opening: Option<Receiver<Opened>>,
    /// "Agent connections…": the Agents tab opens on its Connections view.
    pub show_connections: bool,
    /// [`ai_off`], as last read.
    ai_cache: std::cell::Cell<Option<(Instant, bool)>>,
    /// 0.12: links between plain notes and notes of unlocked folders, by
    /// (vault index version, `locked_version`). Memory only.
    cross: overlay::CrossCache,
}

impl std::fmt::Debug for SecureState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureState").field("job", &self.job.as_ref().map(|j| &j.label)).finish_non_exhaustive()
    }
}

impl SecureState {
    /// Apply the AI switch again at the next check (settings changed).
    pub(crate) const fn reapply_ai_switch(&mut self) {
        self.ai_off_applied = false;
    }
}

/// This device's record of the vault's locked folders
/// (`<config>/vaults/<key>/locked-folders.json`), kept outside the vault so
/// nobody who only reaches the vault can change it.
pub fn local_record(root: &std::path::Path) -> Option<std::path::PathBuf> {
    quire_agent::approval::default_state_dir(root).map(|d| d.join(lock::LOCAL_RECORD))
}

/// Open the vault at `root`, remembering its locked folders on this device.
///
/// A locked folder whose key file disappears stays locked
/// (docs/formats/locked-folders.md#remembered-locked-folders). Use it
/// wherever a vault on disk is opened to be written.
pub fn open_vault(root: &std::path::Path) -> Result<Vault, String> {
    #[expect(clippy::disallowed_methods, reason = "this is the gate: it opens the vault, then remembers its locked folders")]
    let v = Vault::open_local(root).map_err(|e| e.to_string())?;
    v.remember_locked_folders(local_record(root).as_deref());
    Ok(v)
}

/// [`open_vault`] for the app, before its first frame: the records of the
/// vault's locked folders are applied at once, without listing the vault;
/// checking them against the disk waits for a background pass
/// ([`vault_opened`]). Until then every recorded folder counts as locked.
pub(crate) fn open_vault_now(root: &std::path::Path) -> Result<Vault, String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the gate for the app's first frame: it opens the vault, then applies the locked-folder records"
    )]
    let v = Vault::open_local(root).map_err(|e| e.to_string())?;
    v.remember_locked_records(local_record(root).as_deref());
    Ok(v)
}

/// What the background pass over a newly opened vault found.
struct Opened {
    states: Vec<LockedFolderState>,
    left: Vec<lock::PlainLeft>,
}

/// What to tell the person about a locked folder whose key file is missing.
pub fn key_missing_text(folder: &str) -> String {
    format!(
        "{folder}/ is locked, but its key file ({}) is missing or was renamed. Quire keeps it locked and writes nothing plain there. \
         Restore the key file (from a backup, git or another device), or unlock it with the recovery key and a new passphrase to make a new one.",
        lock::KEY_FILE
    )
}

/// What to tell the person about an unfinished unlock for good.
///
/// Its key file has the `unlocking` name, and this process isn't running
/// that conversion: until it does, nothing plain is written there.
pub fn unlock_unfinished_text(folder: &str) -> String {
    format!(
        "Unlocking {folder}/ for good didn't finish, so its notes can't be saved yet. Run “Stop locking a folder…” on it again to finish \
         (it asks for the passphrase). If you didn't start that, someone renamed its key file: rename {} back to {} to keep it locked.",
        lock::UNLOCKING_FILE,
        lock::KEY_FILE
    )
}

/// The keychain entry of a folder's key ("unlock automatically").
fn keychain_name(root: &std::path::Path, id: &str) -> String {
    format!("locked-folder:{}:{id}", quire_agent::approval::vault_key(root))
}

/// Whether secrets go to a real OS keychain (not the private-file fallback,
/// which would leave the key next to the vault it opens).
pub fn system_keychain() -> bool {
    crate::secrets::store().kind() == "system keychain"
}

/// Whether AI is off for the open vault (agent policy `ai = "off"`), read
/// from the policy at most once a second (the UI asks every frame).
pub fn ai_off(ws: &Workspace) -> bool {
    let cache = &ws.features.secure.ai_cache;
    if let Some((at, off)) = cache.get()
        && at.elapsed() < Duration::from_secs(1)
    {
        return off;
    }
    let off = ws.agent.as_ref().is_some_and(|a| a.policy_state().active().iter().any(|p| p.ai_off()));
    cache.set(Some((Instant::now(), off)));
    off
}

/// Whether AI is off for the open vault, as last applied (each tick and
/// when the vault opens): for checks that can't read the policy.
pub(crate) const fn ai_off_applied(ws: &Workspace) -> bool {
    ws.features.secure.ai_off_applied
}

/// Save a note of a locked folder over what it last read (`base`): the
/// editor's compare-and-swap, through the folder's key.
pub(crate) fn save_locked(vault: &Vault, path: &NotePath, base: &str, text: &str) -> quire_vault::Result<bool> {
    if !vault.exists(path) {
        vault.write(path, text)?;
        return Ok(true);
    }
    vault.write_if(path, Some(base), text)
}

/// Export writes to `Exports/` in the vault, unencrypted: refused for a
/// note of a locked folder, with the reason. Returns whether it refused.
pub(crate) fn refuse_export(ws: &mut Workspace) -> bool {
    let locked =
        ws.active_doc().and_then(|d| d.path.clone()).is_some_and(|p| ws.vault.as_ref().is_some_and(|v| v.locked_folder(&p).is_some()));
    if locked {
        ws.notify(
            NoticeLevel::Info,
            "A locked note isn't exported: the file would go to Exports/ unencrypted. Copy as HTML puts it on the clipboard instead.",
        );
    }
    locked
}

/// Writing help (summarize, rewrite…) through an AI provider on a locked
/// note: refused, with the reason, so its text never reaches the provider.
/// The offline assistant is fine. Returns whether it refused.
pub(crate) fn refuse_ai_for_active(ws: &mut Workspace) -> bool {
    let locked =
        ws.active_doc().and_then(|d| d.path.clone()).is_some_and(|p| ws.vault.as_ref().is_some_and(|v| v.locked_folder(&p).is_some()));
    let provider = ws.settings.ai_provider != "local";
    if locked && provider {
        ws.notify(NoticeLevel::Info, "Writing help through your AI provider is off for locked notes: their text isn't sent anywhere.");
    }
    locked && provider
}

/// A note in a locked folder a link names (names aren't secret): for links
/// to locked notes, which the vault index doesn't hold.
pub(crate) fn resolve_locked(ws: &Workspace, target: &str) -> Option<NotePath> {
    let vault = ws.vault.as_ref()?;
    let notes: Vec<NotePath> = vault.locked_folders().iter().flat_map(|f| vault.locked_notes(&f.folder.path)).collect();
    if notes.is_empty() {
        return None;
    }
    let from = ws.active_doc().and_then(|d| d.path.clone());
    Vault::resolve_among(&notes, target, from.as_ref())
}

impl Workspace {
    /// Every locked folder of the vault and whether it is unlocked.
    pub fn locked_folders(&self) -> Vec<LockedFolderState> {
        self.vault.as_ref().map(Vault::locked_folders).unwrap_or_default()
    }

    /// The locked folder `path` is in (by plain or stored name).
    pub fn locked_folder_of(&self, path: &NotePath) -> Option<lock::LockedFolder> {
        let plain = lock::plain_name(path).unwrap_or_else(|| path.clone());
        self.vault.as_ref()?.locked_folder(&plain)
    }

    /// The locked folder `path` is in, from the folders this process knows
    /// are locked ([`lock::Known`], kept in memory: cheap enough for every
    /// row of the sidebar, and never a walk of the vault). It learns of a
    /// folder when it is opened (records first, then the disk, in the
    /// background), locked, unlocked for good, or when a key file arrives.
    pub fn locked_folder_cached(&self, path: &NotePath) -> Option<String> {
        self.vault.as_ref()?.known_locked().folder_of(path.as_str())
    }

    /// The locked folders this process knows of and whether each is
    /// unlocked (no walk of the vault; see [`Self::locked_folder_cached`]).
    pub fn known_locked_folders(&self) -> Vec<(String, bool)> {
        let Some(v) = &self.vault else { return Vec::new() };
        v.known_locked().folders().into_iter().map(|k| (k.folder.clone(), v.keys().is_unlocked(&k.folder))).collect()
    }

    /// Whether the background pass over the vault just opened is still running.
    pub fn locked_pass_pending(&self) -> bool {
        self.features.secure.opening.is_some()
    }

    /// Whether `folder` is a locked folder (cached as [`Self::locked_folder_cached`]).
    pub fn is_locked_folder(&self, folder: &str) -> bool {
        self.locked_folder_cached(&NotePath::new(format!("{folder}/x"))).is_some_and(|f| f == folder)
    }

    /// Whether `folder` is unlocked here.
    pub fn is_unlocked(&self, folder: &str) -> bool {
        self.vault.as_ref().is_some_and(|v| v.keys().is_unlocked(folder))
    }

    /// The person did something (a key, a click): idle locking counts from here.
    pub fn note_user_activity(&mut self) {
        self.features.secure.last_input = Some(Instant::now());
    }

    /// Unlock `folder` with its passphrase. `remember` keeps the key in the
    /// OS keychain so this computer unlocks it at start.
    pub fn unlock_folder(&mut self, folder: &str, passphrase: &str, remember: bool) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let key = vault.unlock(folder, passphrase).map_err(|e| unlock_error(&vault, folder, &e))?;
        if remember {
            self.remember_key(folder, &key)?;
        }
        self.after_unlock(folder);
        Ok(())
    }

    /// Unlock `folder` with its recovery key, and give it a new passphrase
    /// when one is given.
    pub fn unlock_with_recovery_key(&mut self, folder: &str, recovery: &str, new_passphrase: Option<&str>) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let key = FolderKey::from_recovery_key(recovery).map_err(|e| e.to_string())?;
        // Checked before anything is written: against the key file, or, with
        // the key file lost, against the folder's encrypted files.
        vault.unlock_with_key(folder, key.clone()).map_err(|e| unlock_error(&vault, folder, &e))?;
        if let Some(p) = new_passphrase {
            vault.change_passphrase(folder, Some(key), p, crate::history_ops::now_ms()).map_err(|e| e.to_string())?;
        }
        self.after_unlock(folder);
        Ok(())
    }

    fn after_unlock(&mut self, folder: &str) {
        self.features.secure.last_input = Some(Instant::now());
        // Edits kept while the folder was locked can be saved again.
        let f = lock::LockedFolder { path: folder.to_string(), unlocking: false };
        for d in &mut self.docs {
            if d.conflict && d.pending_merge.is_none() && d.path.as_ref().is_some_and(|p| f.contains(p.as_str())) {
                d.conflict = false;
            }
        }
        self.rebuild_locked_index();
        self.recheck_locked_docs();
        let n = self.vault.as_ref().map_or(0, |v| v.locked_notes(folder).len());
        self.notify(NoticeLevel::Info, format!("Unlocked {folder}/ ({n} note{})", if n == 1 { "" } else { "s" }));
        if let Some(v) = self.vault.clone() {
            // A folder locked on another device: this device's own history of
            // its notes (plain, from before) is encrypted now, in the background.
            let f = folder.to_string();
            std::thread::Builder::new()
                .name("quire-lock-history".into())
                .spawn(move || {
                    for note in v.locked_notes(&f).into_iter().chain(v.history_paths_under(&f).into_iter().flatten()) {
                        let _ = v.rehome_history(&note, &note);
                    }
                })
                .expect("the system starts a thread");
        }
        if let Some(v) = &self.vault
            && let Some(left) = lock::plain_left(v).into_iter().find(|p| p.folder == folder)
        {
            plain_left_notice(self, &left);
        }
    }

    /// Lock `folder` now: its open notes are saved and closed, their text
    /// and search entries dropped, and the key forgotten. A note that can't
    /// be saved (it changed on disk meanwhile, say) is kept as an encrypted
    /// copy beside it (`Day (unsaved 2026-11-08 14.30).md`), never in plain
    /// text; if even that fails the folder stays unlocked. Returns how many
    /// notes were closed, or `None` when the folder isn't locked yet: it was
    /// kept unlocked, or it waits for a drawing's edits to be written (the
    /// lock follows on a later tick, or is refused with a notice; a drawing
    /// that is in "changed elsewhere" with edits keeps it unlocked until the
    /// person chooses what to keep).
    pub fn lock_folder_now(&mut self, folder: &str) -> Option<usize> {
        // A drawing's edits wait up to 0.6 s for autosave, and a locked
        // folder can't be written: have the editors save first, and lock on
        // a later tick (`lock_when_saved`).
        if self.features.canvas.unsaved_in(folder) {
            if !self.is_lock_waiting(folder) {
                self.features.canvas.save_now += 1;
                self.features.secure.lock_waiting.push((folder.to_string(), Instant::now()));
            }
            return None;
        }
        let open = self.docs_in(folder);
        for id in &open {
            self.save(*id);
        }
        for id in &open {
            if self.doc(*id).is_some_and(Document::is_dirty) && !self.keep_unsaved_copy(*id) {
                let title = self.doc(*id).map(Document::title).unwrap_or_default();
                self.notify(NoticeLevel::Warn, format!("{folder}/ stays unlocked: “{title}” has edits that couldn't be saved."));
                return None;
            }
        }
        let mut closed = 0;
        for id in open {
            if self.doc(id).is_some_and(|d| !d.is_dirty()) {
                self.close(id);
                closed += 1;
            }
        }
        if let Some(v) = &self.vault {
            v.lock_folder_now(folder);
        }
        self.forget_unlocked_text();
        self.rebuild_locked_index();
        Some(closed)
    }

    /// Whether `folder` already waits for its drawings to save.
    fn is_lock_waiting(&self, folder: &str) -> bool {
        self.features.secure.lock_waiting.iter().any(|(f, _)| f == folder)
    }

    /// The open notes (tabs and hidden ones) in `folder`.
    fn docs_in(&self, folder: &str) -> Vec<crate::document::DocId> {
        let f = lock::LockedFolder { path: folder.to_string(), unlocking: false };
        self.docs.iter().filter(|d| d.path.as_ref().is_some_and(|p| f.contains(p.as_str()))).map(|d| d.id).collect()
    }

    /// Keep the unsaved text of open note `id` (in an unlocked locked
    /// folder) as an encrypted copy beside it, and mark the note saved.
    /// Returns whether it did.
    fn keep_unsaved_copy(&mut self, id: crate::document::DocId) -> bool {
        let (Some(vault), Some(doc)) = (self.vault.clone(), self.doc(id)) else { return false };
        let Some(path) = doc.path.clone() else { return false };
        let Some(f) = vault.locked_folder(&path).filter(|f| !f.unlocking && vault.keys().is_unlocked(&f.path)) else { return false };
        let text = doc.text().to_string();
        let stamp = dates::now_fmt("%Y-%m-%d %H.%M");
        let Ok(copy) = vault.unique_path(path.folder(), &format!("{} (unsaved {stamp})", path.stem())) else { return false };
        match vault.write_if(&copy, None, &text) {
            Ok(true) if vault.read(&copy).is_ok_and(|t| t == text) => {
                if let Some(d) = self.docs.iter_mut().find(|d| d.id == id) {
                    d.conflict = false;
                    d.pending_merge = None;
                    d.mark_saved(&text);
                }
                self.notify(
                    NoticeLevel::Warn,
                    format!(
                        "“{}” couldn't be saved before {}/ was locked: your edits are in “{}”, encrypted.",
                        path.stem(),
                        f.path,
                        copy.stem()
                    ),
                );
                true
            }
            _ => false,
        }
    }

    /// Lock every unlocked folder now. Returns the folders locked.
    pub fn lock_all_now(&mut self) -> Vec<String> {
        let unlocked = self.vault.as_ref().map(|v| v.keys().unlocked()).unwrap_or_default();
        unlocked.into_iter().filter(|f| self.lock_folder_now(f).is_some()).collect()
    }

    /// Idle locking: like [`Self::lock_all_now`], except that a folder with
    /// a note that can't be saved isn't locked yet (its edits stay where
    /// they are, and idle locking tries again in a minute).
    fn lock_idle(&mut self) -> Vec<String> {
        let unlocked = self.vault.as_ref().map(|v| v.keys().unlocked()).unwrap_or_default();
        let mut locked = Vec::new();
        for f in unlocked {
            if self.is_lock_waiting(&f) {
                continue; // already asked its drawings to save; `lock_when_saved` decides
            }
            let open = self.docs_in(&f);
            for id in &open {
                self.save(*id);
            }
            if open.iter().any(|id| self.doc(*id).is_some_and(Document::is_dirty)) {
                self.features.secure.idle_retry = Some(Instant::now() + Duration::from_mins(1));
                self.notify(
                    NoticeLevel::Warn,
                    format!("{f}/ wasn't locked after the idle time: a note in it has edits that couldn't be saved yet. Save or close it."),
                );
                continue;
            }
            if self.lock_folder_now(&f).is_some() {
                locked.push(f);
            }
        }
        locked
    }

    /// Drop what the window keeps of notes no longer readable: the
    /// insights of the open note and other per-note caches.
    fn forget_unlocked_text(&mut self) {
        self.clear_note_caches();
        self.features.decor_generation += 1;
    }

    /// Notes in locked folders with edits that can't be saved or kept: the
    /// folder is locked here (by another device or the CLI) and this
    /// process has no key. Their edits are lost when Quire quits, so the
    /// app asks first.
    pub fn unsaveable_locked_notes(&self) -> Vec<String> {
        let Some(v) = &self.vault else { return Vec::new() };
        self.docs
            .iter()
            .filter(|d| d.is_dirty())
            .filter(|d| d.path.as_ref().and_then(|p| v.locked_folder(p)).is_some_and(|f| !f.unlocking && !v.keys().is_unlocked(&f.path)))
            .map(Document::title)
            .collect()
    }

    /// The person confirmed quitting without the notes
    /// [`Self::unsaveable_locked_notes`] lists.
    pub const fn confirm_quit(&mut self) {
        self.features.secure.quit_confirmed = true;
        self.features.secure.quit_now = true;
    }

    /// Whether quitting was confirmed ([`Self::confirm_quit`]).
    pub const fn quit_confirmed(&self) -> bool {
        self.features.secure.quit_confirmed
    }

    /// Whether the window should close now (once).
    pub const fn take_quit_request(&mut self) -> bool {
        std::mem::replace(&mut self.features.secure.quit_now, false)
    }

    /// Before quitting: notes in unlocked locked folders that couldn't be
    /// saved are kept as encrypted copies beside them.
    pub(crate) fn keep_unsaved_locked(&mut self) {
        let Some(v) = self.vault.clone() else { return };
        let dirty: Vec<_> = self
            .docs
            .iter()
            .filter(|d| d.is_dirty() && d.path.as_ref().is_some_and(|p| v.locked_folder(p).is_some()))
            .map(|d| d.id)
            .collect();
        for id in dirty {
            self.keep_unsaved_copy(id);
        }
    }

    /// Start making `folder` a locked folder (or finishing that) in the
    /// background. A new folder's recovery key is then offered once
    /// ([`Self::take_recovery_key`]).
    pub fn start_locking(&mut self, folder: &str, passphrase: &str, remember: bool) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let folder = folder.trim().trim_matches('/').to_string();
        if let Some(why) = lock::refuse_to_lock(&vault, &folder) {
            return Err(why);
        }
        if self.features.secure.job.is_some() {
            return Err("A folder is already being converted; wait for it to finish.".into());
        }
        // Edits in the folder are saved first, so the conversion encrypts them.
        self.save_open_notes();
        let pass = passphrase.to_string();
        let (tx, rx) = bounded(1);
        let f = folder.clone();
        std::thread::Builder::new()
            .name("quire-lock".into())
            .spawn(move || {
                let pass = lock::zeroize::Zeroizing::new(pass);
                // The key file first, and its recovery key back before any file is
                // converted: if the conversion then stops, the key is still shown.
                let failed = |folder, error: VaultError, recovery| Done::Failed {
                    folder,
                    error: error.to_string(),
                    recovery,
                    what: Conversion::Lock,
                };
                let done = match lock::begin_lock(&vault, &f, Some(&pass), KDF, crate::history_ops::now_ms()) {
                    Err(e) => failed(f, e, None),
                    Ok(recovery) => match lock::lock_folder(&vault, &f, Some(&pass), KDF, crate::history_ops::now_ms()) {
                        Ok(r) => Done::Locked { folder: f, recovery, remember, conflicts: r.conflicts.len() },
                        Err(e) => failed(f, e, recovery),
                    },
                };
                let _ = tx.send(done);
            })
            .expect("the system starts a thread");
        self.features.secure.job = Some(Job { label: format!("Locking {folder}/…"), folder, what: Conversion::Lock, rx });
        Ok(())
    }

    /// Start turning `folder` back into an ordinary folder (decrypting it)
    /// in the background. Needs it unlocked or `passphrase`.
    pub fn start_unlocking_for_good(&mut self, folder: &str, passphrase: Option<&str>) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let folder = folder.trim().trim_matches('/').to_string();
        if self.features.secure.job.is_some() {
            return Err("A folder is already being converted; wait for it to finish.".into());
        }
        if !vault.keys().is_unlocked(&folder) {
            let p = passphrase.ok_or_else(|| format!("{folder}/ is locked; enter its passphrase."))?;
            vault.unlock(&folder, p).map_err(|e| unlock_error(&vault, &folder, &e))?;
        }
        self.save_open_notes();
        let id = vault.key_file(&folder).map(|(_, kf)| kf.folder_id).unwrap_or_default();
        let (tx, rx) = bounded(1);
        let f = folder.clone();
        std::thread::Builder::new()
            .name("quire-unlock".into())
            .spawn(move || {
                let done = match lock::unlock_folder_permanently(&vault, &f, None) {
                    Ok(_) => Done::Unlocked { folder: f, id },
                    Err(e) => Done::Failed { folder: f, error: e.to_string(), recovery: None, what: Conversion::UnlockForGood },
                };
                let _ = tx.send(done);
            })
            .expect("the system starts a thread");
        self.features.secure.job = Some(Job { label: format!("Decrypting {folder}/…"), folder, what: Conversion::UnlockForGood, rx });
        Ok(())
    }

    /// The conversion under way, in words.
    pub fn lock_job(&self) -> Option<&str> {
        self.features.secure.job.as_ref().map(|j| j.label.as_str())
    }

    /// The recovery key of the folder just locked, once: (folder, key).
    pub fn take_recovery_key(&mut self) -> Option<(String, String)> {
        self.features.secure.recovery.take().map(|(f, k)| (f, k.to_string()))
    }

    /// `folder`'s recovery key, to show again (it is the folder key in
    /// another form). Asks for the passphrase, even while the folder is
    /// unlocked, so someone at an unlocked computer can't take it away.
    pub fn folder_recovery_key(&self, folder: &str, passphrase: &str) -> Result<String, String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let (_, kf) = vault.key_file(folder).map_err(|e| unlock_error(&vault, folder, &e))?;
        let key = kf.unwrap_key(passphrase).map_err(|e| match e {
            lock::LockError::WrongPassphrase => "Wrong passphrase.".to_string(),
            e => e.to_string(),
        })?;
        Ok(key.recovery_key())
    }

    /// Keep both sides of the plain files that sit next to an encrypted
    /// copy in `folder` (see [`lock::keep_both`]). Needs it unlocked.
    pub fn keep_both_copies(&mut self, folder: &str) -> Result<usize, String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let kept = lock::keep_both(&vault, folder).map_err(|e| e.to_string())?;
        self.rebuild_locked_index();
        self.notify(
            NoticeLevel::Info,
            format!("Kept {} plain cop{} in {folder}/, now encrypted", kept.len(), if kept.len() == 1 { "y" } else { "ies" }),
        );
        Ok(kept.len())
    }

    /// A new passphrase for `folder`, with the current one or the recovery key.
    pub fn change_folder_passphrase(&mut self, folder: &str, current_or_recovery: &str, new: &str) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let key = if let Ok(k) = FolderKey::from_recovery_key(current_or_recovery) {
            k
        } else {
            let (_, kf) = vault.key_file(folder).map_err(|e| e.to_string())?;
            kf.unwrap_key(current_or_recovery).map_err(|e| match e {
                lock::LockError::WrongPassphrase => "Wrong passphrase.".to_string(),
                e => e.to_string(),
            })?
        };
        vault.change_passphrase(folder, Some(key), new, crate::history_ops::now_ms()).map_err(|e| e.to_string())?;
        // A key remembered on this computer is the same key: it keeps working.
        self.notify(NoticeLevel::Info, format!("{folder}/ has a new passphrase"));
        Ok(())
    }

    /// Start giving `folder` a new key and passphrase in the background
    /// ([`lock::change_key`]): every file is encrypted again, and old
    /// copies of the key file and the old recovery key stop opening it. The
    /// new recovery key is then offered once ([`Self::take_recovery_key`]),
    /// also when the rest of the change stops: the key file has the new
    /// key before any file changes. `current_or_recovery` unlocks it first
    /// if needed. After a key change that didn't finish, that is the new
    /// passphrase; `new` is then the passphrase the folder keeps.
    pub fn start_changing_key(&mut self, folder: &str, current_or_recovery: &str, new: &str) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        let folder = folder.trim().trim_matches('/').to_string();
        if self.features.secure.job.is_some() {
            return Err("A folder is already being converted; wait for it to finish.".into());
        }
        if new.chars().count() < lock::MIN_PASSPHRASE {
            return Err("The passphrase must be at least 8 characters.".into());
        }
        let (_, kf) = vault.key_file(&folder).map_err(|e| unlock_error(&vault, &folder, &e))?;
        let key = match FolderKey::from_recovery_key(current_or_recovery) {
            Ok(k) => k,
            Err(_) => kf.unwrap_key(current_or_recovery).map_err(|e| match e {
                lock::LockError::WrongPassphrase => format!("Wrong passphrase.{}", rekey_unfinished(&vault, &folder).unwrap_or_default()),
                e => e.to_string(),
            })?,
        };
        vault.unlock_with_key(&folder, key).map_err(|e| unlock_error(&vault, &folder, &e))?;
        self.save_open_notes();
        let remembered = self.remembers_folder(&folder);
        let new = lock::zeroize::Zeroizing::new(new.to_string());
        let (tx, rx) = bounded(2);
        let f = folder.clone();
        std::thread::Builder::new()
            .name("quire-lock-key".into())
            .spawn(move || run_key_change(&vault, f, &new, remembered, &tx))
            .expect("the system starts a thread");
        let what = Conversion::ChangeKey { begun: false };
        self.features.secure.job = Some(Job { label: format!("Changing the key of {folder}/…"), folder, what, rx });
        Ok(())
    }

    /// Keep `folder`'s current key in this computer's keychain again (after
    /// a key change, the one kept there is the old one).
    fn remember_new_key(&mut self, folder: &str) {
        if let Some((_, key)) = self.vault.as_ref().and_then(|v| v.keys().get(folder))
            && let Err(e) = self.remember_key(folder, &key)
        {
            self.notify(NoticeLevel::Warn, e);
        }
    }

    /// Whether this computer unlocks `folder` automatically.
    pub fn remembers_folder(&self, folder: &str) -> bool {
        let (Some(root), Some(v)) = (self.vault_root(), &self.vault) else { return false };
        v.key_file(folder).ok().is_some_and(|(_, kf)| crate::secrets::store().get(&keychain_name(root, &kf.folder_id)).is_some())
    }

    /// Stop unlocking `folder` automatically on this computer.
    pub fn forget_folder_key(&self, folder: &str) -> Result<(), String> {
        let (Some(root), Some(v)) = (self.vault_root().map(std::path::Path::to_path_buf), self.vault.clone()) else {
            return Ok(());
        };
        let (_, kf) = v.key_file(folder).map_err(|e| e.to_string())?;
        crate::secrets::store().delete(&keychain_name(&root, &kf.folder_id))
    }

    fn remember_key(&self, folder: &str, key: &FolderKey) -> Result<(), String> {
        if !system_keychain() {
            return Err("This computer has no system keychain Quire can use, so the folder can't unlock automatically.".into());
        }
        let root = self.vault_root().ok_or("No vault is open")?.to_path_buf();
        let (_, kf) = self.vault.as_ref().ok_or("No vault is open")?.key_file(folder).map_err(|e| e.to_string())?;
        crate::secrets::store().set(&keychain_name(&root, &kf.folder_id), &lock::zeroize::Zeroizing::new(key.to_hex()))
    }

    /// Search the notes of unlocked locked folders (never the vault index).
    pub fn locked_search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        self.features.secure.locked_index.as_ref().map(|ix| ix.search(query, limit)).unwrap_or_default()
    }

    /// Quick open over the notes of unlocked locked folders.
    pub fn locked_quick_open(&self, query: &str, limit: usize) -> Vec<QuickOpenHit> {
        self.features.secure.locked_index.as_ref().map(|ix| ix.quick_open(query, limit)).unwrap_or_default()
    }

    /// Read the notes of every unlocked folder into the in-memory index (in
    /// the background), or drop it when nothing is unlocked.
    fn rebuild_locked_index(&mut self) {
        self.forget_overlay_links();
        let st = &mut self.features.secure;
        st.indexed.clear();
        st.locked_version += 1;
        let Some(vault) = self.vault.clone() else {
            st.locked_index = None;
            return;
        };
        let unlocked = vault.keys().unlocked();
        if unlocked.is_empty() {
            st.locked_index = None;
            st.building = None;
            return;
        }
        // 0.12: notes of a folder that locked leave the overlay now, not
        // when the new one is built (the features read it).
        if let Some(ix) = st.locked_index.as_mut() {
            let gone: Vec<NotePath> =
                ix.paths().into_iter().filter(|p| vault.locked_folder(p).is_none_or(|f| !vault.keys().is_unlocked(&f.path))).collect();
            if !gone.is_empty() {
                for p in &gone {
                    ix.remove_no_refresh(p);
                }
                ix.refresh();
            }
        }
        let (tx, rx) = bounded(1);
        std::thread::Builder::new()
            .name("quire-lock-folders".into())
            .spawn(move || {
                let mut ix = Index::new();
                for f in &unlocked {
                    for p in vault.locked_notes(f) {
                        if let Ok(text) = vault.read(&p) {
                            ix.insert_record(quire_index::NoteRecord::from_source(p, &text), text);
                        }
                    }
                }
                ix.refresh();
                let _ = tx.send(ix);
            })
            .expect("the system starts a thread");
        st.building = Some(rx);
    }

    /// Open notes of a folder just unlocked or changed elsewhere: take in
    /// what changed on disk.
    fn recheck_locked_docs(&mut self) {
        let Some(vault) = self.vault.clone() else { return };
        let open: Vec<NotePath> = self.docs.iter().filter_map(|d| d.path.clone()).filter(|p| vault.locked_folder(p).is_some()).collect();
        for p in open {
            if let Ok(content) = vault.read(&p) {
                self.absorb_external_change(&p, &content);
            }
        }
    }

    /// Turn AI off or back on for the chat model, assistant and embedder,
    /// following the vault's switch.
    pub(crate) fn apply_ai_switch(&mut self) {
        let off = ai_off(self);
        if off == self.features.secure.ai_off_applied {
            return;
        }
        self.features.secure.ai_off_applied = off;
        if off {
            let mut local = self.settings.clone();
            local.ai_provider = "local".into();
            local.embeddings = "lsa".into();
            self.assistant = crate::ai::make_assistant(&local);
            self.chat = None;
            self.chat_generation += 1;
            self.index.set_semantic_builder(None);
            self.refresh_job_chat();
            self.notify(NoticeLevel::Info, "AI is off for this vault: no agent or AI provider can use it.");
        } else {
            self.apply_ai_settings();
        }
    }

    /// Turn AI off (or back on) for this vault, in its agent policy.
    pub fn set_ai_off(&mut self, off: bool) -> Result<(), String> {
        let store = self.agent.clone().ok_or("No vault is open")?;
        let text = store.vault().storage().read(&NotePath::new(quire_agent::policy::POLICY_PATH)).ok();
        let mut doc = quire_agent::policy_edit::PolicyDoc::parse(text.as_deref())?;
        doc.set_ai_off(off);
        store.write_policy(&doc.text())?;
        self.features.secure.ai_cache.set(None);
        self.apply_ai_switch();
        Ok(())
    }
}

/// After a key change that didn't finish, what opens the folder now.
fn rekey_unfinished(vault: &Vault, folder: &str) -> Option<String> {
    vault.key_file(folder).ok().filter(|(_, kf)| kf.rotating).map(|_| {
        format!(
            " A key change of {folder}/ didn't finish: it opens only with the new passphrase chosen then, or the new recovery key shown then \
             (the old ones no longer open it). Once unlocked, change its key again to finish."
        )
    })
}

/// A key change, on its own thread: the key file first, and the new
/// recovery key sent ([`Done::KeyReady`]) before any file is encrypted
/// again, so a crash or quit during the long part never loses it.
fn run_key_change(vault: &Vault, f: String, new: &str, remembered: bool, tx: &crossbeam_channel::Sender<Done>) {
    let failed = |folder, error: VaultError, begun| Done::Failed {
        folder,
        error: error.to_string(),
        recovery: None,
        what: Conversion::ChangeKey { begun },
    };
    let done = match lock::begin_change_key(vault, &f, new, crate::history_ops::now_ms()) {
        Err(e) => failed(f, e, false),
        Ok(rk) => {
            let _ = tx.send(Done::KeyReady { folder: f.clone(), recovery: rk, remembered });
            match lock::finish_change_key(vault, &f) {
                Ok(_) => Done::Rekeyed { folder: f, recovery: None, remembered: false },
                Err(e) => failed(f, e, true),
            }
        }
    };
    let _ = tx.send(done);
}

fn unlock_error(vault: &Vault, folder: &str, e: &VaultError) -> String {
    let hint = || rekey_unfinished(vault, folder).unwrap_or_default();
    match e {
        VaultError::Locked(m) if m.contains("wrong passphrase") => format!("Wrong passphrase.{}", hint()),
        VaultError::Damaged { reason, .. } if reason.contains("recovery key") => format!("That key doesn't open this folder.{}", hint()),
        VaultError::NotFound(m) if m.contains("no encrypted file") => format!("{m}."),
        VaultError::NotFound(_) if vault.known_locked().contains(folder) => key_missing_text(folder),
        VaultError::NotFound(_) => format!("{folder}/ isn't a locked folder (its key file, {}, is missing).", lock::KEY_FILE),
        e => e.to_string(),
    }
}

// ---------------------------------------------------------------- feature hooks

pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let mut changed = false;
    ws.apply_ai_switch();
    if let Some(o) = ws.features.secure.opening.as_ref().and_then(|rx| rx.try_recv().ok()) {
        ws.features.secure.opening = None;
        opened(ws, &o);
        changed = true;
    }
    // A finished conversion, or one whose thread stopped without a word (a panic).
    let done = ws.features.secure.job.as_ref().and_then(|j| match j.rx.try_recv() {
        Ok(d) => Some(d),
        Err(crossbeam_channel::TryRecvError::Empty) => None,
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            let what = match j.what {
                Conversion::ChangeKey { .. } => {
                    let begun = ws.vault.as_ref().is_some_and(|v| v.key_file(&j.folder).is_ok_and(|(_, kf)| kf.rotating));
                    Conversion::ChangeKey { begun }
                }
                w => w,
            };
            Some(Done::Failed { folder: j.folder.clone(), error: "it stopped unexpectedly".into(), recovery: None, what })
        }
    });
    if let Some(done) = done {
        changed = true;
        if let Done::KeyReady { folder, recovery, remembered } = done {
            // The job goes on: the new recovery key is shown now.
            if remembered {
                ws.remember_new_key(&folder);
            }
            ws.features.secure.recovery = Some((folder, recovery.into()));
            ws.request_ui(UiRequest::Feature(Command::ChangeFolderPassphrase));
        } else {
            ws.features.secure.job = None;
            finished(ws, done);
        }
    }
    if let Some(ix) = ws.features.secure.building.as_ref().and_then(|rx| rx.try_recv().ok()) {
        let st = &mut ws.features.secure;
        st.building = None;
        st.locked_index = Some(ix);
        st.locked_version += 1;
        changed = true;
    }
    // Open locked notes, as edited, in the in-memory index.
    if ws.features.secure.locked_index.is_some() {
        let edited: Vec<(NotePath, u64, String)> = ws
            .docs
            .iter()
            .filter_map(|d| {
                let p = d.path.clone()?;
                let rev = d.revision();
                let v = ws.vault.as_ref()?;
                // Only notes of folders still unlocked: an edit kept open after its folder locked stays out.
                let unlocked = v.locked_folder(&p).is_some_and(|f| v.keys().is_unlocked(&f.path));
                (ws.features.secure.indexed.get(&p) != Some(&rev) && unlocked).then(|| (p, rev, d.text().to_string()))
            })
            .collect();
        for (p, rev, text) in edited {
            let st = &mut ws.features.secure;
            if let Some(ix) = st.locked_index.as_mut() {
                ix.upsert(p.clone(), &text);
                ix.refresh();
            }
            st.indexed.insert(p, rev);
            st.locked_version += 1;
        }
    }
    // Unsaved edits to a note whose folder got locked (elsewhere: another
    // device, the CLI): kept open, autosave paused until it is unlocked.
    if let Some(vault) = ws.vault.clone() {
        let stuck: Vec<usize> = ws
            .docs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.is_dirty() && !d.conflict)
            .filter(|(_, d)| {
                d.path.as_ref().and_then(|p| vault.locked_folder(p)).is_some_and(|f| !f.unlocking && !vault.keys().is_unlocked(&f.path))
            })
            .map(|(i, _)| i)
            .collect();
        for i in stuck {
            ws.docs[i].conflict = true;
            let (title, folder) = (ws.docs[i].title(), ws.docs[i].path.as_ref().map(|p| p.folder().to_string()).unwrap_or_default());
            ws.notify(
                NoticeLevel::Warn,
                format!(
                    "“{title}” is in {folder}/, which is locked now. Unlock it to save your edits; they're kept until then (Quire asks before quitting)."
                ),
            );
            changed = true;
        }
    }
    changed |= lock_when_saved(ws);
    // Idle locking.
    if let Some(at) = idle_deadline(ws)
        && now >= at
    {
        ws.features.secure.idle_retry = None;
        let locked = ws.lock_idle();
        if !locked.is_empty() {
            let mins = ws.settings.security.lock_after_idle_minutes;
            ws.notify(NoticeLevel::Info, format!("Locked {} after {mins} minutes without use", folders_words(&locked)));
            changed = true;
        }
    }
    changed
}

/// How long a folder waits for its drawings to save before staying unlocked.
const DRAWING_SAVE_WAIT: Duration = Duration::from_secs(5);

/// Lock the folders that waited for drawing editors to save, once they have;
/// one whose drawing still can't be saved stays unlocked, as for a note.
fn lock_when_saved(ws: &mut Workspace) -> bool {
    let waiting = std::mem::take(&mut ws.features.secure.lock_waiting);
    let mut changed = false;
    for (folder, since) in waiting {
        if ws.features.canvas.unsaved_in(&folder) {
            if since.elapsed() < DRAWING_SAVE_WAIT {
                ws.features.secure.lock_waiting.push((folder, since));
            } else {
                ws.notify(
                    NoticeLevel::Warn,
                    format!(
                        "{folder}/ stays unlocked: a drawing in it has edits that couldn't be saved, or changed elsewhere and waits for you to choose what to keep."
                    ),
                );
                // Idle locking tries again in a minute, not every frame.
                ws.features.secure.idle_retry = Some(Instant::now() + Duration::from_mins(1));
                changed = true;
            }
        } else if ws.lock_folder_now(&folder).is_some() {
            ws.notify(NoticeLevel::Info, format!("Locked {folder}/"));
            changed = true;
        }
    }
    changed
}

fn folders_words(f: &[String]) -> String {
    f.iter().map(|f| format!("{f}/")).collect::<Vec<_>>().join(", ")
}

/// The person's last input (keys, mouse) or typing, if any yet.
pub(crate) fn last_activity(ws: &Workspace) -> Option<Instant> {
    let typed = ws.docs.iter().filter_map(Document::last_edit).max();
    ws.features.secure.last_input.into_iter().chain(typed).max()
}

/// When unlocked folders lock for idleness: the later of the last input and
/// the last typing, plus the setting.
fn idle_deadline(ws: &Workspace) -> Option<Instant> {
    let mins = ws.settings.security.lock_after_idle_minutes;
    if mins == 0 || ws.vault.as_ref().is_none_or(|v| v.keys().is_empty()) {
        return None;
    }
    let typed = ws.docs.iter().filter_map(Document::last_edit).max();
    let last = ws.features.secure.last_input.into_iter().chain(typed).max()?;
    let at = last + Duration::from_secs(u64::from(mins) * 60);
    Some(ws.features.secure.idle_retry.map_or(at, |r| r.max(at)))
}

fn finished(ws: &mut Workspace, done: Done) {
    match done {
        Done::Locked { folder, recovery, remember, conflicts } => {
            if let Some(v) = ws.vault.clone() {
                // Out of the vault index and everything built on it.
                for p in v.locked_notes(&folder) {
                    ws.index.remove(p);
                }
                forget_derived(ws, &folder);
                if remember
                    && let Some((_, key)) = v.keys().get(&folder)
                    && let Err(e) = ws.remember_key(&folder, &key)
                {
                    ws.notify(NoticeLevel::Warn, e);
                }
            }
            if let Some(rk) = recovery {
                ws.features.secure.recovery = Some((folder.clone(), rk.into()));
                ws.request_ui(UiRequest::Feature(Command::LockFolder));
            }
            ws.rebuild_locked_index();
            ws.notify(NoticeLevel::Info, format!("{folder}/ is locked: its notes are encrypted on disk"));
            if conflicts > 0
                && let Some(left) = ws.vault.as_ref().and_then(|v| lock::plain_left(v).into_iter().find(|l| l.folder == folder))
            {
                plain_left_notice(ws, &left);
            }
        }
        Done::KeyReady { .. } => {} // handled in `tick`
        Done::Rekeyed { folder, recovery, remembered } => {
            // The key this computer kept is the old one: keep the new one instead.
            if remembered {
                ws.remember_new_key(&folder);
            }
            ws.notify(
                NoticeLevel::Info,
                format!("{folder}/ has a new key and passphrase: every file was encrypted again. Old copies of its key file and the old recovery key no longer open it."),
            );
            if let Some(rk) = recovery {
                ws.features.secure.recovery = Some((folder, rk.into()));
                ws.request_ui(UiRequest::Feature(Command::ChangeFolderPassphrase));
            }
            ws.rebuild_locked_index();
            ws.recheck_locked_docs();
        }
        Done::Unlocked { folder, id } => {
            if let Some(root) = ws.vault_root() {
                let _ = crate::secrets::store().delete(&keychain_name(root, &id));
            }
            ws.reload_index();
            ws.rebuild_locked_index();
            ws.notify(NoticeLevel::Info, format!("{folder}/ is an ordinary folder again (the encrypted files are in the trash)"));
        }
        Done::Failed { folder, error, recovery, what } => {
            ws.rebuild_locked_index();
            ws.recheck_locked_docs();
            ws.notify(NoticeLevel::Error, failed_text(&folder, &error, recovery.is_some(), what));
            if let Some(rk) = recovery {
                ws.features.secure.recovery = Some((folder, rk.into()));
                ws.request_ui(UiRequest::Feature(Command::LockFolder));
            }
        }
    }
}

/// What to tell the person when a conversion of `folder` stopped: what
/// state the folder is in, and how to finish.
fn failed_text(folder: &str, error: &str, key_shown: bool, what: Conversion) -> String {
    match what {
        Conversion::Lock => {
            let key = if key_shown {
                "Its recovery key is shown now: store it."
            } else {
                "To see its recovery key, use “Show a locked folder's recovery key…”."
            };
            format!(
                "Locking {folder}/ stopped: {error}. No plain file was removed before its encrypted copy was checked. \
                 Run “Lock a folder…” on it again to finish. {key}"
            )
        }
        Conversion::UnlockForGood => format!(
            "Decrypting {folder}/ stopped: {error}. Every note in it still opens, and it opens with its passphrase as before. \
             Run “Stop locking a folder…” on it again to finish; until then, notes in it can't be saved after Quire restarts."
        ),
        Conversion::ChangeKey { begun: false } => format!(
            "Changing the key of {folder}/ stopped before anything changed: {error}. Its passphrase and recovery key are as before."
        ),
        Conversion::ChangeKey { begun: true } => format!(
            "Changing the key of {folder}/ stopped: {error}. Every note in it still opens, but only with the new passphrase you chose \
             and the new recovery key (shown when the change began; with the new passphrase, “Show a locked folder's recovery key…” \
             shows it again); the old passphrase and recovery key no longer open it. \
             To finish, run “Change a locked folder's passphrase…” with “Also change the folder's key”, giving the new passphrase as the current one."
        ),
    }
}

/// Tell the person about plain files in a locked folder.
fn plain_left_notice(ws: &mut Workspace, left: &lock::PlainLeft) {
    let s = |n: usize| if n == 1 { "" } else { "s" };
    if !left.files.is_empty() {
        let n = left.files.len();
        ws.notify(
            NoticeLevel::Warn,
            format!(
                "{}/ is locked but holds {n} file{} that aren't encrypted. Run “Lock a folder” on it again to encrypt them.",
                left.folder,
                s(n)
            ),
        );
    }
    if !left.conflicts.is_empty() {
        let n = left.conflicts.len();
        let first = left.conflicts[0].file_name();
        ws.notify(
            NoticeLevel::Warn,
            format!(
                "{}/ has {n} plain file{} next to its encrypted version ({first}). Quire shows the encrypted one and never saves over either. \
                 Run “Lock a folder…” on it again: that finishes a lock that was interrupted. Plain files that still differ can then be kept \
                 as well (“Keep both” encrypts them under a new name).",
                left.folder,
                s(n)
            ),
        );
    }
}

/// What this device derived from a folder's notes before it was locked:
/// agents' suggestions about them (they hold text), and the caches built
/// from plain text (content-keyed, so cleared whole; they're rebuilt).
pub(crate) fn forget_derived(ws: &mut Workspace, folder: &str) {
    if let Some(agent) = ws.agent.clone() {
        let n = agent.forget_suggestions_under(folder);
        if n > 0 {
            ws.notify(NoticeLevel::Info, format!("Removed {n} agent suggestion{} about notes in {folder}/", if n == 1 { "" } else { "s" }));
        }
    }
    // The index's startup cache: deleted now by its worker, which also
    // keeps a write already scheduled from putting the text back.
    ws.index.purge_cache();
    if let Some(root) = ws.vault_root() {
        clear_caches(root);
    }
}

/// The caches under `.quire/cache/` that can hold text or vectors of notes.
pub fn clear_caches(root: &std::path::Path) {
    let cache = root.join(".quire").join("cache");
    for dir in ["served", "claims", "contradict", "pdf-text", "ocr"] {
        let _ = std::fs::remove_dir_all(cache.join(dir));
    }
    // The index's startup cache: every file but `writer.lock`, which another
    // process may hold; deleting it would let a second process become a
    // writer beside the first (`docs/formats/index-cache.md`).
    for e in std::fs::read_dir(cache.join("index")).into_iter().flatten().flatten() {
        if e.file_name() != "writer.lock" {
            let _ = std::fs::remove_file(e.path());
        }
    }
    for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let f16 = std::path::Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("f16"));
        if name == "embeddings.bin" || name == "stale.json" || (name.starts_with("vectors-") && f16) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let busy = ws.features.secure.job.is_some()
        || ws.features.secure.building.is_some()
        || ws.features.secure.opening.is_some()
        || !ws.features.secure.lock_waiting.is_empty();
    let poll = busy.then(|| Instant::now() + Duration::from_millis(100));
    poll.into_iter().chain(idle_deadline(ws)).min()
}

pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::LockNow => {
            let locked = ws.lock_all_now();
            let waiting: Vec<String> = ws.features.secure.lock_waiting.iter().map(|(f, _)| f.clone()).collect();
            let text = if !waiting.is_empty() {
                format!("Saving drawings, then locking {}", folders_words(&waiting))
            } else if locked.is_empty() {
                "No folder is unlocked".to_string()
            } else {
                format!("Locked {}", folders_words(&locked))
            };
            ws.notify(NoticeLevel::Info, text);
            None
        }
        Command::AgentConnections => {
            ws.features.secure.show_connections = true;
            Some(UiRequest::Feature(Command::AgentConnections))
        }
        _ => Some(UiRequest::Feature(cmd)),
    }
}

pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        Command::AgentConnections => ws.agent.is_some(),
        _ => ws.vault.is_some(),
    }
}

pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    let Some(vault) = ws.vault.clone() else { return };
    // This process's own conversion rewrites the folder's files one by one:
    // reading them back now (the locked index, versions) would race with
    // it, and on Windows make its replaces fail. It is all read once, when
    // the conversion ends (`finished`).
    if let Some(job) = &ws.features.secure.job
        && (lock::LockedFolder { path: job.folder.clone(), unlocking: false }).contains(ev.path().as_str())
    {
        return;
    }
    match ev {
        VaultEvent::AttachmentChanged(p) => {
            if lock::is_key_file(p.as_str()) {
                // A folder was locked or unlocked elsewhere (another device, the CLI).
                let folder = p.folder().to_string();
                let new = !vault.known_locked().contains(&folder);
                // The key was changed elsewhere (another device, the command
                // line): the one held here is the old one, and the vault
                // refuses to seal with it.
                let stale =
                    vault.keys().get(&folder).is_some_and(|(_, k)| vault.key_file(&folder).is_ok_and(|(_, kf)| kf.check_key(&k).is_err()));
                if stale {
                    key_changed_elsewhere(ws, &folder);
                }
                if vault.note_locked_folder(&folder).is_ok() && new {
                    // What this device derived from its notes while they were plain goes too.
                    forget_derived(ws, &folder);
                }
                for n in vault.locked_notes(&folder) {
                    ws.index.remove(n);
                }
                ws.rebuild_locked_index();
                return;
            }
            let Some(plain) = lock::plain_name(p) else { return };
            let Some(f) = vault.locked_folder(&plain) else { return };
            if !vault.keys().is_unlocked(&f.path) || !plain.is_markdown() {
                return;
            }
            if let Ok(content) = vault.read(&plain) {
                if !ws.docs.iter().any(|d| d.path.as_ref() == Some(&plain) && d.matches_disk(&content)) {
                    ws.history.keep(&vault, &plain, &content, crate::history_ops::now_ms(), true);
                }
                ws.absorb_external_change(&plain, &content);
                let st = &mut ws.features.secure;
                if let Some(ix) = st.locked_index.as_mut() {
                    ix.upsert(plain.clone(), &content);
                    ix.refresh();
                    st.locked_version += 1;
                }
                st.indexed.remove(&plain);
            }
        }
        VaultEvent::AttachmentRemoved(p) => {
            let folder = p.folder();
            if lock::is_key_file(p.as_str())
                && ws.features.secure.job.is_none()
                && vault.locked_folders().iter().any(|f| f.folder.path == folder && f.key_missing)
            {
                ws.notify(NoticeLevel::Warn, key_missing_text(folder));
            }
            if let Some(plain) = lock::plain_name(p) {
                let st = &mut ws.features.secure;
                if let Some(ix) = st.locked_index.as_mut() {
                    ix.remove(&plain);
                    st.locked_version += 1;
                }
            }
        }
        VaultEvent::Changed(_) | VaultEvent::Removed(_) => {}
    }
}

/// `folder`'s key was changed elsewhere while it was unlocked here. This
/// computer's remembered key is tried; otherwise the old key is forgotten
/// (it no longer seals anything: [`quire_vault::Vault`] refuses it), open
/// notes without edits close, and notes with edits stay open, kept in
/// memory until the folder is unlocked again.
fn key_changed_elsewhere(ws: &mut Workspace, folder: &str) {
    let Some(vault) = ws.vault.clone() else { return };
    if let (Some(root), Ok((_, kf))) = (ws.vault_root().map(std::path::Path::to_path_buf), vault.key_file(folder))
        && let Some(hex) = crate::secrets::store().get(&keychain_name(&root, &kf.folder_id)).map(lock::zeroize::Zeroizing::new)
        && let Some(key) = FolderKey::from_hex(&hex)
        && vault.unlock_with_key(folder, key).is_ok()
    {
        ws.notify(NoticeLevel::Info, format!("{folder}/ got a new key elsewhere; the key this computer keeps opens it."));
        return;
    }
    let clean: Vec<_> = ws.docs_in(folder).into_iter().filter(|id| ws.doc(*id).is_some_and(|d| !d.is_dirty())).collect();
    for id in clean {
        ws.close(id);
    }
    vault.lock_folder_now(folder);
    ws.forget_unlocked_text();
    ws.rebuild_locked_index();
    ws.notify(
        NoticeLevel::Warn,
        format!(
            "{folder}/ got a new key elsewhere (another device or the command line). Unlock it again with its current passphrase; \
             edits not saved yet stay open until then."
        ),
    );
}

/// A vault was opened. Nothing here lists the vault (it runs before the
/// first frame): one pass in the background checks the remembered locked
/// folders against the disk; [`tick`] then unlocks the folders this
/// computer remembers and reports plain files and missing key files.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.secure = SecureState::default();
    // The app opens its startup vault deferred: the pass then starts with
    // the rest of the deferred work, after the first frame.
    if !ws.deferred_start {
        start_pass(ws);
    }
}

/// Start the background pass over the vault just opened ([`vault_opened`]).
pub(crate) fn start_pass(ws: &mut Workspace) {
    let Some(vault) = ws.vault.clone() else { return };
    let local = ws.vault_root().and_then(local_record);
    let (tx, rx) = bounded(1);
    std::thread::Builder::new()
        .name("quire-lock-scan".into())
        .spawn(move || {
            let listing = vault.storage().list().unwrap_or_default();
            vault.remember_locked_folders_in(local.as_deref(), &listing);
            let states = vault.locked_folders_in(&listing);
            let left = if states.is_empty() { Vec::new() } else { lock::plain_left_in(&vault, &listing) };
            let _ = tx.send(Opened { states, left });
        })
        .expect("the system starts a thread");
    ws.features.secure.opening = Some(rx);
}

/// The background pass over a newly opened vault is done.
fn opened(ws: &mut Workspace, o: &Opened) {
    let Some(vault) = ws.vault.clone() else { return };
    if o.states.is_empty() {
        return;
    }
    if let Some(root) = ws.vault_root().map(std::path::Path::to_path_buf) {
        let store = crate::secrets::store();
        for f in o.states.iter().filter(|f| !f.unlocked) {
            let Ok((_, kf)) = vault.key_file(&f.folder.path) else { continue };
            if let Some(hex) = store.get(&keychain_name(&root, &kf.folder_id)).map(lock::zeroize::Zeroizing::new)
                && let Some(key) = FolderKey::from_hex(&hex)
                && vault.unlock_with_key(&f.folder.path, key).is_err()
            {
                ws.notify(NoticeLevel::Warn, format!("The key this computer kept for {}/ no longer opens it.", f.folder.path));
            }
        }
    }
    for f in o.states.iter().filter(|f| f.key_missing) {
        let unlocking = vault.storage().exists(&NotePath::join(&f.folder.path, lock::UNLOCKING_FILE));
        let text = if unlocking { unlock_unfinished_text(&f.folder.path) } else { key_missing_text(&f.folder.path) };
        ws.notify(NoticeLevel::Warn, text);
    }
    for left in &o.left {
        plain_left_notice(ws, left);
    }
    if !vault.keys().is_empty() {
        ws.rebuild_locked_index();
    }
}

pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    if let Some(j) = &ws.features.secure.job {
        out.push(StatusItem {
            owner: crate::commands::Owner::Secure,
            text: j.label.clone(),
            tooltip: "Encrypting or decrypting a folder. Quire keeps working meanwhile.".into(),
            command: None,
            warn: false,
        });
    }
    let unlocked = ws.vault.as_ref().map(|v| v.keys().unlocked()).unwrap_or_default();
    if !unlocked.is_empty() {
        out.push(StatusItem {
            owner: crate::commands::Owner::Secure,
            text: format!("🔓 {}", folders_words(&unlocked)),
            tooltip: "Unlocked locked folders. Click to lock them now.".into(),
            command: Some(Command::LockNow),
            warn: false,
        });
    }
}

mod overlay;

#[cfg(test)]
mod tests;
