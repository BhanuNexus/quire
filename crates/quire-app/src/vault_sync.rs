// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire Sync for an open vault: setup, a background worker, and status.
//!
//! The worker runs the [`quire_sync::Engine`] against the vault folder. It
//! writes notes like any other program would, so the editor's existing
//! outside-change handling (merge into unsaved edits, or reload) applies.
//!
//! A vault syncs either through a folder that Dropbox, iCloud Drive and the
//! like already sync (free, nothing to run), or through a Quire Sync server.
//!
//! Per vault, `.quire/sync/config.json` holds where it syncs and its ids, and
//! `.quire/sync/state.json` the engine's state. The vault key (and a server's
//! token) live in the OS keychain (or the private secrets file).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
/// What one sync pass did.
pub use quire_sync::Report as SyncReport;
pub use quire_sync::SpaceInfo;
/// One version of a note from the sync log, for the history dialog.
pub use quire_sync::Version as SyncVersion;
use quire_sync::folder::{self, FolderTransport};
pub use quire_sync::protocol::RevId as SyncRevId;
use quire_sync::{
    DeviceId, Engine, HttpTransport, KdfParams, LocalStore, RecoveryKey, Report, SpaceId, SyncState, Transport, VaultKey, WrappedKey,
};
use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::secrets;
use crate::workspace::{NoticeLevel, Workspace};

/// What's stored in `.quire/sync/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    /// The server's address; empty when syncing through a folder.
    #[serde(default)]
    pub server: String,
    /// The `Quire Sync` folder inside a synced folder, when syncing that way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    pub space: SpaceId,
    pub device: DeviceId,
    pub device_name: String,
    /// Also sync `.quire/shared/` (vault settings, agent policy, review
    /// logs). On a server, turning it on raises the space's minimum so 0.8
    /// clients stop (design §B.5). In a sync folder it travels in a lane of
    /// its own that 0.8 never reads, so 0.8 devices keep syncing notes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_state: bool,
    /// Selective sync: only these folders (`Work/`); empty means all.
    /// Notes outside the selection are never written here, and never sent
    /// as deleted (see [`Selection`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
    /// Selective sync: never these folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// Which folders a device syncs (`quire-cli sync --only/--exclude`), so a
/// server can hold just the folders it needs.
///
/// A note outside the selection is never written, read or sent from this
/// device: its absence here is never taken for a delete. A note moved out
/// of the selection on another device leaves this one (to its trash); one
/// moved into an unselected folder here is sent as a rename. Widening the
/// selection fetches the notes it now includes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Folder prefixes with a trailing `/`; empty: every folder.
    pub only: Vec<String>,
    pub exclude: Vec<String>,
}

impl Selection {
    /// Folder names as typed (`Work`, `Work/`, `./Work`, `Projects/2026`).
    /// Names are compared exactly, case included.
    pub fn new(only: &[String], exclude: &[String]) -> Result<Self, String> {
        let norm = |f: &String| -> Result<String, String> {
            let t = f.trim().trim_start_matches("./").trim_matches('/');
            let bad = t.is_empty()
                || t.contains('\\')
                || t.chars().any(char::is_control)
                || t.split('/').any(|seg| seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('.'));
            if bad {
                return Err(format!("“{f}” isn't a folder in the vault (a relative path, not hidden, without `..`)"));
            }
            Ok(format!("{t}/"))
        };
        let list = |v: &[String]| -> Result<Vec<String>, String> {
            let mut out = v.iter().map(norm).collect::<Result<Vec<_>, _>>()?;
            out.sort();
            out.dedup();
            Ok(out)
        };
        Ok(Self { only: list(only)?, exclude: list(exclude)? })
    }

    pub const fn is_everything(&self) -> bool {
        self.only.is_empty() && self.exclude.is_empty()
    }

    /// Whether this device syncs `path`. Vault settings (`.quire/shared/`)
    /// follow their own switch, not the selection.
    pub fn includes(&self, path: &str) -> bool {
        if path.starts_with(SHARED_PREFIX) {
            return true;
        }
        (self.only.is_empty() || self.only.iter().any(|o| path.starts_with(o.as_str())))
            && !self.exclude.iter().any(|e| path.starts_with(e.as_str()))
    }

    /// "only Work/, Projects/; not Work/Old/", or "everything".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.only.is_empty() {
            parts.push(format!("only {}", self.only.join(", ")));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("not {}", self.exclude.join(", ")));
        }
        if parts.is_empty() { "everything".into() } else { parts.join("; ") }
    }
}

/// The vault-settings folder that travels with Quire Sync when allowed.
pub const SHARED_PREFIX: &str = ".quire/shared/";
/// The largest file synced from it.
const MAX_SHARED_BYTES: u64 = 10 * 1024 * 1024;

/// Shown in the status bar and Settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SyncStatus {
    /// Sync isn't set up for this vault.
    #[default]
    Off,
    Syncing,
    /// Up to date as of this moment.
    Synced(Instant),
    /// Can't reach the server or the sync folder; will keep trying.
    Offline(String),
    /// Synced, but the sync folder has files that haven't downloaded yet.
    Waiting(String),
    /// Something needs the user (wrong token, missing key…).
    Error(String),
}

impl SyncStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Off => "Sync off".into(),
            Self::Syncing => "Syncing…".into(),
            Self::Synced(_) => "Synced".into(),
            Self::Offline(_) => "Offline".into(),
            Self::Waiting(_) => "Waiting for files".into(),
            Self::Error(_) => "Sync needs attention".into(),
        }
    }
}

fn sync_dir(root: &Path) -> PathBuf {
    root.join(".quire").join("sync")
}

/// How hard a passphrase is stretched (cheap in tests).
const KDF: KdfParams = if cfg!(test) { KdfParams::TEST } else { KdfParams::DEFAULT };

fn token_name(space: &SpaceId) -> String {
    format!("sync-token-{space}")
}

fn key_name(space: &SpaceId) -> String {
    format!("sync-key-{space}")
}

/// Where a vault syncs, as chosen in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTarget {
    /// A folder another tool keeps the same on every device (the one the
    /// user picked; Quire works in a `Quire Sync` folder inside it).
    Folder(PathBuf),
    Server {
        url: String,
        token: String,
    },
}

/// A vault that a new device can join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncVaultInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    /// "3 devices", "120 changes".
    pub detail: String,
}

/// Folders that a sync tool on this computer keeps in step, for one-click setup.
pub fn suggested_sync_folders() -> Vec<(String, PathBuf)> {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |label: &str, p: PathBuf| {
        if p.is_dir() && !out.iter().any(|(l, q)| l == label || *q == p) {
            out.push((label.to_string(), p));
        }
    };
    add("iCloud Drive", home.join("Library/Mobile Documents/com~apple~CloudDocs"));
    // macOS puts Dropbox, Google Drive and OneDrive under CloudStorage.
    if let Ok(entries) = std::fs::read_dir(home.join("Library/CloudStorage")) {
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with("Dropbox") {
                add("Dropbox", p);
            } else if name.starts_with("GoogleDrive") {
                add("Google Drive", p.join("My Drive"));
            } else if name.starts_with("OneDrive") {
                add("OneDrive", p);
            }
        }
    }
    add("Dropbox", home.join("Dropbox"));
    add("iCloud Drive", home.join("iCloudDrive"));
    add("OneDrive", home.join("OneDrive"));
    add("Google Drive", home.join("Google Drive"));
    add("Syncthing", home.join("Sync"));
    out
}

/// `p` with symlinks resolved (macOS's `/var` is `/private/var`), even when
/// its last parts don't exist yet: the deepest existing folder is resolved
/// and the rest appended, so two paths compare the same way.
fn canonical(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The `Quire Sync` folder for a picked folder, refusing ones that would
/// sync the vault into itself.
fn sync_folder_root(chosen: &Path, vault_root: &Path) -> Result<PathBuf, String> {
    if !chosen.is_dir() {
        return Err(format!("{} isn't a folder on this computer", chosen.display()));
    }
    let root = folder::sync_root(chosen);
    let (r, v) = (canonical(&root), canonical(vault_root));
    if r.starts_with(&v) {
        return Err("Pick a folder outside this notes folder".into());
    }
    if v.starts_with(&r) {
        return Err("This notes folder is inside that sync folder; pick another one".into());
    }
    Ok(root)
}

impl SyncConfig {
    /// The folders this device syncs.
    pub fn selection(&self) -> Selection {
        Selection::new(&self.only, &self.exclude).unwrap_or_else(|_| Selection {
            // Written by hand and unreadable: hold nothing rather than guess.
            only: vec!["\u{0}/".into()],
            exclude: Vec::new(),
        })
    }

    pub fn load(root: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(sync_dir(root).join("config.json")).ok()?;
        serde_json::from_str(&s).ok()
    }

    fn save(&self, root: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join("config.json"), text.as_bytes(), false).map_err(|e| e.to_string())
    }
}

/// The engine state's file: `state.json`, or `state-partial.json` while a
/// selection is set. Quire 0.9, which knows nothing of selections, never
/// reads the latter: if it opened this folder it would start afresh, which
/// can't delete anything, rather than read notes outside the selection as
/// deleted.
fn state_file(cfg: &SyncConfig) -> &'static str {
    if cfg.selection().is_everything() { "state.json" } else { "state-partial.json" }
}

fn load_state(root: &Path, cfg: &SyncConfig) -> SyncState {
    load_state_named(root, state_file(cfg))
}

fn save_state(root: &Path, cfg: &SyncConfig, state: &SyncState) -> Result<(), String> {
    save_state_named(root, state_file(cfg), state)
}

/// The engine state of a sync folder's settings lane.
const SHARED_STATE_FILE: &str = "state-shared.json";

fn load_state_named(root: &Path, name: &str) -> SyncState {
    std::fs::read_to_string(sync_dir(root).join(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_state_named(root: &Path, name: &str, state: &SyncState) -> Result<(), String> {
    let dir = sync_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(state).unwrap_or_default()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| e.to_string())
}

/// Whether a sync folder's settings lane runs on this device: turned on
/// here, or started by another device and never run (so never turned off)
/// here.
fn runs_shared_lane(root: &Path, cfg: &SyncConfig) -> bool {
    let Some(dir) = &cfg.folder else { return false };
    cfg.shared_state || (folder::lane_exists(dir, &cfg.space, folder::SHARED_LANE) && !sync_dir(root).join(SHARED_STATE_FILE).exists())
}

/// Attachments bigger than this don't sync (each change is sent whole).
pub const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Which files a [`VaultStore`] syncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Notes and attachments, and `.quire/shared/` once turned on (a server).
    Mixed,
    /// Notes and attachments only; `.quire/shared/` doesn't exist for it
    /// (a sync folder's main lane, which 0.8 devices may read).
    Notes,
    /// `.quire/shared/` only (a sync folder's settings lane).
    Shared,
}

/// The vault folder as the engine sees it: notes, and attachments (images,
/// PDFs…) as base64.
///
/// Writes check that the file is still what the engine last read, so a save
/// from the editor in between is never overwritten (the engine retries on
/// its next pass instead).
pub struct VaultStore {
    vault: Vault,
    seen: HashMap<String, Option<String>>,
    /// Attachment fingerprints by (size, modified time), so unchanged images
    /// aren't read and hashed on every pass.
    digests: std::cell::RefCell<HashMap<String, (u64, std::time::SystemTime, String)>>,
    /// Sync `.quire/shared/` too.
    shared: bool,
    lane: Lane,
    /// The revision being applied, when another device's agent wrote it:
    /// its text's hash, who, and the device's name.
    incoming: Option<(quire_vault::ContentHash, quire_sync::ActorRef, String)>,
    /// Selective sync: the folders this device holds.
    selection: Selection,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl VaultStore {
    pub fn new(vault: Vault) -> Self {
        Self {
            vault,
            seen: HashMap::new(),
            digests: std::cell::RefCell::default(),
            shared: false,
            lane: Lane::Mixed,
            incoming: None,
            selection: Selection::default(),
        }
    }

    /// Notes and attachments only: `.quire/shared/` is never listed, read,
    /// written or deleted (a sync folder's main lane).
    #[must_use]
    pub const fn notes_only(mut self) -> Self {
        self.lane = Lane::Notes;
        self.shared = false;
        self
    }

    /// `.quire/shared/` only (a sync folder's settings lane).
    #[must_use]
    pub const fn shared_only(mut self) -> Self {
        self.lane = Lane::Shared;
        self.shared = true;
        self
    }

    /// Hold only the selected folders (selective sync).
    #[must_use]
    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    /// A path this store leaves alone: another lane's, or outside the selection.
    fn outside_lane(&self, path: &str) -> bool {
        let shared = path.starts_with(SHARED_PREFIX);
        match self.lane {
            Lane::Mixed => !self.selection.includes(path),
            Lane::Notes => shared || !self.selection.includes(path),
            Lane::Shared => !shared,
        }
    }

    /// Also sync `.quire/shared/` (only once every device runs 0.9).
    #[must_use]
    pub const fn with_shared_state(mut self, on: bool) -> Self {
        self.shared = on;
        self
    }

    /// Record who wrote an incoming version as its history provenance.
    fn keep_provenance(&self, path: &str, text: &str) {
        let Some((hash, actor, from)) = &self.incoming else { return };
        if *hash != quire_vault::ContentHash::of(text) {
            // Merged with local edits: no longer only theirs.
            return;
        }
        let kind = match actor.kind.as_str() {
            "agent" => quire_agent::ActorKind::Agent,
            "job" => quire_agent::ActorKind::Job,
            "import" => quire_agent::ActorKind::Import,
            _ => return,
        };
        let mut a = quire_agent::Actor::agent(actor.id.clone(), actor.label.clone(), quire_agent::Via::Sync, from);
        a.kind = kind;
        let now = crate::history_ops::now_ms();
        let prov = quire_agent::Provenance::new(a, quire_agent::Op::Write, text, now);
        let _ = self.vault.snapshot_with(&NotePath::new(path), text, now, true, Some(&prov.to_json()));
    }

    fn size_and_time(&self, path: &str) -> Option<(u64, std::time::SystemTime)> {
        let abs = self.vault.storage().absolute(&NotePath::new(path))?;
        let m = std::fs::metadata(abs).ok()?;
        Some((m.len(), m.modified().ok()?))
    }

    fn current(&self, path: &str) -> Option<String> {
        use base64::Engine as _;
        if self.outside_lane(path) {
            return None;
        }
        if quire_sync::engine::is_attachment(path) {
            self.vault.storage().read_bytes(&NotePath::new(path)).ok().map(|b| b64().encode(b))
        } else {
            self.vault.read(&NotePath::new(path)).ok()
        }
    }

    /// Every file under `.quire/shared/`, subfolders (`srs/`) included.
    /// Symlinks and hidden folders are not followed.
    fn shared_files(&self) -> Vec<NotePath> {
        let root = SHARED_PREFIX.trim_end_matches('/');
        let Some(abs) = self.vault.storage().absolute(&NotePath::new(root)) else {
            return self.vault.storage().list_under(root).unwrap_or_default();
        };
        let mut out = Vec::new();
        let mut todo = vec![(abs, root.to_string())];
        while let Some((dir, rel)) = todo.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() && !name.starts_with('.') && todo.len() < 1_000 {
                    todo.push((e.path(), format!("{rel}/{name}")));
                } else if kind.is_file() {
                    out.push(NotePath::new(format!("{rel}/{name}")));
                }
            }
        }
        out.sort();
        out
    }

    fn unchanged_since_read(&self, path: &str) -> Result<(), String> {
        // A path the engine didn't see at the start of the pass must still not exist.
        let expected = self.seen.get(path).cloned().flatten();
        let now = if quire_sync::engine::is_attachment(path) { self.digest(path) } else { self.current(path) };
        if now == expected { Ok(()) } else { Err("it changed while syncing".into()) }
    }
}

impl LocalStore for VaultStore {
    fn list(&self) -> Vec<String> {
        let notes = if self.lane == Lane::Shared {
            Vec::new()
        } else {
            self.vault
                .storage()
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| self.selection.includes(p.as_str()))
                .filter(|p| {
                    p.is_markdown()
                        || (quire_sync::engine::is_attachment(p.as_str())
                            && self.size_and_time(p.as_str()).is_some_and(|(n, _)| n <= MAX_ATTACHMENT_BYTES))
                })
                .map(|p| p.as_str().to_string())
                .collect()
        };
        let mut out: Vec<String> = notes;
        if self.shared && self.lane != Lane::Notes {
            let shared = self.shared_files();
            out.extend(
                shared
                    .into_iter()
                    .filter(|p| p.as_str().starts_with(SHARED_PREFIX) && !p.as_str().contains("/."))
                    .filter(|p| self.vault.storage().read_bytes(p).is_ok_and(|b| b.len() as u64 <= MAX_SHARED_BYTES))
                    .map(|p| p.as_str().to_string()),
            );
        }
        out
    }

    fn actor(&self, path: &str) -> Option<quire_sync::ActorRef> {
        let p = NotePath::new(path);
        if !p.is_markdown() {
            return None;
        }
        let snap = self.vault.history(&p).into_iter().next()?;
        let prov = quire_agent::Provenance::from_json(&self.vault.read_provenance(&snap)?)?;
        // Only when the file is still exactly what that actor wrote.
        if prov.actor.is_human() || prov.hash != quire_vault::ContentHash::of(&self.current(path)?) {
            return None;
        }
        let kind = serde_json::to_value(prov.actor.kind).ok()?.as_str()?.to_string();
        Some(quire_sync::ActorRef { id: prov.actor.id, label: prov.actor.label, kind })
    }

    fn incoming(&mut self, content: &quire_sync::Content) {
        if content.path.starts_with(SHARED_PREFIX) && self.lane == Lane::Mixed {
            self.shared = true;
        }
        self.incoming = content.actor.clone().map(|a| (quire_vault::ContentHash::of(&content.text), a, content.from.clone()));
    }

    fn read(&self, path: &str) -> Option<String> {
        self.current(path)
    }

    fn in_scope(&self, path: &str) -> bool {
        !self.outside_lane(path)
    }

    fn moved_out(&self, base: &str) -> Option<(String, String)> {
        use base64::Engine as _;
        if self.selection.is_everything() || self.lane == Lane::Shared {
            return None;
        }
        // Only when a selected note went missing; the unselected files on
        // this device are few or none (a server holding some folders).
        self.vault.storage().list().ok()?.into_iter().filter(|p| !self.selection.includes(p.as_str())).find_map(|p| {
            let text = if quire_sync::engine::is_attachment(p.as_str()) {
                if self.size_and_time(p.as_str()).is_none_or(|(n, _)| n > MAX_ATTACHMENT_BYTES) {
                    return None;
                }
                b64().encode(self.vault.storage().read_bytes(&p).ok()?)
            } else if p.is_markdown() {
                self.vault.read(&p).ok()?
            } else {
                return None;
            };
            (quire_sync::engine::fingerprint(p.as_str(), &text) == base).then(|| (p.as_str().to_string(), text))
        })
    }

    fn digest(&self, path: &str) -> Option<String> {
        if !quire_sync::engine::is_attachment(path) || self.outside_lane(path) {
            return self.current(path);
        }
        let stamp = self.size_and_time(path)?;
        if let Some((n, t, d)) = self.digests.borrow().get(path)
            && (*n, *t) == stamp
        {
            return Some(d.clone());
        }
        let d = quire_sync::engine::fingerprint(path, &self.current(path)?);
        self.digests.borrow_mut().insert(path.to_string(), (stamp.0, stamp.1, d.clone()));
        Some(d)
    }

    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        use base64::Engine as _;
        if self.outside_lane(path) {
            // Not this lane's (vault settings in a sync folder's notes lane,
            // which only a 0.9 build from before the lanes could send):
            // never written, so never deleted here either.
            return Ok(());
        }
        self.unchanged_since_read(path)?;
        if path.starts_with(SHARED_PREFIX) {
            if path.contains("/.") || path.contains("..") {
                return Err("not a vault settings file".into());
            }
            // Another device shares vault settings through this space, so
            // this one must list them too, or it would pass on deletions.
            self.shared = true;
        }
        if quire_sync::engine::is_attachment(path) {
            let bytes = b64().decode(text).map_err(|e| format!("damaged attachment: {e}"))?;
            self.vault.storage().write_bytes_atomic(&NotePath::new(path), &bytes).map_err(|e| e.to_string())?;
            self.seen.insert(path.to_string(), Some(quire_sync::engine::fingerprint(path, text)));
            return Ok(());
        }
        if path.starts_with(SHARED_PREFIX) {
            self.vault.storage().write_atomic(&NotePath::new(path), text).map_err(|e| e.to_string())?;
        } else {
            self.vault.write(&NotePath::new(path), text).map_err(|e| e.to_string())?;
            self.keep_provenance(path, text);
        }
        self.seen.insert(path.to_string(), Some(text.to_string()));
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), String> {
        if self.outside_lane(path) {
            return Ok(());
        }
        self.unchanged_since_read(path)?;
        // Deleted elsewhere: goes to the vault's trash, so it can come back.
        self.vault.trash(&NotePath::new(path)).map_err(|e| e.to_string())?;
        self.seen.insert(path.to_string(), None);
        Ok(())
    }
}

/// `VaultStore` that records what it reads (the engine's view of `read` is
/// `&self`, so reads are recorded just before each pass instead).
fn snapshot_reads(store: &mut VaultStore) {
    store.seen.clear();
    for p in store.list() {
        // Attachments are checked by fingerprint (see `unchanged_since_read`).
        let cur = if quire_sync::engine::is_attachment(&p) { store.digest(&p) } else { store.current(&p) };
        store.seen.insert(p, cur);
    }
}

enum Msg {
    Nudge,
    Remote,
    Stop,
}

/// What the background sync reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    Status(SyncStatus),
    /// A pass that changed something.
    Report(Report),
}

/// How the worker reaches the other devices.
enum Link {
    Server(HttpTransport),
    Folder(Box<FolderTransport>),
}

impl Link {
    fn new(cfg: &SyncConfig, token: &str, state_dir: Option<&Path>) -> Self {
        match &cfg.folder {
            Some(root) => Self::Folder(Box::new(FolderTransport::new(root, &cfg.space, cfg.device.clone(), state_dir))),
            None => Self::Server(HttpTransport::new(&cfg.server, cfg.space.clone(), token)),
        }
    }

    fn transport(&self) -> &dyn Transport {
        match self {
            Self::Server(t) => t,
            Self::Folder(t) => &**t,
        }
    }
}

/// One device's sync of one vault: the engine, its state and the way to
/// the other devices. The app's background worker and `quire-cli sync`
/// both run passes of this.
struct Session {
    root: PathBuf,
    vault: Vault,
    cfg: SyncConfig,
    key: VaultKey,
    link: Link,
    engine: Engine,
    store: VaultStore,
    lane: Option<(FolderTransport, Engine, VaultStore)>,
}

/// The outcome of one pass, and when to try again without a nudge.
struct Pass {
    status: SyncStatus,
    report: Option<Report>,
    again: Duration,
}

impl Session {
    fn new(root: PathBuf, vault: Vault, cfg: SyncConfig, token: &str, key: VaultKey) -> Self {
        let link = Link::new(&cfg, token, Some(&sync_dir(&root)));
        let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), key.keys(), load_state(&root, &cfg));
        // In a sync folder, vault settings travel in a lane of their own,
        // which 0.8 never reads; the notes lane never touches them.
        let store = if cfg.folder.is_some() {
            VaultStore::new(vault.clone()).notes_only()
        } else {
            VaultStore::new(vault.clone()).with_shared_state(cfg.shared_state)
        }
        .with_selection(cfg.selection());
        Self { root, vault, cfg, key, link, engine, store, lane: None }
    }

    /// A server that needs a newer Quire, in words.
    fn too_old(&self) -> Option<String> {
        if let Link::Server(_) = &self.link
            && let Ok(n @ quire_sync::Negotiated::TooOld { .. }) = HttpTransport::negotiate(&self.cfg.server)
        {
            return Some(n.notice().unwrap_or_default());
        }
        None
    }

    /// One pass: pull, merge, push, for notes and (in a sync folder) the
    /// settings lane.
    fn pass(&mut self, poll: Duration) -> Pass {
        let (root, cfg) = (&self.root, &self.cfg);
        snapshot_reads(&mut self.store);
        let mut res = self.engine.sync(&mut self.store, self.link.transport(), &today());
        let _ = save_state(root, cfg, &self.engine.state);
        if self.lane.is_none()
            && runs_shared_lane(root, cfg)
            && let Some(dir) = &cfg.folder
        {
            let t = FolderTransport::lane(dir, &cfg.space, cfg.device.clone(), Some(&sync_dir(root)), folder::SHARED_LANE);
            let e = Engine::new(
                cfg.space.clone(),
                cfg.device.clone(),
                cfg.device_name.clone(),
                self.key.keys(),
                load_state_named(root, SHARED_STATE_FILE),
            );
            self.lane = Some((t, e, VaultStore::new(self.vault.clone()).shared_only()));
        }
        if let Some((t, e, st)) = self.lane.as_mut()
            && res.is_ok()
        {
            snapshot_reads(st);
            let r = e.sync(st, &*t, &today());
            let _ = save_state_named(root, SHARED_STATE_FILE, &e.state);
            res = match (res, r) {
                (Ok(mut a), Ok(b)) => {
                    a.pulled += b.pulled;
                    a.pushed += b.pushed;
                    a.merged.extend(b.merged);
                    a.conflict_copies.extend(b.conflict_copies);
                    a.kept_over_delete.extend(b.kept_over_delete);
                    Ok(a)
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            };
            if !cfg.shared_state {
                self.store.shared = true;
            }
        }
        // Another device turned on sharing vault settings: remember it here.
        if self.store.shared && !self.cfg.shared_state {
            self.cfg.shared_state = true;
            let _ = self.cfg.save(&self.root);
        }
        let (status, report, again) = match res {
            Ok(report) => {
                let status = match &self.link {
                    Link::Folder(dir) => match (dir.problem(), dir.waiting()) {
                        (Some(p), _) => SyncStatus::Error(p),
                        (None, Some(w)) => SyncStatus::Waiting(w),
                        (None, None) => SyncStatus::Synced(Instant::now()),
                    },
                    Link::Server(_) => SyncStatus::Synced(Instant::now()),
                };
                (status, report.changed_anything().then_some(report), poll)
            }
            Err(quire_sync::SyncError::Transport(quire_sync::TransportError::Offline(e))) => {
                (SyncStatus::Offline(e), None, Duration::from_secs(30).min(poll))
            }
            // A note changed mid-pass, or other devices are busy: try again soon.
            Err(quire_sync::SyncError::Local { .. } | quire_sync::SyncError::Busy) => (SyncStatus::Syncing, None, Duration::from_secs(2)),
            Err(e) => (SyncStatus::Error(e.to_string()), None, Duration::from_mins(5).min(poll.max(Duration::from_secs(30)))),
        };
        let _ = LastPass::record(&self.root, &status, report.as_ref());
        Pass { status, report, again }
    }
}

/// What the last pass on this device did, in `.quire/sync/last.json`, for
/// `quire-cli sync status` (another process may be running the sync).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPass {
    pub format: u32,
    /// Unix ms.
    pub at: i64,
    /// `synced`, `waiting`, `offline`, `syncing` or `error`.
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default)]
    pub pulled: usize,
    #[serde(default)]
    pub pushed: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl LastPass {
    fn record(root: &Path, status: &SyncStatus, report: Option<&Report>) -> Result<(), String> {
        let (state, detail) = match status {
            SyncStatus::Synced(_) => ("synced", String::new()),
            SyncStatus::Waiting(w) => ("waiting", w.clone()),
            SyncStatus::Offline(e) => ("offline", e.clone()),
            SyncStatus::Error(e) => ("error", e.clone()),
            SyncStatus::Syncing | SyncStatus::Off => ("syncing", String::new()),
        };
        let last = Self {
            format: 1,
            at: crate::history_ops::now_ms(),
            state: state.into(),
            detail,
            pulled: report.map_or(0, |r| r.pulled),
            pushed: report.map_or(0, |r| r.pushed),
            conflicts: report.map(|r| r.conflict_copies.clone()).unwrap_or_default(),
            other: serde_json::Map::new(),
        };
        let text = serde_json::to_vec_pretty(&last).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join("last.json"), &text, false).map_err(|e| e.to_string())
    }

    pub fn load(root: &Path) -> Option<Self> {
        let s = std::fs::read(sync_dir(root).join("last.json")).ok()?;
        serde_json::from_slice::<Self>(&s).ok().filter(|l| l.format <= 1)
    }
}

/// The background sync thread for one vault.
pub struct SyncWorker {
    tx: Sender<Msg>,
    rx: Receiver<SyncEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for SyncWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncWorker")
    }
}

impl Drop for SyncWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Stop);
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// How often the app syncs without a reason to (a save, another device's change).
pub const APP_POLL: Duration = Duration::from_mins(2);

impl SyncWorker {
    fn start(root: PathBuf, vault: Vault, cfg: SyncConfig, token: String, key: VaultKey, poll: Duration) -> Self {
        let (tx, msgs) = crossbeam_channel::unbounded();
        let (events, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let known = Arc::new(AtomicU64::new(0));

        // Watch thread: tells the worker when another device pushed.
        {
            let (tx, stop, known) = (tx.clone(), Arc::clone(&stop), Arc::clone(&known));
            match Link::new(&cfg, &token, Some(&sync_dir(&root))) {
                // A server answers a long poll as soon as something changes.
                Link::Server(http) => std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match http.wait(known.load(Ordering::Relaxed), 25) {
                            Ok(latest) if latest > known.load(Ordering::Relaxed) => {
                                let _ = tx.send(Msg::Remote);
                                std::thread::sleep(Duration::from_millis(300));
                            }
                            Ok(_) => {}
                            Err(_) => std::thread::sleep(Duration::from_secs(15)),
                        }
                    }
                }),
                // A folder is checked every couple of seconds (sizes and
                // times only, so it's cheap).
                Link::Folder(dir) => {
                    let lane =
                        cfg.folder.as_ref().map(|r| FolderTransport::lane(r, &cfg.space, cfg.device.clone(), None, folder::SHARED_LANE));
                    std::thread::spawn(move || {
                        let print = |d: &FolderTransport| d.fingerprint() ^ lane.as_ref().map_or(0, |l| l.fingerprint().rotate_left(1));
                        let mut last = print(&dir);
                        // The worker's first pass may have read the folder before
                        // `last` was taken; a write in between would never show up
                        // as a change. One more pass after taking it closes that gap.
                        let _ = tx.send(Msg::Remote);
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_secs(2));
                            let now = print(&dir);
                            if now != last {
                                last = now;
                                let _ = tx.send(Msg::Remote);
                            }
                        }
                    })
                }
            };
        }

        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let stop = worker_stop;
            let mut session = Session::new(root, vault, cfg, &token, key);
            // A server that needs a newer Quire says so plainly instead of failing each pass.
            if let Some(notice) = session.too_old() {
                let _ = events.send(SyncEvent::Status(SyncStatus::Error(notice)));
                return;
            }
            let mut wait = Duration::ZERO;
            loop {
                match msgs.recv_timeout(wait) {
                    Ok(Msg::Stop) => return,
                    Ok(Msg::Nudge) => {
                        // Let a burst of saves finish first.
                        std::thread::sleep(Duration::from_millis(800));
                        while let Ok(m) = msgs.try_recv() {
                            if matches!(m, Msg::Stop) {
                                return;
                            }
                        }
                    }
                    Ok(Msg::Remote) | Err(_) => {}
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let _ = events.send(SyncEvent::Status(SyncStatus::Syncing));
                let pass = session.pass(poll);
                known.store(session.engine.state.cursor, Ordering::Relaxed);
                if let Some(r) = pass.report {
                    let _ = events.send(SyncEvent::Report(r));
                }
                wait = pass.again;
                let _ = events.send(SyncEvent::Status(pass.status));
            }
        });
        Self { tx, rx, stop, thread: Some(thread) }
    }

    /// Sync the vault at `root` in the background, as set up by
    /// [`sync_join_at`] or [`sync_create_at`], without the app: passes run
    /// when another device changes something, after [`Self::nudge`], and
    /// every `poll` otherwise.
    pub fn start_headless(root: &Path, poll: Duration) -> Result<Self, String> {
        let (vault, cfg, token, key) = open_parts(root)?;
        Ok(Self::start(root.to_path_buf(), vault, cfg, token, key, poll))
    }

    /// Sync soon (local files changed).
    pub fn nudge(&self) {
        let _ = self.tx.send(Msg::Nudge);
    }

    /// What happened since the last call.
    pub fn events(&self) -> Vec<SyncEvent> {
        self.rx.try_iter().collect()
    }

    /// Wait up to `timeout` for the next event.
    pub fn next_event(&self, timeout: Duration) -> Option<SyncEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Stop after the pass under way, if any, and wait for it: every file
    /// it writes is written whole (atomically) before this returns.
    pub fn stop_and_wait(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The vault, configuration and secrets that syncing `root` needs.
fn open_parts(root: &Path) -> Result<(Vault, SyncConfig, String, VaultKey), String> {
    let cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault. Run `quire-cli sync join` first.")?;
    let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
    let store = secrets::store();
    // A folder needs no token.
    let token = if cfg.folder.is_some() { Some(String::new()) } else { store.get(&token_name(&cfg.space)) };
    let key = store.get(&key_name(&cfg.space)).and_then(|k| VaultKey::from_recovery_key(&k).ok());
    match (token, key) {
        (Some(t), Some(k)) => Ok((vault, cfg, t, k)),
        _ => Err(format!("This device is missing the sync key ({}). Join the vault again.", secrets::store().kind())),
    }
}

/// One sync pass of the vault at `root`, in this thread (`quire-cli sync once`).
pub fn sync_once_at(root: &Path) -> Result<Report, String> {
    let (vault, cfg, token, key) = open_parts(root)?;
    let mut session = Session::new(root.to_path_buf(), vault, cfg, &token, key);
    if let Some(notice) = session.too_old() {
        return Err(notice);
    }
    let pass = session.pass(APP_POLL);
    match pass.status {
        SyncStatus::Synced(_) | SyncStatus::Waiting(_) => Ok(pass.report.unwrap_or_default()),
        SyncStatus::Syncing => Err("a note changed while syncing, or other devices were busy; run it again".into()),
        SyncStatus::Offline(e) | SyncStatus::Error(e) => Err(e),
        SyncStatus::Off => Err("sync is off".into()),
    }
}

/// Sync's part of the workspace.
#[derive(Debug, Default)]
pub struct SyncHandle {
    worker: Option<SyncWorker>,
    pub status: SyncStatus,
    pub config: Option<SyncConfig>,
}

/// A default device name, e.g. "Asha's laptop" isn't knowable, so the host name.
pub fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| format!("{} device", std::env::consts::OS))
}

/// Get ready for vault settings and review history to travel with sync.
///
/// In a sync folder they go in a lane of their own, which Quire 0.8 never
/// reads: a 0.8 device that already joined keeps syncing notes and can
/// never see those files, so it can't pass on their deletion (it would:
/// its `list` skips hidden paths). On a server, only Quire 0.9 or later may
/// connect from now on.
fn raise_min_reader(cfg: &SyncConfig) -> Result<(), String> {
    if let Some(dir) = &cfg.folder {
        return folder::open_lane(dir, &cfg.space, folder::SHARED_LANE).map_err(|e| e.to_string());
    }
    let negotiated = HttpTransport::negotiate(&cfg.server).map_err(|e| e.to_string())?;
    if !matches!(negotiated, quire_sync::Negotiated::Version(v) if v >= 2) {
        return Err("Update your sync server first: it needs Quire Sync Server 0.9 or later.".into());
    }
    let token = secrets::store().get(&token_name(&cfg.space)).ok_or("This device is missing the sync token")?;
    HttpTransport::new(&cfg.server, cfg.space.clone(), &token).set_min_protocol(2).map_err(|e| e.to_string())
}

fn sync_config_for(root: &Path, target: &SyncTarget, space: SpaceId, device_name: &str, sel: &Selection) -> Result<SyncConfig, String> {
    let (server, folder) = match target {
        SyncTarget::Folder(chosen) => (String::new(), Some(sync_folder_root(chosen, root)?)),
        SyncTarget::Server { url, .. } => (url.trim().to_string(), None),
    };
    Ok(SyncConfig {
        server,
        folder,
        space,
        device: DeviceId::random().map_err(|e| e.to_string())?,
        device_name: device_name.trim().to_string(),
        shared_state: false,
        only: sel.only.clone(),
        exclude: sel.exclude.clone(),
    })
}

fn store_sync_secrets(cfg: &SyncConfig, target: &SyncTarget, key: &VaultKey) -> Result<(), String> {
    let store = secrets::store();
    if let SyncTarget::Server { token, .. } = target {
        store.set(&token_name(&cfg.space), token.trim())?;
    }
    store.set(&key_name(&cfg.space), &key.recovery_key().to_string())
}

/// Start syncing the vault folder `root` as a new sync vault (the app's
/// Settings → Sync, `quire-cli sync join --create`). Returns the recovery
/// key, to show once.
pub fn sync_create_at(
    root: &Path,
    target: &SyncTarget,
    passphrase: &str,
    device_name: &str,
    selection: &Selection,
) -> Result<RecoveryKey, String> {
    if passphrase.chars().count() < 8 {
        return Err("Use a passphrase of at least 8 characters".into());
    }
    let key = VaultKey::generate().map_err(|e| e.to_string())?;
    let cfg = sync_config_for(root, target, SpaceId::random().map_err(|e| e.to_string())?, device_name, selection)?;
    let wrapped = WrappedKey::wrap(&key, passphrase, KDF).map_err(|e| e.to_string())?;
    match (target, &cfg.folder) {
        (_, Some(dir)) => folder::create_space(dir, &cfg.space, &wrapped).map_err(|e| e.to_string())?,
        (SyncTarget::Server { url, token }, None) => {
            HttpTransport::new(url.trim(), cfg.space.clone(), token.trim()).create_space(&wrapped).map_err(|e| e.to_string())?;
        }
        (SyncTarget::Folder(_), None) => return Err("No sync folder".into()),
    }
    store_sync_secrets(&cfg, target, &key)?;
    cfg.save(root)?;
    let _ = save_state(root, &cfg, &SyncState::default());
    Ok(key.recovery_key())
}

/// Join an existing sync vault from the vault folder `root` with its
/// passphrase (or recovery key). The key and a server's token go to the
/// keychain or the private secrets file, never into the vault.
pub fn sync_join_at(
    root: &Path,
    target: &SyncTarget,
    space: &SpaceId,
    secret: &str,
    device_name: &str,
    selection: &Selection,
) -> Result<(), String> {
    let cfg = sync_config_for(root, target, space.clone(), device_name, selection)?;
    let key = if let Ok(k) = VaultKey::from_recovery_key(secret) {
        k
    } else {
        let wrapped = match (target, &cfg.folder) {
            (_, Some(dir)) => folder::wrapped_key(dir, space),
            (SyncTarget::Server { url, token }, None) => HttpTransport::new(url.trim(), space.clone(), token.trim()).wrapped_key(),
            (SyncTarget::Folder(_), None) => return Err("No sync folder".into()),
        };
        wrapped.map_err(|e| e.to_string())?.unwrap(secret).map_err(|e| e.to_string())?
    };
    store_sync_secrets(&cfg, target, &key)?;
    cfg.save(root)?;
    let _ = save_state(root, &cfg, &SyncState::default());
    let _ = std::fs::remove_file(sync_dir(root).join("folder-seq"));
    Ok(())
}

/// Change which folders the vault at `root` syncs. The engine's state
/// moves with it, so notes now included are fetched and notes now left out
/// are never sent as deletes.
pub fn sync_set_selection_at(root: &Path, selection: &Selection) -> Result<(), String> {
    let mut cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault")?;
    let from = state_file(&cfg);
    cfg.only.clone_from(&selection.only);
    cfg.exclude.clone_from(&selection.exclude);
    let to = state_file(&cfg);
    if from != to {
        let dir = sync_dir(root);
        if dir.join(from).exists() {
            std::fs::rename(dir.join(from), dir.join(to)).map_err(|e| e.to_string())?;
        }
    }
    cfg.save(root)
}

/// What `quire-cli sync status` shows about the vault at `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    pub config: SyncConfig,
    /// The keychain or secrets file has this vault's key (and token).
    pub has_secrets: bool,
    /// Notes and attachments the engine tracks, and how many of them are
    /// outside the selection.
    pub tracked: usize,
    pub outside: usize,
    pub last: Option<LastPass>,
}

/// Sync's setup and last result for the vault at `root`; `None` when sync
/// isn't set up there. Reads no secret values.
pub fn sync_summary_at(root: &Path) -> Option<SyncSummary> {
    let config = SyncConfig::load(root)?;
    let state = load_state(root, &config);
    let store = secrets::store();
    let has_secrets =
        store.get(&key_name(&config.space)).is_some() && (config.folder.is_some() || store.get(&token_name(&config.space)).is_some());
    let live: Vec<&quire_sync::Tracked> = state.files.values().filter(|t| !t.deleted).collect();
    Some(SyncSummary {
        tracked: live.len(),
        outside: live.iter().filter(|t| t.outside).count(),
        has_secrets,
        last: LastPass::load(root),
        config,
    })
}

/// The vaults a new device can join, in a sync folder or on a server.
pub fn sync_list(target: &SyncTarget) -> Result<Vec<SyncVaultInfo>, String> {
    Workspace::sync_list(target)
}

impl Workspace {
    fn sync_parts(&self) -> Result<(PathBuf, Vault), String> {
        let root = self.vault_root().ok_or("Open a vault folder first")?.to_path_buf();
        let vault = self.vault().ok_or("Open a vault folder first")?.clone();
        Ok((root, vault))
    }

    /// The vaults a new device can join, in a sync folder or on a server.
    pub fn sync_list(target: &SyncTarget) -> Result<Vec<SyncVaultInfo>, String> {
        match target {
            SyncTarget::Folder(chosen) => Ok(folder::list_spaces(&folder::sync_root(chosen))
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|s| SyncVaultInfo {
                    space: s.space,
                    created: s.created,
                    detail: if s.devices == 1 { "1 device".into() } else { format!("{} devices", s.devices) },
                })
                .collect()),
            SyncTarget::Server { url, token } => Ok(HttpTransport::list_spaces(url.trim(), token.trim())
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|s: SpaceInfo| SyncVaultInfo { space: s.space, created: s.created, detail: format!("{} changes", s.revisions) })
                .collect()),
        }
    }

    /// Start syncing this vault as a new sync vault. Returns the recovery
    /// key, to show once.
    pub fn sync_create(&mut self, target: &SyncTarget, passphrase: &str, device_name: &str) -> Result<RecoveryKey, String> {
        let (root, _) = self.sync_parts()?;
        let rk = sync_create_at(&root, target, passphrase, device_name, &Selection::default())?;
        self.start_sync();
        Ok(rk)
    }

    /// Join an existing sync vault with its passphrase (or recovery key).
    pub fn sync_join(&mut self, target: &SyncTarget, space: &SpaceId, secret: &str, device_name: &str) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        sync_join_at(&root, target, space, secret, device_name, &Selection::default())?;
        self.start_sync();
        Ok(())
    }

    /// Start (or restart) the worker if this vault is set up for sync.
    pub fn start_sync(&mut self) {
        self.sync.worker = None;
        let Ok((root, vault)) = self.sync_parts() else {
            self.sync = SyncHandle::default();
            return;
        };
        let Some(cfg) = SyncConfig::load(&root) else {
            self.sync = SyncHandle::default();
            return;
        };
        let store = secrets::store();
        // A folder needs no token.
        let token = if cfg.folder.is_some() { Some(String::new()) } else { store.get(&token_name(&cfg.space)) };
        let key = store.get(&key_name(&cfg.space)).and_then(|k| VaultKey::from_recovery_key(&k).ok());
        let (Some(token), Some(key)) = (token, key) else {
            self.sync.config = Some(cfg);
            self.sync.status = SyncStatus::Error("This device is missing the sync key. Join the vault again in Settings → Sync.".into());
            return;
        };
        self.sync.worker = Some(SyncWorker::start(root, vault, cfg.clone(), token, key, APP_POLL));
        self.sync.config = Some(cfg);
        self.sync.status = SyncStatus::Syncing;
    }

    /// "Also sync vault settings and review history" (design §B.5). A 0.8
    /// device would see the settings files as deleted and pass that on, so
    /// turning it on first keeps them away from 0.8: a 0.9 server answers
    /// older clients `426`; a sync folder carries them in a separate lane.
    pub fn sync_share_vault_settings(&mut self, on: bool) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        let cfg = SyncConfig::load(&root).ok_or("Sync isn't set up for this vault")?;
        if on && !cfg.shared_state {
            raise_min_reader(&cfg)?;
        }
        self.sync_set_shared_state(on)
    }

    /// [`Self::sync_share_vault_settings`] for the UI thread: turning it on
    /// first raises the oldest version that may read the vault (a request
    /// to the sync server) in the background. Give the result to
    /// [`Self::sync_set_shared_state`] when it is `Ok`.
    pub fn sync_share_vault_settings_in_background(&self, on: bool) -> Result<Receiver<Result<(), String>>, String> {
        let (root, _) = self.sync_parts()?;
        let cfg = SyncConfig::load(&root).ok_or("Sync isn't set up for this vault")?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        if on && !cfg.shared_state {
            std::thread::spawn(move || {
                let _ = tx.send(raise_min_reader(&cfg));
            });
        } else {
            let _ = tx.send(Ok(()));
        }
        Ok(rx)
    }

    /// Record whether vault settings and review history sync, and restart sync.
    pub fn sync_set_shared_state(&mut self, on: bool) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        let mut cfg = SyncConfig::load(&root).ok_or("Sync isn't set up for this vault")?;
        cfg.shared_state = on;
        cfg.save(&root)?;
        self.start_sync();
        Ok(())
    }

    /// Stop syncing on this device. Notes stay as they are; the server copy is untouched.
    pub fn stop_sync(&mut self) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        if let Some(cfg) = self.sync.config.take() {
            let store = secrets::store();
            let _ = store.delete(&token_name(&cfg.space));
            let _ = store.delete(&key_name(&cfg.space));
        }
        self.sync = SyncHandle::default();
        let dir = sync_dir(&root);
        let _ = std::fs::remove_file(dir.join("config.json"));
        let _ = std::fs::remove_file(dir.join("state.json"));
        let _ = std::fs::remove_file(dir.join("state-partial.json"));
        let _ = std::fs::remove_file(dir.join("last.json"));
        let _ = std::fs::remove_file(dir.join("folder-seq"));
        Ok(())
    }

    /// Every version of `path` the sync server or folder holds, from every
    /// device, worked out in the background. `None` when sync is off.
    pub fn fetch_sync_history(&self, path: &NotePath) -> Option<Receiver<Result<Vec<SyncVersion>, String>>> {
        let cfg = self.sync.config.clone()?;
        let (root, _) = self.sync_parts().ok()?;
        let store = secrets::store();
        let key = store.get(&key_name(&cfg.space)).and_then(|k| VaultKey::from_recovery_key(&k).ok())?;
        let token = store.get(&token_name(&cfg.space)).unwrap_or_default();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = path.as_str().to_string();
        std::thread::spawn(move || {
            let link = Link::new(&cfg, &token, None);
            let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), key.keys(), load_state(&root, &cfg));
            let _ = tx.send(engine.history(link.transport(), &path).map_err(|e| e.to_string()));
        });
        Some(rx)
    }

    /// Ask the worker to sync soon (after a save, or "Sync now").
    pub fn sync_nudge(&self) {
        if let Some(w) = &self.sync.worker {
            w.nudge();
        }
    }

    /// The recovery key for this vault, to show again in Settings.
    pub fn sync_recovery_key(&self) -> Option<String> {
        let cfg = self.sync.config.as_ref()?;
        secrets::store().get(&key_name(&cfg.space))
    }

    /// Collect worker events. Returns true when something changed.
    pub(crate) fn poll_sync(&mut self) -> bool {
        let Some(w) = &self.sync.worker else { return false };
        let events = w.events();
        if events.is_empty() {
            return false;
        }
        for e in events {
            match e {
                SyncEvent::Status(s) => self.sync.status = s,
                SyncEvent::Report(r) => {
                    for p in &r.conflict_copies {
                        self.notify(
                            NoticeLevel::Warn,
                            format!("Two devices changed the same lines. The other version was saved as “{p}” (Resolve sync conflicts…)"),
                        );
                    }
                    for p in &r.kept_over_delete {
                        self.notify(NoticeLevel::Info, format!("Kept “{p}”: it was edited on one device and deleted on another"));
                    }
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_store_never_overwrites_a_newer_save() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        vault.write(&NotePath::new("A.md"), "one").unwrap();
        let mut store = VaultStore::new(vault.clone());
        snapshot_reads(&mut store);
        // The editor saves after the snapshot…
        vault.write(&NotePath::new("A.md"), "edited in the app").unwrap();
        // …so the engine's write is refused.
        assert!(store.write("A.md", "from sync").is_err());
        assert_eq!(vault.read(&NotePath::new("A.md")).unwrap(), "edited in the app");
        snapshot_reads(&mut store);
        store.write("A.md", "from sync").unwrap();
        assert_eq!(store.list(), vec!["A.md".to_string()]);
        // Deletes go to the trash.
        snapshot_reads(&mut store);
        store.remove("A.md").unwrap();
        assert!(store.read("A.md").is_none());
    }

    fn wait_for(ws: &mut [&mut Workspace], what: &str, mut done: impl FnMut(&[&mut Workspace]) -> bool) {
        let start = Instant::now();
        while !done(ws) {
            if start.elapsed() > Duration::from_secs(30) {
                // Say what each side sees, so a failure on another OS can be diagnosed.
                let report: Vec<String> = ws
                    .iter()
                    .map(|w| {
                        let files = w.vault().and_then(|v| v.storage().list().ok()).unwrap_or_default();
                        format!("{:?} files={files:?}", w.sync.status)
                    })
                    .collect();
                panic!("timed out waiting for {what}: {report:#?}");
            }
            for w in ws.iter_mut() {
                w.tick(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn two_vaults_sync_through_a_shared_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Dropbox"));
        for d in [&va, &vb, &shared] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\nShip it.\n![[logo.png]]\n").unwrap();
        std::fs::write(va.join("logo.png"), [0x89u8, b'P', b'N', b'G', 7, 7]).unwrap();
        let target = SyncTarget::Folder(shared.clone());

        let mut a = Workspace::new(crate::Settings::default());
        a.open_vault(&va).unwrap();
        a.sync_create(&target, "correct horse battery", "laptop").unwrap();
        assert_eq!(a.sync.config.as_ref().and_then(|c| c.folder.clone()), Some(shared.join("Quire Sync")));

        let mut b = Workspace::new(crate::Settings::default());
        b.open_vault(&vb).unwrap();
        let found = Workspace::sync_list(&target).unwrap();
        assert_eq!(found.len(), 1);
        assert!(b.sync_join(&target, &found[0].space, "wrong passphrase", "phone").is_err());
        b.sync_join(&target, &found[0].space, "correct horse battery", "phone").unwrap();
        wait_for(&mut [&mut a, &mut b], "the note and its image to reach B", |_| {
            std::fs::read_to_string(vb.join("Plan.md")).is_ok_and(|t| t.contains("Ship it."))
                && std::fs::read(vb.join("logo.png")).is_ok_and(|b| b == [0x89u8, b'P', b'N', b'G', 7, 7])
        });

        // An edit on B travels back to A; B's worker notices the save, A's notices the folder change.
        std::fs::write(vb.join("Plan.md"), "# Plan\nShip it.\nFrom the phone.\n").unwrap();
        b.sync_nudge();
        wait_for(&mut [&mut a, &mut b], "the edit to reach A", |_| {
            std::fs::read_to_string(va.join("Plan.md")).is_ok_and(|t| t.contains("From the phone."))
        });
        wait_for(&mut [&mut a, &mut b], "both to settle", |ws| ws.iter().all(|w| matches!(w.sync.status, SyncStatus::Synced(_))));
        // Every device's versions come back for the history dialog.
        let rx = a.fetch_sync_history(&NotePath::new("Plan.md")).unwrap();
        let versions = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert!(versions.iter().any(|v| v.device == "laptop" && !v.text.contains("phone")), "{versions:?}");
        assert!(versions.iter().any(|v| v.device == "phone" && v.text.contains("From the phone.")), "{versions:?}");
        // The sync folder holds only encrypted logs.
        let space_dir = shared.join("Quire Sync").join(&found[0].space.0);
        let logs: String = walk(&space_dir).iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect();
        assert!(!logs.contains("Plan") && !logs.contains("Ship it"));
        b.stop_sync().unwrap();
        assert_eq!(b.sync.status, SyncStatus::Off);
    }

    /// Drain a headless worker's events while ticking the app.
    fn wait_both(a: &mut Workspace, b: &SyncWorker, what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(40), "timed out waiting for {what}: app {:?}", a.sync.status);
            a.tick(Instant::now());
            let _ = b.events();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn files(root: &Path) -> Vec<String> {
        let v = Vault::open_local(root).unwrap();
        let mut out: Vec<String> = v.storage().list().unwrap().into_iter().map(|p| p.as_str().to_string()).collect();
        out.sort();
        out
    }

    #[test]
    fn a_headless_device_and_the_app_converge() {
        let dir = tempfile::tempdir().unwrap();
        let (va, vb, shared) = (dir.path().join("Desktop"), dir.path().join("Server"), dir.path().join("Syncthing"));
        for d in [&va, &vb, &shared] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\none\ntwo\nthree\n").unwrap();
        let target = SyncTarget::Folder(shared);
        let mut a = Workspace::new(crate::Settings::default());
        a.open_vault(&va).unwrap();
        a.sync_create(&target, "correct horse battery", "laptop").unwrap();

        // The headless side joins the way `quire-cli sync join` does.
        let space = sync_list(&target).unwrap().remove(0).space;
        assert!(sync_join_at(&vb, &target, &space, "wrong passphrase", "server", &Selection::default()).is_err());
        sync_join_at(&vb, &target, &space, "correct horse battery", "server", &Selection::default()).unwrap();
        let b = SyncWorker::start_headless(&vb, Duration::from_secs(5)).unwrap();
        wait_both(&mut a, &b, "the note to reach the server", || {
            std::fs::read_to_string(vb.join("Plan.md")).is_ok_and(|t| t.contains("three"))
        });

        // Each side's edits reach the other.
        std::fs::write(vb.join("From server.md"), "written headless\n").unwrap();
        b.nudge();
        wait_both(&mut a, &b, "the server's note to reach the app", || va.join("From server.md").exists());
        // Both change the same note before seeing each other: merged, or
        // both versions kept; never a lost line.
        std::fs::write(va.join("Plan.md"), "# Plan\nONE (laptop)\ntwo\nthree\n").unwrap();
        std::fs::write(vb.join("Plan.md"), "# Plan\none\ntwo\nthree\nfour (server)\n").unwrap();
        a.sync_nudge();
        b.nudge();
        let all_text = |root: &Path| -> String { files(root).iter().filter_map(|p| std::fs::read_to_string(root.join(p)).ok()).collect() };
        wait_both(&mut a, &b, "both edits on both sides, and the same files", || {
            let (ta, tb) = (all_text(&va), all_text(&vb));
            files(&va) == files(&vb) && [&ta, &tb].iter().all(|t| t.contains("ONE (laptop)") && t.contains("four (server)"))
        });
        let summary = sync_summary_at(&vb).unwrap();
        assert_eq!((summary.config.device_name.as_str(), summary.has_secrets), ("server", true));
        assert!(summary.last.is_some_and(|l| l.state == "synced" || l.state == "syncing"));
        b.stop_and_wait();
        // A one-off pass works too, and finds nothing left to do.
        let r = sync_once_at(&vb).unwrap();
        assert_eq!(r.pushed, 0);
    }

    #[test]
    fn a_selective_device_never_deletes_what_it_doesnt_hold() {
        let dir = tempfile::tempdir().unwrap();
        let (va, vb, shared) = (dir.path().join("Desktop"), dir.path().join("Server"), dir.path().join("Dropbox"));
        for d in [&va, &vb, &shared, &va.join("Work"), &va.join("Journal"), &vb.join("Journal")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Work/Plan.md"), "plan\n").unwrap();
        std::fs::write(va.join("Journal/Today.md"), "private day\n").unwrap();
        std::fs::write(va.join("Inbox.md"), "inbox\n").unwrap();
        // Something already on the server, outside its selection.
        std::fs::write(vb.join("Journal/Stale.md"), "never synced\n").unwrap();
        let target = SyncTarget::Folder(shared);
        let mut a = Workspace::new(crate::Settings::default());
        a.open_vault(&va).unwrap();
        a.sync_create(&target, "correct horse battery", "laptop").unwrap();
        let space = sync_list(&target).unwrap().remove(0).space;
        let only = Selection::new(&["Work".to_string()], &[]).unwrap();
        sync_join_at(&vb, &target, &space, "correct horse battery", "server", &only).unwrap();
        let start = Instant::now();
        while !vb.join("Work/Plan.md").exists() {
            assert!(start.elapsed() < Duration::from_secs(30), "the selected note never arrived");
            a.tick(Instant::now());
            let _ = sync_once_at(&vb);
            std::thread::sleep(Duration::from_millis(100));
        }
        for _ in 0..5 {
            a.tick(Instant::now());
            sync_once_at(&vb).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(files(&vb), ["Journal/Stale.md", "Work/Plan.md"], "only the selection is written here");
        assert!(!dir.path().join("Desktop/Journal/Stale.md").exists(), "unselected files here are never sent");
        a.sync_nudge();
        std::fs::write(va.join("Journal/Today.md"), "private day, later\n").unwrap();
        for _ in 0..5 {
            a.tick(Instant::now());
            sync_once_at(&vb).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(files(&va), ["Inbox.md", "Journal/Today.md", "Work/Plan.md"], "nothing deleted on the app's side");
        assert_eq!(std::fs::read_to_string(va.join("Journal/Today.md")).unwrap(), "private day, later\n");
        let s = sync_summary_at(&vb).unwrap();
        assert_eq!((s.tracked, s.outside), (3, 2));
        assert!(sync_dir(&vb).join("state-partial.json").exists() && !sync_dir(&vb).join("state.json").exists());
        // Widening fetches the rest, still deleting nothing.
        sync_set_selection_at(&vb, &Selection::default()).unwrap();
        sync_once_at(&vb).unwrap();
        assert!(vb.join("Journal/Today.md").exists() && vb.join("Inbox.md").exists());
        assert!(sync_dir(&vb).join("state.json").exists());
        assert_eq!(std::fs::read_to_string(va.join("Work/Plan.md")).unwrap(), "plan\n");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
        out
    }

    #[test]
    fn attachments_sync_as_bytes_and_big_ones_stay_local() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("attachments")).unwrap();
        let png = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 255];
        std::fs::write(dir.path().join("attachments/a.png"), png).unwrap();
        std::fs::write(dir.path().join("huge.mov"), vec![0u8; usize::try_from(MAX_ATTACHMENT_BYTES).unwrap() + 1]).unwrap();
        vault.write(&NotePath::new("N.md"), "![[a.png]]").unwrap();
        let mut store = VaultStore::new(vault);
        let mut listed = store.list();
        listed.sort();
        assert_eq!(listed, ["N.md", "attachments/a.png"], "too-big files stay out");
        let text = store.read("attachments/a.png").unwrap();
        snapshot_reads(&mut store);
        store.write("attachments/b.png", &text).unwrap();
        assert_eq!(std::fs::read(dir.path().join("attachments/b.png")).unwrap(), png);
        assert_eq!(store.digest("attachments/b.png"), Some(quire_sync::engine::fingerprint("attachments/b.png", &text)));
        // A change underneath is noticed for attachments too.
        snapshot_reads(&mut store);
        std::fs::write(dir.path().join("attachments/a.png"), [1u8, 2, 3]).unwrap();
        assert!(store.write("attachments/a.png", &text).is_err());
    }

    #[test]
    fn a_sync_folder_must_be_outside_the_vault_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("Notes");
        let dropbox = dir.path().join("Dropbox");
        std::fs::create_dir_all(vault.join("inner")).unwrap();
        std::fs::create_dir_all(&dropbox).unwrap();
        assert_eq!(sync_folder_root(&dropbox, &vault).unwrap(), dropbox.join("Quire Sync"));
        assert!(sync_folder_root(&vault.join("inner"), &vault).is_err());
        assert!(sync_folder_root(dir.path(), &dir.path().join("Quire Sync").join("v")).is_err());
        assert!(sync_folder_root(&dir.path().join("missing"), &vault).is_err());
        // The same folder reached through a symlink (macOS's /var → /private/var).
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            assert!(sync_folder_root(dir.path(), &link.join("Quire Sync").join("v")).is_err());
            assert!(sync_folder_root(&link.join("Notes").join("inner"), &vault).is_err());
        }
    }

    #[test]
    fn a_0_5_config_still_loads_and_folder_configs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let old = r#"{"server":"https://s.example.com","space":"ab","device":"cd","device_name":"laptop"}"#;
        std::fs::create_dir_all(sync_dir(dir.path())).unwrap();
        std::fs::write(sync_dir(dir.path()).join("config.json"), old).unwrap();
        let cfg = SyncConfig::load(dir.path()).unwrap();
        assert_eq!((cfg.server.as_str(), cfg.folder.as_ref()), ("https://s.example.com", None));
        let cfg = SyncConfig { server: String::new(), folder: Some(dir.path().join("Dropbox/Quire Sync")), ..cfg };
        cfg.save(dir.path()).unwrap();
        assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
    }

    #[test]
    fn config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SyncConfig {
            server: "https://sync.example.com".into(),
            folder: None,
            space: SpaceId::random().unwrap(),
            device: DeviceId::random().unwrap(),
            device_name: "laptop".into(),
            shared_state: false,
            only: Vec::new(),
            exclude: Vec::new(),
        };
        cfg.save(dir.path()).unwrap();
        assert!(!std::fs::read_to_string(sync_dir(dir.path()).join("config.json")).unwrap().contains("shared_state"), "unchanged for 0.8");
        assert_eq!(SyncConfig::load(dir.path()).as_ref(), Some(&cfg));
        assert_eq!(load_state(dir.path(), &cfg), SyncState::default());
    }

    #[test]
    fn vault_settings_sync_only_when_turned_on() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        vault.write(&NotePath::new("A.md"), "a").unwrap();
        vault.storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
        vault.storage().write_atomic(&NotePath::new(".quire/agent/ledger/x.jsonl"), "{}\n").unwrap();
        let off = VaultStore::new(vault.clone());
        assert_eq!(off.list(), vec!["A.md".to_string()], "never .quire/ by default");
        let mut on = VaultStore::new(vault.clone()).with_shared_state(true);
        assert_eq!(on.list(), vec!["A.md".to_string(), ".quire/shared/vault.toml".to_string()], "only the shared folder");
        vault.storage().write_atomic(&NotePath::new(".quire/shared/srs/d1.jsonl"), "{}\n").unwrap();
        assert!(on.list().contains(&".quire/shared/srs/d1.jsonl".to_string()), "review logs in subfolders too");
        snapshot_reads(&mut on);
        on.write(".quire/shared/agent-policy.toml", &b64().encode("format = 1\n")).unwrap();
        assert_eq!(vault.storage().read(&NotePath::new(".quire/shared/agent-policy.toml")).unwrap(), "format = 1\n");
        assert!(on.write(".quire/shared/../x.toml", "").is_err());
        // A device that hasn't turned it on joins in as soon as another device's settings arrive.
        let mut joined = VaultStore::new(vault);
        snapshot_reads(&mut joined);
        joined.incoming(&quire_sync::Content {
            path: ".quire/shared/srs/d1.jsonl".into(),
            deleted: false,
            text: String::new(),
            from: "x".into(),
            at: 0,
            actor: None,
        });
        assert!(joined.list().contains(&".quire/shared/vault.toml".to_string()));
    }

    #[test]
    fn agent_attribution_travels_with_versions() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        let store = quire_agent::AgentStore::open(
            &vault,
            quire_agent::DeviceInfo { id: "d1".into(), name: "laptop".into() },
            Arc::new(quire_agent::SystemClock),
        );
        let agent = quire_agent::Actor::agent("mcp:claude-code", "Claude Code", quire_agent::Via::McpStdio, "d1");
        store
            .submit(
                &agent,
                quire_agent::ChangeSet::one(quire_agent::Change::Create { path: NotePath::new("wiki/A.md"), text: "by agent\n".into() }),
            )
            .unwrap();
        vault.write(&NotePath::new("Mine.md"), "by me\n").unwrap();
        let mut vs = VaultStore::new(vault.clone());
        let a = vs.actor("wiki/A.md").unwrap();
        assert_eq!((a.id.as_str(), a.label.as_str(), a.kind.as_str()), ("mcp:claude-code", "Claude Code", "agent"));
        assert_eq!(vs.actor("Mine.md"), None, "the person's own edits carry nothing");
        vault.write(&NotePath::new("wiki/A.md"), "edited by me since\n").unwrap();
        assert_eq!(vs.actor("wiki/A.md"), None, "only while the file is what the agent wrote");
        // Receiving: the version is kept with the sender's actor.
        let content = quire_sync::Content {
            path: "wiki/B.md".into(),
            deleted: false,
            text: "from the other laptop\n".into(),
            from: "desktop".into(),
            at: 1,
            actor: Some(a),
        };
        snapshot_reads(&mut vs);
        vs.incoming(&content);
        vs.write("wiki/B.md", &content.text).unwrap();
        let snap = vault.history(&NotePath::new("wiki/B.md")).remove(0);
        let prov = quire_agent::Provenance::from_json(&vault.read_provenance(&snap).unwrap()).unwrap();
        assert_eq!(
            (prov.actor.id.as_str(), prov.actor.via, prov.actor.device.as_str()),
            ("mcp:claude-code", quire_agent::Via::Sync, "desktop")
        );
    }

    #[test]
    fn sharing_vault_settings_in_a_folder_opens_a_lane_and_keeps_0_8_in() {
        let vault_dir = tempfile::tempdir().unwrap();
        let sync_dir_root = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(crate::Settings::default());
        ws.open_vault(vault_dir.path()).unwrap();
        let root = folder::sync_root(sync_dir_root.path());
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        folder::create_space(&root, &space, &WrappedKey::wrap(&key, "passphrase", KDF).unwrap()).unwrap();
        let cfg = SyncConfig {
            server: String::new(),
            folder: Some(root.clone()),
            space: space.clone(),
            device: DeviceId::random().unwrap(),
            device_name: "x".into(),
            shared_state: false,
            only: Vec::new(),
            exclude: Vec::new(),
        };
        cfg.save(vault_dir.path()).unwrap();
        ws.sync_share_vault_settings(true).unwrap();
        assert_eq!(folder::min_reader(&root, &space).unwrap(), 1, "0.8 devices keep syncing notes");
        assert!(folder::lane_exists(&root, &space, folder::SHARED_LANE));
        assert!(SyncConfig::load(vault_dir.path()).unwrap().shared_state);
        ws.sync_share_vault_settings(false).unwrap();
        assert!(!SyncConfig::load(vault_dir.path()).unwrap().shared_state);
    }

    /// Quire 0.8.0's `LocalStore`, as shipped (`git show
    /// v0.8.0:crates/quire-app/src/vault_sync.rs`): notes and attachments
    /// are listed, and any path is written. With `FolderTransport::new`
    /// (its scan of `devices/` is unchanged since 0.8.0) and the engine
    /// (unchanged but for two hooks whose defaults do what 0.8 did), this
    /// syncs as a 0.8 device that joined the vault earlier does: it never
    /// reads `space.json` again.
    struct Store08 {
        vault: Vault,
    }

    impl LocalStore for Store08 {
        fn list(&self) -> Vec<String> {
            self.vault
                .storage()
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.is_markdown() || quire_sync::engine::is_attachment(p.as_str()))
                .map(|p| p.as_str().to_string())
                .collect()
        }

        fn read(&self, path: &str) -> Option<String> {
            use base64::Engine as _;
            if quire_sync::engine::is_attachment(path) {
                self.vault.storage().read_bytes(&NotePath::new(path)).ok().map(|b| b64().encode(b))
            } else {
                self.vault.read(&NotePath::new(path)).ok()
            }
        }

        fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
            use base64::Engine as _;
            if quire_sync::engine::is_attachment(path) {
                let bytes = b64().decode(text).map_err(|e| e.to_string())?;
                return self.vault.storage().write_bytes_atomic(&NotePath::new(path), &bytes).map_err(|e| e.to_string());
            }
            self.vault.write(&NotePath::new(path), text).map_err(|e| e.to_string())
        }

        fn remove(&mut self, path: &str) -> Result<(), String> {
            self.vault.trash(&NotePath::new(path)).map_err(|e| e.to_string())
        }
    }

    /// Three devices in one sync folder: `a` and `c` run 0.9, `b` runs 0.8.
    struct Fleet {
        _dir: tempfile::TempDir,
        root: PathBuf,
        space: SpaceId,
        keys: quire_sync::Keys,
        vaults: [Vault; 3],
        devices: [DeviceId; 3],
        engines: Vec<Engine>,
        lanes: Vec<Engine>,
    }

    impl Fleet {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = folder::sync_root(&dir.path().join("Dropbox"));
            let space = SpaceId::random().unwrap();
            let key = VaultKey::generate().unwrap();
            folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
            let vaults = ["A", "B", "C"].map(|n| {
                std::fs::create_dir_all(dir.path().join(n)).unwrap();
                Vault::open_local(dir.path().join(n)).unwrap()
            });
            let devices = [(); 3].map(|()| DeviceId::random().unwrap());
            let engine = |i: usize| Engine::new(space.clone(), devices[i].clone(), ["a", "b", "c"][i], key.keys(), SyncState::default());
            let engines = (0..3).map(engine).collect();
            let lanes = (0..3).map(engine).collect();
            Self { _dir: dir, root, space, keys: key.keys(), vaults, devices, engines, lanes }
        }

        /// One pass of device `i`: 0.9 (both lanes) or 0.8.
        fn pass(&mut self, i: usize) {
            let main = FolderTransport::new(&self.root, &self.space, self.devices[i].clone(), None);
            if i == 1 {
                let mut st = Store08 { vault: self.vaults[1].clone() };
                self.engines[1].sync(&mut st, &main, "2026-11-03").unwrap();
                return;
            }
            let mut notes = VaultStore::new(self.vaults[i].clone()).notes_only();
            snapshot_reads(&mut notes);
            self.engines[i].sync(&mut notes, &main, "2026-11-03").unwrap();
            let lane = FolderTransport::lane(&self.root, &self.space, self.devices[i].clone(), None, folder::SHARED_LANE);
            let mut shared = VaultStore::new(self.vaults[i].clone()).shared_only();
            snapshot_reads(&mut shared);
            self.lanes[i].sync(&mut shared, &lane, "2026-11-03").unwrap();
        }

        fn rounds(&mut self, n: usize) {
            for _ in 0..n {
                for i in 0..3 {
                    self.pass(i);
                }
            }
        }

        fn shared(&self, i: usize, p: &str) -> Option<String> {
            self.vaults[i].storage().read(&NotePath::new(format!("{SHARED_PREFIX}{p}"))).ok()
        }
    }

    #[test]
    fn review_a_0_8_device_never_deletes_shared_vault_settings() {
        let mut f = Fleet::new();
        let a = &f.vaults[0];
        a.write(&NotePath::new("Plan.md"), "# Plan\n").unwrap();
        a.storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
        a.storage().write_atomic(&NotePath::new(".quire/shared/agent-policy.toml"), "format = 1\n").unwrap();
        a.storage().write_atomic(&NotePath::new(".quire/shared/srs/a.jsonl"), "{\"v\":1}\n").unwrap();
        f.rounds(3);
        // Notes reach every device, 0.8 included; settings reach only 0.9.
        for i in 0..3 {
            assert_eq!(f.vaults[i].read(&NotePath::new("Plan.md")).unwrap(), "# Plan\n", "device {i}");
        }
        assert_eq!(f.shared(2, "vault.toml").as_deref(), Some("format = 1\n"));
        assert_eq!(f.shared(2, "srs/a.jsonl").as_deref(), Some("{\"v\":1}\n"));
        assert!(f.vaults[1].storage().list_under(".quire/shared").unwrap_or_default().is_empty(), "0.8 never gets them");
        assert!(f.engines[1].state.files.values().all(|t| !t.path.starts_with(".quire")), "0.8 never tracks them");
        // More changes and passes: nothing is ever deleted.
        f.vaults[2].storage().write_atomic(&NotePath::new(".quire/shared/srs/c.jsonl"), "{\"v\":1}\n").unwrap();
        f.vaults[1].write(&NotePath::new("From 0.8.md"), "old device\n").unwrap();
        f.rounds(3);
        for i in [0, 2] {
            for p in ["vault.toml", "agent-policy.toml", "srs/a.jsonl", "srs/c.jsonl"] {
                assert!(f.shared(i, p).is_some(), "device {i} lost {p}");
            }
            assert_eq!(f.vaults[i].read(&NotePath::new("From 0.8.md")).unwrap(), "old device\n");
        }
        // No revision a 0.8 device can read names a settings file.
        let main = FolderTransport::new(&f.root, &f.space, DeviceId::random().unwrap(), None);
        for r in main.pull(0).unwrap() {
            let c = quire_sync::Content::open(&f.keys, &f.space, &r).unwrap();
            assert!(!c.path.starts_with(".quire"), "{}", c.path);
        }
        // A settings file that reached the notes lane anyway (a build from
        // before the lanes) is neither written nor deleted by 0.9 there.
        let mut notes = VaultStore::new(f.vaults[0].clone()).notes_only();
        snapshot_reads(&mut notes);
        notes.remove(".quire/shared/vault.toml").unwrap();
        notes.write(".quire/shared/vault.toml", "x").unwrap();
        assert_eq!(f.shared(0, "vault.toml").as_deref(), Some("format = 1\n"));
    }

    /// The simulation above catches the review's finding: with settings in
    /// the notes lane (the design before the lanes), the 0.8 device writes
    /// them, misses them on its next pass, and deletes them everywhere.
    #[test]
    fn the_0_8_simulation_sees_the_delete_the_lanes_prevent() {
        let mut f = Fleet::new();
        f.vaults[0].storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
        let mixed = |f: &mut Fleet| {
            let main = FolderTransport::new(&f.root, &f.space, f.devices[0].clone(), None);
            let mut st = VaultStore::new(f.vaults[0].clone()).with_shared_state(true);
            snapshot_reads(&mut st);
            f.engines[0].sync(&mut st, &main, "2026-11-03").unwrap();
        };
        for _ in 0..3 {
            mixed(&mut f);
            f.pass(1);
        }
        mixed(&mut f);
        assert!(f.shared(0, "vault.toml").is_none(), "deleted through the 0.8 device");
    }
}
