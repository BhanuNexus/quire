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
pub use quire_sync::SpaceInfo;
/// One version of a note from the sync log, for the history dialog.
pub use quire_sync::Version as SyncVersion;
use quire_sync::folder::{self, FolderTransport};
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
}

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
    pub fn load(root: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(sync_dir(root).join("config.json")).ok()?;
        serde_json::from_str(&s).ok()
    }

    fn save(&self, root: &Path) -> Result<(), String> {
        let dir = sync_dir(root);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("config.json"), serde_json::to_string_pretty(self).unwrap_or_default()).map_err(|e| e.to_string())
    }
}

fn load_state(root: &Path) -> SyncState {
    std::fs::read_to_string(sync_dir(root).join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_state(root: &Path, state: &SyncState) -> Result<(), String> {
    let dir = sync_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state).unwrap_or_default()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("state.json")).map_err(|e| e.to_string())
}

/// Attachments bigger than this don't sync (each change is sent whole).
pub const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

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
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl VaultStore {
    pub fn new(vault: Vault) -> Self {
        Self { vault, seen: HashMap::new(), digests: std::cell::RefCell::default() }
    }

    fn size_and_time(&self, path: &str) -> Option<(u64, std::time::SystemTime)> {
        let abs = self.vault.storage().absolute(&NotePath::new(path))?;
        let m = std::fs::metadata(abs).ok()?;
        Some((m.len(), m.modified().ok()?))
    }

    fn current(&self, path: &str) -> Option<String> {
        use base64::Engine as _;
        if quire_sync::engine::is_attachment(path) {
            self.vault.storage().read_bytes(&NotePath::new(path)).ok().map(|b| b64().encode(b))
        } else {
            self.vault.read(&NotePath::new(path)).ok()
        }
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
        self.vault
            .storage()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| {
                p.is_markdown()
                    || (quire_sync::engine::is_attachment(p.as_str())
                        && self.size_and_time(p.as_str()).is_some_and(|(n, _)| n <= MAX_ATTACHMENT_BYTES))
            })
            .map(|p| p.as_str().to_string())
            .collect()
    }

    fn read(&self, path: &str) -> Option<String> {
        self.current(path)
    }

    fn digest(&self, path: &str) -> Option<String> {
        if !quire_sync::engine::is_attachment(path) {
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
        self.unchanged_since_read(path)?;
        if quire_sync::engine::is_attachment(path) {
            let bytes = b64().decode(text).map_err(|e| format!("damaged attachment: {e}"))?;
            self.vault.storage().write_bytes_atomic(&NotePath::new(path), &bytes).map_err(|e| e.to_string())?;
            self.seen.insert(path.to_string(), Some(quire_sync::engine::fingerprint(path, text)));
            return Ok(());
        }
        self.vault.write(&NotePath::new(path), text).map_err(|e| e.to_string())?;
        self.seen.insert(path.to_string(), Some(text.to_string()));
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), String> {
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

enum Event {
    Status(SyncStatus),
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

/// The background sync thread for one vault.
pub struct SyncWorker {
    tx: Sender<Msg>,
    rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
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

impl SyncWorker {
    fn start(root: PathBuf, vault: Vault, cfg: SyncConfig, token: String, key: VaultKey) -> Self {
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
                Link::Folder(dir) => std::thread::spawn(move || {
                    let mut last = dir.fingerprint();
                    // The worker's first pass may have read the folder before
                    // `last` was taken; a write in between would never show up
                    // as a change. One more pass after taking it closes that gap.
                    let _ = tx.send(Msg::Remote);
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                        let now = dir.fingerprint();
                        if now != last {
                            last = now;
                            let _ = tx.send(Msg::Remote);
                        }
                    }
                }),
            };
        }

        let worker_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let stop = worker_stop;
            let link = Link::new(&cfg, &token, Some(&sync_dir(&root)));
            let mut engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), key.keys(), load_state(&root));
            let mut store = VaultStore::new(vault);
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
                let _ = events.send(Event::Status(SyncStatus::Syncing));
                snapshot_reads(&mut store);
                let res = engine.sync(&mut store, link.transport(), &today());
                let _ = save_state(&root, &engine.state);
                known.store(engine.state.cursor, Ordering::Relaxed);
                let status = match res {
                    Ok(report) => {
                        if report.changed_anything() {
                            let _ = events.send(Event::Report(report));
                        }
                        wait = Duration::from_mins(2);
                        match &link {
                            Link::Folder(dir) => match (dir.problem(), dir.waiting()) {
                                (Some(p), _) => SyncStatus::Error(p),
                                (None, Some(w)) => SyncStatus::Waiting(w),
                                (None, None) => SyncStatus::Synced(Instant::now()),
                            },
                            Link::Server(_) => SyncStatus::Synced(Instant::now()),
                        }
                    }
                    Err(quire_sync::SyncError::Transport(quire_sync::TransportError::Offline(e))) => {
                        wait = Duration::from_secs(30);
                        SyncStatus::Offline(e)
                    }
                    Err(quire_sync::SyncError::Local { .. } | quire_sync::SyncError::Busy) => {
                        // A note changed mid-pass, or other devices are busy: try again soon.
                        wait = Duration::from_secs(2);
                        SyncStatus::Syncing
                    }
                    Err(e) => {
                        wait = Duration::from_mins(5);
                        SyncStatus::Error(e.to_string())
                    }
                };
                let _ = events.send(Event::Status(status));
            }
        });
        Self { tx, rx, stop }
    }

    fn nudge(&self) {
        let _ = self.tx.send(Msg::Nudge);
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

    fn sync_config_for(&self, target: &SyncTarget, space: SpaceId, device_name: &str) -> Result<SyncConfig, String> {
        let (root, _) = self.sync_parts()?;
        let (server, folder) = match target {
            SyncTarget::Folder(chosen) => (String::new(), Some(sync_folder_root(chosen, &root)?)),
            SyncTarget::Server { url, .. } => (url.trim().to_string(), None),
        };
        Ok(SyncConfig {
            server,
            folder,
            space,
            device: DeviceId::random().map_err(|e| e.to_string())?,
            device_name: device_name.trim().to_string(),
        })
    }

    /// Start syncing this vault as a new sync vault. Returns the recovery
    /// key, to show once.
    pub fn sync_create(&mut self, target: &SyncTarget, passphrase: &str, device_name: &str) -> Result<RecoveryKey, String> {
        let (root, _) = self.sync_parts()?;
        if passphrase.chars().count() < 8 {
            return Err("Use a passphrase of at least 8 characters".into());
        }
        let key = VaultKey::generate().map_err(|e| e.to_string())?;
        let cfg = self.sync_config_for(target, SpaceId::random().map_err(|e| e.to_string())?, device_name)?;
        let wrapped = WrappedKey::wrap(&key, passphrase, KDF).map_err(|e| e.to_string())?;
        match (target, &cfg.folder) {
            (_, Some(dir)) => folder::create_space(dir, &cfg.space, &wrapped).map_err(|e| e.to_string())?,
            (SyncTarget::Server { url, token }, None) => {
                HttpTransport::new(url.trim(), cfg.space.clone(), token.trim()).create_space(&wrapped).map_err(|e| e.to_string())?;
            }
            (SyncTarget::Folder(_), None) => return Err("No sync folder".into()),
        }
        Self::store_sync_secrets(&cfg, target, &key)?;
        cfg.save(&root)?;
        let _ = save_state(&root, &SyncState::default());
        let rk = key.recovery_key();
        self.start_sync();
        Ok(rk)
    }

    /// Join an existing sync vault with its passphrase (or recovery key).
    pub fn sync_join(&mut self, target: &SyncTarget, space: &SpaceId, secret: &str, device_name: &str) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        let cfg = self.sync_config_for(target, space.clone(), device_name)?;
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
        Self::store_sync_secrets(&cfg, target, &key)?;
        cfg.save(&root)?;
        let _ = save_state(&root, &SyncState::default());
        let _ = std::fs::remove_file(sync_dir(&root).join("folder-seq"));
        self.start_sync();
        Ok(())
    }

    fn store_sync_secrets(cfg: &SyncConfig, target: &SyncTarget, key: &VaultKey) -> Result<(), String> {
        let store = secrets::store();
        if let SyncTarget::Server { token, .. } = target {
            store.set(&token_name(&cfg.space), token.trim())?;
        }
        store.set(&key_name(&cfg.space), &key.recovery_key().to_string())
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
        self.sync.worker = Some(SyncWorker::start(root, vault, cfg.clone(), token, key));
        self.sync.config = Some(cfg);
        self.sync.status = SyncStatus::Syncing;
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
            let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), key.keys(), load_state(&root));
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
        let events: Vec<Event> = w.rx.try_iter().collect();
        if events.is_empty() {
            return false;
        }
        for e in events {
            match e {
                Event::Status(s) => self.sync.status = s,
                Event::Report(r) => {
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
        };
        cfg.save(dir.path()).unwrap();
        assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
        assert_eq!(load_state(dir.path()), SyncState::default());
    }
}
