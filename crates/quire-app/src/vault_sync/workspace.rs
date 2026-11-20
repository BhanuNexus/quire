//! The `impl Workspace` side of sync: the methods the app and the CLI call to
//! set sync up, run a pass, change its settings and read its status.

use super::{
    APP_POLL, DeviceRemoved, Engine, FolderTransport, HeldDeletes, HeldId, HttpTransport, Link, NotePath, NoticeLevel, Path, PathBuf, Pins,
    Receiver, RecoveryKey, RemoteSecret, RemoteTransport, Selection, SetupDone, SetupKind, SetupResult, SpaceId, SpaceInfo, SyncConfig,
    SyncDevices, SyncEvent, SyncHandle, SyncStatus, SyncTarget, SyncVaultInfo, SyncVersion, SyncWorker, UpgradeInfo, Vault, Workspace,
    folder, harden, in_background, journal, key_name, load_state, raise_min_reader, read_key, remote, remote_dir, secrets, skipped_notice,
    sync_allow_deletes_at, sync_create_at, sync_devices_at, sync_dir, sync_join_at, sync_remove_device_at, sync_set_remote_secret_at,
    sync_test, sync_upgrade_at, sync_upgrade_info_at, token_name,
};

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

    /// Drop sync's state for this vault. A setup step still running stays:
    /// its result is dealt with when it arrives, whatever vault is open then.
    fn reset_sync(&mut self) {
        let setup = std::mem::take(&mut self.sync.setup);
        self.sync = SyncHandle { setup, ..SyncHandle::default() };
    }

    fn start_setup(&mut self, kind: SetupKind, work: impl FnOnce(PathBuf) -> SetupResult + Send + 'static) -> Result<(), String> {
        if let Some((running, ..)) = &self.sync.setup.running {
            return Err(match running {
                SetupKind::List => "Still looking for vaults. Wait for it to finish.",
                SetupKind::Create => "Sync is still being set up. Wait for it to finish.",
                SetupKind::Join => "Still joining. Wait for it to finish.",
            }
            .into());
        }
        let root =
            if kind == SetupKind::List { self.vault_root().map(Path::to_path_buf).unwrap_or_default() } else { self.sync_parts()?.0 };
        // Older errors and lists are stale now.
        self.sync.setup.done.retain(|d| matches!(d, SetupDone::Created(_)));
        let rx = in_background({
            let root = root.clone();
            move || work(root)
        });
        self.sync.setup.running = Some((kind, root, rx));
        Ok(())
    }

    /// "Find vaults", off the UI thread. The result waits for
    /// [`Self::take_sync_setup`]. One setup step runs at a time.
    pub fn sync_list_in_background(&mut self, target: SyncTarget) -> Result<(), String> {
        self.start_setup(SetupKind::List, move |_| SetupResult::Listed(Self::sync_list(&target)))
    }

    /// [`Self::sync_create`] off the UI thread (the passphrase is stretched
    /// and the storage is reached). The workspace starts sync when it is
    /// done, even if Settings was closed; the recovery key waits for
    /// [`Self::take_sync_setup`]. One setup step runs at a time.
    pub fn sync_create_in_background(&mut self, target: SyncTarget, passphrase: String, device_name: String) -> Result<(), String> {
        self.start_setup(SetupKind::Create, move |root| {
            let passphrase = remote::Zeroizing::new(passphrase);
            SetupResult::Created(sync_create_at(&root, &target, &passphrase, &device_name, &Selection::default()).map(|rk| rk.to_string()))
        })
    }

    /// [`Self::sync_join`] off the UI thread, as [`Self::sync_create_in_background`].
    pub fn sync_join_in_background(
        &mut self,
        target: SyncTarget,
        space: SpaceId,
        secret: String,
        device_name: String,
    ) -> Result<(), String> {
        self.start_setup(SetupKind::Join, move |root| {
            let secret = remote::Zeroizing::new(secret);
            SetupResult::Joined(sync_join_at(&root, &target, &space, &secret, &device_name, &Selection::default()))
        })
    }

    /// The setup step running, if any.
    pub fn sync_setup_running(&self) -> Option<SetupKind> {
        self.sync.setup.running.as_ref().map(|r| r.0)
    }

    /// What finished since the last call (the recovery key is shown once).
    pub fn take_sync_setup(&mut self) -> Vec<SetupDone> {
        std::mem::take(&mut self.sync.setup.done)
    }

    /// Take in a finished setup step. Returns true when something changed.
    fn poll_setup(&mut self) -> bool {
        use crossbeam_channel::TryRecvError::{Disconnected, Empty};
        let Some((kind, root, rx)) = &self.sync.setup.running else { return false };
        let (kind, root) = (*kind, root.clone());
        let result = match rx.try_recv() {
            Ok(r) => Ok(r),
            Err(Empty) => return false,
            Err(Disconnected) => Err(()),
        };
        self.sync.setup.running = None;
        let same_vault = self.vault_root() == Some(root.as_path());
        let done = match result {
            Err(()) => SetupDone::Failed(kind, "Setting up sync stopped unexpectedly. Try again.".into()),
            Ok(SetupResult::Listed(l)) => SetupDone::Listed(l),
            Ok(SetupResult::Created(Err(e)) | SetupResult::Joined(Err(e))) => SetupDone::Failed(kind, e),
            Ok(SetupResult::Created(Ok(rk))) => {
                if same_vault {
                    self.start_sync();
                    SetupDone::Created(rk)
                } else {
                    self.notify_other_vault(&root);
                    return true;
                }
            }
            Ok(SetupResult::Joined(Ok(()))) => {
                if same_vault {
                    self.start_sync();
                    SetupDone::Joined
                } else {
                    self.notify_other_vault(&root);
                    return true;
                }
            }
        };
        match &done {
            SetupDone::Failed(_, e) => self.notify(NoticeLevel::Warn, format!("Sync setup failed: {e}")),
            SetupDone::Created(_) => {
                self.notify(NoticeLevel::Info, "Sync is on. Settings → Sync shows the recovery key. Write it down.");
            }
            SetupDone::Joined => self.notify(NoticeLevel::Info, "Sync is on for this vault."),
            SetupDone::Listed(_) => {}
        }
        self.sync.setup.done.push(done);
        true
    }

    fn notify_other_vault(&mut self, root: &Path) {
        self.notify(
            NoticeLevel::Warn,
            format!(
                "Sync was set up for the vault at {}, but another vault is open now. Open that vault and look in Settings → Sync (the recovery key is there).",
                root.display()
            ),
        );
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
            self.reset_sync();
            return;
        };
        let Some(cfg) = SyncConfig::load(&root) else {
            self.reset_sync();
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
        // One manifest per vault: the one the index loads through.
        self.sync.worker = Some(SyncWorker::start(root, vault, cfg.clone(), token, key, APP_POLL, self.manifest().cloned()));
        self.sync.config = Some(cfg);
        self.sync.status = SyncStatus::Syncing;
    }

    /// Whether vault settings and review history sync for this vault.
    pub fn sync_shares_state(&self) -> bool {
        self.sync.config.as_ref().is_some_and(|c| c.shared_state)
    }

    /// "Also sync vault settings and review history". A 0.8
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
            std::thread::Builder::new()
                .name("quire-sync-shared".into())
                .spawn(move || {
                    let _ = tx.send(raise_min_reader(&cfg));
                })
                .expect("the system starts a thread");
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
        self.reset_sync();
        let dir = sync_dir(&root);
        let _ = std::fs::remove_file(dir.join("config.json"));
        journal::remove(&root, "state.json");
        journal::remove(&root, "state-partial.json");
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
    /// Remove a device off the UI thread. The result waits here, for
    /// [`Self::take_sync_removal`], even if Settings closes meanwhile.
    pub fn sync_remove_device_in_background(&self, device: String, passphrase: String) -> Result<(), String> {
        let (root, _) = self.sync_parts()?;
        let mut r = self.sync.removal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if r.running.is_some() {
            return Err("A device is being removed already.".into());
        }
        let pass = remote::Zeroizing::new(passphrase);
        r.running = Some(in_background(move || sync_remove_device_at(&root, &device, &pass)));
        Ok(())
    }

    /// Whether a device removal is still running.
    pub fn sync_removal_running(&self) -> bool {
        let mut r = self.sync.removal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        r.poll();
        r.running.is_some()
    }

    /// The finished device removal, once (its recovery key is shown once).
    pub fn take_sync_removal(&self) -> Option<Result<DeviceRemoved, String>> {
        let mut r = self.sync.removal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        r.poll();
        r.done.take()
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
        std::thread::Builder::new()
            .name("quire-sync-test".into())
            .spawn(move || {
                let _ = tx.send(sync_test(&target));
            })
            .expect("the system starts a thread");
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
        std::thread::Builder::new()
            .name("quire-sync-secret".into())
            .spawn(move || {
                let _ = tx.send(sync_set_remote_secret_at(&root, &secret));
            })
            .expect("the system starts a thread");
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
        std::thread::Builder::new()
            .name("quire-sync-review".into())
            .spawn(move || {
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
                let keys = link.as_ref().ok().and_then(|l| l.admin().key_records().ok()).map_or_else(
                    || keys.clone(),
                    |recs| quire_sync::keyring::resolve(&keys, &recs, &cfg.device, None, &Pins::default()).keys,
                );
                let link = link.map(|l| l.with_keys(&keys));
                let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), keys, load_state(&root, &cfg));
                let _ = tx.send(link.and_then(|l| engine.history(l.transport(), &path).map_err(|e| e.to_string())));
            })
            .expect("the system starts a thread");
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

    /// Send the many deletes sync held back, now, if they are still the
    /// set shown (`seen`; see [`sync_allow_deletes_at`]). On a refusal the
    /// held set is read again, so the new list shows.
    pub fn sync_allow_deletes(&mut self, seen: &HeldId) -> Result<usize, String> {
        let (root, _) = self.sync_parts()?;
        let r = sync_allow_deletes_at(&root, seen);
        self.sync.held = HeldDeletes::load(&root);
        if r.is_ok() {
            self.sync_nudge();
        }
        r
    }

    /// The recovery key for this vault, to show again in Settings.
    pub fn sync_recovery_key(&self) -> Option<String> {
        let cfg = self.sync.config.as_ref()?;
        secrets::store().get(&key_name(&cfg.space)).map(|k| k.split_once(':').map_or_else(|| k.clone(), |(_, rk)| rk.to_string()))
    }

    /// Collect worker events. Returns true when something changed.
    pub(crate) fn poll_sync(&mut self) -> bool {
        let setup = self.poll_setup();
        // A removal finished while Settings was closed: say where its key is.
        let finished = {
            let mut r = self.sync.removal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            r.poll();
            let say = r.done.is_some() && !r.announced;
            r.announced |= say;
            say
        };
        if finished {
            self.notify(
                NoticeLevel::Warn,
                "A device was removed from sync. Open Settings → Sync → Devices and sync format to see the vault's new recovery key (shown once).",
            );
        }
        let finished = finished | setup;
        let Some(w) = &self.sync.worker else { return finished };
        let events = w.events();
        if events.is_empty() {
            return finished;
        }
        for e in events {
            match e {
                SyncEvent::Status(s) => {
                    if !matches!(s, SyncStatus::Syncing) {
                        self.sync.held = self.vault_root().and_then(HeldDeletes::load);
                    }
                    self.sync.status = s;
                }
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
                    for p in &r.held_back {
                        if self.sync.held_back_said.insert(p.clone()) {
                            self.notify(
                                NoticeLevel::Warn,
                                format!("“{p}” is over 10 MB, too big to sync: other devices keep their last copy until it's smaller."),
                            );
                        }
                    }
                    for (p, why) in &r.skipped {
                        self.notify(NoticeLevel::Warn, skipped_notice(p, why));
                    }
                    for n in r.notices {
                        self.notify(NoticeLevel::Warn, n);
                    }
                }
                SyncEvent::Notice(n) => self.notify(NoticeLevel::Info, n),
            }
        }
        true
    }
}
