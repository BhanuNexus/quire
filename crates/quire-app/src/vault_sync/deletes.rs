//! Mass deletes held for a decision: `HeldDeletes`, the allow-for-15-minutes
//! call, the shared-lane switch and the notice for a file that can't be stored.

use super::*;

/// In `.quire/sync/`: the deletes a pass held back because they were many
/// ([`HeldDeletes`], 0.13.2).
pub(super) const HELD_DELETES: &str = "held-deletes.json";

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
    pub(super) fn new(notes: &[(quire_sync::FileId, String)], settings: bool) -> Self {
        let mut notes: Vec<HeldNote> = notes.iter().map(|(f, p)| HeldNote { file: f.0.clone(), path: p.clone() }).collect();
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        Self { at: crate::history_ops::now_ms(), notes, settings, allowed_until: None }
    }

    /// Keep this set as the one held back, unless it is what `before`
    /// holds already (keeping its approval). A different set (more went
    /// missing, or others than were allowed) is asked again from scratch.
    pub(super) fn keep(self, root: &Path, before: Option<&Self>) {
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

    pub(super) fn save(&self, root: &Path) -> Result<(), String> {
        let b = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join(HELD_DELETES), &b, false).map_err(|e| e.to_string())
    }

    /// The notes whose deletes may be sent now: these, while allowed.
    pub(super) fn allowed_now(&self) -> std::collections::BTreeSet<quire_sync::FileId> {
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
