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

use std::collections::{BTreeMap, HashMap};
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
    DeviceId, Engine, HttpTransport, KdfParams, LanePos, LocalStore, RecoveryKey, Report, SpaceId, SyncState, Transport, VaultKey,
    WrappedKey,
};
use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::secrets;
use crate::workspace::{NoticeLevel, Workspace};

mod harden;
pub use harden::{
    DeviceRemoved, Hardening, SyncDevice, SyncDevices, UpgradeInfo, sync_compact_at, sync_devices_at, sync_remove_device_at,
    sync_trust_key_at, sync_upgrade_at, sync_upgrade_info_at,
};
use quire_sync::admin::SpaceAdmin;
use quire_sync::keyring::{DeviceSecret, KeySet, Pins};

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
    /// The files here by their name with capitals folded, when this disk
    /// doesn't tell capitals apart (0.13.2): filled with `seen`.
    folded: HashMap<String, String>,
    /// Whether the vault's disk ignores capital letters in names (found
    /// out once).
    case_blind: std::cell::Cell<Option<bool>>,
    /// The files outside the selection couldn't be listed this pass.
    unsure: std::cell::Cell<bool>,
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
            folded: HashMap::new(),
            case_blind: std::cell::Cell::new(None),
            unsure: std::cell::Cell::new(false),
        }
    }

    /// Whether the vault's disk ignores capital letters in names (Windows,
    /// macOS by default): `.quire` is found as `.QUIRE` too. A vault with
    /// no folder on disk (in memory) tells them apart.
    fn case_blind(&self) -> bool {
        if let Some(b) = self.case_blind.get() {
            return b;
        }
        let b = match self.vault.storage().absolute(&NotePath::new(".quire")) {
            None => false,
            Some(p) if p.exists() => p.with_file_name(".QUIRE").exists(),
            Some(_) => cfg!(any(windows, target_os = "macos")),
        };
        self.case_blind.set(Some(b));
        b
    }

    /// Whether `path` opens a file here that its folder doesn't list under
    /// exactly that name (the disk takes it for another name).
    fn answers_for_another(&self, path: &str) -> bool {
        let Some(abs) = self.vault.storage().absolute(&NotePath::new(path)) else { return false };
        let (Some(dir), Some(name)) = (abs.parent(), abs.file_name()) else { return false };
        abs.exists() && std::fs::read_dir(dir).is_ok_and(|rd| !rd.flatten().any(|e| e.file_name() == name))
    }

    /// Remember that `path` is here now (`true`) or gone, for names that
    /// differ only in capitals.
    fn note_name(&mut self, path: &str, here: bool) {
        let key = quire_sync::names::fold_case(path);
        if here {
            self.folded.insert(key, path.to_string());
        } else if self.folded.get(&key).is_some_and(|p| p == path) {
            self.folded.remove(&key);
        }
    }

    /// The files outside the selection: unselected notes and attachments.
    fn unselected(&self) -> Vec<NotePath> {
        if self.selection.is_everything() || self.lane == Lane::Shared {
            return Vec::new();
        }
        if let Ok(all) = self.vault.storage().list() {
            all.into_iter().filter(|p| !self.selection.includes(p.as_str())).collect()
        } else {
            // A note that left the selection can't be told from a deleted
            // one now: no delete is sent this pass (0.13.2).
            self.unsure.set(true);
            Vec::new()
        }
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
    /// A folder that can't be read is an error (0.13.2): its files would
    /// read as deleted. One that is gone (deleted meanwhile) has none.
    fn shared_files(&self) -> Result<Vec<NotePath>, String> {
        let root = SHARED_PREFIX.trim_end_matches('/');
        let Some(abs) = self.vault.storage().absolute(&NotePath::new(root)) else {
            return self.vault.storage().list_under(root).map_err(|e| e.to_string());
        };
        let mut out = Vec::new();
        let mut todo = vec![(abs, root.to_string())];
        while let Some((dir, rel)) = todo.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            };
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
        Ok(out)
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
    fn list(&self) -> Result<Vec<String>, String> {
        let notes = if self.lane == Lane::Shared {
            Vec::new()
        } else {
            self.vault
                .storage()
                .list()
                .map_err(|e| e.to_string())?
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
            let shared = self.shared_files()?;
            out.extend(
                shared
                    .into_iter()
                    .filter(|p| p.as_str().starts_with(SHARED_PREFIX) && !p.as_str().contains("/."))
                    .filter(|p| self.vault.storage().read_bytes(p).is_ok_and(|b| b.len() as u64 <= MAX_SHARED_BYTES))
                    .map(|p| p.as_str().to_string()),
            );
        }
        Ok(out)
    }

    fn missing(&self, path: &str) -> bool {
        // Only "not found" counts: a file that can't be read just now is
        // still here. So is one iCloud evicted to a `.Name.md.icloud` stub
        // ("Optimise Mac Storage"): it downloads again when opened.
        !self.outside_lane(path)
            && !self.unsure.get()
            && !self.vault.storage().exists(&NotePath::new(icloud_stub(path)))
            && matches!(self.vault.storage().read_bytes(&NotePath::new(path)), Err(quire_vault::VaultError::NotFound(_)))
    }

    fn is_settings(&self, path: &str) -> bool {
        // Vault settings, the agent policy, the locked-folders record and
        // review logs; not the review history the agent prunes itself.
        path.starts_with(SHARED_PREFIX) && !path.starts_with(SHARED_REVIEW_PREFIX)
    }

    fn counts_as_delete(&self, path: &str) -> bool {
        // App data the app prunes itself (review history, vault settings).
        if path.starts_with(".quire/") {
            return false;
        }
        // A note replaced by its encrypted file (locking a folder), or an
        // encrypted file by its plain note (unlocking for good).
        let exists = |p: &str| self.vault.storage().exists(&NotePath::new(p));
        let swapped = match path.strip_suffix(".qenc") {
            Some(plain) => exists(plain),
            None => exists(&format!("{path}.qenc")) || self.vault.locked_folder(&NotePath::new(path)).is_some(),
        };
        !swapped
    }

    fn unstorable(&self, path: &str, own: Option<&str>) -> Option<String> {
        if let Some(why) = quire_sync::names::problem_here(path) {
            return Some(why);
        }
        if !self.case_blind() || own.is_some_and(|o| quire_sync::names::same_ignoring_case(o, path)) {
            return None;
        }
        if let Some(other) = self.folded.get(&quire_sync::names::fold_case(path)).filter(|p| p.as_str() != path) {
            return Some(format!("“{other}” is already here, and this computer doesn't tell names apart by capital letters"));
        }
        // Another folding this disk applies (Unicode forms on macOS, say):
        // the name opens a file that isn't listed under it.
        if own.is_none() && self.seen.get(path).is_none_or(Option::is_none) && self.answers_for_another(path) {
            return Some("another file here has a name this computer takes as the same".into());
        }
        None
    }

    fn held_back(&self, path: &str) -> bool {
        self.lane != Lane::Shared
            && quire_sync::engine::is_attachment(path)
            && self.selection.includes(path)
            && self.size_and_time(path).is_some_and(|(n, _)| n > MAX_ATTACHMENT_BYTES)
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
            self.note_name(path, true);
            return Ok(());
        }
        if !self.vault.storage().write_bytes_if(&NotePath::new(path), &still, text.as_bytes()).map_err(|e| e.to_string())? {
            return Err(changed());
        }
        if !path.starts_with(SHARED_PREFIX) {
            self.keep_provenance(path, text);
        }
        self.seen.insert(path.to_string(), Some(text.to_string()));
        self.note_name(path, true);
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
            self.note_name(path, false);
            return Ok(());
        }
        // Deleted elsewhere: goes to the vault's trash, so it can come back.
        if !self.vault.storage().trash_if(&NotePath::new(path), &self.as_read(path)).map_err(|e| e.to_string())? {
            return Err("it changed while syncing".into());
        }
        self.seen.insert(path.to_string(), None);
        self.note_name(path, false);
        Ok(())
    }
}

/// `VaultStore` that records what it reads (the engine's view of `read` is
/// `&self`, so reads are recorded just before each pass instead).
///
/// A vault folder that can't be listed is an error (0.13.2).
fn snapshot_reads(store: &mut VaultStore) -> Result<(), String> {
    store.seen.clear();
    store.folded.clear();
    store.unsure.set(false);
    let blind = store.case_blind();
    for p in store.list()? {
        // Attachments are checked by fingerprint (see `unchanged_since_read`).
        let cur = if quire_sync::engine::is_attachment(&p) { store.digest(&p) } else { store.current(&p) };
        if blind {
            store.note_name(&p, true);
        }
        store.seen.insert(p, cur);
    }
    Ok(())
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
    /// Things to tell the person with the next pass's report (0.13.2).
    notices: Vec<String>,
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
        let (state, notice) = open_state_named(&root, state_file(&cfg))?;
        let engine = Engine::new(cfg.space.clone(), cfg.device.clone(), cfg.device_name.clone(), keys.clone(), state);
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
            notices: notice.into_iter().collect(),
            _lock: lock,
        })
    }

    /// 0.13, at the start of a pass: on a 0.13 server, swap the server's
    /// token for this device's own; check the vault's level never went
    /// down; publish this device's record; take any change of the vault's
    /// key (a device was removed), and give the transports what removed
    /// devices and older keys may no longer write.
    fn refresh_keys(&mut self) {
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

/// In `.quire/sync/`: the deletes a pass held back because they were many
/// ([`HeldDeletes`], 0.13.2).
const HELD_DELETES: &str = "held-deletes.json";

/// How long "Send these deletes" stays good for.
pub const ALLOW_DELETES_FOR: Duration = Duration::from_mins(15);

/// Deletes sync held back because they were many.
///
/// More than [`quire_sync::MANY_DELETES`] notes and more than half of the vault,
/// [`quire_sync::SyncError::ManyDeletes`]), and whether the person said
/// they are meant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldDeletes {
    /// When they were held back (Unix ms).
    pub at: i64,
    pub notes: Vec<HeldNote>,
    /// Vault settings files (`.quire/shared/`), not notes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settings: bool,
    /// Until when they may be sent (Unix ms): set by "Send these deletes".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_until: Option<i64>,
}

/// One note whose delete is held back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldNote {
    pub file: String,
    pub path: String,
}

/// Which held-back set the person saw (0.13.2): how many and a digest of
/// their ids. Approving checks it, so it never covers more than was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldId {
    pub count: usize,
    pub digest: String,
}

impl HeldDeletes {
    fn new(notes: &[(quire_sync::FileId, String)], settings: bool) -> Self {
        let mut notes: Vec<HeldNote> = notes.iter().map(|(f, p)| HeldNote { file: f.0.clone(), path: p.clone() }).collect();
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        Self { at: crate::history_ops::now_ms(), notes, settings, allowed_until: None }
    }

    /// Keep this set as the one held back, unless it is what `before`
    /// holds already (keeping its approval). A different set (more went
    /// missing, or others than were allowed) is asked again from scratch.
    fn keep(self, root: &Path, before: Option<&Self>) {
        if before.is_none_or(|h| h.notes != self.notes) {
            let _ = self.save(root);
        }
    }

    /// What identifies this set for an approval.
    pub fn id(&self) -> HeldId {
        let mut ids: Vec<&str> = self.notes.iter().map(|n| n.file.as_str()).collect();
        ids.sort_unstable();
        HeldId { count: ids.len(), digest: quire_vault::ContentHash::of(&ids.join("\n")).hex().to_string() }
    }

    /// The deletes held back for the vault at `root`, if any.
    pub fn load(root: &Path) -> Option<Self> {
        let b = std::fs::read(sync_dir(root).join(HELD_DELETES)).ok()?;
        serde_json::from_slice(&b).ok()
    }

    fn save(&self, root: &Path) -> Result<(), String> {
        let b = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join(HELD_DELETES), &b, false).map_err(|e| e.to_string())
    }

    /// The notes whose deletes may be sent now: these, while allowed.
    fn allowed_now(&self) -> std::collections::BTreeSet<quire_sync::FileId> {
        if self.allowed_until.is_none_or(|u| u <= crate::history_ops::now_ms()) {
            return std::collections::BTreeSet::new();
        }
        self.notes.iter().map(|n| quire_sync::FileId(n.file.clone())).collect()
    }
}

/// Send the deletes sync held back for the vault at `root`.
///
/// Those of [`HeldDeletes`] exactly, at its next pass within
/// [`ALLOW_DELETES_FOR`], whichever process runs it, and only if they are
/// still the set the person saw (`seen`, from [`HeldDeletes::id`]): a set
/// that changed meanwhile is refused, to be looked at again. Returns how
/// many.
pub fn sync_allow_deletes_at(root: &Path, seen: &HeldId) -> Result<usize, String> {
    let mut held = HeldDeletes::load(root).ok_or("Sync isn't holding back any deletes for this vault.")?;
    if held.id() != *seen {
        return Err(format!(
            "The deletes held back changed since they were shown: now {} instead of {}. Nothing was sent; look at the new list and choose again.",
            held.notes.len(),
            seen.count
        ));
    }
    held.allowed_until = Some(crate::history_ops::now_ms() + i64::try_from(ALLOW_DELETES_FOR.as_millis()).unwrap_or(i64::MAX));
    held.save(root)?;
    Ok(held.notes.len())
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

/// What to tell the person about a note this device can't store (0.13.2).
pub fn skipped_notice(path: &str, why: &str) -> String {
    format!(
        "“{path}” from another device can't be stored on this computer: {why}. It stays on your other devices and the rest keeps syncing; rename it there to get it here."
    )
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
    /// A device removal running or done, kept here (not in Settings) so
    /// its one-time recovery key isn't lost when Settings closes.
    removal: std::sync::Mutex<Removal>,
    /// Find vaults, Start syncing or Join running (or just finished), kept
    /// here so closing Settings never drops the result.
    setup: SetupJob,
    /// Files already said to be too big to sync, this session.
    held_back_said: std::collections::BTreeSet<String>,
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

enum SetupResult {
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
struct SetupJob {
    /// The step running, for which vault, and its result.
    running: Option<(SetupKind, PathBuf, Receiver<SetupResult>)>,
    done: Vec<SetupDone>,
}

impl std::fmt::Debug for SetupJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupJob").field("running", &self.running.as_ref().map(|r| r.0)).field("done", &self.done).finish()
    }
}

/// See [`SyncHandle`]'s `removal`.
#[derive(Debug, Default)]
struct Removal {
    running: Option<Receiver<Result<DeviceRemoved, String>>>,
    done: Option<Result<DeviceRemoved, String>>,
    announced: bool,
}

impl Removal {
    fn poll(&mut self) {
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

mod config;
pub use config::*;
mod state;
pub use state::*;
mod workspace;

#[cfg(test)]
mod tests;
