// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vault's agent instructions (`AGENTS.md` or `CLAUDE.md`, design §A.6)
//! and the device-local state that says whether they're approved.
//!
//! Instructions reach agents only once the person approved their current
//! text on this device (P7): `approved_instructions = "sha256:…"` in
//! `<config>/vaults/<vault-key>/state.toml`. Until then agents are told
//! they await review.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quire_app::vault::{ContentHash, NotePath, Vault};

/// How much of the instructions goes into `server/discover` and `initialize`.
pub const INLINE_LIMIT: usize = 8 * 1024;
/// What agents see while the instructions await approval.
pub const AWAITING: &str = "Vault instructions changed outside Quire on this device and await review.";

/// The vault's instructions file and its text: the first of `AGENTS.md`
/// and `CLAUDE.md` at the root (any case), as the agent store finds it.
pub fn find(vault: &Vault) -> Option<(NotePath, String)> {
    let p = quire_agent::instructions::find(vault)?;
    let text = vault.read(&p).ok()?;
    Some((p, text))
}

/// The 16-hex FNV-1a key of a vault folder, as used by
/// `<config>/vaults/<vault-key>/` (design §2.4). The agent store's function,
/// so the app, the CLI and this server share one `state.toml`.
pub use quire_agent::approval::vault_key;

/// This device's folder for a vault on disk (`None` for in-memory vaults
/// and unit tests, which never touch the real config folder).
pub fn state_dir(vault: &Vault) -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let root = vault.storage().absolute(&NotePath::new(""))?;
    Some(quire_app::settings::config_dir()?.join("vaults").join(vault_key(&root)))
}

/// Whether `hash` is the approved text of the instructions on this device.
pub fn approved(state_dir: Option<&Path>, hash: &ContentHash) -> bool {
    approved_value(state_dir, "approved_instructions", hash)
}

/// Whether `key` in this device's `state.toml` names `hash`
/// (`approved_instructions`, `approved_policy`: design §A.2).
pub fn approved_value(state_dir: Option<&Path>, key: &str, hash: &ContentHash) -> bool {
    let Some(dir) = state_dir else { return false };
    let Ok(text) = std::fs::read_to_string(dir.join("state.toml")) else { return false };
    let Ok(table) = text.parse::<toml::Table>() else { return false };
    table.get(key).and_then(toml::Value::as_str) == Some(hash.as_str())
}

/// What agents are told about the instructions: the approved text (cut
/// at `limit` bytes), or that it awaits review. `None` without a file.
pub fn for_agents(vault: &Vault, state_dir: Option<&Path>, limit: usize) -> Option<(NotePath, bool, String)> {
    let (path, text) = find(vault)?;
    let ok = approved(state_dir, &ContentHash::of(&text));
    if !ok {
        return Some((path, false, AWAITING.to_string()));
    }
    let mut cut = text.len().min(limit);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text[..cut].to_string();
    if cut < text.len() {
        let _ = write!(out, "\n… (cut; read the whole file as resource quire://vault/instructions or `view` {path})");
    }
    Some((path, true, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_need_approval_on_this_device() {
        let v = Vault::in_memory(&[("CLAUDE.md", "claude"), ("agents.MD", "be careful"), ("sub/AGENTS.md", "no")]);
        let (p, text) = find(&v).unwrap();
        assert_eq!((p.as_str(), text.as_str()), ("agents.MD", "be careful"));
        assert_eq!(for_agents(&v, None, 100).unwrap().2, AWAITING);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.toml"), format!("approved_instructions = \"{}\"\n", ContentHash::of("be careful"))).unwrap();
        assert_eq!(for_agents(&v, Some(dir.path()), 100).unwrap(), (p, true, "be careful".into()));
        assert!(for_agents(&v, Some(dir.path()), 4).unwrap().2.starts_with("be c\n… (cut"));
        assert!(find(&Vault::in_memory(&[("A.md", "x")])).is_none());
        assert_eq!(vault_key(Path::new("/no/such/Vault")), vault_key(Path::new("/no/such/vault")));
    }
}
