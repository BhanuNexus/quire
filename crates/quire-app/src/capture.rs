// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quick capture: a thought, appended to today's daily note with the time.

use chrono::{DateTime, Local};
use quire_text::{EditKind, Transaction};
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

/// Where `entry` goes in `existing`: a byte range to replace and the text to put there.
///
/// Under a `Log`, `Inbox`, `Captures` or `Journal` heading if the note has
/// one (taking the place of an empty `- ` bullet the template left),
/// otherwise at the end.
pub fn placement(existing: &str, entry: &str) -> (std::ops::Range<usize>, String) {
    const SECTIONS: [&str; 5] = ["log", "inbox", "captures", "capture", "journal"];
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    for l in existing.split_inclusive('\n') {
        lines.push((at, l));
        at += l.len();
    }
    let heading = |l: &str| -> Option<(usize, String)> {
        let t = l.trim_end();
        let level = t.bytes().take_while(|b| *b == b'#').count();
        (level > 0 && level <= 6 && t[level..].starts_with(' ')).then(|| (level, t[level..].trim().to_lowercase()))
    };
    if let Some(h) = lines.iter().position(|(_, l)| heading(l).is_some_and(|(_, name)| SECTIONS.contains(&name.as_str()))) {
        let level = heading(lines[h].1).map_or(1, |(lv, _)| lv);
        let end = lines[h + 1..].iter().position(|(_, l)| heading(l).is_some_and(|(lv, _)| lv <= level)).map_or(lines.len(), |k| h + 1 + k);
        let last = (h + 1..end).rev().find(|&k| !lines[k].1.trim().is_empty());
        return match last {
            // The template's empty bullet: fill it in.
            Some(k) if lines[k].1.trim() == "-" => {
                let (start, l) = lines[k];
                let body = l.trim_end_matches(['\n', '\r']);
                (start..start + body.len(), entry.to_string())
            }
            Some(k) => {
                let (start, l) = lines[k];
                let body = l.trim_end_matches(['\n', '\r']);
                (start + body.len()..start + body.len(), format!("\n{entry}"))
            }
            None => {
                let (start, l) = lines[h];
                let body = l.trim_end_matches(['\n', '\r']);
                (start + body.len()..start + body.len(), format!("\n{entry}"))
            }
        };
    }
    let end = existing.len();
    let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
    (end..end, format!("{sep}{entry}\n"))
}

/// `existing` with `entry` placed by [`placement`].
pub fn append_entry(existing: &str, entry: &str) -> String {
    let (range, text) = placement(existing, entry);
    let mut out = existing.to_string();
    out.replace_range(range, &text);
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
            let (range, insert) = placement(&doc.text(), &entry);
            let (a, b) = (doc.byte_to_char(range.start), doc.byte_to_char(range.end));
            // No explicit selection: the caret moves with the text around it.
            if doc.apply(Transaction::replace(a, b, insert), EditKind::Other).is_err() {
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
        // Into the Log section, filling the template's empty bullet.
        let daily = "# 2026-11-02\n\n## Log\n- \n\n## Tasks\n- [ ] \n";
        assert_eq!(append_entry(daily, "- 09:05 a"), "# 2026-11-02\n\n## Log\n- 09:05 a\n\n## Tasks\n- [ ] \n");
        let once = append_entry(daily, "- 09:05 a");
        assert_eq!(append_entry(&once, "- 09:06 b"), "# 2026-11-02\n\n## Log\n- 09:05 a\n- 09:06 b\n\n## Tasks\n- [ ] \n");
        // A Log heading with nothing under it, at the end of the note.
        assert_eq!(append_entry("# Day\n## Inbox", "- x"), "# Day\n## Inbox\n- x");
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
        // The caret stays with the text it was in, even when the entry lands above it.
        let doc = ws.active_doc_mut().unwrap();
        let typed_at = doc.text().find("typed").unwrap();
        let c = doc.byte_to_char(typed_at);
        doc.set_selection(quire_text::Selection::cursor(c));
        ws.capture("third thought").unwrap();
        let doc = ws.active_doc().unwrap();
        let caret_byte = doc.buffer().char_to_byte(doc.selection().head);
        assert_eq!(&doc.text()[caret_byte..caret_byte + 5], "typed");
        assert!(ws.capture("   ").is_none());
        // Command line path.
        let p2 = capture_to_vault(ws.vault().unwrap(), "from the terminal", Local::now()).unwrap();
        assert_eq!(p, p2);
    }
}
