// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Making notes from templates (design §E.3).
//!
//! * "+ New" in a view (`new: Templates/Project.md`) asks the template's
//!   questions in the view, then makes the note in the template's
//!   `folder:`, named by its `name:`, with the board column preset.
//! * Periodic notes and new empty notes in a folder a template `applies_to`
//!   get the template at once; questions without a default stay as
//!   `{{prompt:…}}` and the note offers to fill them in (as does any note
//!   made by the 0.8 "New note from template" command).
//!
//! Owned by WS-E.

use std::collections::HashMap;

use chrono::{Local, NaiveDate};
use quire_markdown::Value;
use quire_text::Selection;
use quire_vault::NotePath;
use quire_vault::templates::{self, Prompt, TemplateMeta, TemplateVars};

use crate::document::DocId;
use crate::workspace::{NoticeLevel, Workspace};

/// Templates and their settings, read from disk (they aren't indexed).
pub(crate) fn template_metas(ws: &Workspace) -> Vec<(NotePath, TemplateMeta)> {
    let Some(vault) = ws.vault() else { return Vec::new() };
    ws.templates().into_iter().filter_map(|p| Some((p.clone(), templates::template_meta(&vault.read(&p).ok()?)))).collect()
}

fn in_folder(path: &str, folder: &str) -> bool {
    let f = folder.trim_matches('/');
    !f.is_empty() && path.len() > f.len() && path[..f.len()].eq_ignore_ascii_case(f) && path.as_bytes()[f.len()] == b'/'
}

/// Placeholders left in `text`, each key once.
pub fn pending_prompts(text: &str) -> Vec<Prompt> {
    let mut v: Vec<Prompt> = Vec::new();
    for (_, p) in templates::pending_prompts(text) {
        if !v.iter().any(|o| o.key.eq_ignore_ascii_case(&p.key)) {
            v.push(p);
        }
    }
    v
}

fn answer<'a>(answers: &'a HashMap<String, String>, p: &'a Prompt) -> &'a str {
    answers.iter().find(|(k, _)| k.eq_ignore_ascii_case(&p.key)).map_or(p.default.as_str(), |(_, v)| v.as_str())
}

impl Workspace {
    /// The template whose `applies_to` covers `path`'s folder (deepest wins).
    pub fn folder_template(&self, path: &NotePath) -> Option<NotePath> {
        template_metas(self)
            .into_iter()
            .flat_map(|(p, m)| m.applies_to.into_iter().map(move |f| (f, p.clone())))
            .filter(|(f, _)| in_folder(path.as_str(), f))
            .max_by_key(|(f, _)| f.len())
            .map(|(_, p)| p)
    }

    /// A template's questions (its `name:` pattern's too).
    pub fn template_prompts(&self, template: &NotePath) -> Vec<Prompt> {
        self.vault().and_then(|v| v.read(template).ok()).map(|t| templates::required_prompts(&t)).unwrap_or_default()
    }

    /// Make a note from `template` with these answers: in `folder` (else the
    /// template's `folder:`), named by its `name:` (else the template's
    /// name), with `preset` properties set (dotted keys for nested ones).
    pub fn new_note_from_template(
        &mut self,
        template: &NotePath,
        folder: Option<&str>,
        answers: &HashMap<String, String>,
        preset: &[(String, Value)],
    ) -> Option<DocId> {
        self.new_note_from_template_titled(template, None, folder, answers, preset)
    }

    /// [`Workspace::new_note_from_template`], named `title` when one is
    /// given (it wins over the template's `name:`): "New note from template…"
    /// with a title typed first.
    pub fn new_note_from_template_titled(
        &mut self,
        template: &NotePath,
        title: Option<&str>,
        folder: Option<&str>,
        answers: &HashMap<String, String>,
        preset: &[(String, Value)],
    ) -> Option<DocId> {
        let text = match self.vault()?.read(template) {
            Ok(t) => t,
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't read the template {}: {e}", template.stem()));
                return None;
            }
        };
        let now = Local::now();
        let meta = templates::template_meta(&text);
        let folder = folder.map(str::to_string).or_else(|| meta.folder.clone()).unwrap_or_default();
        let vars0 = TemplateVars { title: String::new(), now };
        let typed = title.map(quire_vault::sanitize_title).filter(|t| !t.trim().is_empty());
        let name = typed
            .or_else(|| {
                meta.name.as_deref().map(|n| templates::expand_name(&text, n, &vars0, &folder, answers)).filter(|n| !n.trim().is_empty())
            })
            .unwrap_or_else(|| template.stem().to_string());
        let path = self.vault()?.unique_path(&folder, &name).ok()?;
        let vars = TemplateVars { title: path.stem().to_string(), now };
        let mut e = templates::expand(&text, &vars, &folder, answers);
        if !preset.is_empty() {
            for (k, v) in preset {
                let parts: Vec<&str> = k.split('.').collect();
                e.text = quire_markdown::frontmatter::set_field_path(&e.text, &parts, v).apply(&e.text);
            }
            e.cursor = None; // the text moved; the caret goes to the end
        }
        self.create_note_at(&path, &e.text, e.cursor)
    }

    /// Make `path` from a template for `date` (a periodic note). Questions
    /// without a default are left to fill in.
    pub fn new_note_at_from_template(&mut self, path: &NotePath, template: &NotePath, date: NaiveDate) -> Option<DocId> {
        let text = self.vault()?.read(template).ok()?;
        let when = date.and_hms_opt(9, 0, 0).and_then(|d| d.and_local_timezone(Local).single()).unwrap_or_else(Local::now);
        let vars = TemplateVars { title: path.stem().to_string(), now: when };
        let e = templates::expand_deferred(&text, &vars, path.folder());
        self.create_note_at(path, &e.text, e.cursor)
    }

    /// Fill a note's `{{prompt:…}}` placeholders: one undoable edit.
    pub fn fill_template_fields(&mut self, doc: DocId, answers: &HashMap<String, String>) -> bool {
        let Some(i) = self.docs.iter().position(|d| d.id == doc) else { return false };
        let text = self.docs[i].text().to_string();
        let found = templates::pending_prompts(&text);
        if found.is_empty() {
            return false;
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (range, p) in found {
            out.push_str(&text[at..range.start]);
            out.push_str(answer(answers, &p));
            at = range.end;
        }
        out.push_str(&text[at..]);
        self.docs[i].replace_minimally(&out);
        self.features.decor_generation += 1;
        true
    }

    /// Write a new note, open it, and put the caret at `cursor` (else the end).
    pub(crate) fn create_note_at(&mut self, path: &NotePath, text: &str, cursor: Option<usize>) -> Option<DocId> {
        let vault = self.vault()?.clone();
        if vault.exists(path) {
            return self.open_note(path);
        }
        if let Err(e) = vault.write(path, text) {
            self.notify(NoticeLevel::Error, format!("Couldn't create {}: {e}", path.stem()));
            return None;
        }
        self.index.upsert(path.clone(), text.to_string());
        let id = self.open_note(path)?;
        if let Some(doc) = self.active_doc_mut() {
            let end = doc.buffer().len_chars();
            doc.set_selection(Selection::cursor(cursor.unwrap_or(end).min(end)));
        }
        Some(id)
    }
}

/// A brand-new empty note in a folder with a default template gets it.
pub(crate) fn fill_new_notes(ws: &mut Workspace) -> bool {
    let Some(doc) = ws.active_doc() else { return false };
    if !doc.auto_name || doc.revision() != 0 || doc.buffer().len_chars() != 0 {
        return false;
    }
    let (id, Some(path)) = (doc.id, doc.path.clone()) else { return false };
    if !ws.features.tasks.checked_new.insert(id) {
        return false;
    }
    let Some(tpl) = ws.folder_template(&path) else { return false };
    let Some(text) = ws.vault().and_then(|v| v.read(&tpl).ok()) else { return false };
    let vars = TemplateVars { title: path.stem().to_string(), now: Local::now() };
    let e = templates::expand_deferred(&text, &vars, path.folder());
    let Some(i) = ws.docs.iter().position(|d| d.id == id) else { return false };
    ws.docs[i].replace_minimally(&e.text);
    if let Some(c) = e.cursor {
        ws.docs[i].set_selection(Selection::cursor(c));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const TPL: &str = "---\nquire-template:\n  folder: Meetings\n  name: \"{{date:%Y}} {{prompt:Topic}}\"\n  applies_to: [Calls]\n---\n# {{prompt:Topic}}\nStatus: {{select:Status|planned,done}}\n{{cursor}}\n";

    fn ws() -> Workspace {
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(&[
            ("Templates/Meeting.md", TPL),
            ("Templates/Plain.md", "# {{title}}\n"),
            ("Calls/x.md", ""),
        ]));
        w.index().flush();
        w
    }

    #[test]
    fn creates_in_the_templates_folder_with_answers() {
        let mut w = ws();
        let tpl = NotePath::new("Templates/Meeting.md");
        assert_eq!(w.template_prompts(&tpl).len(), 2);
        let answers = HashMap::from([("Topic".to_string(), "Roadmap".to_string())]);
        let id = w.new_note_from_template(&tpl, None, &answers, &[("status".into(), Value::Str("doing".into()))]).unwrap();
        let year = Local::now().format("%Y").to_string();
        let doc = w.doc(id).unwrap();
        assert_eq!(doc.path.as_ref().unwrap().as_str(), format!("Meetings/{year} Roadmap.md"));
        assert_eq!(&*doc.text(), "---\nstatus: doing\n---\n# Roadmap\nStatus: planned\n\n");
        let id = w.new_note_from_template(&"Templates/Plain.md".into(), Some("Notes"), &HashMap::new(), &[]).unwrap();
        assert_eq!(w.doc(id).unwrap().path.as_ref().unwrap().as_str(), "Notes/Plain.md");
    }

    #[test]
    fn folder_defaults_and_filling_fields_later() {
        let mut w = ws();
        w.open_note(&"Calls/x.md".into()).unwrap();
        w.active_doc_mut().unwrap().auto_name = true;
        assert!(fill_new_notes(&mut w));
        assert!(!fill_new_notes(&mut w), "once");
        assert_eq!(&*w.active_doc().unwrap().text(), "# {{prompt:Topic}}\nStatus: planned\n\n");
        let id = w.active_doc().unwrap().id;
        assert_eq!(pending_prompts(&w.active_doc().unwrap().text()).len(), 1);
        assert!(w.fill_template_fields(id, &HashMap::from([("topic".to_string(), "Call".to_string())])));
        assert_eq!(&*w.active_doc().unwrap().text(), "# Call\nStatus: planned\n\n");
        assert!(w.active_doc_mut().unwrap().undo(), "one undoable edit");
    }
}
