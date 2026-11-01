// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Projects: a note in `Projects/` tagged `#project`, with `status`, `due`
//! and `owner` properties, so it shows up on the project board (a
//! `quire-view` kanban). There's no separate database: it's all Markdown.

use chrono::Local;
use quire_markdown::frontmatter::{self, Value};
use quire_text::Selection;
use quire_vault::{NotePath, TemplateVars, expand_template};

use crate::DocId;
use crate::workspace::{NoticeLevel, Workspace};

pub const PROJECTS_FOLDER: &str = "Projects";
/// Used when the vault has no `Templates/Project.md`.
pub const PROJECT_TEMPLATE: &str =
    "---\ntags: [project]\nstatus: planned\n---\n# {{title}}\n\n## Goal\n{{cursor}}\n\n## Tasks\n- [ ] \n\n## Notes\n";
pub const BOARD_TEMPLATE: &str = "# Projects\n\nEvery note tagged `#project` appears here. Drag a card by its handle to change its status; click a title to open it.\n\n```quire-view\nfrom: \"#project\"\nlayout: kanban\ngroup: status\ncolumns: [planned, active, done]\nfields: [title, due, owner]\n```\n\n## Open projects by due date\n\n```quire-view\nfrom: \"#project\"\nwhere: status != done\nsort: due asc\nlayout: table\nfields: [title, status, due, owner, open_tasks]\n```\n";
pub const STATUSES: [&str; 3] = ["planned", "active", "done"];

/// What the "New project" dialog collects.
#[derive(Debug, Clone, Default)]
pub struct NewProject {
    pub name: String,
    pub status: String,
    /// `YYYY-MM-DD` or empty.
    pub due: String,
    pub owner: String,
}

impl Workspace {
    /// Create `Projects/<name>.md`, open it with the caret under "Goal", and
    /// add a project board to the folder if it has none.
    pub fn new_project(&mut self, p: &NewProject) -> Result<DocId, String> {
        let name = p.name.trim();
        if quire_vault::sanitize_title(name).is_empty() {
            return Err("Give the project a name".into());
        }
        let due = p.due.trim();
        if !due.is_empty() && !frontmatter::is_date(due) {
            return Err("Use a date like 2026-12-31 for the due date".into());
        }
        let vault = self.vault.clone().ok_or("Open a vault first")?;
        let tpl_path = NotePath::join(&vault.config.templates_folder, "Project.md");
        let mut tpl = vault.read(&tpl_path).unwrap_or_else(|_| PROJECT_TEMPLATE.to_string());

        // Properties go into the template before expansion, so {{cursor}} stays put.
        let fm = frontmatter::parse(&tpl);
        let mut tags = fm.as_ref().map(|f| f.tags()).unwrap_or_default();
        if !tags.iter().any(|t| t.eq_ignore_ascii_case("project")) {
            tags.push("project".into());
            tpl = frontmatter::set_field(&tpl, "tags", &Value::List(tags)).apply(&tpl);
        }
        let status = if p.status.trim().is_empty() { "planned" } else { p.status.trim() };
        tpl = frontmatter::set_field(&tpl, "status", &Value::Str(status.into())).apply(&tpl);
        for (key, val) in [("due", due), ("owner", p.owner.trim())] {
            if !val.is_empty() {
                tpl = frontmatter::set_field(&tpl, key, &Value::Str(val.into())).apply(&tpl);
            }
        }
        let (body, cursor) = expand_template(&tpl, &TemplateVars { title: name.into(), now: Local::now() });
        let path = vault.unique_path(PROJECTS_FOLDER, name).map_err(|e| e.to_string())?;
        vault.write(&path, &body).map_err(|e| e.to_string())?;
        self.index.upsert(path.clone(), body.clone());

        let board = self.ensure_project_board();
        let id = self.open_note(&path).ok_or("Couldn't open the new project")?;
        if let (Some(c), Some(doc)) = (cursor, self.active_doc_mut()) {
            doc.set_selection(Selection::cursor(c));
        }
        let extra = if board { " · added a project board in Projects/" } else { "" };
        self.notify(NoticeLevel::Info, format!("Created project “{}”{extra}", path.stem()));
        Ok(id)
    }

    /// Create `Projects/Board.md` unless some note in Projects/ already has a
    /// view. Returns true if it was created.
    fn ensure_project_board(&mut self) -> bool {
        let Some(vault) = self.vault.clone() else { return false };
        let prefix = format!("{PROJECTS_FOLDER}/");
        let has_view = vault
            .notes()
            .unwrap_or_default()
            .iter()
            .filter(|p| p.as_str().starts_with(&prefix))
            .any(|p| vault.read(p).is_ok_and(|s| s.contains("```quire-view")));
        if has_view {
            return false;
        }
        match vault.unique_path(PROJECTS_FOLDER, "Board") {
            Ok(path) if vault.write(&path, BOARD_TEMPLATE).is_ok() => {
                self.index.upsert(path, BOARD_TEMPLATE.to_string());
                true
            }
            _ => false,
        }
    }

    /// New "Untitled" note inside `folder` (named from its first line).
    pub fn new_note_in(&mut self, folder: &str) -> Result<DocId, String> {
        let folder = folder.trim().trim_matches('/');
        if folder.split('/').any(|seg| seg.is_empty() || seg != quire_vault::sanitize_title(seg) || seg.starts_with('.'))
            && !folder.is_empty()
        {
            return Err(format!("“{folder}” isn't a valid folder name"));
        }
        let vault = self.vault.clone().ok_or("Open a vault first")?;
        let path = vault.create_note(folder, "Untitled", "").map_err(|e| e.to_string())?;
        self.index.upsert(path.clone(), String::new());
        let id = self.open_note(&path).ok_or("Couldn't open the new note")?;
        if let Some(d) = self.active_doc_mut() {
            d.auto_name = true;
        }
        Ok(id)
    }

    /// Folders for pickers (folders that contain notes).
    pub fn folders(&self) -> Vec<String> {
        self.vault.as_ref().map(|v| v.folders()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::Vault;

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(files));
        w.index().flush();
        w
    }

    #[test]
    fn creates_project_note_and_board() {
        let mut w = ws(&[("Welcome.md", "# Hi")]);
        let p = NewProject { name: "Website relaunch".into(), status: "active".into(), due: "2026-12-15".into(), owner: "Asha".into() };
        w.new_project(&p).unwrap();
        let v = w.vault().unwrap().clone();
        let text = v.read(&NotePath::new("Projects/Website relaunch.md")).unwrap();
        assert!(
            text.starts_with("---\ntags: [project]\nstatus: active\ndue: 2026-12-15\nowner: Asha\n---\n# Website relaunch\n"),
            "{text}"
        );
        // Caret sits under "## Goal", where {{cursor}} was.
        let doc = w.active_doc().unwrap();
        let caret = doc.selection().head;
        let line = doc.buffer().char_to_line(caret);
        assert_eq!(doc.buffer().line(line - 1).trim_end(), "## Goal");
        assert_eq!(doc.buffer().line(line).trim_end(), "");
        assert!(v.read(&NotePath::new("Projects/Board.md")).unwrap().contains("layout: kanban"));
        // The board shows the project.
        w.index().flush();
        let r = w.run_view("from: \"#project\"\nlayout: kanban", None);
        assert_eq!(r.as_ref().as_ref().unwrap().rows[0].group, "active");
        // A second project doesn't add a second board.
        w.new_project(&NewProject { name: "Hiring".into(), ..Default::default() }).unwrap();
        assert!(!v.exists(&NotePath::new("Projects/Board 1.md")));
        assert!(v.read(&NotePath::new("Projects/Hiring.md")).unwrap().contains("status: planned"));
    }

    #[test]
    fn honours_a_custom_template_and_validates() {
        let mut w = ws(&[("Templates/Project.md", "---\ntags: [work]\n---\n# {{title}}\nOwner checklist\n")]);
        w.new_project(&NewProject { name: "Q1 plan".into(), ..Default::default() }).unwrap();
        let text = w.vault().unwrap().read(&NotePath::new("Projects/Q1 plan.md")).unwrap();
        assert_eq!(text, "---\ntags: [work, project]\nstatus: planned\n---\n# Q1 plan\nOwner checklist\n");
        assert!(w.new_project(&NewProject { name: "  ".into(), ..Default::default() }).is_err());
        assert!(w.new_project(&NewProject { name: "X".into(), due: "next week".into(), ..Default::default() }).is_err());
    }

    #[test]
    fn notes_in_folders_and_moving() {
        let mut w = ws(&[("Ideas.md", "# Ideas\n[[Inbox/Plan]]"), ("Inbox/Plan.md", "# Plan")]);
        w.new_note_in("Reading/Books").unwrap();
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Reading/Books/Untitled.md");
        assert!(w.new_note_in("../x").is_err());
        w.open_note(&NotePath::new("Inbox/Plan.md")).unwrap();
        w.move_active("Projects").unwrap();
        assert_eq!(w.active_doc().unwrap().path.as_ref().unwrap().as_str(), "Projects/Plan.md");
        assert_eq!(w.vault().unwrap().read(&NotePath::new("Ideas.md")).unwrap(), "# Ideas\n[[Projects/Plan]]");
        assert!(w.folders().contains(&"Projects".to_string()));
    }
}
