// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sync safety: resolving merges that need a decision, and the conflict
//! copies that sync tools leave next to a note.
//!
//! Folder-sync tools (Dropbox, Syncthing, Nextcloud…) don't merge text. When
//! two devices change a note, one version is renamed to a "conflicted copy".
//! Quire spots those copies and merges them back three-way, using version
//! history to find the version both sides started from.

use quire_text::merge::{self, MergeResult};
use quire_vault::NotePath;

use crate::DocId;
use crate::workspace::{NoticeLevel, Workspace};

/// How to settle a note whose outside change conflicted with unsaved edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Keep this device's text and overwrite the file (the other version
    /// stays in version history).
    KeepMine,
    /// Replace this device's text with the other version (undoable).
    TakeTheirs,
    /// Put both versions in the note with conflict markers to edit by hand.
    MergeWithMarkers,
}

/// A sync tool's conflict copy and the note it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictCopy {
    pub copy: NotePath,
    pub original: NotePath,
    /// Which tool's naming it matched, for display.
    pub tool: &'static str,
}

/// If `path` is a sync tool's conflict copy, the original's file name stem.
///
/// * Dropbox: `Note (conflicted copy 2026-11-01).md`, `Note (Asha's conflicted copy 2026-11-01).md`
/// * Nextcloud / ownCloud: `Note (conflicted copy 2026-11-01 101500).md`
/// * Syncthing: `Note.sync-conflict-20261101-101500-ABCDEFG.md`
/// * Quire Sync: `Note (conflict from laptop 2026-11-01).md`
pub fn conflict_original(stem: &str) -> Option<(String, &'static str)> {
    if let Some(i) = stem.find(".sync-conflict-") {
        let tail = &stem[i + ".sync-conflict-".len()..];
        let ok = tail.len() >= 15 && tail[..8].bytes().all(|b| b.is_ascii_digit()) && tail.as_bytes()[8] == b'-';
        return ok.then(|| (stem[..i].to_string(), "Syncthing"));
    }
    let open = stem.rfind(" (")?;
    let inner = stem[open + 2..].strip_suffix(')')?;
    if let Some(rest) = inner.strip_prefix("conflict from ") {
        // Ends with a date, maybe followed by a counter: "laptop 2026-11-01 2".
        let looks_dated =
            rest.split_whitespace().rev().take(2).any(|w| w.len() == 10 && w.as_bytes()[4] == b'-' && w.as_bytes()[7] == b'-');
        return looks_dated.then(|| (stem[..open].to_string(), "Quire Sync"));
    }
    if inner.contains("conflicted copy") {
        let tool = if inner.chars().filter(char::is_ascii_digit).count() > 8 { "Nextcloud" } else { "Dropbox" };
        return Some((stem[..open].to_string(), tool));
    }
    None
}

/// Pick the version-history snapshot closest to both sides as the common
/// ancestor (none when there is no history).
fn best_base<'a>(snapshots: &'a [String], a: &str, b: &str) -> Option<&'a str> {
    snapshots.iter().min_by_key(|s| merge::distance(s, a) + merge::distance(s, b)).map(String::as_str)
}

impl Workspace {
    /// Settle a pending merge on an open note.
    pub fn resolve_merge(&mut self, id: DocId, how: Resolution) -> bool {
        let Some(i) = self.docs.iter().position(|d| d.id == id) else { return false };
        let Some(pending) = self.docs[i].pending_merge.take() else { return false };
        let title = self.docs[i].title();
        let doc = &mut self.docs[i];
        doc.set_disk_base(&pending.theirs);
        doc.conflict = false;
        match how {
            Resolution::KeepMine => {}
            Resolution::TakeTheirs => doc.replace_minimally(&pending.theirs),
            Resolution::MergeWithMarkers => doc.replace_minimally(&pending.merged),
        }
        let saved = self.save(id);
        let msg = match how {
            Resolution::KeepMine => format!("Kept your version of “{title}”. The other one is in version history."),
            Resolution::TakeTheirs => format!("Took the other version of “{title}” (Undo brings yours back)."),
            Resolution::MergeWithMarkers => format!("Both versions are in “{title}”; edit between the <<<<<<< and >>>>>>> lines."),
        };
        self.notify(NoticeLevel::Info, msg);
        saved
    }

    /// How many conflict copies the vault has (cached per index version).
    pub fn conflict_copy_count(&self) -> usize {
        let v = self.index.version();
        if let Some((cv, n)) = self.conflict_count_cache.get()
            && cv == v
        {
            return n;
        }
        let n = self.conflict_copies().len();
        self.conflict_count_cache.set(Some((v, n)));
        n
    }

    /// Conflict copies in the vault whose original note exists.
    pub fn conflict_copies(&self) -> Vec<ConflictCopy> {
        let ix = self.index.read();
        let mut out = Vec::new();
        for p in ix.paths() {
            if let Some((stem, tool)) = conflict_original(p.stem()) {
                let original = p.with_stem(&stem);
                if ix.get(&original).is_some() {
                    out.push(ConflictCopy { copy: p.clone(), original, tool });
                }
            }
        }
        out
    }

    /// Preview merging a conflict copy into its original.
    pub fn preview_copy_merge(&self, c: &ConflictCopy) -> Option<MergeResult> {
        let vault = self.vault.as_ref()?;
        let mine = self.doc_text_or_file(&c.original)?;
        let theirs = vault.read(&c.copy).ok()?;
        self.history.flush();
        let snapshots: Vec<String> = vault.history(&c.original).iter().take(50).filter_map(|s| vault.read_snapshot(s).ok()).collect();
        Some(merge::merge(best_base(&snapshots, &mine, &theirs), &mine, &theirs))
    }

    /// Merge a conflict copy into its original, then move the copy to the
    /// trash. Conflicts are written with markers. Returns the number of
    /// conflicts.
    pub fn merge_conflict_copy(&mut self, c: &ConflictCopy) -> Result<usize, String> {
        let r = self.preview_copy_merge(c).ok_or("couldn't read the notes")?;
        self.write_note_text(&c.original, &r.text)?;
        self.discard_conflict_copy(c)?;
        Ok(r.conflicts)
    }

    /// Move a conflict copy to the vault's trash (recoverable).
    pub fn discard_conflict_copy(&mut self, c: &ConflictCopy) -> Result<(), String> {
        let vault = self.vault.clone().ok_or("no vault")?;
        if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(&c.copy)) {
            let id = self.docs[i].id;
            self.close(id);
        }
        vault.trash(&c.copy).map_err(|e| e.to_string())?;
        self.index.remove(c.copy.clone());
        Ok(())
    }

    fn doc_text_or_file(&self, p: &NotePath) -> Option<String> {
        match self.docs.iter().find(|d| d.path.as_ref() == Some(p)) {
            Some(d) => Some(d.text().to_string()),
            None => self.vault.as_ref()?.read(p).ok(),
        }
    }

    /// Replace a note's text: an undoable edit if it's open, otherwise a
    /// versioned write.
    fn write_note_text(&mut self, p: &NotePath, text: &str) -> Result<(), String> {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(p)) {
            self.docs[i].replace_minimally(text);
            let id = self.docs[i].id;
            return if self.save(id) { Ok(()) } else { Err("couldn't save".into()) };
        }
        let vault = self.vault.clone().ok_or("no vault")?;
        let old = vault.read(p).map_err(|e| e.to_string())?;
        let now = crate::history_ops::now_ms();
        self.history.keep(&vault, p, &old, now - 1, true);
        vault.write(p, text).map_err(|e| e.to_string())?;
        self.history.keep(&vault, p, text, now, true);
        self.index.upsert(p.clone(), text.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::Vault;

    #[test]
    fn recognises_sync_tool_copies() {
        assert_eq!(conflict_original("Plan (conflicted copy 2026-11-01)"), Some(("Plan".into(), "Dropbox")));
        assert_eq!(conflict_original("Plan (Asha's conflicted copy 2026-11-01)"), Some(("Plan".into(), "Dropbox")));
        assert_eq!(conflict_original("Plan (conflicted copy 2026-11-01 101500)"), Some(("Plan".into(), "Nextcloud")));
        assert_eq!(conflict_original("Plan.sync-conflict-20261101-101500-ABCDEFG"), Some(("Plan".into(), "Syncthing")));
        assert_eq!(conflict_original("Plan (conflict from Asha's laptop 2026-11-01)"), Some(("Plan".into(), "Quire Sync")));
        assert_eq!(conflict_original("Plan (conflict from phone 2026-11-01 2)"), Some(("Plan".into(), "Quire Sync")));
        assert_eq!(conflict_original("Plan (conflict from the past)"), None);
        assert_eq!(conflict_original("Plan (draft)"), None);
        assert_eq!(conflict_original("Plan.sync-conflict-later"), None);
        assert_eq!(conflict_original("Plan 2"), None, "iCloud-style names are ambiguous, not guessed");
    }

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(files));
        w.index().flush();
        w
    }

    #[test]
    fn merges_a_conflict_copy_using_history_as_the_base() {
        let base = "# Trip\n- flights\n- hotel\n";
        let mut w = ws(&[("Trip.md", base)]);
        let v = w.vault().expect("vault").clone();
        let t = NotePath::new("Trip.md");
        // The version both devices started from is in history.
        v.snapshot(&t, base, 1_000, true).expect("snapshot");
        v.write(&t, "# Trip\n- flights (booked)\n- hotel\n").expect("write");
        let copy = NotePath::new("Trip (conflicted copy 2026-11-01).md");
        v.write(&copy, "# Trip\n- flights\n- hotel\n- visa\n").expect("write");
        w.reload_index();
        w.index().flush();
        let found = w.conflict_copies();
        assert_eq!(found, vec![ConflictCopy { copy: copy.clone(), original: t.clone(), tool: "Dropbox" }]);
        assert_eq!(w.merge_conflict_copy(&found[0]), Ok(0));
        assert_eq!(v.read(&t).expect("read"), "# Trip\n- flights (booked)\n- hotel\n- visa\n");
        assert!(!v.exists(&copy), "the copy goes to the trash");
        w.index().flush();
        assert!(w.conflict_copies().is_empty());
    }

    #[test]
    fn outside_change_merges_into_unsaved_edits() {
        let mut w = ws(&[("N.md", "# N\none\ntwo\nthree\n")]);
        let p = NotePath::new("N.md");
        let id = w.open_note(&p).expect("open");
        // Unsaved edit here: change "one", caret at the end of the note.
        w.active_doc_mut().expect("doc").replace_minimally("# N\nONE\ntwo\nthree\n");
        let end = w.active_doc().expect("doc").buffer().len_chars();
        w.active_doc_mut().expect("doc").set_selection(quire_text::Selection::cursor(end));
        // Meanwhile a phone edits "three" and the file changes on disk.
        let theirs = "# N\none\ntwo\nthree (phone)\n";
        w.vault().expect("vault").write(&p, theirs).expect("write");
        assert!(!w.absorb_external_change(&p, theirs));
        let doc = w.active_doc().expect("doc");
        assert_eq!(&*doc.text(), "# N\nONE\ntwo\nthree (phone)\n", "both edits kept");
        assert_eq!(doc.selection().head, doc.buffer().len_chars(), "caret stays at the end");
        assert!(doc.is_dirty() && !doc.conflict && doc.pending_merge.is_none());
        assert!(w.save(id));
        assert_eq!(w.vault().expect("vault").read(&p).expect("read"), "# N\nONE\ntwo\nthree (phone)\n");
        // Undo takes the merged-in change back out.
        w.active_doc_mut().expect("doc").undo();
        assert_eq!(&*w.active_doc().expect("doc").text(), "# N\nONE\ntwo\nthree\n");
    }

    #[test]
    fn conflicting_outside_change_waits_for_a_decision() {
        let mut w = ws(&[("N.md", "a\nb\n")]);
        let p = NotePath::new("N.md");
        let id = w.open_note(&p).expect("open");
        w.active_doc_mut().expect("doc").replace_minimally("a\nmine\n");
        assert!(!w.absorb_external_change(&p, "a\ntheirs\n"));
        let doc = w.active_doc().expect("doc");
        assert!(doc.conflict, "autosave pauses until resolved");
        assert_eq!(doc.pending_merge.as_ref().map(|m| m.conflicts), Some(1));
        assert_eq!(&*doc.text(), "a\nmine\n", "nothing changes until the user decides");
        assert!(w.resolve_merge(id, Resolution::TakeTheirs));
        assert_eq!(&*w.active_doc().expect("doc").text(), "a\ntheirs\n");
        // Our echo is recognised and ignored.
        assert!(w.absorb_external_change(&p, "a\ntheirs\n"));
    }

    #[test]
    fn resolving_a_pending_merge() {
        let mut w = ws(&[("N.md", "a\nb\n")]);
        let id = w.open_note(&NotePath::new("N.md")).expect("open");
        // Simulate: we edited line b, the file changed b differently elsewhere.
        w.active_doc_mut().expect("doc").replace_minimally("a\nmine\n");
        let r = merge::merge3("a\nb\n", "a\nmine\n", "a\ntheirs\n");
        w.active_doc_mut().expect("doc").pending_merge =
            Some(crate::PendingMerge { theirs: "a\ntheirs\n".into(), merged: r.text, conflicts: 1 });
        assert!(w.resolve_merge(id, Resolution::MergeWithMarkers));
        let text = w.active_doc().expect("doc").text().to_string();
        assert!(text.contains("mine") && text.contains("theirs") && text.contains(merge::MINE));
        assert!(w.active_doc().expect("doc").pending_merge.is_none());
        assert_eq!(w.vault().expect("vault").read(&NotePath::new("N.md")).expect("read"), text, "saved");
        assert!(!w.resolve_merge(id, Resolution::KeepMine), "nothing left to resolve");
    }
}
