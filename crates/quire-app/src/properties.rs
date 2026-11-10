// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Properties (front matter) editing and `quire-view` evaluation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use quire_index::views::{self, ViewResult};
use quire_markdown::frontmatter::{self, Edit, Kind, Value};
use quire_text::{EditKind, Selection, Transaction};
use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

/// Inserted by "Insert view": a starting point that works in any vault.
pub const VIEW_TEMPLATE: &str =
    "```quire-view\nfrom: all\nwhere: status exists\nsort: title\nlayout: table\nfields: [title, status, tags]\n```\n";

/// A property shown in the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub key: String,
    pub value: Value,
    pub kind: Kind,
}

/// Parse what the user typed in a property field. `[a, b]` or a value for a
/// field that is already a list becomes a list.
pub fn parse_input(text: &str, was_list: bool) -> Value {
    let t = text.trim();
    if let Some(inner) = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return Value::List(split_list(inner));
    }
    if was_list {
        return Value::List(split_list(t));
    }
    Value::Str(t.to_string())
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().trim_matches('"').to_string()).filter(|x| !x.is_empty()).collect()
}

/// Memoized view results, keyed by the notes' version and spec text.
type ViewKey = (String, Option<NotePath>);
type ViewEntry = ((u64, u64), String, Rc<Result<ViewResult, String>>);

#[derive(Default)]
pub struct ViewCache {
    entries: RefCell<HashMap<ViewKey, ViewEntry>>,
}

impl ViewCache {
    /// Forget every result (a locked folder locked: rows may hold its text).
    pub(crate) fn clear(&self) {
        self.entries.borrow_mut().clear();
    }
}

impl Workspace {
    /// Front-matter fields of the active note, in file order.
    pub fn active_properties(&self) -> Vec<Property> {
        let Some(doc) = self.active_doc() else { return Vec::new() };
        let text = doc.text();
        let Some(fm) = frontmatter::parse(&text) else { return Vec::new() };
        // Keep the author's order rather than the map's.
        let mut keys: Vec<(usize, &String)> =
            fm.fields.keys().map(|k| (text[..fm.range.end].find(&format!("\n{k}:")).unwrap_or(usize::MAX), k)).collect();
        keys.sort();
        keys.into_iter().map(|(_, k)| Property { key: k.clone(), value: fm.fields[k].clone(), kind: fm.fields[k].kind() }).collect()
    }

    fn apply_fm_edit_active(&mut self, edit: Edit) -> bool {
        let Some(doc) = self.active_doc_mut() else { return false };
        let buf = doc.buffer();
        let (s, e) = (buf.byte_to_char(edit.range.start), buf.byte_to_char(edit.range.end));
        let head = doc.selection().head;
        let new_len = edit.text.chars().count();
        // Keep the caret on the same body text.
        let caret = if head >= e { head - (e - s) + new_len } else { head.min(s + new_len) };
        doc.apply(Transaction::replace(s, e, edit.text).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok()
    }

    /// Set a property on the active note (an undoable edit).
    pub fn set_property(&mut self, key: &str, value: &Value) -> bool {
        let key = key.trim();
        if key.is_empty() || key.contains(':') {
            self.notify(NoticeLevel::Warn, "Property names can't be empty or contain “:”");
            return false;
        }
        let Some(doc) = self.active_doc() else { return false };
        let edit = frontmatter::set_field(&doc.text(), key, value);
        self.apply_fm_edit_active(edit)
    }

    /// Set a field inside a nested map of the active note (`["author",
    /// "name"]`): only that line changes, as an undoable edit.
    pub fn set_property_path(&mut self, path: &[&str], value: &Value) -> bool {
        if path.iter().any(|k| k.trim().is_empty() || k.contains(':')) {
            self.notify(NoticeLevel::Warn, "Property names can't be empty or contain “:”");
            return false;
        }
        let Some(doc) = self.active_doc() else { return false };
        let edit = frontmatter::set_field_path(&doc.text(), path, value);
        self.apply_fm_edit_active(edit)
    }

    /// Remove a field inside a nested map of the active note.
    pub fn remove_property_path(&mut self, path: &[&str]) -> bool {
        let Some(doc) = self.active_doc() else { return false };
        match frontmatter::remove_field_path(&doc.text(), path) {
            Some(edit) => self.apply_fm_edit_active(edit),
            None => false,
        }
    }

    pub fn remove_property(&mut self, key: &str) -> bool {
        let Some(doc) = self.active_doc() else { return false };
        match frontmatter::remove_field(&doc.text(), key) {
            Some(edit) => self.apply_fm_edit_active(edit),
            None => false,
        }
    }

    /// Set a property on any note (kanban drag). Open notes get an undoable
    /// edit and save; closed notes are rewritten with history snapshots.
    pub fn set_note_property(&mut self, path: &NotePath, key: &str, value: &Value) -> bool {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(path)) {
            let prev = self.active;
            self.active = Some(i);
            let ok = self.set_property(key, value);
            let id = self.docs[i].id;
            self.active = prev;
            return ok && self.save(id);
        }
        let Some(vault) = self.vault.clone() else { return false };
        let Ok(src) = vault.read(path) else { return false };
        let out = frontmatter::set_field(&src, key, value).apply(&src);
        let now = crate::history_ops::now_ms();
        self.history.keep(&vault, path, &src, now - 1, true);
        match vault.write(path, &out) {
            Ok(()) => {
                self.history.keep(&vault, path, &out, now, true);
                self.index.upsert(path.clone(), out);
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't update {}: {e}", path.stem()));
                false
            }
        }
    }

    /// Evaluate a view spec (memoized until the index changes).
    pub fn run_view(&self, spec: &str, from: Option<&NotePath>) -> Rc<Result<ViewResult, String>> {
        // 0.12: notes of unlocked folders are in views too (memory only).
        let version = self.notes_version();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let key = (spec.to_string(), from.cloned());
        if let Some((v, d, r)) = self.view_cache.entries.borrow().get(&key)
            && *v == version
            && *d == today
        {
            return Rc::clone(r);
        }
        let result = Rc::new(views::parse(spec).map(|s| self.with_notes(|n| views::run_in(&s, n.records(), &today, from, &|_| true))));
        let mut cache = self.view_cache.entries.borrow_mut();
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(key, (version, today, Rc::clone(&result)));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    fn ws() -> Workspace {
        let vault = quire_vault::Vault::in_memory(&[
            ("Board.md", "# Board\n```quire-view\nfrom: Tasks/\nlayout: kanban\n```\n"),
            ("Tasks/A.md", "---\nstatus: todo\n---\n# A\n"),
            ("Tasks/B.md", "---\nstatus: done\n---\n# B\n"),
        ]);
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(vault);
        w.index().flush();
        w
    }

    #[test]
    fn edit_properties_on_the_active_note() {
        let mut w = ws();
        w.open_note(&NotePath::new("Tasks/A.md")).unwrap();
        assert!(w.set_property("due", &Value::Str("2026-11-05".into())));
        assert!(w.set_property("status", &parse_input("doing", false)));
        assert_eq!(&*w.active_doc().unwrap().text(), "---\nstatus: doing\ndue: 2026-11-05\n---\n# A\n");
        let props = w.active_properties();
        assert_eq!(props.iter().map(|p| (p.key.as_str(), p.kind)).collect::<Vec<_>>(), vec![("status", Kind::Text), ("due", Kind::Date)]);
        assert!(w.remove_property("due"));
        assert!(!w.remove_property("due"));
        assert_eq!(parse_input("[a, b]", false), Value::List(vec!["a".into(), "b".into()]));
        assert_eq!(parse_input("a, b", true), Value::List(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn review_a_multi_line_value_stays_one_property() {
        let mut w = ws();
        w.open_note(&NotePath::new("Tasks/A.md")).unwrap();
        let evil = Value::Str("one\ntags: [evil]\n---\ninjected".into());
        assert!(w.set_property("summary", &evil));
        let text = w.active_doc().unwrap().text().to_string();
        let fm = frontmatter::parse(&text).unwrap();
        assert!(!fm.tags().contains(&"evil".to_string()), "{text}");
        assert_eq!(fm.get("summary"), Some(&evil), "{text}");
        assert!(w.set_property_path(&["author", "name"], &evil));
        let text = w.active_doc().unwrap().text().to_string();
        assert_eq!(frontmatter::parse(&text).unwrap().get_path("author.name"), Some(&evil), "{text}");
    }

    #[test]
    fn views_run_and_kanban_moves_closed_notes() {
        let mut w = ws();
        let spec = "from: Tasks/\nlayout: kanban\n";
        let r = w.run_view(spec, None);
        let r = r.as_ref().as_ref().unwrap();
        assert_eq!(r.groups, vec!["done", "todo"]);
        assert!(w.set_note_property(&NotePath::new("Tasks/A.md"), "status", &Value::Str("done".into())));
        w.index().flush();
        let r = w.run_view(spec, None);
        assert!(r.as_ref().as_ref().unwrap().rows.iter().all(|x| x.group == "done"));
        w.history.flush();
        let v = w.vault().unwrap();
        assert_eq!(v.read(&NotePath::new("Tasks/A.md")).unwrap(), "---\nstatus: done\n---\n# A\n");
        assert!(v.history(&NotePath::new("Tasks/A.md")).len() >= 2, "the move is reversible");
        assert!(w.run_view("layout: pie", None).is_err());
    }
}
