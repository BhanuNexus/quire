//! How sync sees the vault: `VaultStore`, the `LocalStore` the engine reads
//! and writes notes through, and the lanes (which files a store syncs).
//!
//! Each pass starts with [`begin_pass`]: one scan of the vault manifest
//! (`quire_vault::changes`), which reads only files whose size or time
//! changed. No note is read until the engine asks, and the engine asks only
//! for notes whose manifest hash differs from the stamp it keeps
//! (`docs/design/0.14-sync-speed.md` §3.1).

use std::cell::RefCell;

use quire_sync::{Print, Stamp};
use quire_vault::FileStat;
use quire_vault::changes::{FileHash, Manifest};

use super::{HashMap, LocalStore, MAX_SHARED_BYTES, NotePath, SHARED_PREFIX, SHARED_REVIEW_PREFIX, Selection, Vault, icloud_stub};

/// How [`LocalStore::unstorable`] starts for a file that can't be read now
/// (see [`super::skipped_notice`]).
pub const UNREADABLE: &str = "the file here can't be read";

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
/// Writes check that the file is still what the engine first read in this
/// pass (by hash), or what the pass's scan found if it read nothing, so a
/// save from the editor in between is never overwritten (the engine
/// retries on its next pass instead).
pub struct VaultStore {
    pub(super) vault: Vault,
    /// The vault manifest: this pass's listing and each file's hash.
    manifest: Manifest,
    /// The files of this store's lanes as this pass sees them, with the
    /// hash of their bytes: from the scan, then from the engine's own reads
    /// and writes. A file not here is not listed.
    listed: RefCell<HashMap<String, FileHash>>,
    /// Every file of the vault this pass's scan found (for the files outside
    /// the selection).
    everything: Vec<String>,
    /// The hash of the bytes the engine first read at a path in this pass
    /// (`None`: it found no file there): what a write may replace.
    first_read: RefCell<HashMap<String, Option<FileHash>>>,
    /// Attachment prints by the hash of their bytes, so an unchanged image
    /// isn't read and encoded again.
    prints: RefCell<HashMap<FileHash, Print>>,
    /// `.quire/shared/` files (outside the manifest: a hidden folder) by
    /// path: size, time and hash, so unchanged ones aren't read again.
    shared_hashes: RefCell<HashMap<String, (FileStat, FileHash)>>,
    /// Files listed but unreadable this pass, with why: each is skipped
    /// (never sent, never sent as deleted) while the rest syncs.
    unreadable: RefCell<std::collections::BTreeMap<String, String>>,
    /// The next [`begin_pass`] rehashes every file (a verify pass).
    pub(super) verify_next: bool,
    /// A [`begin_pass`] listed the vault: without one there is no listing.
    began: bool,
    /// Sync `.quire/shared/` too.
    pub(super) shared: bool,
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
    /// doesn't tell capitals apart (0.13.2): filled with `listed`.
    folded: HashMap<String, String>,
    /// Whether the vault's disk ignores capital letters in names (found
    /// out once).
    pub(super) case_blind: std::cell::Cell<Option<bool>>,
}

pub(super) fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl VaultStore {
    pub fn new(vault: Vault) -> Self {
        let manifest = Manifest::open(&vault);
        Self::with_manifest(vault, manifest)
    }

    /// A store that scans with `manifest` (one scanner per vault: the app's
    /// shared one, docs/design/0.14-architecture.md §2).
    pub fn with_manifest(vault: Vault, manifest: Manifest) -> Self {
        Self {
            vault,
            manifest,
            listed: RefCell::default(),
            everything: Vec::new(),
            first_read: RefCell::default(),
            prints: RefCell::default(),
            shared_hashes: RefCell::default(),
            unreadable: RefCell::default(),
            verify_next: false,
            began: false,
            shared: false,
            lane: Lane::Mixed,
            incoming: None,
            selection: Selection::default(),
            outside_before: None,
            folded: HashMap::new(),
            case_blind: std::cell::Cell::new(None),
        }
    }

    /// Whether the vault's disk ignores capital letters in names (Windows,
    /// macOS by default): `.quire` is found as `.QUIRE` too. A vault with
    /// no folder on disk (in memory) tells them apart.
    pub(super) fn case_blind(&self) -> bool {
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

    /// The manifest this store scans with.
    pub(super) const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The files this pass found unreadable, with why.
    pub(super) fn unreadable(&self) -> Vec<(String, String)> {
        self.unreadable.borrow().iter().map(|(p, e)| (p.clone(), e.clone())).collect()
    }

    /// The files outside the selection: unselected notes and attachments,
    /// from this pass's scan.
    fn unselected(&self) -> Vec<NotePath> {
        if self.selection.is_everything() || self.lane == Lane::Shared {
            return Vec::new();
        }
        self.everything.iter().filter(|p| !self.selection.includes(p)).map(NotePath::new).collect()
    }

    /// After a pass that finished: remember which files are outside the
    /// selection now.
    pub(super) fn remember_unselected(&mut self) {
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

    /// The bytes at `path` as stored, `Ok(None)` when there is no file. A
    /// read that finds other bytes than the manifest's row corrects the
    /// row; the first read of a path in a pass is what a write there may
    /// replace (see [`Self::as_read`]).
    fn read_raw(&self, path: &str) -> Result<Option<(Vec<u8>, FileHash)>, String> {
        let p = NotePath::new(path);
        match self.vault.storage().read_bytes(&p) {
            Ok(b) => {
                let hash = FileHash::of(&b);
                if !path.starts_with(SHARED_PREFIX) && self.listed.borrow().get(path) != Some(&hash) {
                    self.learned(&p, &b);
                }
                if self.listed.borrow().contains_key(path) {
                    self.listed.borrow_mut().insert(path.to_string(), hash);
                }
                self.first_read.borrow_mut().entry(path.to_string()).or_insert(Some(hash));
                self.unreadable.borrow_mut().remove(path);
                Ok(Some((b, hash)))
            }
            Err(quire_vault::VaultError::NotFound(_)) => {
                self.first_read.borrow_mut().entry(path.to_string()).or_insert(None);
                Ok(None)
            }
            Err(e) => {
                // There, but unreadable now: neither "gone" nor known.
                self.unreadable.borrow_mut().insert(path.to_string(), e.to_string());
                Err(e.to_string())
            }
        }
    }

    /// The file's text as the engine syncs it, and the hash of its bytes.
    fn current_stamped(&self, path: &str) -> Option<(String, FileHash)> {
        use base64::Engine as _;
        if self.outside_lane(path) {
            return None;
        }
        let (bytes, hash) = self.read_raw(path).ok().flatten()?;
        let text = if quire_sync::engine::is_attachment(path) {
            b64().encode(bytes)
        } else {
            // The file as stored: sync never decrypts a locked folder's
            // files (they travel as their `.qenc` bytes), 0.11. A BOM is
            // dropped and a stray byte decoded lossily, as the editor does.
            String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes)).into_owned()
        };
        Some((text, hash))
    }

    fn current(&self, path: &str) -> Option<String> {
        self.current_stamped(path).map(|(t, _)| t)
    }

    /// The `.quire/shared/` files this store syncs, with their hashes.
    fn shared_listing(&self) -> Result<Vec<(String, FileHash)>, String> {
        let mut out = Vec::new();
        for p in self.shared_files()? {
            let p = p.as_str();
            if !p.starts_with(SHARED_PREFIX) || p.contains("/.") || self.size_and_time(p).is_none_or(|(n, _)| n > MAX_SHARED_BYTES) {
                continue;
            }
            if let Some(h) = self.shared_hash(p) {
                out.push((p.to_string(), h));
            }
        }
        Ok(out)
    }

    /// A `.quire/shared/` file's hash, read only when its size or time changed.
    fn shared_hash(&self, path: &str) -> Option<FileHash> {
        let stat = self.size_and_time(path).map(|(size, t)| FileStat { size, mtime_ns: FileStat::nanos(t) });
        if let Some(stat) = stat
            && stat.mtime_ns != 0
            && let Some((s, h)) = self.shared_hashes.borrow().get(path)
            && *s == stat
        {
            return Some(*h);
        }
        let bytes = self.vault.storage().read_bytes(&NotePath::new(path)).ok()?;
        let h = FileHash::of(&bytes);
        if let Some(stat) = stat {
            self.shared_hashes.borrow_mut().insert(path.to_string(), (stat, h));
        }
        Some(h)
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
                #[expect(
                    clippy::filetype_is_file,
                    reason = "a symlinked file or folder is not synced, as the vault listing skips it: it could point outside the vault"
                )]
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

    /// What a write or removal may replace: the bytes the engine first
    /// read at `path` in this pass, or, if it read none there, what the
    /// pass's scan found (no file, if it isn't listed). Compared by hash, as
    /// one step with the write ([`quire_vault::Storage::write_bytes_if`]),
    /// so a save from the editor, the web app or an agent in between is
    /// never overwritten (the engine retries on its next pass instead).
    fn expected(&self, path: &str) -> Option<FileHash> {
        match self.first_read.borrow().get(path) {
            Some(first) => *first,
            None => self.listed.borrow().get(path).copied(),
        }
    }

    fn as_read(&self, path: &str) -> impl Fn(Option<&[u8]>) -> bool + use<> {
        let expected = self.expected(path);
        // A file found unreadable in this pass is never replaced or removed.
        let unreadable = self.unreadable.borrow().contains_key(path);
        move |now: Option<&[u8]>| !unreadable && now.map(FileHash::of) == expected
    }

    /// A write or removal was refused because the file changed: the
    /// manifest learns its bytes now (`docs/design/0.14-sync-speed.md` §3.1),
    /// so the next pass reads it and merges, instead of failing the same
    /// way while its size and time stay put.
    ///
    /// This pass still expects what it read: a later write there in the
    /// same pass is refused too.
    fn refused(&self, path: &str) -> String {
        let p = NotePath::new(path);
        if !path.starts_with(SHARED_PREFIX) {
            match self.vault.storage().read_bytes(&p) {
                Ok(b) => {
                    self.learned(&p, &b);
                }
                Err(quire_vault::VaultError::NotFound(_)) => self.manifest.note_removed(&p),
                // There but unreadable: named, and its row left as it is.
                Err(e) => {
                    self.unreadable.borrow_mut().insert(path.to_string(), e.to_string());
                }
            }
        }
        "it changed while syncing".into()
    }

    /// Vault settings start to sync in the middle of a pass: they are
    /// listed from now on. A settings folder that can't be listed stops the
    /// pass (0.13.2), at the engine's next listing.
    fn share_now(&mut self) {
        if !self.began || self.lane == Lane::Notes {
            return;
        }
        match self.shared_listing() {
            Ok(files) => self.listed.borrow_mut().extend(files),
            Err(_) => self.began = false,
        }
    }

    /// Whether the vault's own folder is still there to list: a file is
    /// "not found" for every path of a vault whose drive was unplugged.
    fn root_is_there(&self) -> bool {
        let Some(quire) = self.vault.storage().absolute(&NotePath::new(".quire")) else { return true };
        quire.parent().is_none_or(|root| std::fs::read_dir(root).is_ok())
    }

    /// A read found bytes the manifest's row doesn't have (written by
    /// another program): the row learns them. The one place to switch to
    /// the manifest's call for bytes Quire didn't write.
    fn learned(&self, p: &NotePath, bytes: &[u8]) -> FileHash {
        self.manifest.note_written(p, bytes)
    }

    /// Record a write of `bytes` at `path` by the engine.
    fn wrote(&self, path: &str, bytes: &[u8]) {
        let p = NotePath::new(path);
        let h = if path.starts_with(SHARED_PREFIX) { FileHash::of(bytes) } else { self.manifest.note_written(&p, bytes) };
        self.listed.borrow_mut().insert(path.to_string(), h);
        self.first_read.borrow_mut().insert(path.to_string(), Some(h));
    }

    /// Record that the engine removed `path`.
    fn gone(&self, path: &str) {
        if !path.starts_with(SHARED_PREFIX) {
            self.manifest.note_removed(&NotePath::new(path));
        }
        self.listed.borrow_mut().remove(path);
        self.first_read.borrow_mut().insert(path.to_string(), None);
    }

    fn unchanged_since_read(&self, path: &str) -> Result<(), String> {
        let now = match self.vault.storage().read_bytes(&NotePath::new(path)) {
            Ok(b) => Some(FileHash::of(&b)),
            Err(quire_vault::VaultError::NotFound(_)) => None,
            Err(e) => return Err(e.to_string()),
        };
        if now == self.expected(path) { Ok(()) } else { Err(self.refused(path)) }
    }
}

impl LocalStore for VaultStore {
    /// This pass's scan, with the engine's own writes and removals since
    /// (`begin_pass` failed if the vault couldn't be listed).
    fn list(&self) -> Result<Vec<String>, String> {
        if !self.began {
            return Err("the vault wasn't listed for this pass".into());
        }
        let mut out: Vec<String> = self.listed.borrow().keys().cloned().collect();
        out.sort();
        Ok(out)
    }

    fn missing(&self, path: &str) -> bool {
        // Only "not found" counts: a file that can't be read just now is
        // still here. So is one iCloud evicted to a `.Name.md.icloud` stub
        // ("Optimise Mac Storage"): it downloads again when opened.
        !self.outside_lane(path)
            && self.root_is_there()
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
        if let Some(e) = self.unreadable.borrow().get(path) {
            // The file here can't be read now: it is neither overwritten
            // nor sent (as changed or deleted) until it can.
            return Some(format!("{UNREADABLE} ({e})"));
        }
        if !self.case_blind() || own.is_some_and(|o| quire_sync::names::same_ignoring_case(o, path)) {
            return None;
        }
        if let Some(other) = self.folded.get(&quire_sync::names::fold_case(path)).filter(|p| p.as_str() != path) {
            return Some(format!("“{other}” is already here, and this computer doesn't tell names apart by capital letters"));
        }
        // Another folding this disk applies (Unicode forms on macOS, say):
        // the name opens a file that isn't listed under it.
        if own.is_none() && !self.listed.borrow().contains_key(path) && self.answers_for_another(path) {
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
        if content.path.starts_with(SHARED_PREFIX) && self.lane == Lane::Mixed && !self.shared {
            // Another device shares vault settings: this pass lists them too.
            self.shared = true;
            self.share_now();
        }
        self.incoming = content.actor.clone().map(|a| (quire_vault::ContentHash::of(&content.text), a, content.from.clone()));
    }

    fn read(&self, path: &str) -> Option<String> {
        self.current(path)
    }

    fn stamp(&self, path: &str) -> Option<Stamp> {
        if self.outside_lane(path) {
            return None;
        }
        self.listed.borrow().get(path).map(|h| Stamp(h.0))
    }

    fn read_stamped(&self, path: &str) -> Option<(String, Option<Stamp>)> {
        self.current_stamped(path).map(|(t, h)| (t, Some(Stamp(h.0))))
    }

    fn in_scope(&self, path: &str) -> bool {
        !self.outside_lane(path)
    }

    fn moved_out(&self, base: &Print, skip: &std::collections::BTreeSet<String>) -> Option<(String, String)> {
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
            (!text.trim().is_empty() && Print::of(&text) == *base).then(|| (p.as_str().to_string(), text))
        })
    }

    fn print_stamped(&self, path: &str) -> Option<(Print, Option<Stamp>)> {
        if quire_sync::engine::is_attachment(path)
            && let Some(h) = self.stamp(path)
            && let Some(p) = self.prints.borrow().get(&FileHash(h.0))
        {
            return Some((*p, Some(h)));
        }
        let (text, h) = self.current_stamped(path)?;
        let p = Print::of(&text);
        if quire_sync::engine::is_attachment(path) {
            self.prints.borrow_mut().insert(h, p);
        }
        Some((p, Some(Stamp(h.0))))
    }

    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        use base64::Engine as _;
        if self.outside_lane(path) {
            // Not this lane's (vault settings in a sync folder's notes lane,
            // which only a 0.9 build from before the lanes could send):
            // never written, so never deleted here either.
            return Ok(());
        }
        if let Some(rest) = path.strip_prefix(SHARED_PREFIX) {
            // A plain settings file (quire_base::path::Rule::PlainNote), and,
            // as before, no `..` anywhere in its name.
            if !quire_base::path::Rule::PlainNote.accepts(rest) || rest.contains("..") {
                return Err("not a vault settings file".into());
            }
            // Another device shares vault settings through this space, so
            // this one must list them too, or it would pass on deletions.
            if !self.shared {
                self.shared = true;
                self.share_now();
            }
        }
        let still = self.as_read(path);
        if quire_sync::engine::is_attachment(path) {
            let bytes = b64().decode(text).map_err(|e| format!("damaged attachment: {e}"))?;
            if !self.vault.storage().write_bytes_if(&NotePath::new(path), &still, &bytes).map_err(|e| e.to_string())? {
                return Err(self.refused(path));
            }
            self.wrote(path, &bytes);
            self.prints.borrow_mut().insert(FileHash::of(&bytes), Print::of(text));
            self.note_name(path, true);
            return Ok(());
        }
        if !self.vault.storage().write_bytes_if(&NotePath::new(path), &still, text.as_bytes()).map_err(|e| e.to_string())? {
            return Err(self.refused(path));
        }
        self.wrote(path, text.as_bytes());
        if !path.starts_with(SHARED_PREFIX) {
            self.keep_provenance(path, text);
        }
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
            self.gone(path);
            self.note_name(path, false);
            return Ok(());
        }
        // Deleted elsewhere: goes to the vault's trash, so it can come back.
        if !self.vault.storage().trash_if(&NotePath::new(path), &self.as_read(path)).map_err(|e| e.to_string())? {
            return Err(self.refused(path));
        }
        self.gone(path);
        self.note_name(path, false);
        Ok(())
    }
}

/// Start a pass: scan the vault manifest (once a day, and on request, a
/// verify pass that rehashes every file) and take this pass's listing from
/// it. Reads only files whose size or time changed
/// (`docs/design/0.14-sync-speed.md` §3.1).
///
/// A vault that can't be listed is an error (0.13.2): the pass stops. A
/// file that is listed but can't be read is not: it is left out of the
/// listing and reported ([`VaultStore::unreadable`]), its note is skipped
/// (never sent, never sent as deleted, never written over) and the rest
/// syncs.
pub(super) fn begin_pass(store: &mut VaultStore) -> Result<(), String> {
    store.began = false;
    store.listed.borrow_mut().clear();
    store.first_read.borrow_mut().clear();
    store.unreadable.borrow_mut().clear();
    store.everything.clear();
    store.folded.clear();
    let verify = std::mem::take(&mut store.verify_next);
    let listed: Vec<(String, u64, FileHash)> = if store.lane == Lane::Shared { Vec::new() } else { scan(store, verify)? };
    store.everything = listed.iter().map(|(p, _, _)| p.clone()).collect();
    let mut keep: HashMap<String, FileHash> = listed
        .into_iter()
        .filter(|(p, _, _)| store.selection.includes(p))
        .filter(|(p, size, _)| NotePath::new(p).is_markdown() || (quire_sync::engine::is_attachment(p) && *size <= MAX_ATTACHMENT_BYTES))
        .map(|(p, _, h)| (p, h))
        .collect();
    if store.shared && store.lane != Lane::Notes {
        if verify {
            store.shared_hashes.borrow_mut().clear();
        }
        keep.extend(store.shared_listing()?);
    }
    let blind = store.case_blind();
    if blind {
        let names: Vec<String> = keep.keys().cloned().collect();
        for p in names {
            store.note_name(&p, true);
        }
    }
    *store.listed.borrow_mut() = keep;
    store.began = true;
    Ok(())
}

/// The vault's files with their size and hash, from one manifest scan.
fn scan(store: &VaultStore, verify: bool) -> Result<Vec<(String, u64, FileHash)>, String> {
    let scanned = if verify { store.manifest.verify() } else { store.manifest.scan() };
    if scanned.is_ok() {
        return Ok(store.manifest.rows().into_iter().map(|r| (r.path.as_str().to_string(), r.size, r.hash)).collect());
    }
    // The scan stops at the first error, whether the listing failed or one
    // file couldn't be read. Which: list again. A listing error stops the
    // pass; otherwise each file the manifest can't vouch for is read here,
    // and one that can't be read is left out and reported.
    let storage = store.vault.storage();
    let listing = storage.list_stat().map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(listing.len());
    for (path, stat) in listing {
        if !verify
            && let Some(r) = store.manifest.get(&path)
            && !r.is_racy()
            && r.mtime_ns != 0
            && stat == Some(FileStat { size: r.size, mtime_ns: r.mtime_ns })
        {
            out.push((path.as_str().to_string(), r.size, r.hash));
            continue;
        }
        match storage.read_bytes(&path) {
            Ok(b) => {
                let h = store.learned(&path, &b);
                out.push((path.as_str().to_string(), b.len() as u64, h));
            }
            Err(quire_vault::VaultError::NotFound(_)) => {}
            Err(e) => {
                store.unreadable.borrow_mut().insert(path.as_str().to_string(), e.to_string());
            }
        }
    }
    Ok(out)
}
