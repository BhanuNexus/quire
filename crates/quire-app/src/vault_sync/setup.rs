//! Setting sync up and taking it down: `SyncHandle`, the create, join, move
//! and stop jobs that run off the UI thread, and the selection and summary
//! calls.

use super::{
    ALREADY_SYNCING, APP_POLL, Arc, DeviceId, DeviceRemoved, FolderTransport, Hardening, HeldDeletes, HttpTransport, KDF, LastPass, Link,
    LockError, Path, PathBuf, Receiver, RecoveryKey, RemoteSecret, RemoteTransport, Selection, Session, SpaceId, SyncConfig, SyncLock,
    SyncState, SyncStatus, SyncTarget, SyncVaultInfo, SyncWorker, VaultKey, Workspace, WrappedKey, folder, harden, key_name, load_state,
    lock_sync, open_parts, remote, remote_dir, save_state, secrets, state_file, sync_dir, sync_folder_root, sync_share_at, sync_test,
    token_name,
};

/// Run `f` on a thread of its own; its result arrives on the receiver.
pub(super) fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

/// Sync's part of the workspace.
#[derive(Debug, Default)]
pub struct SyncHandle {
    pub(super) worker: Option<SyncWorker>,
    pub status: SyncStatus,
    pub config: Option<SyncConfig>,
    /// A device removal running or done, kept here (not in Settings) so
    /// its one-time recovery key isn't lost when Settings closes.
    pub(super) removal: std::sync::Mutex<Removal>,
    /// Find vaults, Start syncing or Join running (or just finished), kept
    /// here so closing Settings never drops the result.
    pub(super) setup: SetupJob,
    /// Files already said to be too big to sync, this session.
    pub(super) held_back_said: std::collections::BTreeSet<String>,
    /// Deletes held back because they were many (0.13.2), as last read.
    pub held: Option<HeldDeletes>,
}

/// What Settings → Sync asks for in the background, one at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupKind {
    /// "Find vaults".
    List,
    /// "Start syncing".
    Create,
    /// "Join and sync".
    Join,
}

pub(super) enum SetupResult {
    Listed(Result<Vec<SyncVaultInfo>, String>),
    Created(Result<String, String>),
    Joined(Result<(), String>),
}

/// A finished setup step, for Settings to show (see [`Workspace::take_sync_setup`]).
pub enum SetupDone {
    /// The vaults found.
    Listed(Result<Vec<SyncVaultInfo>, String>),
    /// Sync is running; the recovery key, to show once.
    Created(String),
    /// Sync is running.
    Joined,
    /// The step failed.
    Failed(SetupKind, String),
}

impl std::fmt::Debug for SetupDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listed(_) => f.write_str("Listed"),
            Self::Created(_) => f.write_str("Created(<recovery key>)"),
            Self::Joined => f.write_str("Joined"),
            Self::Failed(k, _) => write!(f, "Failed({k:?})"),
        }
    }
}

#[derive(Default)]
pub(super) struct SetupJob {
    /// The step running, for which vault, and its result.
    pub(super) running: Option<(SetupKind, PathBuf, Receiver<SetupResult>)>,
    pub(super) done: Vec<SetupDone>,
}

impl std::fmt::Debug for SetupJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupJob").field("running", &self.running.as_ref().map(|r| r.0)).field("done", &self.done).finish()
    }
}

/// See [`SyncHandle`]'s `removal`.
#[derive(Debug, Default)]
pub(super) struct Removal {
    pub(super) running: Option<Receiver<Result<DeviceRemoved, String>>>,
    pub(super) done: Option<Result<DeviceRemoved, String>>,
    pub(super) announced: bool,
}

impl Removal {
    pub(super) fn poll(&mut self) {
        if let Some(rx) = &self.running {
            match rx.try_recv() {
                Ok(r) => {
                    self.done = Some(r);
                    self.running = None;
                    self.announced = false;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.running = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
    }
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
pub(super) fn raise_min_reader(cfg: &SyncConfig) -> Result<(), String> {
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
