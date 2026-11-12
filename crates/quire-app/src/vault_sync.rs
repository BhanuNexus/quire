// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire Sync for an open vault: setup, a background worker, and status.
//!
//! The worker runs the [`quire_sync::Engine`] against the vault folder. It
//! writes notes like any other program would, so the editor's existing
//! outside-change handling (merge into unsaved edits, or reload) applies.
//!
//! A vault syncs through a folder that Dropbox, iCloud Drive and the like
//! already sync (free, nothing to run), through a Quire Sync server, or
//! (0.12) straight to S3-compatible storage or a WebDAV folder.
//!
//! Per vault, `.quire/sync/config.json` holds where it syncs and its ids, and
//! `.quire/sync/state.json` the engine's state. The vault key (and a server's
//! token, or the storage's credentials) live in the OS keychain (or the
//! private secrets file), never in the vault.

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
use quire_sync::remote::{self, ObjectStore, RemoteTransport};
use quire_sync::{
    DeviceId, Engine, HttpTransport, KdfParams, LocalStore, RecoveryKey, Report, SpaceId, SyncState, Transport, VaultKey, WrappedKey,
};
use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::secrets;
use crate::workspace::{NoticeLevel, Workspace};

mod harden;
pub use harden::{
    DeviceRemoved, Hardening, SyncDevice, SyncDevices, UpgradeInfo, sync_compact_at, sync_devices_at, sync_remove_device_at,
    sync_upgrade_at, sync_upgrade_info_at,
};
use quire_sync::admin::SpaceAdmin;
use quire_sync::keyring::{DeviceSecret, KeySet, Pins};

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
    /// 0.12: S3-compatible storage or a WebDAV folder, when syncing that way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteProfile>,
}

/// Where in S3-compatible storage or WebDAV a vault syncs (no secrets: the
/// credentials are in the keychain, see [`RemoteSecret`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteProfile {
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        virtual_hosted: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_http: bool,
    },
    Webdav {
        url: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_http: bool,
    },
}

/// The storage's credentials, as kept in the keychain (JSON).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteSecret {
    S3 {
        access_key: String,
        secret_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_token: Option<String>,
    },
    Webdav {
        user: String,
        password: String,
    },
}

impl std::fmt::Debug for RemoteSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::S3 { access_key, .. } => f.debug_struct("S3").field("access_key", access_key).finish_non_exhaustive(),
            Self::Webdav { user, .. } => f.debug_struct("Webdav").field("user", user).finish_non_exhaustive(),
        }
    }
}

/// `url` without a `user:password@` part, for status lines. (Such an
/// address is refused when sync is set up; this covers a hand-edited
/// `config.json`.)
fn without_userinfo(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

impl RemoteProfile {
    /// "S3 bucket notes at https://…", "WebDAV folder https://…".
    pub fn describe(&self) -> String {
        match self {
            Self::S3 { endpoint, bucket, prefix, .. } => {
                let p = if prefix.trim_matches('/').is_empty() { String::new() } else { format!("/{}", prefix.trim_matches('/')) };
                format!("S3 bucket {bucket}{p} at {endpoint}")
            }
            Self::Webdav { url, .. } => format!("WebDAV folder {}", without_userinfo(url)),
        }
    }

    /// A client for the storage, checking the address and credentials' shape.
    pub fn open(&self, secret: &RemoteSecret) -> Result<Arc<dyn ObjectStore>, String> {
        use quire_sync::remote::Zeroizing;
        match (self, secret) {
            (
                Self::S3 { endpoint, region, bucket, prefix, virtual_hosted, allow_http },
                RemoteSecret::S3 { access_key, secret_key, session_token },
            ) => {
                let cfg = remote::S3Config {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    virtual_hosted: *virtual_hosted,
                    allow_http: *allow_http,
                };
                let creds = remote::S3Credentials {
                    access_key: access_key.trim().to_string(),
                    secret_key: Zeroizing::new(secret_key.trim().to_string()),
                    session_token: session_token.as_ref().filter(|t| !t.trim().is_empty()).map(|t| Zeroizing::new(t.trim().to_string())),
                };
                Ok(Arc::new(remote::S3Store::new(cfg, creds).map_err(|e| e.to_string())?))
            }
            (Self::Webdav { url, allow_http }, RemoteSecret::Webdav { user, password }) => {
                let cfg = remote::WebDavConfig { url: url.clone(), allow_http: *allow_http };
                let creds = remote::WebDavCredentials { user: user.trim().to_string(), password: Zeroizing::new(password.clone()) };
                Ok(Arc::new(remote::WebDavStore::new(&cfg, &creds).map_err(|e| e.to_string())?))
            }
            _ => Err("The saved credentials are for another kind of storage; enter them again.".into()),
        }
    }
}

impl RemoteSecret {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|_| "The storage credentials on this device are unreadable; enter them again.".to_string())
    }
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
/// Review history shared between devices (0.10), inside the settings.
const SHARED_REVIEW_PREFIX: &str = ".quire/shared/agent/";

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
    /// Another Quire process syncs this vault on this device (the app,
    /// `quire-cli serve --sync` or `sync watch`); this one waits and takes
    /// over when it stops. Says so in words.
    Elsewhere(String),
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
            Self::Elsewhere(_) => "Synced by another Quire".into(),
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

/// The vault key in the keychain and its key epoch: the recovery key, or
/// (0.13, after a device was removed) `e<epoch>:<recovery key>`.
fn parse_key(s: &str) -> Option<(u32, VaultKey)> {
    let (epoch, rk) = match s.strip_prefix('e').and_then(|r| r.split_once(':')) {
        Some((e, rk)) => (e.parse().ok()?, rk),
        None => (0, s),
    };
    VaultKey::from_recovery_key(rk).ok().map(|k| (epoch, k))
}

/// The newest vault key this device has for `space`.
fn read_key(space: &SpaceId) -> Option<VaultKey> {
    secrets::store().get(&key_name(space)).and_then(|k| parse_key(&k)).map(|(_, k)| k)
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
    /// 0.12: S3-compatible storage or a WebDAV folder.
    Remote {
        profile: RemoteProfile,
        secret: RemoteSecret,
    },
}

impl SyncTarget {
    fn store(&self) -> Result<Option<Arc<dyn ObjectStore>>, String> {
        match self {
            Self::Remote { profile, secret } => profile.open(secret).map(Some),
            _ => Ok(None),
        }
    }
}

/// Whether an S3 endpoint wants `bucket.host` URLs (AWS) rather than paths.
pub fn s3_virtual_hosted_default(endpoint: &str, bucket: &str) -> bool {
    remote::S3Config::default_virtual_hosted(endpoint, bucket)
}

/// The place a vault syncs with, secrets included (for "Test connection"
/// on a vault that's set up).
pub fn sync_target_of(cfg: &SyncConfig) -> Result<SyncTarget, String> {
    let token = secrets::store().get(&token_name(&cfg.space));
    match (&cfg.remote, &cfg.folder) {
        (Some(profile), _) => Ok(SyncTarget::Remote {
            profile: profile.clone(),
            secret: RemoteSecret::from_json(&token.ok_or("This device is missing the storage credentials.")?)?,
        }),
        (None, Some(dir)) => Ok(SyncTarget::Folder(dir.clone())),
        (None, None) => Ok(SyncTarget::Server { url: cfg.server.clone(), token: token.ok_or("This device is missing the sync token.")? }),
    }
}

/// "Test connection": can Quire list, write, read and delete there, and
/// does the storage support conditional writes. For S3 and WebDAV; folders
/// and servers say whether they're reachable.
pub fn sync_test(target: &SyncTarget) -> Result<String, String> {
    match target {
        SyncTarget::Remote { profile, secret } => {
            let store = profile.open(secret)?;
            remote::probe(&*store).map(|p| p.summary()).map_err(|e| e.to_string())
        }
        SyncTarget::Folder(_) | SyncTarget::Server { .. } => {
            let n = sync_list(target)?.len();
            Ok(format!("Connected: {n} Quire vault(s) there."))
        }
    }
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
    /// Where this vault syncs, in words ("folder …", "server …", "S3 bucket …").
    pub fn transport_label(&self) -> String {
        match (&self.remote, &self.folder) {
            (Some(r), _) => r.describe(),
            (None, Some(f)) => format!("folder {}", f.display()),
            (None, None) => format!("server {}", self.server),
        }
    }

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
        let mut v: serde_json::Value = serde_json::from_str(&s).ok()?;
        for alias in [PARTIAL_DEVICE, REMOTE_DEVICE] {
            if let Some(o) = v.as_object_mut()
                && !o.contains_key("device")
                && let Some(d) = o.remove(alias)
            {
                o.insert("device".into(), d);
            }
        }
        serde_json::from_value(v).ok()
    }

    fn save(&self, root: &Path) -> Result<(), String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        if !self.selection().is_everything()
            && let Some(o) = v.as_object_mut()
            && let Some(d) = o.remove("device")
        {
            // Quire 0.9 can't read this file then, so it shows sync as off
            // for this vault rather than sync it from an empty state.
            o.insert(PARTIAL_DEVICE.into(), d);
        }
        if self.remote.is_some()
            && let Some(o) = v.as_object_mut()
            && let Some(d) = o.remove("device").or_else(|| o.remove(PARTIAL_DEVICE))
        {
            // Quire 0.11 and older can't read this file then (they'd take
            // it for a server with no address), so they show sync as off.
            o.insert(REMOTE_DEVICE.into(), d);
        }
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join("config.json"), text.as_bytes(), false).map_err(|e| e.to_string())
    }
}

/// Where `config.json` keeps the device id while a selection is set: Quire
/// 0.9 needs `device`, so it doesn't take the file for a sync setup. 0.9
/// knows nothing of selections and keeps no state for them; syncing such a
/// folder it would start afresh over the notes already there.
const PARTIAL_DEVICE: &str = "partial_device";
/// Where `config.json` keeps the device id for S3 or WebDAV sync (0.12).
const REMOTE_DEVICE: &str = "remote_device";

/// What another Quire process syncing the same vault on this device says.
pub const ALREADY_SYNCING: &str = "Quire is already syncing this vault (the app or `quire-cli serve`)";

/// One sync process per vault and device.
///
/// An exclusive lock on `.quire/sync/lock`, held while a worker runs (the
/// app, `sync watch`, `serve --sync`) or for one `sync once` / `sync scope`.
/// Two processes with their own engine state would each send a new note as
/// a note of its own. The OS lets go of the lock when the process ends,
/// however it ends.
#[derive(Debug)]
pub struct SyncLock {
    _file: std::fs::File,
    root: PathBuf,
}

/// Vaults this process holds the sync lock for: a worker being replaced
/// lets go after its pass, and the new one waits quietly for that.
static LOCKED_HERE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn locked_here() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    LOCKED_HERE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let mut here = locked_here();
        if let Some(i) = here.iter().position(|r| *r == self.root) {
            here.swap_remove(i);
        }
    }
}

/// Why [`lock_sync`] didn't get the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Another process (or another worker in this one) syncs the vault.
    Busy {
        in_this_process: bool,
    },
    Io(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { .. } => f.write_str(ALREADY_SYNCING),
            Self::Io(e) => write!(f, "can't lock the sync state: {e}"),
        }
    }
}

/// Take the sync lock for the vault at `root`, without waiting.
pub fn lock_sync(root: &Path) -> Result<SyncLock, LockError> {
    let dir = sync_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| LockError::Io(e.to_string()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lock"))
        .map_err(|e| LockError::Io(e.to_string()))?;
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match file.try_lock() {
        Ok(()) => {
            locked_here().push(key.clone());
            Ok(SyncLock { _file: file, root: key })
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy { in_this_process: locked_here().contains(&key) }),
        Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e.to_string())),
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

/// 0.11: no plain text of a locked folder's notes stays in the sync state.
/// A note that was plain there (before the folder was locked, or before it
/// was moved in) is gone from its path once the delete is sent, and its
/// text with it; what can stay longer is cleared here: the last synced text
/// of deleted notes and of copies kept outside the selection, at paths in
/// a locked folder.
fn scrub_locked_text(vault: &Vault, state: &mut SyncState) {
    let locked = |p: &str| !quire_sync::engine::is_attachment(p) && vault.locked_folder(&NotePath::new(p)).is_some();
    for t in state.files.values_mut() {
        if t.deleted && locked(&t.path) {
            t.base.clear();
        }
        if let Some(k) = t.kept.as_mut().filter(|k| locked(&k.path)) {
            k.base.clear();
        }
    }
    for (p, left) in &mut state.left {
        if locked(p) {
            left.base = None;
        }
    }
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
    /// The files outside the selection after the last pass that finished:
    /// a note that went missing can only have been moved out to one that
    /// appeared since (see [`LocalStore::moved_out`]). `None` before the
    /// first pass of this process.
    outside_before: Option<std::collections::HashSet<String>>,
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
            outside_before: None,
        }
    }

    /// The files outside the selection: unselected notes and attachments.
    fn unselected(&self) -> Vec<NotePath> {
        if self.selection.is_everything() || self.lane == Lane::Shared {
            return Vec::new();
        }
        self.vault.storage().list().unwrap_or_default().into_iter().filter(|p| !self.selection.includes(p.as_str())).collect()
    }

    /// After a pass that finished: remember which files are outside the
    /// selection now.
    fn remember_unselected(&mut self) {
        if self.selection.is_everything() || self.lane == Lane::Shared {
            return;
        }
        self.outside_before = Some(self.unselected().into_iter().map(|p| p.as_str().to_string()).collect());
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
            // The file as stored: sync never decrypts a locked folder's
            // files (they travel as their `.qenc` bytes), 0.11.
            self.vault.storage().read(&NotePath::new(path)).ok()
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

    /// What a write or removal may replace: the file exactly as the engine
    /// read it at the start of the pass (or no file). Checked as one step
    /// with the write ([`quire_vault::Storage::write_bytes_if`]), so a save
    /// from the editor, the web app or an agent in between is never
    /// overwritten (the engine retries on its next pass instead).
    fn as_read(&self, path: &str) -> impl Fn(Option<&[u8]>) -> bool + use<> {
        use base64::Engine as _;
        let expected = self.seen.get(path).cloned().flatten();
        let attachment = quire_sync::engine::is_attachment(path);
        let path = path.to_string();
        move |now: Option<&[u8]>| {
            let now = now.map(|b| {
                if attachment {
                    quire_sync::engine::fingerprint(&path, &b64().encode(b))
                } else {
                    String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b)).into_owned()
                }
            });
            now == expected
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

    fn moved_out(&self, base: &str, skip: &std::collections::BTreeSet<String>) -> Option<(String, String)> {
        use base64::Engine as _;
        // Only when a selected note went missing; the unselected files on
        // this device are few or none (a server holding some folders). A
        // file another synced note has, or one that was already here at the
        // end of the last pass, is another note with the same text: the
        // note was deleted, not moved there.
        let fresh =
            |p: &NotePath| !skip.contains(p.as_str()) && self.outside_before.as_ref().is_none_or(|before| !before.contains(p.as_str()));
        self.unselected().into_iter().filter(fresh).find_map(|p| {
            let text = if quire_sync::engine::is_attachment(p.as_str()) {
                if self.size_and_time(p.as_str()).is_none_or(|(n, _)| n > MAX_ATTACHMENT_BYTES) {
                    return None;
                }
                b64().encode(self.vault.storage().read_bytes(&p).ok()?)
            } else if p.is_markdown() {
                self.vault.storage().read(&p).ok()?
            } else {
                return None;
            };
            (!text.trim().is_empty() && quire_sync::engine::fingerprint(p.as_str(), &text) == base).then(|| (p.as_str().to_string(), text))
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
        if path.starts_with(SHARED_PREFIX) {
            if path.contains("/.") || path.contains("..") {
                return Err("not a vault settings file".into());
            }
            // Another device shares vault settings through this space, so
            // this one must list them too, or it would pass on deletions.
            self.shared = true;
        }
        let changed = || "it changed while syncing".to_string();
        let still = self.as_read(path);
        if quire_sync::engine::is_attachment(path) {
            let bytes = b64().decode(text).map_err(|e| format!("damaged attachment: {e}"))?;
            if !self.vault.storage().write_bytes_if(&NotePath::new(path), &still, &bytes).map_err(|e| e.to_string())? {
                return Err(changed());
            }
            self.seen.insert(path.to_string(), Some(quire_sync::engine::fingerprint(path, text)));
            return Ok(());
        }
        if !self.vault.storage().write_bytes_if(&NotePath::new(path), &still, text.as_bytes()).map_err(|e| e.to_string())? {
            return Err(changed());
        }
        if !path.starts_with(SHARED_PREFIX) {
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
        if path.starts_with(SHARED_REVIEW_PREFIX) {
            // Review history another device stopped sharing (resolved long
            // ago): a copy of its own data, nothing to keep in the trash.
            self.vault.storage().remove(&NotePath::new(path)).map_err(|e| e.to_string())?;
            self.seen.insert(path.to_string(), None);
            return Ok(());
        }
        // Deleted elsewhere: goes to the vault's trash, so it can come back.
        if !self.vault.storage().trash_if(&NotePath::new(path), &self.as_read(path)).map_err(|e| e.to_string())? {
            return Err("it changed while syncing".into());
        }
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
    /// Compact now (Settings → Sync → Compact now).
    Compact,
    Stop,
}

/// What the background sync reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    Status(SyncStatus),
    /// A pass that changed something.
    Report(Report),
    /// Something to tell the person once (what compaction did).
    Notice(String),
}

/// How the worker reaches the other devices.
enum Link {
    Server(HttpTransport),
    Folder(Box<FolderTransport>),
    Remote(Box<RemoteTransport>),
}

/// Where S3 and WebDAV sync keep their mirror (`.quire/sync/remote`).
fn remote_dir(root: &Path) -> PathBuf {
    sync_dir(root).join("remote")
}

/// The storage client for a config, with the credentials in `secret` (JSON).
fn open_store(profile: &RemoteProfile, secret: &str) -> Result<Arc<dyn ObjectStore>, String> {
    profile.open(&RemoteSecret::from_json(secret)?)
}

impl Link {
    /// `token` is a server's token, or the storage's credentials (JSON).
    /// `root` is the vault folder (for a remote mirror), when syncing.
    fn new(cfg: &SyncConfig, token: &str, state_dir: Option<&Path>, root: Option<&Path>) -> Result<Self, String> {
        if let Some(profile) = &cfg.remote {
            let store = open_store(profile, token)?;
            let mirror = root.map(remote_dir);
            let t = RemoteTransport::new(store, &cfg.space, cfg.device.clone(), mirror.as_deref()).map_err(|e| e.to_string())?;
            return Ok(Self::Remote(Box::new(t)));
        }
        Ok(match &cfg.folder {
            Some(dir) => Self::Folder(Box::new(FolderTransport::new(dir, &cfg.space, cfg.device.clone(), state_dir))),
            None => Self::Server(HttpTransport::new(&cfg.server, cfg.space.clone(), token)),
        })
    }

    fn transport(&self) -> &dyn Transport {
        match self {
            Self::Server(t) => t,
            Self::Folder(t) => &**t,
            Self::Remote(t) => &**t,
        }
    }

    /// Check what's read with the vault's keys (folder, S3, WebDAV).
    fn with_keys(self, keys: &KeySet) -> Self {
        match self {
            Self::Folder(t) => Self::Folder(Box::new(t.with_keys(keys.clone()))),
            Self::Remote(t) => Self::Remote(Box::new(t.with_keys(keys.clone()))),
            s @ Self::Server(_) => s,
        }
    }

    /// The vault-level side (0.13): device records, key changes, upgrade.
    fn admin(&self) -> &dyn SpaceAdmin {
        match self {
            Self::Server(t) => t,
            Self::Folder(t) => &**t,
            Self::Remote(t) => &**t,
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
    /// This device's agent store, to share review history (0.10).
    agent: quire_agent::AgentStore,
    /// 0.13: the vault's keys this device has, its own key, pinned keys.
    keys: KeySet,
    secret: Option<DeviceSecret>,
    pins: Pins,
    /// This session published its device record and looked for key changes.
    refreshed: bool,
    /// Why this device can't take the vault's newest key (it was removed).
    key_stuck: Option<String>,
    /// Compaction was tried this session.
    compacted: bool,
    /// Held for the session's life: no other process syncs this vault here.
    _lock: SyncLock,
}

/// The outcome of one pass, and when to try again without a nudge.
struct Pass {
    status: SyncStatus,
    report: Option<Report>,
    again: Duration,
}

impl Session {
    fn new(root: PathBuf, vault: Vault, cfg: SyncConfig, token: &str, key: VaultKey, lock: SyncLock) -> Result<Self, String> {
        let keys = harden::keyset(&cfg.space, &key);
        // Every revision fetched (S3, WebDAV) or read (a folder) is decrypted
        // before it's used, so a damaged one is skipped rather than stopping sync.
        let link = Link::new(&cfg, token, Some(&sync_dir(&root)), Some(&root))?.with_keys(&keys);
        let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), keys.clone(), load_state(&root, &cfg));
        // In a sync folder, vault settings travel in a lane of their own,
        // which 0.8 never reads; the notes lane never touches them.
        let store = if cfg.folder.is_some() {
            VaultStore::new(vault.clone()).notes_only()
        } else {
            VaultStore::new(vault.clone()).with_shared_state(cfg.shared_state)
        }
        .with_selection(cfg.selection());
        // Other devices see this one under its sync name ("home server").
        let mut device = crate::device::this_device();
        if !cfg.device_name.trim().is_empty() {
            device.name = cfg.device_name.trim().to_string();
        }
        let agent = quire_agent::AgentStore::open(&vault, device, Arc::new(quire_agent::SystemClock));
        let secret = harden::device_secret(&cfg.device).ok();
        let pins = harden::load_pins(&root);
        Ok(Self {
            root,
            vault,
            cfg,
            key,
            link,
            engine,
            store,
            lane: None,
            agent,
            keys,
            secret,
            pins,
            refreshed: false,
            key_stuck: None,
            compacted: false,
            _lock: lock,
        })
    }

    /// 0.13, at the start of a pass: on a 0.13 server, swap the server's
    /// token for this device's own; publish this device's record; take
    /// any change of the vault's key (a device was removed), and ignore
    /// what removed devices write after their cut.
    fn refresh_keys(&mut self) {
        if let Link::Server(_) = &self.link
            && !Hardening::load(&self.root).device_token
        {
            self.enroll();
        }
        let news = match &self.link {
            Link::Remote(t) => t.take_key_news(),
            // A folder is read here; a server answers one small request.
            _ => true,
        };
        if !news && self.refreshed {
            return;
        }
        let Some(secret) = self.secret.clone() else { return };
        let admin = self.link.admin();
        if !self.refreshed {
            let _ = quire_sync::admin::publish_self(admin, &self.cfg.space, &self.cfg.device, &secret, &self.cfg.device_name, &self.keys);
        }
        let Ok(r) = quire_sync::admin::refresh_keys(admin, &self.cfg.space, &self.cfg.device, Some(&secret), &self.keys, &mut self.pins)
        else {
            return;
        };
        self.refreshed = true;
        harden::save_pins(&self.root, &self.pins);
        if r.keys.current() > self.keys.current() && harden::store_keys(&self.cfg.space, &r.keys).is_ok() {
            if let Some(k) = r.keys.current_raw() {
                self.key = k.clone();
            }
            // A newer key: this device's record says so again.
            let _ = quire_sync::admin::publish_self(admin, &self.cfg.space, &self.cfg.device, &secret, &self.cfg.device_name, &r.keys);
        }
        self.keys = r.keys.clone();
        self.engine.set_keys(r.keys.clone());
        admin.set_removed(harden::removed_lanes(&r.removed, ""));
        if let Some((t, e, _)) = self.lane.as_mut() {
            t.set_keys(r.keys.clone());
            t.set_removed(harden::removed_lanes(&r.removed, folder::SHARED_LANE));
            e.set_keys(r.keys);
        }
        self.key_stuck = r.stuck.map(|(_, why)| quire_sync::admin::stuck_notice(&why));
    }

    /// Swap the server's token for one of this device's own (a 0.13
    /// server): the server can then refuse this device alone if it's removed.
    fn enroll(&mut self) {
        let store = secrets::store();
        let Some(token) = store.get(&token_name(&self.cfg.space)) else { return };
        let Ok(own) = HttpTransport::enroll(&self.cfg.server, &self.cfg.space, &token, &self.cfg.device) else { return };
        if store.set(&token_name(&self.cfg.space), &own).is_err() {
            return;
        }
        let mut h = Hardening::load(&self.root);
        h.device_token = true;
        if h.save(&self.root).is_ok() {
            self.link = Link::Server(HttpTransport::new(&self.cfg.server, self.cfg.space.clone(), &own));
        }
    }

    /// Fold old revisions into a snapshot (folder, S3, WebDAV; a server
    /// compacts by itself) and delete what settled snapshots replaced.
    fn compact(&self, force: bool) -> Result<String, String> {
        let mut h = Hardening::load(&self.root);
        let policy = h.policy();
        h.last_compact = chrono::Utc::now().timestamp();
        let _ = h.save(&self.root);
        let out = match &self.link {
            Link::Server(_) => return Ok("A sync server compacts its own history (QUIRE_SYNC_KEEP_DAYS on the server).".into()),
            Link::Folder(t) => {
                let c = t.compact(policy, None, force).map_err(|e| e.to_string())?;
                let _ = t.tidy();
                let mut out = harden::describe(&c, "Notes");
                if let Some((lane, _, _)) = &self.lane {
                    let c = lane.compact(policy, None, force).map_err(|e| e.to_string())?;
                    let _ = lane.tidy();
                    out.push(' ');
                    out.push_str(&harden::describe(&c, "Vault settings"));
                }
                out
            }
            Link::Remote(t) => harden::describe(&t.compact(policy, force).map_err(|e| e.to_string())?, "Notes"),
        };
        Ok(out)
    }

    /// After a good pass, at most once a day: compact if worth it.
    fn maybe_compact(&mut self) {
        if self.compacted || matches!(self.link, Link::Server(_)) {
            return;
        }
        self.compacted = true;
        let h = Hardening::load(&self.root);
        if chrono::Utc::now().timestamp() - h.last_compact < 24 * 3600 {
            return;
        }
        let _ = self.compact(false);
    }

    /// Whether `.quire/shared/` travels on this device now (turned on here,
    /// or another device's settings arrived).
    fn shares_state(&self) -> bool {
        self.cfg.shared_state || self.store.shared || self.lane.is_some() || runs_shared_lane(&self.root, &self.cfg)
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
        // Review history: this device's proposals, decisions and ledger go
        // out with the vault settings, and other devices' come in after.
        self.refresh_keys();
        if let Some(why) = self.key_stuck.clone() {
            let status = SyncStatus::Error(why);
            let _ = LastPass::record(&self.root, &status, None);
            return Pass { status, report: None, again: poll.max(Duration::from_mins(5)) };
        }
        if self.shares_state() {
            export_review(&self.agent);
        }
        let (root, cfg) = (&self.root, &self.cfg);
        snapshot_reads(&mut self.store);
        let mut res = self.engine.sync(&mut self.store, self.link.transport(), &today());
        // S3 and WebDAV: upload what the pass pushed (kept here until then).
        if let Link::Remote(t) = &self.link {
            let flushed = t.flush();
            if res.is_ok()
                && let Err(e) = flushed
            {
                res = Err(quire_sync::SyncError::Transport(e));
            }
        }
        scrub_locked_text(&self.store.vault, &mut self.engine.state);
        let _ = save_state(root, cfg, &self.engine.state);
        if res.is_ok() {
            self.store.remember_unselected();
        }
        if self.lane.is_none()
            && runs_shared_lane(root, cfg)
            && let Some(dir) = &cfg.folder
        {
            let t = FolderTransport::lane(dir, &cfg.space, cfg.device.clone(), Some(&sync_dir(root)), folder::SHARED_LANE)
                .with_keys(self.keys.clone());
            let e = Engine::new(
                cfg.space.clone(),
                cfg.device.clone(),
                cfg.device_name.clone(),
                self.keys.clone(),
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
            // Only that switch: the rest of the file stays as it is now.
            if let Some(mut now) = SyncConfig::load(&self.root) {
                now.shared_state = true;
                let _ = now.save(&self.root);
            }
        }
        if res.is_ok() && self.shares_state() {
            import_review(&self.agent);
        }
        let (status, report, again) = match res {
            Ok(report) => {
                let status = match &self.link {
                    Link::Folder(dir) => match (dir.problem(), dir.waiting()) {
                        (Some(p), _) => SyncStatus::Error(p),
                        (None, Some(w)) => SyncStatus::Waiting(w),
                        (None, None) => SyncStatus::Synced(Instant::now()),
                    },
                    Link::Remote(t) => match (t.problem(), t.waiting()) {
                        (Some(p), _) => SyncStatus::Error(p),
                        (None, Some(w)) => SyncStatus::Waiting(w),
                        (None, None) => SyncStatus::Synced(Instant::now()),
                    },
                    Link::Server(_) => SyncStatus::Synced(Instant::now()),
                };
                // A pass stops at its download budget: the next one soon.
                let again = match &self.link {
                    Link::Remote(t) if t.more_to_fetch() => Duration::from_secs(1),
                    _ => poll,
                };
                (status, report.changed_anything().then_some(report), again)
            }
            Err(quire_sync::SyncError::Transport(quire_sync::TransportError::Offline(e))) => {
                let e = match &self.link {
                    // "3 change(s) saved here, not uploaded yet: <why>".
                    Link::Remote(t) => t.pending().unwrap_or(e),
                    _ => e,
                };
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

/// Publish this device's review history before a pass.
///
/// Proposals its agents made, what the person decided here and its ledger
/// go to `.quire/shared/agent/`. Only with shared state on: then the
/// settings lane carries them.
pub fn export_review(agent: &quire_agent::AgentStore) -> usize {
    agent.export_shared()
}

/// Take in other devices' review history after a pass: their agents'
/// suggestions appear in this device's Inbox, and decisions made elsewhere
/// resolve them here. Never writes a note.
pub fn import_review(agent: &quire_agent::AgentStore) -> quire_agent::ImportReport {
    agent.import_shared()
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
            SyncStatus::Elsewhere(e) => ("elsewhere", e.clone()),
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
/// How often S3 and WebDAV are checked for other devices' changes (or the
/// poll interval, when that's longer than the app's).
pub const REMOTE_POLL: Duration = Duration::from_mins(1);

impl SyncWorker {
    fn start(root: PathBuf, vault: Vault, cfg: SyncConfig, token: String, key: VaultKey, poll: Duration) -> Self {
        let (tx, msgs) = crossbeam_channel::unbounded();
        let (events, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let known = Arc::new(AtomicU64::new(0));

        // Watch thread: tells the worker when another device pushed.
        {
            let (tx, stop, known) = (tx.clone(), Arc::clone(&stop), Arc::clone(&known));
            match Link::new(&cfg, &token, Some(&sync_dir(&root)), None) {
                // Can't even make a client (a bad address): the worker says why.
                Err(_) => std::thread::spawn(|| {}),
                // S3 and WebDAV: one listing of the devices' tips a minute
                // (see docs/sync-backends.md for what that costs).
                Ok(Link::Remote(t)) => std::thread::spawn(move || {
                    // Less often when asked (`sync watch --poll 5m`).
                    let every = if poll > APP_POLL { poll } else { REMOTE_POLL };
                    let mut last = t.fingerprint().ok();
                    let _ = tx.send(Msg::Remote);
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..every.as_secs() {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(Duration::from_secs(1));
                        }
                        let now = t.fingerprint().ok();
                        if now.is_some() && now != last {
                            last = now;
                            let _ = tx.send(Msg::Remote);
                        }
                    }
                }),
                // A server answers a long poll as soon as something changes.
                Ok(Link::Server(mut http)) => {
                    let (server, space) = (cfg.server.clone(), cfg.space.clone());
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            match http.wait(known.load(Ordering::Relaxed), 25) {
                                Ok(latest) if latest > known.load(Ordering::Relaxed) => {
                                    let _ = tx.send(Msg::Remote);
                                    std::thread::sleep(Duration::from_millis(300));
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    // The worker may have swapped the server's token
                                    // for this device's own (0.13): use what's kept now.
                                    if let Some(t) = secrets::store().get(&token_name(&space)) {
                                        http = HttpTransport::new(&server, space.clone(), &t);
                                    }
                                    std::thread::sleep(Duration::from_secs(15));
                                }
                            }
                        }
                    })
                }
                // A folder is checked every couple of seconds (sizes and
                // times only, so it's cheap).
                Ok(Link::Folder(dir)) => {
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
            // One sync per vault and device: while another process syncs
            // this vault, wait (saying so) and take over when it stops.
            let mut told = false;
            let since = Instant::now();
            let lock = loop {
                match lock_sync(&root) {
                    Ok(l) => break l,
                    Err(e) => {
                        // A worker of this process being replaced lets go after
                        // its pass: wait for that quietly, for a while.
                        let quiet = matches!(e, LockError::Busy { in_this_process: true }) && since.elapsed() < Duration::from_secs(3);
                        if !quiet && !told {
                            let what = match &e {
                                LockError::Busy { .. } => {
                                    SyncStatus::Elsewhere(format!("{ALREADY_SYNCING}. This one takes over when it stops."))
                                }
                                LockError::Io(_) => SyncStatus::Error(e.to_string()),
                            };
                            let _ = events.send(SyncEvent::Status(what));
                            told = true;
                        }
                        let wait = if quiet { Duration::from_millis(200) } else { Duration::from_secs(5) };
                        if matches!(msgs.recv_timeout(wait), Ok(Msg::Stop)) || stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                }
            };
            let mut session = match Session::new(root, vault, cfg, &token, key, lock) {
                Ok(s) => s,
                Err(e) => {
                    let _ = events.send(SyncEvent::Status(SyncStatus::Error(e)));
                    return;
                }
            };
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
                    Ok(Msg::Compact) => {
                        let _ = events.send(SyncEvent::Status(SyncStatus::Syncing));
                        let said = session.compact(true).unwrap_or_else(|e| format!("Compaction failed: {e}"));
                        let _ = events.send(SyncEvent::Notice(said));
                    }
                    Ok(Msg::Remote) | Err(_) => {}
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let _ = events.send(SyncEvent::Status(SyncStatus::Syncing));
                let pass = session.pass(poll);
                if matches!(pass.status, SyncStatus::Synced(_)) {
                    session.maybe_compact();
                }
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

    /// Compact the vault's history now (a [`SyncEvent::Notice`] says how it went).
    pub fn compact(&self) {
        let _ = self.tx.send(Msg::Compact);
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
    let vault = crate::secure_state::open_vault(root)?;
    let store = secrets::store();
    // A folder needs no token.
    let token = if cfg.folder.is_some() { Some(String::new()) } else { store.get(&token_name(&cfg.space)) };
    let key = read_key(&cfg.space);
    if cfg.remote.is_some() && token.is_none() && key.is_some() {
        return Err(format!(
            "This device is missing the storage credentials ({}). Enter them again: `quire-cli sync credentials`, or Settings → Sync.",
            secrets::store().kind()
        ));
    }
    match (token, key) {
        (Some(t), Some(k)) => Ok((vault, cfg, t, k)),
        _ => Err(format!("This device is missing the sync key ({}). Join the vault again.", secrets::store().kind())),
    }
}

/// One sync pass of the vault at `root`, in this thread (`quire-cli sync once`).
pub fn sync_once_at(root: &Path) -> Result<Report, String> {
    let (vault, cfg, token, key) = open_parts(root)?;
    let lock = lock_sync(root).map_err(|e| match e {
        LockError::Busy { .. } => format!("{ALREADY_SYNCING}; it keeps this vault in sync, nothing to do here."),
        LockError::Io(_) => e.to_string(),
    })?;
    let mut session = Session::new(root.to_path_buf(), vault, cfg, &token, key, lock)?;
    if let Some(notice) = session.too_old() {
        return Err(notice);
    }
    let pass = session.pass(APP_POLL);
    match pass.status {
        SyncStatus::Synced(_) | SyncStatus::Waiting(_) => Ok(pass.report.unwrap_or_default()),
        SyncStatus::Syncing => Err("a note changed while syncing, or other devices were busy; run it again".into()),
        SyncStatus::Offline(e) | SyncStatus::Error(e) | SyncStatus::Elsewhere(e) => Err(e),
        SyncStatus::Off => Err("sync is off".into()),
    }
}

/// "Also sync vault settings and review history" for the vault at `root`.
///
/// For `quire-cli sync share`. Turning it on first keeps 0.8 devices out of
/// those files (a server's minimum version, a sync folder's own lane).
pub fn sync_share_at(root: &Path, on: bool) -> Result<(), String> {
    let mut cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault. Run `quire-cli sync join` first.")?;
    if on && !cfg.shared_state {
        raise_min_reader(&cfg)?;
    }
    cfg.shared_state = on;
    cfg.save(root)
}

/// Run `f` on a thread of its own; its result arrives on the receiver.
fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
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
    // S3 and WebDAV vaults are new in 0.12: no older device can read them.
    if cfg.remote.is_some() {
        return Ok(());
    }
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
    let (server, folder, remote) = match target {
        SyncTarget::Folder(chosen) => (String::new(), Some(sync_folder_root(chosen, root)?), None),
        SyncTarget::Server { url, .. } => (url.trim().to_string(), None, None),
        SyncTarget::Remote { profile, .. } => (String::new(), None, Some(profile.clone())),
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
        remote,
    })
}

fn store_sync_secrets(cfg: &SyncConfig, target: &SyncTarget, key: &VaultKey) -> Result<(), String> {
    let store = secrets::store();
    match target {
        SyncTarget::Server { token, .. } => store.set(&token_name(&cfg.space), token.trim())?,
        SyncTarget::Remote { secret, .. } => store.set(&token_name(&cfg.space), &secret.to_json())?,
        SyncTarget::Folder(_) => {}
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
    create_at(root, target, passphrase, device_name, selection).map(|(rk, _)| rk)
}

/// [`sync_create_at`]: the new vault is made in the storage and its secrets
/// stored (under its own space id) before anything of an earlier setup in
/// `root` is touched; then that setup's files go and the new config is
/// saved. Returns the new config too.
fn create_at(
    root: &Path,
    target: &SyncTarget,
    passphrase: &str,
    device_name: &str,
    selection: &Selection,
) -> Result<(RecoveryKey, SyncConfig), String> {
    if passphrase.chars().count() < 8 {
        return Err("Use a passphrase of at least 8 characters".into());
    }
    let key = VaultKey::generate().map_err(|e| e.to_string())?;
    let cfg = sync_config_for(root, target, SpaceId::random().map_err(|e| e.to_string())?, device_name, selection)?;
    let wrapped = WrappedKey::wrap(&key, passphrase, KDF).map_err(|e| e.to_string())?;
    // 0.13: a new vault binds revision headers and can remove devices
    // (docs/design/0.13-sync.md); Quire 0.12 and older can't join it.
    let mut own_token = None;
    match (target, &cfg.folder) {
        (SyncTarget::Remote { .. }, _) => {
            let store = target.store()?.ok_or("No storage")?;
            remote::create_space_at(&*store, &cfg.space, &wrapped, remote::LEVEL_HARDENED).map_err(|e| e.to_string())?;
        }
        (_, Some(dir)) => folder::create_space_at(dir, &cfg.space, &wrapped, folder::LEVEL_HARDENED).map_err(|e| e.to_string())?,
        (SyncTarget::Server { url, token }, None) => {
            HttpTransport::new(url.trim(), cfg.space.clone(), token.trim()).create_space(&wrapped).map_err(|e| e.to_string())?;
            // A 0.13 server: this device's own token, then the 0.13 minimum.
            if let Ok(own) = HttpTransport::enroll(url.trim(), &cfg.space, token.trim(), &cfg.device) {
                let _ = HttpTransport::new(url.trim(), cfg.space.clone(), &own).set_min_protocol(quire_sync::protocol::HARDENED_PROTOCOL);
                own_token = Some(own);
            }
        }
        (SyncTarget::Folder(_), None) => return Err("No sync folder".into()),
    }
    store_sync_secrets(&cfg, target, &key)?;
    remove_sync_files(root);
    cfg.save(root)?;
    if let Some(own) = &own_token {
        secrets::store().set(&token_name(&cfg.space), own)?;
    }
    Hardening { device_token: own_token.is_some(), ..Hardening::default() }.save(root)?;
    let _ = save_state(root, &cfg, &SyncState::default());
    Ok((key.recovery_key(), cfg))
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
    // A 0.13 server gives this device a token of its own first.
    let own_token = match target {
        SyncTarget::Server { url, token } => HttpTransport::enroll(url.trim(), space, token.trim(), &cfg.device).ok(),
        _ => None,
    };
    let (wrapped, records) = match (target, &cfg.folder) {
        (SyncTarget::Remote { .. }, _) => {
            let store = target.store()?.ok_or("No storage")?;
            let records =
                RemoteTransport::new(Arc::clone(&store), space, cfg.device.clone(), None).and_then(|t| t.key_records()).unwrap_or_default();
            (remote::wrapped_key(&*store, space), records)
        }
        (_, Some(dir)) => (folder::wrapped_key(dir, space), FolderTransport::new(dir, space, cfg.device.clone(), None).key_records()),
        (SyncTarget::Server { url, token }, None) => {
            let t = HttpTransport::new(url.trim(), space.clone(), own_token.as_deref().unwrap_or_else(|| token.trim()));
            (t.wrapped_key(), t.key_records().unwrap_or_default())
        }
        (SyncTarget::Folder(_), None) => return Err("No sync folder".into()),
    };
    let wrapped = match wrapped {
        Ok(w) => Some(w),
        Err(_) if VaultKey::from_recovery_key(secret).is_ok() => None,
        Err(e) => return Err(e.to_string()),
    };
    let keys = harden::join_keys(wrapped.as_ref(), &records, secret)?;
    let key = keys.current_raw().ok_or("no key")?.clone();
    store_sync_secrets(&cfg, target, &key)?;
    harden::store_keys(&cfg.space, &keys)?;
    if let Some(own) = &own_token {
        secrets::store().set(&token_name(&cfg.space), own)?;
    }
    cfg.save(root)?;
    Hardening { device_token: own_token.is_some(), ..Hardening::default() }.save(root)?;
    let _ = save_state(root, &cfg, &SyncState::default());
    let _ = std::fs::remove_file(sync_dir(root).join("folder-seq"));
    // A new device id: a mirror from an earlier setup holds nothing to send.
    let _ = std::fs::remove_dir_all(remote_dir(root));
    Ok(())
}

/// New credentials for the S3 or WebDAV storage of the vault at `root`
/// (they expired, or were rotated). Checked against the storage first.
pub fn sync_set_remote_secret_at(root: &Path, secret: &RemoteSecret) -> Result<String, String> {
    let cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault")?;
    let profile = cfg.remote.as_ref().ok_or("This vault doesn't sync through S3 or WebDAV")?;
    let summary = remote::probe(&*profile.open(secret)?).map_err(|e| e.to_string())?.summary();
    secrets::store().set(&token_name(&cfg.space), &secret.to_json())?;
    Ok(summary)
}

/// Stop syncing the vault at `root` on this device: its sync settings,
/// state and secrets go; notes stay as they are, and so does the copy
/// other devices sync with.
pub fn sync_stop_at(root: &Path) {
    if let Some(cfg) = SyncConfig::load(root) {
        remove_sync_secrets(&cfg.space);
        let _ = secrets::store().delete(&harden::device_secret_name(&cfg.device));
    }
    remove_sync_files(root);
}

fn remove_sync_secrets(space: &SpaceId) {
    let store = secrets::store();
    let _ = store.delete(&token_name(space));
    let _ = store.delete(&key_name(space));
}

/// A sync setup's files on this device: config, engine state, the S3 or
/// WebDAV mirror.
fn remove_sync_files(root: &Path) {
    let dir = sync_dir(root);
    for f in [
        "config.json",
        "state.json",
        "state-partial.json",
        "state-shared.json",
        "last.json",
        "folder-seq",
        "hardening.json",
        "peers.json",
        "snapshots-seen.json",
    ] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let _ = std::fs::remove_dir_all(remote_dir(root));
}

/// Move the vault at `root` to another place (a folder, a server, S3 or WebDAV).
///
/// One last pass where it syncs now, then a new sync vault there, from
/// this device's notes. Other devices join the new vault (their notes are
/// adopted, not copied). Returns the new vault's recovery key, and the last
/// pass's problem if it had one.
///
/// The old setup is removed only once the new vault exists and its secrets
/// are stored: if making it fails, this device still syncs with the old
/// place. Changes saved here but not yet uploaded to S3 or WebDAV stop the
/// move unless `force` (they're in the notes, so they go to the new place
/// either way, but the old place's devices wouldn't get them).
pub fn sync_move_at(root: &Path, target: &SyncTarget, passphrase: &str, force: bool) -> Result<(RecoveryKey, Option<String>), String> {
    let cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault")?;
    if passphrase.chars().count() < 8 {
        return Err("Use a passphrase of at least 8 characters".into());
    }
    // Check the new place before leaving the old one.
    sync_test(target)?;
    let lock = lock_sync(root).map_err(|e| match e {
        LockError::Busy { .. } => format!("{ALREADY_SYNCING}. Stop it first, then move the vault."),
        LockError::Io(_) => e.to_string(),
    })?;
    let (last, unsent, held) = last_pass(root, lock);
    let unsent = match unsent {
        Some(0) => None,
        Some(n) => Some(format!("{n} change(s) made here haven't")),
        // The pass stopped before it could tell (offline at the start).
        None if last.is_some() && cfg.remote.is_some() => Some("Changes made here may not have".to_string()),
        None => None,
    };
    if let Some(what) = unsent.filter(|_| !force) {
        return Err(format!(
            "{what} reached {} yet ({}). Move when it's reachable, or force the move: they're in your notes and go to the new place either way, but devices still on the old place won't get them.",
            cfg.transport_label(),
            last.as_deref().unwrap_or("not uploaded")
        ));
    }
    // Still this process's: the pass's session holds the lock, or it's taken again.
    let _relock = if held.is_none() { Some(lock_sync(root).map_err(|e| e.to_string())?) } else { None };
    let _held = held;
    let (rk, new) = create_at(root, target, passphrase, &cfg.device_name, &cfg.selection())?;
    if new.space != cfg.space {
        remove_sync_secrets(&cfg.space);
    }
    if cfg.shared_state {
        let _ = sync_share_at(root, true);
    }
    Ok((rk, last))
}

/// A pass for `sync move` under `lock`: its problem, if any; how many
/// changes made here are still waiting to be uploaded to S3 or WebDAV
/// (`None`: it couldn't tell); and the session, which holds the lock.
fn last_pass(root: &Path, lock: SyncLock) -> (Option<String>, Option<usize>, Option<Session>) {
    let mut session =
        match open_parts(root).and_then(|(vault, cfg, token, key)| Session::new(root.to_path_buf(), vault, cfg, &token, key, lock)) {
            Ok(s) => s,
            Err(e) => return (Some(e), None, None),
        };
    let pass = session.pass(APP_POLL);
    let problem = match pass.status {
        SyncStatus::Synced(_) | SyncStatus::Waiting(_) => None,
        SyncStatus::Syncing => Some("a note changed while syncing, or other devices were busy".into()),
        SyncStatus::Offline(e) | SyncStatus::Error(e) | SyncStatus::Elsewhere(e) => Some(e),
        SyncStatus::Off => Some("sync is off".into()),
    };
    let unsent = match &session.link {
        Link::Remote(t) => t.unsent(),
        _ => Some(0),
    };
    (problem, unsent, Some(session))
}

/// Change which folders the vault at `root` syncs. The engine's state
/// moves with it, so notes now included are fetched and notes now left out
/// are never sent as deletes.
///
/// Refused while a sync process runs for the vault on this device: it
/// would go on with the old selection (and its own copy of the state)
/// until restarted.
pub fn sync_set_selection_at(root: &Path, selection: &Selection) -> Result<(), String> {
    let mut cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault")?;
    let _lock = lock_sync(root).map_err(|e| match e {
        LockError::Busy { .. } => {
            format!(
                "{ALREADY_SYNCING}. Stop it first (quit the app, or stop `quire-cli sync watch` / `serve --sync`), then change the folders."
            )
        }
        LockError::Io(_) => e.to_string(),
    })?;
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
            SyncTarget::Remote { .. } => Ok(remote::list_spaces(&*target.store()?.ok_or("No storage")?)
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
        let key = read_key(&cfg.space);
        if cfg.remote.is_some() && token.is_none() && key.is_some() {
            self.sync.config = Some(cfg);
            self.sync.status = SyncStatus::Error("This device is missing the storage credentials. Enter them in Settings → Sync.".into());
            return;
        }
        let (Some(token), Some(key)) = (token, key) else {
            self.sync.config = Some(cfg);
            self.sync.status = SyncStatus::Error("This device is missing the sync key. Join the vault again in Settings → Sync.".into());
            return;
        };
        self.sync.worker = Some(SyncWorker::start(root, vault, cfg.clone(), token, key, APP_POLL));
        self.sync.config = Some(cfg);
        self.sync.status = SyncStatus::Syncing;
    }

    /// Whether vault settings and review history sync for this vault.
    pub fn sync_shares_state(&self) -> bool {
        self.sync.config.as_ref().is_some_and(|c| c.shared_state)
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
        let _ = std::fs::remove_dir_all(remote_dir(&root));
        Ok(())
    }

    /// The vault's devices, off the UI thread (Settings → Sync → Devices).
    pub fn sync_devices_in_background(&self) -> Result<Receiver<Result<SyncDevices, String>>, String> {
        let (root, _) = self.sync_parts()?;
        Ok(in_background(move || sync_devices_at(&root)))
    }

    /// Remove a device, off the UI thread; the worker takes the new key at its next pass.
    pub fn sync_remove_device_in_background(
        &self,
        device: String,
        passphrase: String,
    ) -> Result<Receiver<Result<DeviceRemoved, String>>, String> {
        let (root, _) = self.sync_parts()?;
        let pass = remote::Zeroizing::new(passphrase);
        Ok(in_background(move || sync_remove_device_at(&root, &device, &pass)))
    }

    /// Whether the vault is upgraded, and which devices would stop syncing.
    pub fn sync_upgrade_info_in_background(&self) -> Result<Receiver<Result<UpgradeInfo, String>>, String> {
        let (root, _) = self.sync_parts()?;
        Ok(in_background(move || sync_upgrade_info_at(&root)))
    }

    /// Upgrade the vault's sync format, off the UI thread.
    pub fn sync_upgrade_in_background(&self) -> Result<Receiver<Result<(), String>>, String> {
        let (root, _) = self.sync_parts()?;
        Ok(in_background(move || sync_upgrade_at(&root)))
    }

    /// "Test connection" off the UI thread.
    pub fn sync_test_in_background(target: SyncTarget) -> Receiver<Result<String, String>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(sync_test(&target));
        });
        rx
    }

    /// New credentials for this vault's S3 or WebDAV storage, checked
    /// first; sync restarts with them.
    pub fn sync_set_remote_secret(&mut self, secret: &RemoteSecret) -> Result<String, String> {
        let (root, _) = self.sync_parts()?;
        let summary = sync_set_remote_secret_at(&root, secret)?;
        self.start_sync();
        Ok(summary)
    }

    /// [`Self::sync_set_remote_secret`] off the UI thread: checks and saves
    /// in the background; call [`Self::start_sync`] when it says `Ok`.
    pub fn sync_set_remote_secret_in_background(&self, secret: RemoteSecret) -> Result<Receiver<Result<String, String>>, String> {
        let (root, _) = self.sync_parts()?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(sync_set_remote_secret_at(&root, &secret));
        });
        Ok(rx)
    }

    /// Every version of `path` the sync server or folder holds, from every
    /// device, worked out in the background. `None` when sync is off.
    pub fn fetch_sync_history(&self, path: &NotePath) -> Option<Receiver<Result<Vec<SyncVersion>, String>>> {
        let cfg = self.sync.config.clone()?;
        let (root, _) = self.sync_parts().ok()?;
        let store = secrets::store();
        let key = read_key(&cfg.space)?;
        let token = store.get(&token_name(&cfg.space)).unwrap_or_default();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let path = path.as_str().to_string();
        std::thread::spawn(move || {
            // S3 and WebDAV: read the mirror the worker keeps, not the storage.
            let keys = harden::keyset(&cfg.space, &key);
            let link = if cfg.remote.is_some() {
                let mirror = RemoteTransport::mirror_root(&remote_dir(&root));
                let mut t = FolderTransport::new(&mirror, &cfg.space, cfg.device.clone(), None);
                t.set_check_lines(false);
                Ok(Link::Folder(Box::new(t)))
            } else {
                Link::new(&cfg, &token, None, None)
            };
            // Older key epochs, from the vault's key records (0.13).
            let link = link.map(|l| l.with_keys(&keys));
            let keys = link
                .as_ref()
                .ok()
                .and_then(|l| l.admin().key_records().ok())
                .map_or_else(|| keys.clone(), |recs| quire_sync::keyring::resolve(&keys, &recs, &cfg.device, None, &Pins::default()).keys);
            let link = link.map(|l| l.with_keys(&keys));
            let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), keys, load_state(&root, &cfg));
            let _ = tx.send(link.and_then(|l| engine.history(l.transport(), &path).map_err(|e| e.to_string())));
        });
        Some(rx)
    }

    /// Compact the vault's sync history now, in the background (Settings →
    /// Sync); a notice says how it went.
    pub fn sync_compact(&self) -> Result<(), String> {
        self.sync.worker.as_ref().ok_or("Sync isn't running")?.compact();
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
        secrets::store().get(&key_name(&cfg.space)).map(|k| k.split_once(':').map_or_else(|| k.clone(), |(_, rk)| rk.to_string()))
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
                SyncEvent::Notice(n) => self.notify(NoticeLevel::Info, n),
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
        // Off by default: an agent's suggestion on the server stays there,
        // and no review history is written on either side.
        let server_store = quire_agent::AgentStore::open(
            &Vault::open_local(&vb).unwrap(),
            crate::device::this_device(),
            Arc::new(quire_agent::SystemClock),
        );
        let agent = quire_agent::Actor::agent("oauth:claude.ai", "Claude", quire_agent::Via::McpHttp, &server_store.device().id);
        server_store
            .submit(
                &agent,
                quire_agent::ChangeSet::one(quire_agent::Change::Edit { path: NotePath::new("Plan.md"), new_text: "suggested\n".into() }),
            )
            .unwrap();
        std::fs::write(vb.join("Marker.md"), "after the suggestion\n").unwrap();
        b.nudge();
        wait_both(&mut a, &b, "the marker to reach the app", || va.join("Marker.md").exists());
        assert!(!va.join(".quire/shared/agent").exists() && !vb.join(".quire/shared/agent").exists());
        assert!(a.agent_store().unwrap().proposals(&quire_agent::ProposalFilter::pending()).is_empty());
        let summary = sync_summary_at(&vb).unwrap();
        assert_eq!((summary.config.device_name.as_str(), summary.has_secrets), ("server", true));
        assert!(summary.last.is_some_and(|l| l.state == "synced" || l.state == "syncing"));
        b.stop_and_wait();
        // A one-off pass works too, and finds nothing left to do.
        let r = sync_once_at(&vb).unwrap();
        assert_eq!(r.pushed, 0);
    }

    fn remote_targets() -> Vec<(remote::mock::MockServer, SyncTarget)> {
        use quire_sync::remote::mock::{Kind, MockServer};
        let s3 = MockServer::start(Kind::S3 {
            bucket: "notes".into(),
            access_key: "AK".into(),
            secret_key: "SK/+".into(),
            region: "auto".into(),
        });
        let s3_target = SyncTarget::Remote {
            profile: RemoteProfile::S3 {
                endpoint: s3.origin(),
                region: "auto".into(),
                bucket: "notes".into(),
                prefix: "q".into(),
                virtual_hosted: false,
                allow_http: false,
            },
            secret: RemoteSecret::S3 { access_key: "AK".into(), secret_key: "SK/+".into(), session_token: None },
        };
        let dav = MockServer::start(Kind::WebDav { user: "asha".into(), password: "pw".into() });
        let dav_target = SyncTarget::Remote {
            profile: RemoteProfile::Webdav { url: dav.dav_url(), allow_http: false },
            secret: RemoteSecret::Webdav { user: "asha".into(), password: "pw".into() },
        };
        vec![(s3, s3_target), (dav, dav_target)]
    }

    /// 0.12: a vault syncs through S3 and WebDAV as through a folder: create,
    /// join (a wrong passphrase refused), selective sync, settings, new
    /// credentials, and no secret in the vault.
    #[test]
    fn s3_and_webdav_vaults_sync_end_to_end() {
        for (srv, target) in remote_targets() {
            let dir = tempfile::tempdir().unwrap();
            let (va, vb, vc) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("C"));
            for d in [&va, &vb, &vc] {
                std::fs::create_dir_all(d.join("Work")).unwrap();
            }
            std::fs::write(va.join("Plan.md"), "# Plan\none\n").unwrap();
            std::fs::write(va.join("Work/Task.md"), "work\n").unwrap();
            assert!(sync_test(&target).unwrap().starts_with("Connected"));
            sync_create_at(&va, &target, "correct horse battery", "laptop", &Selection::default()).unwrap();
            sync_once_at(&va).unwrap();
            let space = sync_list(&target).unwrap().remove(0).space;
            assert!(sync_join_at(&vb, &target, &space, "wrong passphrase", "phone", &Selection::default()).is_err());
            sync_join_at(&vb, &target, &space, "correct horse battery", "phone", &Selection::default()).unwrap();
            let only_work = Selection::new(&["Work".into()], &[]).unwrap();
            sync_join_at(&vc, &target, &space, "correct horse battery", "server", &only_work).unwrap();
            for v in [&vb, &vc] {
                sync_once_at(v).unwrap();
            }
            assert_eq!(files(&va), files(&vb));
            assert_eq!(files(&vc), ["Work/Task.md"], "selective sync holds only Work/");
            // Edits both ways, and the settings switch (no older readers to keep out).
            std::fs::write(vb.join("From phone.md"), "hi\n").unwrap();
            std::fs::write(vc.join("Work/Task.md"), "work\ndone\n").unwrap();
            sync_share_at(&va, true).unwrap();
            for _ in 0..2 {
                for v in [&va, &vb, &vc] {
                    sync_once_at(v).unwrap();
                }
            }
            assert_eq!(files(&va), files(&vb));
            assert_eq!(std::fs::read_to_string(va.join("Work/Task.md")).unwrap(), "work\ndone\n");
            assert!(!vc.join("Plan.md").exists());
            // config.json holds no secret, and Quire 0.11 can't take it for a server.
            let cfg_text = std::fs::read_to_string(va.join(".quire/sync/config.json")).unwrap();
            for secret in ["SK/+", "\"pw\"", "correct horse"] {
                assert!(!cfg_text.contains(secret), "{cfg_text}");
            }
            assert!(cfg_text.contains("remote_device") && !cfg_text.contains("\"device\""), "{cfg_text}");
            let summary = sync_summary_at(&va).unwrap();
            assert!(summary.has_secrets);
            assert!(summary.config.transport_label().contains("S3 bucket notes/q") || summary.config.transport_label().contains("WebDAV"));
            // Expired credentials say so; new ones are checked before they're kept.
            srv.state.expired.store(true, Ordering::Relaxed);
            let e = sync_once_at(&va).unwrap_err();
            assert!(e.contains("expired") || e.contains("user name or password"), "{e}");
            srv.state.expired.store(false, Ordering::Relaxed);
            let SyncTarget::Remote { secret, .. } = &target else { unreachable!() };
            let wrong = match secret {
                RemoteSecret::S3 { access_key, .. } => {
                    RemoteSecret::S3 { access_key: access_key.clone(), secret_key: "nope".into(), session_token: None }
                }
                RemoteSecret::Webdav { user, .. } => RemoteSecret::Webdav { user: user.clone(), password: "nope".into() },
            };
            assert!(sync_set_remote_secret_at(&va, &wrong).is_err());
            assert!(sync_set_remote_secret_at(&va, secret).unwrap().starts_with("Connected"));
            sync_once_at(&va).unwrap();
            // The storage only ever saw ciphertext and ids.
            for (k, v) in srv.objects() {
                assert!(!k.contains("Plan") && !String::from_utf8_lossy(&v).contains("Plan"), "{k}");
            }
        }
    }

    /// `sync move`: from a synced folder to WebDAV; the other device joins
    /// the new vault and its notes are adopted, not doubled.
    #[test]
    fn a_vault_moves_from_a_folder_to_webdav() {
        let (_srv, dav) = remote_targets().pop().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Dropbox"));
        for d in [&va, &vb, &shared] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
        let folder = SyncTarget::Folder(shared);
        sync_create_at(&va, &folder, "correct horse battery", "laptop", &Selection::default()).unwrap();
        sync_once_at(&va).unwrap();
        let space = sync_list(&folder).unwrap().remove(0).space;
        sync_join_at(&vb, &folder, &space, "correct horse battery", "phone", &Selection::default()).unwrap();
        sync_once_at(&vb).unwrap();
        std::fs::write(va.join("Late.md"), "written just before the move\n").unwrap();
        let (_rk, last) = sync_move_at(&va, &dav, "another passphrase", false).unwrap();
        assert!(last.is_none());
        assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("WebDAV"));
        sync_once_at(&va).unwrap();
        // B (here in the same test process, so A's stop took the shared
        // key from this secrets file too): stop, then join the new vault.
        sync_stop_at(&vb);
        let new_space = sync_list(&dav).unwrap().remove(0).space;
        sync_join_at(&vb, &dav, &new_space, "another passphrase", "phone", &Selection::default()).unwrap();
        sync_once_at(&vb).unwrap();
        sync_once_at(&va).unwrap();
        assert_eq!(files(&va), ["Late.md", "Plan.md"]);
        assert_eq!(files(&va), files(&vb), "adopted, no copies");
    }

    /// 0.12 review (security L4, data M1): a move that fails to make the
    /// new vault leaves the old setup working; changes not yet uploaded to
    /// the old storage stop the move unless forced.
    #[test]
    fn a_failed_move_keeps_the_old_setup_and_unsent_changes_stop_it() {
        let (srv, dav) = remote_targets().pop().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (va, shared, other) = (dir.path().join("A"), dir.path().join("Dropbox"), dir.path().join("Elsewhere"));
        for d in [&va, &shared, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
        let folder = SyncTarget::Folder(shared);
        sync_create_at(&va, &folder, "correct horse battery", "laptop", &Selection::default()).unwrap();
        sync_once_at(&va).unwrap();
        let before = SyncConfig::load(&va).unwrap();
        // The test connection's writes succeed, then the storage drops the
        // connection: making the new vault fails.
        srv.state.puts_left.store(2, Ordering::Relaxed);
        assert!(sync_move_at(&va, &dav, "another passphrase", false).is_err());
        let after = SyncConfig::load(&va).unwrap();
        assert_eq!((&after.space, after.transport_label()), (&before.space, before.transport_label()));
        std::fs::write(va.join("Still.md"), "still syncing\n").unwrap();
        sync_once_at(&va).unwrap();
        // Now on WebDAV, with a change that can't be uploaded.
        srv.state.puts_left.store(u64::MAX, Ordering::Relaxed);
        sync_move_at(&va, &dav, "another passphrase", false).unwrap();
        sync_once_at(&va).unwrap();
        std::fs::write(va.join("Unsent.md"), "made offline\n").unwrap();
        // The pass's tip write goes through, then the connection drops: the
        // new revision stays in the outbox.
        srv.state.puts_left.store(1, Ordering::Relaxed);
        let to = SyncTarget::Folder(other);
        let e = sync_move_at(&va, &to, "third passphrase", false).unwrap_err();
        assert!(e.contains("change(s) made here haven't reached WebDAV"), "{e}");
        assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("WebDAV"));
        let (_rk, last) = sync_move_at(&va, &to, "third passphrase", true).unwrap();
        assert!(last.is_some());
        sync_once_at(&va).unwrap();
        assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("folder"));
    }

    /// Review finding 5: one sync process per vault and device. A second
    /// worker (the app next to `serve --sync`) waits and says so, then takes
    /// over; one-off commands refuse.
    #[test]
    fn one_sync_process_per_vault_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let (va, shared) = (dir.path().join("Vault"), dir.path().join("Syncthing"));
        for d in [&va, &shared] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
        sync_create_at(&va, &SyncTarget::Folder(shared), "correct horse battery", "server", &Selection::default()).unwrap();
        let first = SyncWorker::start_headless(&va, Duration::from_mins(1)).unwrap();
        let synced = |w: &SyncWorker, what: &str| {
            let start = Instant::now();
            loop {
                assert!(start.elapsed() < Duration::from_secs(30), "{what}");
                if let Some(SyncEvent::Status(SyncStatus::Synced(_))) = w.next_event(Duration::from_millis(100)) {
                    return;
                }
            }
        };
        synced(&first, "the first worker never synced");
        assert!(matches!(lock_sync(&va), Err(LockError::Busy { .. })));
        let once = sync_once_at(&va).unwrap_err();
        assert!(once.contains(ALREADY_SYNCING), "{once}");
        let scope = sync_set_selection_at(&va, &Selection::new(&["Work".into()], &[]).unwrap()).unwrap_err();
        assert!(scope.contains(ALREADY_SYNCING) && scope.contains("Stop it first"), "{scope}");
        assert!(SyncConfig::load(&va).unwrap().selection().is_everything(), "nothing changed");

        let second = SyncWorker::start_headless(&va, Duration::from_mins(1)).unwrap();
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < Duration::from_secs(30), "the second worker never said why it waits");
            match second.next_event(Duration::from_millis(100)) {
                Some(SyncEvent::Status(SyncStatus::Elsewhere(e))) => {
                    assert!(e.contains(ALREADY_SYNCING), "{e}");
                    break;
                }
                Some(SyncEvent::Status(SyncStatus::Synced(_))) => panic!("two workers synced the same vault"),
                _ => {}
            }
        }
        first.stop_and_wait();
        synced(&second, "the second worker never took over");
        second.stop_and_wait();
        sync_once_at(&va).unwrap();
        // The lock goes with the process; the file stays, harmlessly.
        assert!(sync_dir(&va).join("lock").exists());
        drop(lock_sync(&va).unwrap());
    }

    /// Quire 0.9 needs `device` in config.json: with a selection set, it
    /// doesn't find it, so it shows sync as off instead of syncing the
    /// folder from an empty state. Quire 0.10 reads both.
    #[test]
    fn a_selective_config_is_off_limits_to_0_9() {
        #[derive(Deserialize)]
        #[allow(dead_code, reason = "only parsed")]
        struct Config09 {
            space: SpaceId,
            device: DeviceId,
            device_name: String,
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SyncConfig {
            server: String::new(),
            folder: Some(dir.path().join("f")),
            space: SpaceId::random().unwrap(),
            device: DeviceId::random().unwrap(),
            device_name: "server".into(),
            shared_state: false,
            only: vec!["Work/".into()],
            exclude: Vec::new(),
            remote: None,
        };
        cfg.save(dir.path()).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(sync_dir(dir.path()).join("config.json")).unwrap()).unwrap();
        assert!(raw.get("device").is_none() && raw.get(PARTIAL_DEVICE).is_some());
        assert!(serde_json::from_value::<Config09>(raw).is_err(), "0.9 can't load it");
        assert_eq!(SyncConfig::load(dir.path()), Some(cfg.clone()));
        cfg.only.clear();
        cfg.save(dir.path()).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(sync_dir(dir.path()).join("config.json")).unwrap()).unwrap();
        assert!(serde_json::from_value::<Config09>(raw).is_ok(), "everything again: 0.9 syncs it as before");
        assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
    }

    /// A note changed by the editor (or the web app, or an agent) after the
    /// pass read it is never overwritten by sync, even between its check and
    /// its write.
    #[test]
    fn sync_writes_check_the_file_as_one_step() {
        let vault = Vault::in_memory(&[("Plan.md", "one\n")]);
        let mut store = VaultStore::new(vault.clone());
        snapshot_reads(&mut store);
        vault.write(&NotePath::new("Plan.md"), "one, saved meanwhile\n").unwrap();
        assert!(store.write("Plan.md", "from sync\n").is_err());
        assert!(store.remove("Plan.md").is_err());
        assert_eq!(vault.read(&NotePath::new("Plan.md")).unwrap(), "one, saved meanwhile\n");
        snapshot_reads(&mut store);
        store.write("Plan.md", "from sync\n").unwrap();
        assert_eq!(vault.read(&NotePath::new("Plan.md")).unwrap(), "from sync\n");
        assert!(store.write("New.md", "new\n").is_ok(), "a new file, where none was read");
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
            remote: None,
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
            remote: None,
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

        /// One pass of device `i` as Quire 0.9 runs it: both lanes, no
        /// review history of its own.
        fn pass_09(&mut self, i: usize) {
            let main = FolderTransport::new(&self.root, &self.space, self.devices[i].clone(), None);
            let mut notes = VaultStore::new(self.vaults[i].clone()).notes_only();
            snapshot_reads(&mut notes);
            self.engines[i].sync(&mut notes, &main, "2026-11-03").unwrap();
            let lane = FolderTransport::lane(&self.root, &self.space, self.devices[i].clone(), None, folder::SHARED_LANE);
            let mut shared = VaultStore::new(self.vaults[i].clone()).shared_only();
            snapshot_reads(&mut shared);
            self.lanes[i].sync(&mut shared, &lane, "2026-11-03").unwrap();
        }

        /// One pass of device `i` as 0.10 runs it with shared state on:
        /// publish review history, both lanes, take in the others'.
        fn pass_10(&mut self, i: usize, agent: &quire_agent::AgentStore) {
            export_review(agent);
            self.pass_09(i);
            import_review(agent);
        }
    }

    /// 0.10: a server's agent suggests, the desktop reviews, through the
    /// real engine and lanes, with a Quire 0.9 device in the same folder
    /// passing the shared review history on without reading or deleting it.
    #[test]
    fn review_history_travels_between_devices_and_0_9_passes_it_on() {
        let mut f = Fleet::new();
        let note = "# Launch\nThe venue is booked.\nWe ship on Monday.\nBring snacks.\n";
        f.vaults[0].write(&NotePath::new("Launch.md"), note).unwrap();
        let store = |i: usize, id: &str, name: &str| {
            quire_agent::AgentStore::open(
                &f.vaults[i],
                quire_agent::DeviceInfo { id: id.into(), name: name.into() },
                Arc::new(quire_agent::SystemClock),
            )
        };
        let (server, desk) = (store(0, "d_server", "home server"), store(2, "d_desk", "laptop"));
        let rounds = |f: &mut Fleet, n: usize| {
            for _ in 0..n {
                f.pass_10(0, &server);
                f.pass_09(1);
                f.pass_10(2, &desk);
            }
        };
        rounds(&mut f, 2);
        assert_eq!(f.vaults[2].read(&NotePath::new("Launch.md")).unwrap(), note);
        // The agent on the server suggests two changes.
        let agent = quire_agent::Actor::agent("oauth:claude.ai", "Claude", quire_agent::Via::McpHttp, "d_server");
        let new_text = "# Launch\nThe venue is booked for Friday.\nWe ship on Monday.\nBring snacks and water.\n";
        let quire_agent::SubmitOutcome::Proposed { id, .. } = server
            .submit(
                &agent,
                quire_agent::ChangeSet::one(quire_agent::Change::Edit { path: NotePath::new("Launch.md"), new_text: new_text.into() }),
            )
            .unwrap()
        else {
            panic!("a suggestion")
        };
        rounds(&mut f, 2);
        let pending = desk.proposals(&quire_agent::ProposalFilter::pending());
        assert_eq!(pending.len(), 1, "on the desktop");
        assert_eq!(pending[0].from_device.as_ref().map(|d| d.1.as_str()), Some("home server"));
        // The 0.9 device holds the files as they are, and reads none of them.
        let file = format!("agent/proposals/d_server/{id}.json");
        assert_eq!(f.shared(1, &file), f.shared(0, &file));
        assert!(f.vaults[1].storage().list_under(".quire/agent/proposals").unwrap_or_default().is_empty());
        // The desktop accepts the first hunk and rejects the second.
        let path = NotePath::new("Launch.md");
        let hunks = desk.review(&id, &path, note).hunks;
        assert_eq!(hunks.len(), 2);
        let d = |h: &quire_agent::Hunk, a| quire_agent::HunkDecision {
            hunk: h.id.clone(),
            action: a,
            at: 0,
            text: None,
            other: serde_json::Map::new(),
        };
        desk.accept_in_file(&id, &path, &[d(&hunks[0], quire_agent::HunkAction::Accepted)], None).unwrap();
        let now = f.vaults[2].read(&path).unwrap();
        let left = desk.review(&id, &path, &now).hunks;
        desk.decide_checked(&id, &path, &[d(&left[0], quire_agent::HunkAction::Rejected)], &now, None).unwrap();
        rounds(&mut f, 3);
        // Resolved on the server, the text there once (it came by note sync).
        assert!(server.proposals(&quire_agent::ProposalFilter::pending()).is_empty());
        let resolved = server.archived_proposals().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!(resolved.status, quire_agent::ProposalStatus::PartiallyApplied);
        for i in 0..3 {
            let t = f.vaults[i].read(&path).unwrap();
            assert_eq!(t.matches("for Friday").count(), 1, "device {i}: {t}");
            assert!(!t.contains("water"), "device {i}: {t}");
        }
        // Every device's runs, on the server too; nothing was deleted anywhere.
        assert!(
            server.log(&quire_agent::LedgerQuery::default()).iter().any(|e| e.op == quire_agent::Op::Accept && e.actor.device == "d_desk")
        );
        for i in 0..3 {
            assert!(f.shared(i, &file).is_some(), "device {i}");
            assert!(f.shared(i, &format!("agent/decisions/d_desk/{id}.json")).is_some(), "device {i}");
            assert!(f.shared(i, "agent/devices/d_server.json").is_some(), "device {i}");
        }
        assert!(f.vaults[1].storage().list_under(".quire/agent/proposals").unwrap_or_default().is_empty(), "0.9 never took it in");
        // No revision a 0.8 device could read names review history.
        let main = FolderTransport::new(&f.root, &f.space, DeviceId::random().unwrap(), None);
        for r in main.pull(0).unwrap() {
            let c = quire_sync::Content::open(&f.keys, &f.space, &r).unwrap();
            assert!(!c.path.starts_with(".quire"), "{}", c.path);
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
