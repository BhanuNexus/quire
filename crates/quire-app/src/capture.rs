// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quick capture: a thought, appended to today's daily note with the time.

use chrono::{DateTime, Local};
use quire_text::{EditKind, Selection, Transaction};
use quire_vault::{NotePath, Vault};

use crate::Workspace;
use crate::workspace::NoticeLevel;

/// `text` as a list item stamped with the time: `- 14:05 text`, with any
/// further lines indented under it.
pub fn capture_entry(text: &str, at: DateTime<Local>) -> String {
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or_default();
    let mut out = format!("- {} {first}", at.format("%H:%M"));
    for l in lines {
        out.push_str("\n  ");
        out.push_str(l);
    }
    out
}

/// `existing` with `entry` added at the end on its own line.
pub fn append_entry(existing: &str, entry: &str) -> String {
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry);
    out.push('\n');
    out
}

/// Append to today's daily note on disk (creating it from the daily
/// template if needed). For the command line, where no note is open.
pub fn capture_to_vault(vault: &Vault, text: &str, at: DateTime<Local>) -> Result<NotePath, String> {
    if text.trim().is_empty() {
        return Err("nothing to capture".into());
    }
    let (path, _) = vault.daily_note(at.date_naive()).map_err(|e| e.to_string())?;
    let current = vault.read(&path).map_err(|e| e.to_string())?;
    vault.write(&path, &append_entry(&current, &capture_entry(text, at))).map_err(|e| e.to_string())?;
    Ok(path)
}

/// [`capture_to_vault`] at the current local time.
pub fn capture_now(vault: &Vault, text: &str) -> Result<NotePath, String> {
    capture_to_vault(vault, text, Local::now())
}

impl Workspace {
    /// Add `text` to today's daily note without switching notes. If the note
    /// is open, the text goes into the open document (undoable, saved as
    /// usual), so nothing typed there is overwritten.
    pub fn capture(&mut self, text: &str) -> Option<NotePath> {
        if text.trim().is_empty() {
            return None;
        }
        let now = Local::now();
        let vault = self.vault()?.clone();
        let (path, _) = match vault.daily_note(now.date_naive()) {
            Ok(r) => r,
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't capture: {e}"));
                return None;
            }
        };
        let entry = capture_entry(text, now);
        if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_ref() == Some(&path)) {
            let end = doc.buffer().len_chars();
            let needs_newline = end > 0 && !doc.text().ends_with('\n');
            let insert = format!("{}{entry}\n", if needs_newline { "\n" } else { "" });
            let sel: Selection = doc.selection();
            if doc.apply(Transaction::insert(end, insert).with_selection(sel), EditKind::Other).is_err() {
                return None;
            }
            let text = doc.text().to_string();
            self.index.upsert(path.clone(), text);
        } else {
            match vault.read(&path).and_then(|cur| {
                let next = append_entry(&cur, &entry);
                vault.write(&path, &next).map(|()| next)
            }) {
                Ok(next) => self.index.upsert(path.clone(), next),
                Err(e) => {
                    self.notify(NoticeLevel::Error, format!("Couldn't capture: {e}"));
                    return None;
                }
            }
        }
        self.notify(NoticeLevel::Info, format!("Added to {}", path.stem()));
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn entries_are_stamped_and_indented() {
        let at = Local.with_ymd_and_hms(2026, 11, 2, 9, 5, 0).unwrap();
        assert_eq!(capture_entry("  Call Asha\nabout the budget \n", at), "- 09:05 Call Asha\n  about the budget");
        assert_eq!(append_entry("# Day", "- x"), "# Day\n- x\n");
        assert_eq!(append_entry("# Day\n", "- x"), "# Day\n- x\n");
        assert_eq!(append_entry("", "- x"), "- x\n");
    }

    #[test]
    fn captures_into_the_daily_note_open_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(crate::Settings::default());
        ws.open_vault(dir.path()).unwrap();
        // Not open: written to disk.
        let p = ws.capture("first thought").unwrap();
        let on_disk = ws.vault().unwrap().read(&p).unwrap();
        assert!(on_disk.contains("first thought"), "{on_disk}");
        assert!(ws.active_doc().is_none_or(|d| d.path.as_ref() != Some(&p)), "doesn't switch notes");
        // Open with unsaved typing: goes into the document, typing kept.
        ws.open_note(&p).unwrap();
        let end = ws.active_doc().unwrap().buffer().len_chars();
        ws.active_doc_mut().unwrap().apply(Transaction::insert(end, "typed\n"), EditKind::Other).unwrap();
        ws.capture("second thought").unwrap();
        let text = ws.active_doc().unwrap().text().to_string();
        assert!(text.contains("typed\n") && text.contains("second thought") && text.contains("first thought"), "{text}");
        assert!(ws.capture("   ").is_none());
        // Command line path.
        let p2 = capture_to_vault(ws.vault().unwrap(), "from the terminal", Local::now()).unwrap();
        assert_eq!(p, p2);
    }
}
