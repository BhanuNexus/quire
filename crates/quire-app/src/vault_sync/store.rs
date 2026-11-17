//! How sync sees the vault: `VaultStore`, the `LocalStore` the engine reads
//! and writes notes through, and the lanes (which files a store syncs).

use super::{HashMap, LocalStore, MAX_SHARED_BYTES, NotePath, SHARED_PREFIX, SHARED_REVIEW_PREFIX, Selection, Vault, icloud_stub};

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
    pub(super) vault: Vault,
    seen: HashMap<String, Option<String>>,
    /// Attachment fingerprints by (size, modified time), so unchanged images
    /// aren't read and hashed on every pass.
    digests: std::cell::RefCell<HashMap<String, (u64, std::time::SystemTime, String)>>,
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
    /// doesn't tell capitals apart (0.13.2): filled with `seen`.
    folded: HashMap<String, String>,
    /// Whether the vault's disk ignores capital letters in names (found
    /// out once).
    pub(super) case_blind: std::cell::Cell<Option<bool>>,
    /// The files outside the selection couldn't be listed this pass.
    unsure: std::cell::Cell<bool>,
}

pub(super) fn b64() -> base64::engine::GeneralPurpose {
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
        if let Some(rest) = path.strip_prefix(SHARED_PREFIX) {
            // A plain settings file (quire_base::path::Rule::PlainNote), and,
            // as before, no `..` anywhere in its name.
            if !quire_base::path::Rule::PlainNote.accepts(rest) || rest.contains("..") {
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
pub(super) fn snapshot_reads(store: &mut VaultStore) -> Result<(), String> {
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
