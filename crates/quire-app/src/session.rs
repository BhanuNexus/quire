// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What was open last time, so the app reopens exactly where you left off.
//!
//! Besides the open tabs, `session.json` keeps where you were in the last
//! [`MAX_POSITIONS`] notes you opened (`positions`: caret and scroll per
//! note), so a note opens where you left it even after its tab was closed.
//! Keys this version doesn't know are kept when the file is rewritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OpenTab {
    pub path: String,
    pub caret: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Session {
    pub vault: Option<PathBuf>,
    pub tabs: Vec<OpenTab>,
    pub active: usize,
    pub scratchpad: String,
    /// Recently opened notes, most recent first.
    pub recent: Vec<String>,
}

/// How many notes' positions are remembered.
pub const MAX_POSITIONS: usize = 500;

/// Where you were in a note.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Position {
    /// Caret (char index).
    pub caret: usize,
    /// Vertical scroll offset in points.
    pub scroll: f32,
    /// When it was last updated (Unix ms), to forget the oldest first.
    pub at: i64,
}

/// Positions by vault-relative path.
pub type Positions = BTreeMap<String, Position>;

/// The positions the next [`Session::save_to`] writes, with the vault they
/// belong to. Kept here because the session is written by the workspace on
/// quit, from fields that don't include them.
static PENDING: Mutex<Option<(Option<PathBuf>, Positions)>> = Mutex::new(None);

/// Hand the current positions to the next session save.
pub fn stash_positions(vault: Option<&Path>, positions: &Positions) {
    if let Ok(mut p) = PENDING.lock() {
        *p = Some((vault.map(Path::to_path_buf), positions.clone()));
    }
}

/// Keep the `limit` most recently used positions.
pub fn trim_positions(p: &mut Positions, limit: usize) {
    if p.len() <= limit {
        return;
    }
    let mut ats: Vec<i64> = p.values().map(|v| v.at).collect();
    ats.sort_unstable_by(|a, b| b.cmp(a));
    let cutoff = ats[limit - 1];
    let mut kept = 0;
    p.retain(|_, v| {
        let keep = v.at > cutoff || (v.at == cutoff && kept < limit);
        kept += usize::from(keep);
        keep
    });
    while p.len() > limit {
        let oldest = p.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            p.remove(&k);
        }
    }
}

impl Session {
    /// `session.json` in the config folder. Unit tests get none, so they
    /// never touch a real session.
    pub fn path() -> Option<PathBuf> {
        if cfg!(test) {
            return None;
        }
        crate::settings::config_dir().map(|d| d.join("session.json"))
    }

    pub fn load_from(p: &Path) -> Self {
        std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Write the session. Keys written by other versions stay, and the
    /// positions handed over with [`stash_positions`] are added.
    pub fn save_to(&self, p: &Path) -> std::io::Result<()> {
        let err = std::io::Error::other;
        let mut v = serde_json::to_value(self).map_err(err)?;
        let this_vault = serde_json::to_value(&self.vault).map_err(err)?;
        let old: Option<serde_json::Value> = std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok());
        let pending = PENDING.lock().ok().and_then(|g| g.clone()).filter(|(vault, _)| *vault == self.vault);
        let obj = v.as_object_mut().ok_or_else(|| std::io::Error::other("a session is an object"))?;
        if let Some(serde_json::Value::Object(old)) = old {
            let same_vault = old.get("vault") == Some(&this_vault);
            for (k, val) in old {
                // Positions belong to the vault they were saved for.
                if k != "positions" || same_vault {
                    obj.entry(k).or_insert(val);
                }
            }
        }
        if let Some((_, pos)) = pending {
            obj.insert("positions".into(), serde_json::to_value(pos).map_err(err)?);
        }
        crate::atomic::write(p, serde_json::to_string_pretty(&v).map_err(err)?.as_bytes(), false)
    }
}

/// The positions saved in `session.json`, if they are for `vault`.
pub fn load_positions(p: &Path, vault: &Path) -> Positions {
    let Some(v) = std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else {
        return Positions::new();
    };
    let same_vault = v.get("vault").and_then(serde_json::Value::as_str).map(PathBuf::from).is_some_and(|s| {
        let canon = |x: &Path| x.canonicalize().unwrap_or_else(|_| x.to_path_buf());
        canon(&s) == canon(vault)
    });
    if !same_vault {
        return Positions::new();
    }
    v.get("positions").cloned().and_then(|p| serde_json::from_value(p).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_ride_along_and_unknown_keys_stay() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.json");
        let vault = dir.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(&file, r#"{"vault": null, "from_the_future": {"x": 1}}"#).unwrap();
        let mut pos = Positions::new();
        pos.insert("A.md".into(), Position { caret: 7, scroll: 120.5, at: 1 });
        stash_positions(Some(&vault), &pos);
        let s = Session { vault: Some(vault.clone()), ..Session::default() };
        s.save_to(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("from_the_future"), "{text}");
        assert_eq!(load_positions(&file, &vault), pos);
        assert_eq!(Session::load_from(&file), s);
        assert!(load_positions(&file, dir.path()).is_empty(), "positions are per vault");
    }

    #[test]
    fn only_the_most_recent_positions_are_kept() {
        let mut p: Positions = (0..20).map(|i| (format!("{i}.md"), Position { caret: i, scroll: 0.0, at: i as i64 })).collect();
        trim_positions(&mut p, 5);
        assert_eq!(p.len(), 5);
        assert!(p.contains_key("19.md") && p.contains_key("15.md") && !p.contains_key("14.md"));
        let mut same: Positions = (0..8).map(|i| (format!("{i}.md"), Position::default())).collect();
        trim_positions(&mut same, 3);
        assert_eq!(same.len(), 3);
    }
}
