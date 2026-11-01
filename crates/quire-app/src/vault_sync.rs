// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire Sync for an open vault: setup, a background worker, and status.
//!
//! The worker runs the [`quire_sync::Engine`] against the vault folder. It
//! writes notes like any other program would, so the editor's existing
//! outside-change handling (merge into unsaved edits, or reload) applies.
//!
//! Per vault, `.quire/sync/config.json` holds the server address and ids, and
//! `.quire/sync/state.json` the engine's state. The server token and the
//! vault key live in the OS keychain (or the private secrets file).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
pub use quire_sync::SpaceInfo;
use quire_sync::{DeviceId, Engine, HttpTransport, KdfParams, LocalStore, RecoveryKey, Report, SpaceId, SyncState, VaultKey, WrappedKey};
use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::secrets;
use crate::workspace::{NoticeLevel, Workspace};

/// What's stored in `.quire/sync/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    pub server: String,
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
    /// Can't reach the server; will keep trying.
    Offline(String),
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
            Self::Error(_) => "Sync needs attention".into(),
        }
    }
}

fn sync_dir(root: &Path) -> PathBuf {
    root.join(".quire").join("sync")
}

fn token_name(space: &SpaceId) -> String {
    format!("sync-token-{space}")
}

fn key_name(space: &SpaceId) -> String {
    format!("sync-key-{space}")
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

/// The vault folder as the engine sees it.
///
/// Writes check that the file is still what the engine last read, so a save
/// from the editor in between is never overwritten (the engine retries on
/// its next pass instead).
pub struct VaultStore {
    vault: Vault,
    seen: HashMap<String, Option<String>>,
}

impl VaultStore {
    pub fn new(vault: Vault) -> Self {
        Self { vault, seen: HashMap::new() }
    }

    fn current(&self, path: &str) -> Option<String> {
        self.vault.read(&NotePath::new(path)).ok()
    }

    fn unchanged_since_read(&self, path: &str) -> Result<(), String> {
        // A path the engine didn't see at the start of the pass must still not exist.
        let expected = self.seen.get(path).cloned().flatten();
        if self.current(path) == expected { Ok(()) } else { Err("it changed while syncing".into()) }
    }
}

impl LocalStore for VaultStore {
    fn list(&self) -> Vec<String> {
        self.vault.storage().list().unwrap_or_default().into_iter().filter(NotePath::is_markdown).map(|p| p.as_str().to_string()).collect()
    }

    fn read(&self, path: &str) -> Option<String> {
        self.current(path)
    }

    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.unchanged_since_read(path)?;
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
        let cur = store.current(&p);
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

        // Long-poll thread: tells the worker when another device pushed.
        {
            let (tx, stop, known) = (tx.clone(), Arc::clone(&stop), Arc::clone(&known));
            let http = HttpTransport::new(&cfg.server, cfg.space.clone(), &token);
            std::thread::spawn(move || {
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
            });
        }

        let worker_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let stop = worker_stop;
            let http = HttpTransport::new(&cfg.server, cfg.space.clone(), &token);
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
                let res = engine.sync(&mut store, &http, &today());
                let _ = save_state(&root, &engine.state);
                known.store(engine.state.cursor, Ordering::Relaxed);
                let status = match res {
                    Ok(report) => {
                        if report.changed_anything() {
                            let _ = events.send(Event::Report(report));
                        }
                        wait = Duration::from_mins(2);
                        SyncStatus::Synced(Instant::now())
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

    /// The vaults on a server, so a new device can pick the one to join.
    pub fn sync_list_spaces(server: &str, token: &str) -> Result<Vec<SpaceInfo>, String> {
        HttpTransport::list_spaces(server.trim(), token.trim()).map_err(|e| e.to_string())
    }

    /// Start syncing this vault to a new space on `server`. Returns the
    /// recovery key, to show once.
    pub fn sync_create(&mut self, server: &str, token: &str, passphrase: &str, device_name: &str) -> Result<RecoveryKey, String> {
        let (root, _) = self.sync_parts()?;
        if passphrase.chars().count() < 8 {
            return Err("Use a passphrase of at least 8 characters".into());
        }
        let key = VaultKey::generate().map_err(|e| e.to_string())?;
        let space = SpaceId::random().map_err(|e| e.to_string())?;
        let wrapped = WrappedKey::wrap(&key, passphrase, KdfParams::DEFAULT).map_err(|e| e.to_string())?;
        HttpTransport::new(server.trim(), space.clone(), token.trim()).create_space(&wrapped).map_err(|e| e.to_string())?;
        let cfg = SyncConfig {
            server: server.trim().to_string(),
            space,
            device: DeviceId::random().map_err(|e| e.to_string())?,
            device_name: device_name.trim().to_string(),
        };
        Self::store_sync_secrets(&cfg, token.trim(), &key)?;
        cfg.save(&root)?;
        let _ = save_state(&root, &SyncState::default());
        let rk = key.recovery_key();
        self.start_sync();
        Ok(rk)
    }

    /// Join an existing space with its passphrase (or recovery key).
    pub fn sync_join(&mut self, server: &str, token: &str, space: &SpaceId, secret: &str, device_name: &str) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        let http = HttpTransport::new(server.trim(), space.clone(), token.trim());
        let key = match VaultKey::from_recovery_key(secret) {
            Ok(k) => k,
            Err(_) => http.wrapped_key().map_err(|e| e.to_string())?.unwrap(secret).map_err(|e| e.to_string())?,
        };
        let cfg = SyncConfig {
            server: server.trim().to_string(),
            space: space.clone(),
            device: DeviceId::random().map_err(|e| e.to_string())?,
            device_name: device_name.trim().to_string(),
        };
        Self::store_sync_secrets(&cfg, token.trim(), &key)?;
        cfg.save(&root)?;
        let _ = save_state(&root, &SyncState::default());
        self.start_sync();
        Ok(())
    }

    fn store_sync_secrets(cfg: &SyncConfig, token: &str, key: &VaultKey) -> Result<(), String> {
        let store = secrets::store();
        store.set(&token_name(&cfg.space), token)?;
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
        let token = store.get(&token_name(&cfg.space));
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
        Ok(())
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

    #[test]
    fn config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SyncConfig {
            server: "https://sync.example.com".into(),
            space: SpaceId::random().unwrap(),
            device: DeviceId::random().unwrap(),
            device_name: "laptop".into(),
        };
        cfg.save(dir.path()).unwrap();
        assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
        assert_eq!(load_state(dir.path()), SyncState::default());
    }
}
