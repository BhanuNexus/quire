//! The background sync thread (`SyncWorker`), its poll intervals, and the
//! one-shot `sync_once_at` the CLI uses.

use super::{
    ALREADY_SYNCING, Arc, AtomicBool, AtomicU64, Duration, FolderTransport, HttpTransport, Instant, Link, LockError, Msg, Ordering, Path,
    PathBuf, Receiver, Report, Sender, Session, SyncConfig, SyncEvent, SyncStatus, Vault, VaultKey, folder, lock_sync, read_key, secrets,
    sync_dir, token_name,
};
use quire_base::dates;

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

pub(super) fn today() -> String {
    dates::today_ymd()
}

/// How often the app syncs without a reason to (a save, another device's change).
pub const APP_POLL: Duration = Duration::from_mins(2);
/// How often S3 and WebDAV are checked for other devices' changes (or the
/// poll interval, when that's longer than the app's).
pub const REMOTE_POLL: Duration = Duration::from_mins(1);

impl SyncWorker {
    /// `manifest`: the app workspace's, which the session then shares; `None`
    /// opens its own.
    pub(super) fn start(
        root: PathBuf,
        vault: Vault,
        cfg: SyncConfig,
        token: String,
        key: VaultKey,
        poll: Duration,
        manifest: Option<quire_vault::changes::Manifest>,
    ) -> Self {
        let (tx, msgs) = crossbeam_channel::unbounded();
        let (events, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let known = Arc::new(AtomicU64::new(0));

        // Watch thread: tells the worker when another device pushed.
        {
            let (tx, stop, known) = (tx.clone(), Arc::clone(&stop), Arc::clone(&known));
            match Link::new(&cfg, &token, Some(&sync_dir(&root)), None) {
                // Can't even make a client (a bad address): the worker says why.
                Err(_) => std::thread::Builder::new().name("quire-sync-poll".into()).spawn(|| {}).expect("the system starts a thread"),
                // S3 and WebDAV: one listing of the devices' tips a minute
                // (see docs/sync-backends.md for what that costs).
                Ok(Link::Remote(t)) => std::thread::Builder::new()
                    .name("quire-sync-poll".into())
                    .spawn(move || {
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
                    })
                    .expect("the system starts a thread"),
                // A server answers a long poll as soon as something changes.
                Ok(Link::Server(mut http)) => {
                    let (server, space) = (cfg.server.clone(), cfg.space.clone());
                    std::thread::Builder::new()
                        .name("quire-sync-poll".into())
                        .spawn(move || {
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
                        .expect("the system starts a thread")
                }
                // A folder is checked every couple of seconds (sizes and
                // times only, so it's cheap).
                Ok(Link::Folder(dir)) => {
                    let lane =
                        cfg.folder.as_ref().map(|r| FolderTransport::lane(r, &cfg.space, cfg.device.clone(), None, folder::SHARED_LANE));
                    std::thread::Builder::new()
                        .name("quire-sync-poll".into())
                        .spawn(move || {
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
                        .expect("the system starts a thread")
                }
            };
        }

        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("quire-sync".into())
            .spawn(move || {
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
                let mut session = match Session::open(root, vault, cfg, &token, key, lock, manifest) {
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
                            // Key changes first: a removed device's late lines must not go into a snapshot.
                            session.refresh_keys();
                            let said = match &session.key_stuck {
                                Some(why) => format!("Compaction waits: {why}"),
                                None => session.compact(true).unwrap_or_else(|e| format!("Compaction failed: {e}")),
                            };
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
            })
            .expect("the system starts a thread");
        Self { tx, rx, stop, thread: Some(thread) }
    }

    /// Sync the vault at `root` in the background, as set up by
    /// [`super::sync_join_at`] or [`super::sync_create_at`], without the app: passes run
    /// when another device changes something, after [`Self::nudge`], and
    /// every `poll` otherwise.
    pub fn start_headless(root: &Path, poll: Duration) -> Result<Self, String> {
        let (vault, cfg, token, key) = open_parts(root)?;
        Ok(Self::start(root.to_path_buf(), vault, cfg, token, key, poll, None))
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
pub(super) fn open_parts(root: &Path) -> Result<(Vault, SyncConfig, String, VaultKey), String> {
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

/// One sync of the vault at `root`, in this thread (`quire-cli sync once`):
/// a pass, and more while a first sync of a large vault has notes left to
/// send.
pub fn sync_once_at(root: &Path) -> Result<Report, String> {
    sync_pass_at(root, false, &mut |_, _| {})
}

/// [`sync_once_at`], telling `progress` (sent, of) after each pass that
/// left notes to send.
pub fn sync_once_with(root: &Path, verify: bool, progress: &mut dyn FnMut(usize, usize)) -> Result<Report, String> {
    sync_pass_at(root, verify, progress)
}

/// [`sync_once_at`], reading every note again first: a change that kept its
/// file's size and time is found too (`docs/design/0.14-sync-speed.md` §3.1;
/// a running sync does this by itself once a day).
pub fn sync_verify_at(root: &Path) -> Result<Report, String> {
    sync_pass_at(root, true, &mut |_, _| {})
}

fn sync_pass_at(root: &Path, verify: bool, progress: &mut dyn FnMut(usize, usize)) -> Result<Report, String> {
    let (vault, cfg, token, key) = open_parts(root)?;
    let lock = lock_sync(root).map_err(|e| match e {
        LockError::Busy { .. } => format!("{ALREADY_SYNCING}; it keeps this vault in sync, nothing to do here."),
        LockError::Io(_) => e.to_string(),
    })?;
    let mut session = Session::new(root.to_path_buf(), vault, cfg, &token, key, lock)?;
    session.verify = verify;
    if let Some(notice) = session.too_old() {
        return Err(notice);
    }
    let mut all = Report::default();
    let mut sent = 0;
    loop {
        let pass = session.pass(APP_POLL);
        let report = pass.report.unwrap_or_default();
        let more = report.more_to_send();
        sent += report.pushed;
        if more {
            progress(sent, sent + report.left);
        }
        let status = if more && matches!(pass.status, SyncStatus::Syncing) { SyncStatus::Synced(Instant::now()) } else { pass.status };
        match status {
            SyncStatus::Synced(_) | SyncStatus::Waiting(_) => {}
            SyncStatus::Syncing => return Err("a note changed while syncing, or other devices were busy; run it again".into()),
            SyncStatus::Offline(e) | SyncStatus::Error(e) | SyncStatus::Elsewhere(e) => return Err(e),
            SyncStatus::Off => return Err("sync is off".into()),
        }
        all.pulled += report.pulled;
        all.pushed += report.pushed;
        all.merged.extend(report.merged);
        all.conflict_copies.extend(report.conflict_copies);
        all.kept_over_delete.extend(report.kept_over_delete);
        all.held_back.extend(report.held_back);
        all.skipped.extend(report.skipped);
        all.notices.extend(report.notices);
        if !more {
            return Ok(all);
        }
        session.verify = false;
    }
}
