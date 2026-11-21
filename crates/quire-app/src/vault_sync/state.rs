//! The sync engine's files in `.quire/sync/`: the per-process lock, state
//! load and save, and the shared lane's state.

use super::{NotePath, Path, PathBuf, SyncConfig, SyncState, Vault, folder, journal, sync_dir};

/// Where `config.json` keeps the device id while a selection is set: Quire
/// 0.9 needs `device`, so it doesn't take the file for a sync setup. 0.9
/// knows nothing of selections and keeps no state for them; syncing such a
/// folder it would start afresh over the notes already there.
pub(super) const PARTIAL_DEVICE: &str = "partial_device";
/// Where `config.json` keeps the device id for S3 or WebDAV sync (0.12).
pub(super) const REMOTE_DEVICE: &str = "remote_device";

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

/// For `quire-cli bench sync-013`: write this vault's sync record as
/// Quire 0.13 kept it (`state.json`), so the next pass migrates it once.
/// Its size in bytes. Not for anything but measuring the migration.
#[doc(hidden)]
pub fn bench_write_013_state(root: &Path) -> Result<usize, String> {
    let cfg = SyncConfig::load(root).ok_or("Sync isn't set up for this vault.")?;
    let _lock = lock_sync(root).map_err(|e| e.to_string())?;
    journal::write_013(root, state_file(&cfg))
}

/// Where iCloud keeps a note it evicted: `Notes/.Idea.md.icloud`.
pub(super) fn icloud_stub(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/.{name}.icloud"),
        None => format!(".{path}.icloud"),
    }
}

/// The engine state's file: `state.json`, or `state-partial.json` while a
/// selection is set. Quire 0.9, which knows nothing of selections, never
/// reads the latter: if it opened this folder it would start afresh, which
/// can't delete anything, rather than read notes outside the selection as
/// deleted.
/// Since 0.14 the state is a folder named after it (`state.d`, see
/// [`journal`]); the file is read once, to migrate.
pub(super) fn state_file(cfg: &SyncConfig) -> &'static str {
    if cfg.selection().is_everything() { "state.json" } else { "state-partial.json" }
}

/// The engine state as last saved, for a look (a fresh one when there's
/// none or it can't be read). A pass opens it with [`journal::Journal::open`].
pub(super) fn load_state(root: &Path, cfg: &SyncConfig) -> SyncState {
    journal::peek(root, state_file(cfg))
}

/// 0.11: no plain text of a locked folder's notes stays in the sync state.
/// A note that was plain there (before the folder was locked, or before it
/// was moved in) is gone from its path once the delete is sent, and its
/// text with it; what can stay longer is cleared here: the last synced text
/// of deleted notes and of copies kept outside the selection, at paths in
/// a locked folder. Their texts then leave the packs at the next commit
/// (`journal::Journal::commit`).
pub(super) fn scrub_locked_text(vault: &Vault, state: &mut SyncState) {
    let locked = |p: &str| in_locked_folder(vault, p);
    let empty = quire_sync::Print::of("");
    for t in state.files.values_mut() {
        // Deleted, or gone from its path while its delete waits (0.13.2):
        // either way its plain text is no longer on this disk.
        if locked(&t.path) && t.base != empty && (t.deleted || !vault.storage().exists(&NotePath::new(&t.path))) {
            t.base = empty;
            t.disk = None;
        }
        if let Some(k) = t.kept.as_mut().filter(|k| locked(&k.path)) {
            k.base = empty;
        }
    }
    for (p, left) in &mut state.left {
        if locked(p) {
            left.base = None;
        }
    }
}

/// Whether `path` is a note in a locked folder of `vault` now.
pub(super) fn in_locked_folder(vault: &Vault, path: &str) -> bool {
    !quire_sync::engine::is_attachment(path) && vault.locked_folder(&NotePath::new(path)).is_some()
}

/// The engine state of a sync folder's settings lane.
pub(super) const SHARED_STATE_FILE: &str = "state-shared.json";

/// Whether a sync folder's settings lane runs on this device: turned on
/// here, or started by another device and never run (so never turned off)
/// here.
pub(super) fn runs_shared_lane(root: &Path, cfg: &SyncConfig) -> bool {
    let Some(dir) = &cfg.folder else { return false };
    cfg.shared_state || (folder::lane_exists(dir, &cfg.space, folder::SHARED_LANE) && !journal::exists(root, SHARED_STATE_FILE))
}
