// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Find & replace state and workspace operations.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::Range;
use std::rc::Rc;

use quire_text::{EditKind, Selection, Transaction};
use quire_vault::NotePath;

use crate::document::DocId;
use crate::find::{FileMatches, FindOptions, compile, expand, find_chars, find_in_notes, replace_all, replace_in_string};
use crate::workspace::{NoticeLevel, Workspace};

type MatchKey = (DocId, u64, String, FindOptions);
type MatchCache = RefCell<Option<(MatchKey, Rc<Vec<Range<usize>>>, Option<String>)>>;

#[derive(Default)]
pub struct FindState {
    /// In-note bar.
    pub open: bool,
    pub replace_mode: bool,
    pub query: String,
    pub replacement: String,
    pub opts: FindOptions,
    cache: MatchCache,
    /// Vault-wide dialog.
    pub vault_open: bool,
    pub vault_query: String,
    pub vault_replacement: String,
    pub vault_opts: FindOptions,
    pub vault_results: Vec<FileMatches>,
    /// Notes the user un-ticked in the preview.
    pub vault_excluded: BTreeSet<NotePath>,
    pub vault_error: Option<String>,
}

impl Workspace {
    /// Matches (char ranges) of the find query in the active note, plus a
    /// pattern error if the query is an invalid regex. Cached per revision.
    pub fn find_matches(&self) -> (Rc<Vec<Range<usize>>>, Option<String>) {
        let Some(doc) = self.active_doc() else { return (Rc::default(), None) };
        if !self.find.open || self.find.query.is_empty() {
            return (Rc::default(), None);
        }
        let key = (doc.id, doc.revision(), self.find.query.clone(), self.find.opts);
        if let Some((k, m, e)) = &*self.find.cache.borrow()
            && *k == key
        {
            return (Rc::clone(m), e.clone());
        }
        let (m, e) = match compile(&self.find.query, self.find.opts) {
            Ok(re) => (find_chars(doc.buffer(), &doc.text(), &re), None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let m = Rc::new(m);
        *self.find.cache.borrow_mut() = Some((key, Rc::clone(&m), e.clone()));
        (m, e)
    }

    /// Index of the match the selection is on, if any.
    pub fn find_current(&self) -> Option<usize> {
        let sel = self.active_doc()?.selection();
        let (m, _) = self.find_matches();
        m.iter().position(|r| r.start == sel.start() && r.end == sel.end())
    }

    /// Select the next (or previous) match after the caret, wrapping. False if none.
    pub fn find_step(&mut self, backwards: bool) -> bool {
        let (m, _) = self.find_matches();
        let Some(doc) = self.active_doc_mut() else { return false };
        if m.is_empty() {
            return false;
        }
        let sel = doc.selection();
        let next = if backwards {
            m.iter().rev().find(|r| r.start < sel.start()).or_else(|| m.last())
        } else {
            m.iter().find(|r| r.start > sel.start() || (r.start == sel.start() && sel.is_empty())).or_else(|| m.first())
        };
        if let Some(r) = next {
            doc.set_selection(Selection::new(r.start, r.end));
        }
        true
    }

    /// Replace the selected match, then move to the next one.
    pub fn replace_current(&mut self) {
        let Some(i) = self.find_current() else {
            self.find_step(false);
            return;
        };
        let (m, _) = self.find_matches();
        let Ok(re) = compile(&self.find.query, self.find.opts) else { return };
        let (opts, replacement) = (self.find.opts, self.find.replacement.clone());
        let Some(doc) = self.active_doc_mut() else { return };
        let text = doc.text();
        let r = m[i].clone();
        let byte = doc.buffer().char_to_byte(r.start)..doc.buffer().char_to_byte(r.end);
        let rep = expand(&re, &text, &byte, &replacement, opts);
        let caret = r.start + rep.chars().count();
        let _ = doc.apply(Transaction::replace(r.start, r.end, rep).with_selection(Selection::cursor(caret)), EditKind::Other);
        self.find_step(false);
    }

    /// Replace every match in the note as one undoable step.
    pub fn replace_all_in_note(&mut self) -> usize {
        let Ok(re) = compile(&self.find.query, self.find.opts) else { return 0 };
        let (opts, replacement) = (self.find.opts, self.find.replacement.clone());
        let Some(doc) = self.active_doc_mut() else { return 0 };
        let text = doc.text();
        let (tx, n) = replace_all(doc.buffer(), &text, &re, &replacement, opts);
        if n > 0 {
            let _ = doc.apply(tx, EditKind::Other);
        }
        self.notify(NoticeLevel::Info, format!("Replaced {n} match{}", if n == 1 { "" } else { "es" }));
        n
    }

    /// Current text of every note: open documents win over what's on disk.
    fn note_texts(&self) -> Vec<(NotePath, String)> {
        let ix = self.index.read();
        let mut out: Vec<(NotePath, String)> = ix
            .paths()
            .into_iter()
            .map(|p| {
                let open = self.docs.iter().find(|d| d.path.as_ref() == Some(&p)).map(|d| d.text().to_string());
                let text = open.unwrap_or_else(|| ix.source(&p).unwrap_or_default().to_string());
                (p, text)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Refresh the vault-wide preview.
    pub fn vault_find(&mut self) {
        self.find.vault_results.clear();
        self.find.vault_error = None;
        if self.find.vault_query.is_empty() {
            return;
        }
        match compile(&self.find.vault_query, self.find.vault_opts) {
            Ok(re) => {
                let texts = self.note_texts();
                self.find.vault_results = find_in_notes(texts.iter().map(|(p, t)| (p.clone(), t.as_str())), &re, 4);
            }
            Err(e) => self.find.vault_error = Some(e),
        }
    }

    /// Apply the vault-wide replacement to every ticked note. Returns (notes, matches).
    pub fn vault_replace(&mut self) -> (usize, usize) {
        let Ok(re) = compile(&self.find.vault_query, self.find.vault_opts) else { return (0, 0) };
        let (opts, rep) = (self.find.vault_opts, self.find.vault_replacement.clone());
        let targets: Vec<NotePath> =
            self.find.vault_results.iter().map(|f| f.path.clone()).filter(|p| !self.find.vault_excluded.contains(p)).collect();
        let (mut files, mut total) = (0, 0);
        for path in targets {
            if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(&path)) {
                // Open notes get an undoable edit, then save.
                let doc = &mut self.docs[i];
                let text = doc.text();
                let (tx, n) = replace_all(doc.buffer(), &text, &re, &rep, opts);
                if n > 0 {
                    let _ = doc.apply(tx, EditKind::Other);
                    let id = doc.id;
                    self.save(id);
                    files += 1;
                    total += n;
                }
            } else if let Some(vault) = self.vault.clone()
                && let Ok(src) = vault.read(&path)
            {
                let (out, n) = replace_in_string(&src, &re, &rep, opts);
                if n > 0 {
                    // Closed notes get history too, so a vault-wide replace is reversible.
                    let now = crate::history_ops::now_ms();
                    self.history.keep(&vault, &path, &src, now - 1, true);
                    match vault.write(&path, &out) {
                        Ok(()) => {
                            self.history.keep(&vault, &path, &out, now, true);
                            self.index.upsert(path.clone(), out);
                            files += 1;
                            total += n;
                        }
                        Err(e) => self.notify(NoticeLevel::Error, e.to_string()),
                    }
                }
            }
        }
        self.notify(
            NoticeLevel::Info,
            format!("Replaced {total} match{} in {files} note{}", if total == 1 { "" } else { "es" }, if files == 1 { "" } else { "s" }),
        );
        self.index.flush();
        self.vault_find();
        (files, total)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Settings, Workspace};
    use quire_vault::{NotePath, Vault};

    fn ws() -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[("A.md", "rope one\nRope two"), ("B.md", "a rope here"), ("C.md", "nothing")]));
        w.index().flush();
        w
    }

    #[test]
    fn find_bar_navigates_and_replaces() {
        let mut w = ws();
        w.open_note(&NotePath::new("A.md")).unwrap();
        w.find.open = true;
        w.find.query = "rope".into();
        assert_eq!(w.find_matches().0.len(), 2);
        assert!(w.find_step(false));
        assert_eq!(w.find_current(), Some(0));
        assert!(w.find_step(false));
        assert_eq!(w.find_current(), Some(1));
        assert!(w.find_step(false));
        assert_eq!(w.find_current(), Some(0), "wraps");
        w.find.replacement = "cord".into();
        w.replace_current();
        assert_eq!(&*w.active_doc().unwrap().text(), "cord one\nRope two");
        assert_eq!(w.replace_all_in_note(), 1);
        assert_eq!(&*w.active_doc().unwrap().text(), "cord one\ncord two");
        w.run(crate::Command::Undo);
        assert_eq!(&*w.active_doc().unwrap().text(), "cord one\nRope two");
    }

    #[test]
    fn vault_replace_respects_exclusions_and_open_docs() {
        let mut w = ws();
        w.open_note(&NotePath::new("A.md")).unwrap();
        w.find.vault_query = "rope".into();
        w.find.vault_replacement = "cord".into();
        w.vault_find();
        assert_eq!(w.find.vault_results.iter().map(|f| (f.path.as_str(), f.count)).collect::<Vec<_>>(), vec![("A.md", 2), ("B.md", 1)]);
        w.find.vault_excluded.insert(NotePath::new("B.md"));
        assert_eq!(w.vault_replace(), (1, 2));
        assert_eq!(&*w.active_doc().unwrap().text(), "cord one\ncord two");
        assert_eq!(w.vault().unwrap().read(&NotePath::new("B.md")).unwrap(), "a rope here");
        w.find.vault_excluded.clear();
        assert_eq!(w.vault_replace(), (1, 1));
        assert_eq!(w.vault().unwrap().read(&NotePath::new("B.md")).unwrap(), "a cord here");
        assert_eq!(w.index().read().search("cord", 5).len(), 2);
    }
}
