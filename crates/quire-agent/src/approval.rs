// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What this device approved for a vault (design §A.2, P7):
//! `<config>/vaults/<vault-key>/state.toml`.
//!
//! A vault's agent policy and its agent instructions (`AGENTS.md`,
//! `CLAUDE.md`) travel with the vault. A change made on another device, or
//! by hand outside Quire, takes effect here only after the person approves
//! it on this device; until then agents get the last approved policy (or
//! the defaults), narrowed by the new file. Approval stores the file's hash,
//! and a copy of its text next to `state.toml` so later changes can be
//! shown as a difference.
//!
//! The file also keeps per-vault device preferences: whether reads are
//! logged, and inbox snoozes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quire_vault::ContentHash;
use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = "state.toml";
pub const STATE_FORMAT: u32 = 1;
/// Copies of what was approved (for differences and as the fallback policy).
pub const APPROVED_POLICY_FILE: &str = "approved-policy.toml";
pub const APPROVED_INSTRUCTIONS_FILE: &str = "approved-instructions.md";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceState {
    pub format: u32,
    /// Hash of the approved `agent-policy.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_policy: Option<ContentHash>,
    /// Hash of the approved `AGENTS.md`/`CLAUDE.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_instructions: Option<ContentHash>,
    /// Log which notes agents read (`read` ledger events, kept 30 days).
    pub log_reads: bool,
    /// Inbox items hidden until a time: fingerprint → Unix ms.
    pub snoozed: BTreeMap<String, i64>,
    /// When the person last looked at checks and staleness (Unix ms): those
    /// count in the Inbox badge as a weekly digest.
    #[serde(skip_serializing_if = "is_zero")]
    pub digest_seen: i64,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            format: STATE_FORMAT,
            approved_policy: None,
            approved_instructions: None,
            log_reads: false,
            snoozed: BTreeMap::new(),
            digest_seen: 0,
            other: toml::Table::new(),
        }
    }
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
const fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// The 16-hex FNV-1a of a vault's canonical root (the same function as the
/// history folders), naming its folder under `<config>/vaults/`.
pub fn vault_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.to_string_lossy().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// `<config>/vaults/<key>/` for the vault at `root`, in Quire's usual config
/// folder. `None` when the platform has none.
pub fn default_state_dir(root: &Path) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "Quire", "Quire")?;
    Some(dirs.config_dir().join("vaults").join(vault_key(root)))
}

/// The state folder of one vault on this device.
#[derive(Debug, Clone)]
pub struct StateDir {
    dir: PathBuf,
}

/// Write `bytes` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

impl StateDir {
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// When `state.toml` last changed (to notice another process approving).
    pub fn stamp(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.dir.join(STATE_FILE)).and_then(|m| m.modified()).ok()
    }

    /// The state; defaults when there is none or it can't be read. A file
    /// from a newer Quire is read for what this version knows.
    pub fn load(&self) -> DeviceState {
        std::fs::read_to_string(self.dir.join(STATE_FILE)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    /// Save, unless the file on disk was made by a newer Quire.
    pub fn save(&self, st: &DeviceState) -> Result<(), String> {
        let on_disk = self.load();
        if on_disk.format > STATE_FORMAT {
            return Err("this vault's device state was made by a newer Quire".into());
        }
        let text = toml::to_string(st).map_err(|e| e.to_string())?;
        write_atomic(&self.dir.join(STATE_FILE), text.as_bytes()).map_err(|e| e.to_string())
    }

    /// Change the state with `f` and save it.
    pub fn update(&self, f: impl FnOnce(&mut DeviceState)) -> Result<DeviceState, String> {
        let mut st = self.load();
        f(&mut st);
        self.save(&st)?;
        Ok(st)
    }

    pub fn read_copy(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    pub fn write_copy(&self, name: &str, text: &str) -> Result<(), String> {
        write_atomic(&self.dir.join(name), text.as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sd = StateDir::new(dir.path().join("vaults/abc"));
        assert_eq!(sd.load(), DeviceState::default());
        sd.update(|s| {
            s.approved_policy = Some(ContentHash::of("p"));
            s.snoozed.insert("abcd".into(), 5);
        })
        .unwrap();
        let text = std::fs::read_to_string(sd.path().join(STATE_FILE)).unwrap();
        assert!(text.contains("approved_policy = \"sha256:"), "{text}");
        std::fs::write(sd.path().join(STATE_FILE), format!("future = 1\n{text}")).unwrap();
        sd.update(|s| s.log_reads = true).unwrap();
        let back = sd.load();
        assert!(back.log_reads && back.other.contains_key("future"));
        assert_eq!(back.snoozed["abcd"], 5);
        // A newer Quire's file is read, never rewritten.
        std::fs::write(sd.path().join(STATE_FILE), "format = 2\nlog_reads = true\n").unwrap();
        assert!(sd.load().log_reads);
        assert!(sd.save(&DeviceState::default()).is_err());
        assert_eq!(vault_key(Path::new("/x/Notes")).len(), 16);
        assert_eq!(vault_key(Path::new("/x/Notes")), vault_key(Path::new("/x/notes")));
    }
}
