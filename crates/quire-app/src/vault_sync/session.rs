//! One sync session: the link to a transport, `Session` and its passes
//! (enroll, compact, key refresh), the review import and export, and `LastPass`.

use super::{
    Arc, BTreeMap, Deserialize, DeviceSecret, Duration, Engine, FolderTransport, HELD_DELETES, Hardening, HeldDeletes, HttpTransport,
    Instant, KeySet, LanePos, ObjectStore, Path, PathBuf, Pins, RemoteProfile, RemoteSecret, RemoteTransport, Report, SHARED_STATE_FILE,
    Serialize, SpaceAdmin, SyncConfig, SyncLock, SyncStatus, Transport, Vault, VaultKey, VaultStore, folder, harden, open_state_named,
    runs_shared_lane, save_state, save_state_named, scrub_locked_text, secrets, snapshot_reads, state_file, sync_dir, today, token_name,
};

pub(super) enum Msg {
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
pub(super) enum Link {
    Server(HttpTransport),
    Folder(Box<FolderTransport>),
    Remote(Box<RemoteTransport>),
}

/// Where S3 and WebDAV sync keep their mirror (`.quire/sync/remote`).
pub(super) fn remote_dir(root: &Path) -> PathBuf {
    sync_dir(root).join("remote")
}

/// The storage client for a config, with the credentials in `secret` (JSON).
fn open_store(profile: &RemoteProfile, secret: &str) -> Result<Arc<dyn ObjectStore>, String> {
    profile.open(&RemoteSecret::from_json(secret)?)
}

impl Link {
    /// `token` is a server's token, or the storage's credentials (JSON).
    /// `root` is the vault folder (for a remote mirror), when syncing.
    pub(super) fn new(cfg: &SyncConfig, token: &str, state_dir: Option<&Path>, root: Option<&Path>) -> Result<Self, String> {
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

    pub(super) fn transport(&self) -> &dyn Transport {
        match self {
            Self::Server(t) => t,
            Self::Folder(t) => &**t,
            Self::Remote(t) => &**t,
        }
    }

    /// Check what's read with the vault's keys (folder, S3, WebDAV).
    pub(super) fn with_keys(self, keys: &KeySet) -> Self {
        match self {
            Self::Folder(t) => Self::Folder(Box::new(t.with_keys(keys.clone()))),
            Self::Remote(t) => Self::Remote(Box::new(t.with_keys(keys.clone()))),
            s @ Self::Server(_) => s,
        }
    }

    /// The vault-level side (0.13): device records, key changes, upgrade.
    pub(super) fn admin(&self) -> &dyn SpaceAdmin {
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
pub(super) struct Session {
    root: PathBuf,
    vault: Vault,
    cfg: SyncConfig,
    key: VaultKey,
    pub(super) link: Link,
    pub(super) engine: Engine,
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
    pub(super) key_stuck: Option<String>,
    /// Compaction was tried this session.
    compacted: bool,
    /// Things to tell the person with the next pass's report (0.13.2).
    notices: Vec<String>,
    /// Held for the session's life: no other process syncs this vault here.
    _lock: SyncLock,
}

/// The outcome of one pass, and when to try again without a nudge.
pub(super) struct Pass {
    pub(super) status: SyncStatus,
    pub(super) report: Option<Report>,
    pub(super) again: Duration,
}

impl Session {
    pub(super) fn new(root: PathBuf, vault: Vault, cfg: SyncConfig, token: &str, key: VaultKey, lock: SyncLock) -> Result<Self, String> {
        let keys = harden::keyset(&cfg.space, &key);
        // Every revision fetched (S3, WebDAV) or read (a folder) is decrypted
        // before it's used, so a damaged one is skipped rather than stopping sync.
        let link = Link::new(&cfg, token, Some(&sync_dir(&root)), Some(&root))?.with_keys(&keys);
        let (state, notice) = open_state_named(&root, state_file(&cfg))?;
        let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), keys.clone(), state);
        // In a sync folder, vault settings travel in a lane of their own,
        // which 0.8 never reads; the notes lane never touches them.
        let (selection, selection_warnings) = cfg.selection_with_warnings();
        let store = if cfg.folder.is_some() {
            VaultStore::new(vault.clone()).notes_only()
        } else {
            VaultStore::new(vault.clone()).with_shared_state(cfg.shared_state)
        }
        .with_selection(selection);
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
            notices: notice.into_iter().chain(selection_warnings).collect(),
            _lock: lock,
        })
    }

    /// 0.13, at the start of a pass: on a 0.13 server, swap the server's
    /// token for this device's own; check the vault's level never went
    /// down; publish this device's record; take any change of the vault's
    /// key (a device was removed), and give the transports what removed
    /// devices and older keys may no longer write.
    pub(super) fn refresh_keys(&mut self) {
        if let Link::Server(_) = &self.link
            && !Hardening::load(&self.root).device_token
        {
            self.enroll();
        }
        // The highest level seen here is the floor: lower in the storage
        // stops sync (a removed device lowering it to have new notes
        // sealed with the key it still has).
        let mut h = Hardening::load(&self.root);
        match self.link.admin().level() {
            Ok(l) if l < h.level => {
                self.key_stuck = Some(folder::LOWERED.to_string());
                return;
            }
            Ok(l) if l > h.level => {
                h.level = l;
                let _ = h.save(&self.root);
            }
            Err(quire_sync::TransportError::Refused(why)) if why == folder::LOWERED => {
                self.key_stuck = Some(why);
                return;
            }
            // Unreadable now (offline): the pass says so itself.
            Ok(_) | Err(_) => {}
        }
        match &self.link {
            Link::Folder(t) => t.set_floor(h.level),
            Link::Remote(t) => t.set_floor(h.level),
            Link::Server(_) => {}
        }
        if let Some((t, _, _)) = &self.lane {
            t.set_floor(h.level);
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
        let lanes = |lane: Option<&(FolderTransport, Engine, VaultStore)>| -> BTreeMap<String, LanePos> {
            lane.and_then(|(t, _, _)| t.own_end().ok()).map(|p| BTreeMap::from([(folder::SHARED_LANE.to_string(), p)])).unwrap_or_default()
        };
        if !self.refreshed {
            // Once a session (a server before 0.13 has nowhere to put it).
            let _ = quire_sync::admin::publish_self(
                admin,
                &self.cfg.space,
                &self.cfg.device,
                &secret,
                &self.cfg.device_name,
                &self.keys,
                &lanes(self.lane.as_ref()),
            );
            self.refreshed = true;
        }
        let Ok(r) = quire_sync::admin::refresh_keys_trusting(
            admin,
            &self.cfg.space,
            &self.cfg.device,
            Some(&secret),
            &self.keys,
            &mut self.pins,
            &h.trust,
        ) else {
            return;
        };
        harden::save_pins(&self.root, &self.pins);
        if r.keys.current() > self.keys.current() && harden::store_keys(&self.cfg.space, &r.keys).is_ok() {
            if let Some(k) = r.keys.current_raw() {
                self.key = k.clone();
            }
            // A newer key: this device's record says so again, and vouches
            // for its lanes up to here.
            let _ = quire_sync::admin::publish_self(
                admin,
                &self.cfg.space,
                &self.cfg.device,
                &secret,
                &self.cfg.device_name,
                &r.keys,
                &lanes(self.lane.as_ref()),
            );
        }
        self.keys = r.keys.clone();
        self.engine.set_keys(r.keys.clone());
        admin.set_removed(harden::removed_lanes(&r.removed, ""));
        admin.set_rules(r.rules(""));
        if let Some((t, e, _)) = self.lane.as_mut() {
            t.set_keys(r.keys.clone());
            t.set_removed(harden::removed_lanes(&r.removed, folder::SHARED_LANE));
            t.set_rules(r.rules(folder::SHARED_LANE));
            e.set_keys(r.keys.clone());
        }
        self.key_stuck = r.stuck.map(|(_, why)| quire_sync::admin::stuck_notice(&why));
    }

    /// Swap the server's token for one of this device's own (a 0.13
    /// server): the server can then refuse this device alone if it's removed.
    fn enroll(&mut self) {
        let store = secrets::store();
        let Some(token) = store.get(&token_name(&self.cfg.space)) else { return };
        if token.starts_with("qd_") {
            // Already this device's own (noted before, then lost): a server
            // gives a device one token only.
            let mut h = Hardening::load(&self.root);
            h.device_token = true;
            let _ = h.save(&self.root);
            return;
        }
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
    pub(super) fn compact(&self, force: bool) -> Result<String, String> {
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
    pub(super) fn maybe_compact(&mut self) {
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
    pub(super) fn too_old(&self) -> Option<String> {
        if let Link::Server(_) = &self.link
            && let Ok(n @ quire_sync::Negotiated::TooOld { .. }) = HttpTransport::negotiate(&self.cfg.server)
        {
            return Some(n.notice().unwrap_or_default());
        }
        None
    }

    /// One pass: pull, merge, push, for notes and (in a sync folder) the
    /// settings lane.
    pub(super) fn pass(&mut self, poll: Duration) -> Pass {
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
        if let Err(e) = snapshot_reads(&mut self.store) {
            // Never read as "every note was deleted here" (0.13.2).
            let status = SyncStatus::Error(quire_sync::SyncError::List(e).to_string());
            let _ = LastPass::record(&self.root, &status, None);
            return Pass { status, report: None, again: poll.min(Duration::from_secs(30)) };
        }
        // The deletes the last pass held back, if the person said (within
        // the last 15 minutes) they are meant.
        let held = HeldDeletes::load(root);
        self.engine.allowed_deletes = held.as_ref().map(HeldDeletes::allowed_now).unwrap_or_default();
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
        let mut saved = save_state(root, cfg, &self.engine.state);
        if res.is_ok() {
            self.store.remember_unselected();
        }
        self.engine.allowed_deletes.clear();
        if self.lane.is_none()
            && runs_shared_lane(root, cfg)
            && let Some(dir) = &cfg.folder
        {
            let t = FolderTransport::lane(dir, &cfg.space, cfg.device.clone(), Some(&sync_dir(root)), folder::SHARED_LANE)
                .with_keys(self.keys.clone());
            match open_state_named(root, SHARED_STATE_FILE) {
                Ok((state, notice)) => {
                    self.notices.extend(notice);
                    let e = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), self.keys.clone(), state);
                    self.lane = Some((t, e, VaultStore::new(self.vault.clone()).shared_only()));
                }
                Err(e) => saved = saved.and(Err(e)),
            }
        }
        if let Some((t, e, st)) = self.lane.as_mut()
            && res.is_ok()
        {
            // Vault settings deletes held back in this lane are allowed the same way.
            e.allowed_deletes = held.as_ref().map(HeldDeletes::allowed_now).unwrap_or_default();
            let r = snapshot_reads(st).map_err(quire_sync::SyncError::List).and_then(|()| e.sync(st, &*t, &today()));
            e.allowed_deletes.clear();
            let lane_saved = save_state_named(root, SHARED_STATE_FILE, &e.state);
            saved = saved.and(lane_saved);
            res = match (res, r) {
                (Ok(mut a), Ok(b)) => {
                    a.pulled += b.pulled;
                    a.pushed += b.pushed;
                    a.merged.extend(b.merged);
                    a.conflict_copies.extend(b.conflict_copies);
                    a.kept_over_delete.extend(b.kept_over_delete);
                    a.held_back.extend(b.held_back);
                    a.skipped.extend(b.skipped);
                    Ok(a)
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            };
            if !cfg.shared_state {
                self.store.shared = true;
            }
        }
        match &res {
            // Sent, or nothing to send any more (the notes came back).
            Ok(_) if held.is_some() => {
                let _ = std::fs::remove_file(sync_dir(root).join(HELD_DELETES));
            }
            Err(quire_sync::SyncError::ManyDeletes { notes, .. }) => HeldDeletes::new(notes, false).keep(root, held.as_ref()),
            Err(quire_sync::SyncError::SettingsDeletes { notes }) => HeldDeletes::new(notes, true).keep(root, held.as_ref()),
            _ => {}
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
            Ok(mut report) => {
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
                report.notices.append(&mut self.notices);
                (status, (report.changed_anything() || report.has_news()).then_some(report), again)
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
            Err(e @ (quire_sync::SyncError::ManyDeletes { .. } | quire_sync::SyncError::SettingsDeletes { .. })) => (
                SyncStatus::Error(format!(
                    "{e}. If you deleted them on purpose, choose Send these deletes in Settings → Sync (or run `quire-cli sync allow-deletes \"{}\"`). If not, check that the vault's folder is all there (a drive or cloud folder that went missing); sync goes on by itself once the notes are back.",
                    root.display()
                )),
                None,
                Duration::from_mins(5).min(poll.max(Duration::from_secs(30))),
            ),
            Err(e) => (SyncStatus::Error(e.to_string()), None, Duration::from_mins(5).min(poll.max(Duration::from_secs(30)))),
        };
        // Sync's own record couldn't be saved: the notes are fine, but the
        // next pass may redo (or copy aside) what this one did.
        let status = match (saved, status) {
            (Err(e), SyncStatus::Synced(_) | SyncStatus::Waiting(_)) => SyncStatus::Error(format!(
                "Couldn't save what sync did in {} ({e}). Your notes are fine; sync tries again.",
                sync_dir(root).display()
            )),
            (_, s) => s,
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
