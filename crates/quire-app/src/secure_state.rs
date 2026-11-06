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
    Locked { folder: String, recovery: Option<String>, remember: bool },
    Unlocked { folder: String, id: String },
    Failed { folder: String, error: String },
}

struct Job {
    label: String,
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
    recovery: Option<(String, String)>,
    /// AI-off as last applied to the chat model and embedder.
    ai_off_applied: bool,
    /// The locked folders, read at most once a second (sidebar rows).
    folders: std::cell::RefCell<Option<(Instant, Vec<String>)>>,
    /// "Agent connections…": the Agents tab opens on its Connections view.
    pub show_connections: bool,
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

/// The keychain entry of a folder's key ("unlock automatically").
fn keychain_name(root: &std::path::Path, id: &str) -> String {
    format!("locked-folder:{}:{id}", quire_agent::approval::vault_key(root))
}

/// Whether secrets go to a real OS keychain (not the private-file fallback,
/// which would leave the key next to the vault it opens).
pub fn system_keychain() -> bool {
    crate::secrets::store().kind() == "system keychain"
}

/// Whether AI is off for the open vault (agent policy `ai = "off"`).
pub fn ai_off(ws: &Workspace) -> bool {
    ws.agent.as_ref().is_some_and(|a| a.policy_state().active().iter().any(|p| p.ai_off()))
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

    /// The locked folder `path` is in, from a list of locked folders read
    /// at most once a second (cheap enough for every row of the sidebar).
    pub fn locked_folder_cached(&self, path: &NotePath) -> Option<String> {
        let mut c = self.features.secure.folders.borrow_mut();
        if c.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(1)) {
            let list = self.vault.as_ref().map(|v| v.locked_folders().into_iter().map(|f| f.folder.path).collect()).unwrap_or_default();
            *c = Some((Instant::now(), list));
        }
        let list = &c.as_ref()?.1;
        list.iter().find(|f| path.as_str().strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('/'))).cloned()
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
        let key = vault.unlock(folder, passphrase).map_err(|e| unlock_error(folder, &e))?;
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
        if let Some(p) = new_passphrase {
            vault.change_passphrase(folder, Some(key.clone()), p, crate::history_ops::now_ms()).map_err(|e| e.to_string())?;
        }
        vault.unlock_with_key(folder, key).map_err(|e| unlock_error(folder, &e))?;
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
        if let Some(v) = &self.vault
            && lock::plain_left(v).iter().any(|p| p.folder == folder)
        {
            self.notify(
                NoticeLevel::Warn,
                format!("{folder}/ has files that aren't encrypted yet. Run “Lock a folder” on it again to finish."),
            );
        }
    }

    /// Lock `folder` now: its open notes are saved and closed, their text
    /// and search entries dropped, and the key forgotten. Returns how many
    /// notes were closed.
    pub fn lock_folder_now(&mut self, folder: &str) -> usize {
        let open: Vec<_> = self
            .docs
            .iter()
            .filter(|d| {
                d.path.as_ref().is_some_and(|p| lock::LockedFolder { path: folder.to_string(), unlocking: false }.contains(p.as_str()))
            })
            .map(|d| d.id)
            .collect();
        for id in &open {
            self.save(*id);
        }
        // A note that couldn't be saved stays open with its edits (never lost).
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
        self.rebuild_locked_index();
        closed
    }

    /// Lock every unlocked folder now.
    pub fn lock_all_now(&mut self) -> Vec<String> {
        let unlocked = self.vault.as_ref().map(|v| v.keys().unlocked()).unwrap_or_default();
        for f in &unlocked {
            self.lock_folder_now(f);
        }
        unlocked
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
        std::thread::spawn(move || {
            let done = match lock::lock_folder(&vault, &f, Some(&pass), KDF, crate::history_ops::now_ms()) {
                Ok(r) => Done::Locked { folder: f, recovery: r.recovery_key, remember },
                Err(e) => Done::Failed { folder: f, error: e.to_string() },
            };
            let _ = tx.send(done);
        });
        self.features.secure.job = Some(Job { label: format!("Locking {folder}/…"), rx });
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
            vault.unlock(&folder, p).map_err(|e| unlock_error(&folder, &e))?;
        }
        self.save_open_notes();
        let id = vault.key_file(&folder).map(|(_, kf)| kf.folder_id).unwrap_or_default();
        let (tx, rx) = bounded(1);
        let f = folder.clone();
        std::thread::spawn(move || {
            let done = match lock::unlock_folder_permanently(&vault, &f, None) {
                Ok(_) => Done::Unlocked { folder: f, id },
                Err(e) => Done::Failed { folder: f, error: e.to_string() },
            };
            let _ = tx.send(done);
        });
        self.features.secure.job = Some(Job { label: format!("Decrypting {folder}/…"), rx });
        Ok(())
    }

    /// The conversion under way, in words.
    pub fn lock_job(&self) -> Option<&str> {
        self.features.secure.job.as_ref().map(|j| j.label.as_str())
    }

    /// The recovery key of the folder just locked, once: (folder, key).
    pub fn take_recovery_key(&mut self) -> Option<(String, String)> {
        self.features.secure.recovery.take()
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
        crate::secrets::store().set(&keychain_name(&root, &kf.folder_id), &key.to_hex())
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
        let st = &mut self.features.secure;
        *st.folders.borrow_mut() = None;
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
        let (tx, rx) = bounded(1);
        std::thread::spawn(move || {
            let mut ix = Index::new();
            for f in &unlocked {
                for p in vault.locked_notes(f) {
                    if let Ok(text) = vault.read(&p) {
                        ix.upsert(p, &text);
                    }
                }
            }
            ix.refresh();
            let _ = tx.send(ix);
        });
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
        self.apply_ai_switch();
        Ok(())
    }
}

fn unlock_error(folder: &str, e: &VaultError) -> String {
    match e {
        VaultError::Locked(m) if m.contains("wrong passphrase") => "Wrong passphrase.".into(),
        VaultError::Damaged { reason, .. } if reason.contains("recovery key") => "That key doesn't open this folder.".into(),
        VaultError::NotFound(_) => format!("{folder}/ isn't a locked folder (its key file, {}, is missing).", lock::KEY_FILE),
        e => e.to_string(),
    }
}

// ---------------------------------------------------------------- feature hooks

pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let mut changed = false;
    ws.apply_ai_switch();
    // A finished conversion.
    if let Some(done) = ws.features.secure.job.as_ref().and_then(|j| j.rx.try_recv().ok()) {
        ws.features.secure.job = None;
        changed = true;
        finished(ws, done);
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
                (ws.features.secure.indexed.get(&p) != Some(&rev) && ws.vault.as_ref()?.locked_folder(&p).is_some())
                    .then(|| (p, rev, d.text().to_string()))
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
                format!("“{title}” is in {folder}/, which is locked now. Unlock it to save your edits; they're kept until then."),
            );
            changed = true;
        }
    }
    // Idle locking.
    if let Some(at) = idle_deadline(ws)
        && now >= at
    {
        let locked = ws.lock_all_now();
        if !locked.is_empty() {
            let mins = ws.settings.security.lock_after_idle_minutes;
            ws.notify(NoticeLevel::Info, format!("Locked {} after {mins} minutes without use", folders_words(&locked)));
            changed = true;
        }
    }
    changed
}

fn folders_words(f: &[String]) -> String {
    f.iter().map(|f| format!("{f}/")).collect::<Vec<_>>().join(", ")
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
    Some(last + Duration::from_secs(u64::from(mins) * 60))
}

fn finished(ws: &mut Workspace, done: Done) {
    match done {
        Done::Locked { folder, recovery, remember } => {
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
                ws.features.secure.recovery = Some((folder.clone(), rk));
                ws.request_ui(UiRequest::Feature(Command::LockFolder));
            }
            ws.rebuild_locked_index();
            ws.notify(NoticeLevel::Info, format!("{folder}/ is locked: its notes are encrypted on disk"));
        }
        Done::Unlocked { folder, id } => {
            if let Some(root) = ws.vault_root() {
                let _ = crate::secrets::store().delete(&keychain_name(root, &id));
            }
            ws.reload_index();
            ws.rebuild_locked_index();
            ws.notify(NoticeLevel::Info, format!("{folder}/ is an ordinary folder again (the encrypted files are in the trash)"));
        }
        Done::Failed { folder, error } => {
            ws.rebuild_locked_index();
            ws.notify(NoticeLevel::Error, format!("Converting {folder}/ stopped: {error}. Nothing was lost; run it again to finish."));
        }
    }
}

/// What this device derived from a folder's notes before it was locked:
/// agents' suggestions about them (they hold text), and the caches built
/// from plain text (content-keyed, so cleared whole; they're rebuilt).
fn forget_derived(ws: &mut Workspace, folder: &str) {
    if let Some(agent) = ws.agent.clone() {
        let n = agent.forget_suggestions_under(folder);
        if n > 0 {
            ws.notify(NoticeLevel::Info, format!("Removed {n} agent suggestion{} about notes in {folder}/", if n == 1 { "" } else { "s" }));
        }
    }
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
    for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let f16 = std::path::Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("f16"));
        if name == "embeddings.bin" || name == "stale.json" || (name.starts_with("vectors-") && f16) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let busy = ws.features.secure.job.is_some() || ws.features.secure.building.is_some();
    let poll = busy.then(|| Instant::now() + Duration::from_millis(100));
    poll.into_iter().chain(idle_deadline(ws)).min()
}

pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd {
        Command::LockNow => {
            let locked = ws.lock_all_now();
            let text = if locked.is_empty() { "No folder is unlocked".to_string() } else { format!("Locked {}", folders_words(&locked)) };
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
    match ev {
        VaultEvent::AttachmentChanged(p) => {
            if lock::is_key_file(p.as_str()) {
                // A folder was locked or unlocked elsewhere (another device, the CLI).
                let folder = p.folder().to_string();
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

/// A vault was opened: folders this computer remembers unlock, and plain
/// files left in locked folders are reported.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.secure = SecureState::default();
    let Some(vault) = ws.vault.clone() else { return };
    if let Some(root) = ws.vault_root().map(std::path::Path::to_path_buf) {
        let store = crate::secrets::store();
        for f in vault.locked_folders() {
            let Ok((_, kf)) = vault.key_file(&f.folder.path) else { continue };
            if let Some(hex) = store.get(&keychain_name(&root, &kf.folder_id))
                && let Some(key) = FolderKey::from_hex(&hex)
                && vault.unlock_with_key(&f.folder.path, key).is_err()
            {
                ws.notify(NoticeLevel::Warn, format!("The key this computer kept for {}/ no longer opens it.", f.folder.path));
            }
        }
    }
    for left in lock::plain_left(&vault) {
        ws.notify(
            NoticeLevel::Warn,
            format!(
                "{}/ is locked but holds {} file{} that aren't encrypted. Run “Lock a folder” on it again to encrypt them.",
                left.folder,
                left.files.len(),
                if left.files.len() == 1 { "" } else { "s" }
            ),
        );
    }
    ws.rebuild_locked_index();
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

#[cfg(test)]
mod tests;
