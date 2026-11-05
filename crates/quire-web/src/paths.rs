// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the web app may show: the one check for every path it is given.
//!
//! A path from a URL or a form is decoded, then checked like an agent's
//! path (`quire_agent::canonical_path` and `AgentStore::resolve`): NFC,
//! relative, no `..`, no hidden folder (`.quire`, `.trash`, `.git`…), and
//! the file it reaches through symlinks must be inside the vault and
//! outside hidden folders. Then the web's own rule: folders hidden from the
//! web (`[web] hide` in `.quire/shared/vault.toml`, or `--hide`) don't
//! exist for it, compared without regard to letter case, both where the
//! path points and where it really leads. Anything else answers "not found".

use std::sync::Mutex;
use std::time::SystemTime;

use quire_agent::AgentStore;
use quire_app::vault::{NotePath, Vault};
use unicode_normalization::UnicodeNormalization;

/// Where hidden folders are set in a vault.
pub const VAULT_TOML: &str = ".quire/shared/vault.toml";

/// Folder prefixes as written (`Journal`, `Journal/`), normalized to
/// lowercase NFC with a trailing `/`. Entries that aren't relative folder
/// paths are dropped.
pub fn normalize_hidden(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = list
        .iter()
        .filter_map(|f| {
            let t: String = f.trim().trim_start_matches("./").trim_matches('/').nfc().collect::<String>().to_lowercase();
            let ok = !t.is_empty() && !t.contains('\\') && t.split('/').all(|s| !s.is_empty() && s != "." && s != "..");
            ok.then(|| format!("{t}/"))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `[web] hide = […]` from the vault's shared settings.
pub fn hidden_in_vault(vault: &Vault) -> Vec<String> {
    let Ok(text) = vault.storage().read(&NotePath::new(VAULT_TOML)) else { return Vec::new() };
    let Ok(t) = text.parse::<toml::Table>() else { return Vec::new() };
    let list: Vec<String> = t
        .get("web")
        .and_then(|w| w.get("hide"))
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    normalize_hidden(&list)
}

/// Why a path isn't shown (the web answers "not found" either way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not a vault path (`..`, absolute, control characters…).
    Invalid,
    /// In a hidden folder, `.quire`, or a folder hidden from the web.
    Hidden,
    /// Leads (through a symlink) outside the vault or somewhere hidden.
    Outside,
}

pub struct Guard {
    vault: Vault,
    store: AgentStore,
    /// From the command line.
    flags: Vec<String>,
    /// From `vault.toml`, with the file's time when read.
    cached: Mutex<(Option<SystemTime>, Vec<String>)>,
}

impl Guard {
    pub fn new(vault: Vault, store: AgentStore, hide: &[String]) -> Self {
        Self { vault, store, flags: normalize_hidden(hide), cached: Mutex::new((None, Vec::new())) }
    }

    /// Every folder hidden from the web, lowercase with a trailing `/`.
    pub fn hidden(&self) -> Vec<String> {
        let stamp = self.vault.storage().modified(&NotePath::new(VAULT_TOML)).ok();
        let mut c = self.cached.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if c.0 != stamp || stamp.is_none() {
            *c = (stamp, hidden_in_vault(&self.vault));
        }
        let mut all = self.flags.clone();
        all.extend(c.1.iter().cloned());
        all
    }

    fn web_hidden(hidden: &[String], p: &str) -> bool {
        let lower: String = p.nfc().collect::<String>().to_lowercase();
        hidden.iter().any(|h| lower.starts_with(h.as_str()) || format!("{lower}/") == *h)
    }

    /// For lists (tree, search, backlinks, tags): no disk access.
    pub fn listed(hidden: &[String], p: &NotePath) -> bool {
        quire_agent::canonical_path(p).is_ok_and(|c| !hidden_segment(c.as_str()) && !Self::web_hidden(hidden, c.as_str()))
    }

    /// The full check for a path someone asked for. `raw` is already
    /// percent-decoded.
    pub fn check(&self, raw: &str) -> Result<NotePath, Refused> {
        let p = quire_agent::canonical_path(&NotePath::new(raw)).map_err(|_| Refused::Invalid)?;
        let hidden = self.hidden();
        if hidden_segment(p.as_str()) || Self::web_hidden(&hidden, p.as_str()) {
            return Err(Refused::Hidden);
        }
        let r = self.store.resolve(&p).map_err(|_| Refused::Outside)?;
        if hidden_segment(r.real.as_str()) || Self::web_hidden(&hidden, r.real.as_str()) {
            return Err(Refused::Outside);
        }
        Ok(r.path)
    }

    /// A folder someone asked for (`""` is the vault itself).
    pub fn check_folder(&self, raw: &str) -> Result<String, Refused> {
        let t = raw.trim_matches('/');
        if t.is_empty() {
            return Ok(String::new());
        }
        // Checked as a file inside it, so the same rules apply.
        let p = self.check(&format!("{t}/x"))?;
        Ok(p.as_str().trim_end_matches("/x").to_string())
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }
}

/// `.quire`, `.trash`, `.git` or any other hidden segment, or
/// `node_modules` (which the vault never lists).
pub fn hidden_segment(p: &str) -> bool {
    p.split('/').any(|s| s.starts_with('.') || s == "node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_folders_normalize() {
        let v = normalize_hidden(&["Journal".into(), "./Private/Health/".into(), "../x".into(), String::new(), "journal/".into()]);
        assert_eq!(v, ["journal/", "private/health/"]);
        assert!(Guard::web_hidden(&v, "JOURNAL/2026.md"));
        assert!(Guard::web_hidden(&v, "Journal"));
        assert!(!Guard::web_hidden(&v, "Journalism/a.md"));
        assert!(hidden_segment(".quire/x") && hidden_segment("a/.git/b") && !hidden_segment("a/b.md"));
    }
}
