// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Saving from the web app: a person's write, never a silent overwrite.
//!
//! The editor sends the text it started from and that text's hash. If the
//! note still has that hash, the new text is written atomically and kept
//! in history with who wrote it ("You · via web · <device>"); the version
//! it replaces is kept first, so the edit can be undone. If the note
//! changed meanwhile (another device, an agent, sync), nothing is written:
//! the person sees the three-way merge when it is clean, or both versions
//! and the choice to save theirs as a copy.

use quire_agent::{Actor, Op, Provenance, Via};
use quire_app::vault::{ContentHash, NotePath, Vault, VaultError};

use crate::Web;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Saved {
    Written,
    /// Nothing to write: the note already has this text.
    Unchanged,
    /// The note changed since the editor opened it; nothing was written.
    Conflict {
        current: String,
        /// The clean three-way merge of the editor's start, the note now
        /// and the editor's text, when there is one.
        merged: Option<String>,
    },
}

/// The person at this server, writing through the web.
fn actor(web: &Web) -> Actor {
    let mut a = Actor::human(&web.cfg.device.id);
    a.via = Via::Web;
    a.other.insert("device_name".into(), web.cfg.device.name.clone().into());
    a
}

/// Write `text` to `path` and keep it in history with its provenance,
/// keeping the version it replaces first.
pub(crate) fn write(web: &Web, path: &NotePath, before: Option<&str>, text: &str, op: Op) -> Result<(), String> {
    let vault = web.guard.vault();
    let now = web.now();
    if let Some(b) = before {
        // Synced and outside edits aren't always in history yet.
        let _ = vault.snapshot(path, b, now - 1, true);
    }
    vault.write(path, text).map_err(|e| e.to_string())?;
    let prov = Provenance::new(actor(web), op, text, now);
    vault.snapshot_with(path, text, now, true, Some(&prov.to_json())).map_err(|e| e.to_string())?;
    Ok(())
}

fn read(vault: &Vault, path: &NotePath) -> Result<Option<String>, String> {
    match vault.read(path) {
        Ok(t) => Ok(Some(t)),
        Err(VaultError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Save `mine` over the note, if it is still the version `base` (the hash
/// of `orig`, the text the editor started from).
pub(crate) fn save(web: &Web, path: &NotePath, base: &str, orig: &str, mine: &str) -> Result<Saved, String> {
    let _one_at_a_time = web.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = read(web.guard.vault(), path)?;
    let Some(current) = current else {
        // Deleted (or moved) since: put it back with this text only if the
        // editor started from nothing; otherwise show the person.
        if base.is_empty() {
            write(web, path, None, mine, Op::Create)?;
            return Ok(Saved::Written);
        }
        return Ok(Saved::Conflict { current: String::new(), merged: None });
    };
    // Browsers send lines ending in CR LF; the editor works in LF and the
    // note keeps the line endings it had.
    let crlf = current.contains("\r\n");
    let now_lf = current.replace("\r\n", "\n");
    if now_lf == mine {
        return Ok(Saved::Unchanged);
    }
    let base_ok = ContentHash::of(orig).as_str() == base;
    if base_ok && ContentHash::of(&now_lf).as_str() == base {
        let text = if crlf { mine.replace('\n', "\r\n") } else { mine.to_string() };
        write(web, path, Some(&current), &text, Op::Write)?;
        return Ok(Saved::Written);
    }
    let merged = base_ok
        .then(|| quire_app::text::merge::merge3(orig, &now_lf, mine))
        .filter(quire_app::text::merge::MergeResult::is_clean)
        .map(|m| m.text);
    Ok(Saved::Conflict { current: now_lf, merged })
}

/// A free name next to `path` for the editor's text: "Plan (copy from the
/// web 2026-11-05).md".
pub(crate) fn copy_path(vault: &Vault, path: &NotePath, today: &str) -> NotePath {
    let stem = path.stem().to_string();
    vault.unique_path(path.folder(), &format!("{stem} (copy from the web {today})")).unwrap_or_else(|_| path.clone())
}

/// A new note, `# Title` and nothing else, at a free name in `folder`.
pub(crate) fn create(web: &Web, folder: &str, title: &str) -> Result<NotePath, String> {
    let _one_at_a_time = web.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let vault = web.guard.vault();
    let title = title.trim();
    if title.is_empty() {
        return Err("Give the note a name.".into());
    }
    let p = vault.unique_path(folder, title).map_err(|e| e.to_string())?;
    write(web, &p, None, &format!("# {title}\n\n"), Op::Create)?;
    Ok(p)
}
