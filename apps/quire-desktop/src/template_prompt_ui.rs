// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A template's questions, asked in place (design §E.3): under a view
//! with `new: Templates/X.md` ("+ New"), and under a note that still has
//! `{{prompt:…}}` fields to fill in.
//!
//! Owned by WS-E.

use std::collections::HashMap;

use egui::{Id, RichText, Ui};
use quire_app::Workspace;
use quire_app::document::DocId;
use quire_app::index::views::{Source, ViewSpec};
use quire_app::markdown::Value;
use quire_app::tasks_state::Action;
use quire_app::vault::NotePath;
use quire_app::vault::templates::Prompt;

use crate::theme::Palette;

/// Answers being typed, kept in egui's memory per form.
#[derive(Clone, Default)]
struct Form {
    open: bool,
    answers: HashMap<String, String>,
    /// For a board: the column the new note goes in.
    column: String,
    prompts: Vec<Prompt>,
}

/// A board's column for the new note: the field, the columns, the pick.
type Column<'a> = (&'a str, &'a [String], &'a mut String);

/// One field per question: a text box, or a menu for `{{select:…}}`.
fn fields(ui: &mut Ui, id: Id, column: Option<Column<'_>>, prompts: &[Prompt], answers: &mut HashMap<String, String>) -> bool {
    let mut enter = false;
    egui::Grid::new(id.with("grid")).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
        if let Some((field, groups, pick)) = column {
            ui.label(field);
            egui::ComboBox::from_id_salt(id.with("column"))
                .selected_text(if pick.is_empty() { "(none)".to_string() } else { pick.clone() })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for g in groups {
                        ui.selectable_value(pick, g.clone(), if g.is_empty() { "(none)" } else { g });
                    }
                });
            ui.end_row();
        }
        for p in prompts {
            ui.label(&p.key);
            let a = answers.entry(p.key.clone()).or_insert_with(|| p.default.clone());
            if p.options.is_empty() {
                let r = ui.add(egui::TextEdit::singleline(a).desired_width(220.0).hint_text(&p.default));
                enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            } else {
                egui::ComboBox::from_id_salt(id.with(("pick", &p.key))).selected_text(a.clone()).width(220.0).show_ui(ui, |ui| {
                    for o in &p.options {
                        ui.selectable_value(a, o.clone(), o);
                    }
                });
            }
            ui.end_row();
        }
    });
    enter
}

/// The folder a view's new notes go in: its first folder source.
fn view_folder(spec: &ViewSpec, from: Option<&NotePath>) -> Option<String> {
    spec.from.iter().find_map(|s| match s {
        Source::Folder(f) => {
            let base = from.map_or("", |p| p.folder());
            Some(match f.strip_prefix("./").or_else(|| (f == ".").then_some("")) {
                Some(rest) if base.is_empty() => rest.trim_end_matches('/').to_string(),
                Some("") => base.to_string(),
                Some(rest) => format!("{base}/{}", rest.trim_end_matches('/')),
                None => f.clone(),
            })
        }
        _ => None,
    })
}

/// "+ New" under a view with `new:`; asks the template's questions in place.
pub(crate) fn new_button(ui: &mut Ui, ws: &Workspace, pal: &Palette, spec: &ViewSpec, from: Option<&NotePath>, groups: &[String], key: Id) {
    let Some(tpl) = spec.new.as_deref() else { return };
    let template = NotePath::new(if std::path::Path::new(tpl).extension().is_some() { tpl.to_string() } else { format!("{tpl}.md") });
    let id = key.with("quire-new");
    let mut form: Form = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    ui.add_space(4.0);
    if !form.open {
        let r = ui.small_button("+ New").on_hover_text(format!("A new note from {}", template.stem()));
        if r.clicked() {
            form.prompts = ws.template_prompts(&template);
            form.column = groups.first().cloned().unwrap_or_default();
            if form.prompts.is_empty() && groups.len() <= 1 {
                submit(ws, spec, from, &template, &form);
            } else {
                form.open = true;
            }
        }
        ui.data_mut(|d| d.insert_temp(id, form));
        return;
    }
    let mut create = false;
    let mut cancel = false;
    // A panel fill so the text boxes (drawn in `bg`) stand out.
    egui::Frame::NONE.fill(pal.panel).stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
        ui.label(RichText::new(format!("New note from {}", template.stem())).size(12.0).color(pal.muted).strong());
        let column = spec.group.as_deref().filter(|_| groups.len() > 1).map(|f| (f, groups, &mut form.column));
        create |= fields(ui, id, column, &form.prompts, &mut form.answers);
        ui.horizontal(|ui| {
            create |= ui.button("Create").clicked();
            cancel |= ui.button("Cancel").clicked();
        });
    });
    if create {
        submit(ws, spec, from, &template, &form);
        form = Form::default();
    } else if cancel {
        form = Form::default();
    }
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn submit(ws: &Workspace, spec: &ViewSpec, from: Option<&NotePath>, template: &NotePath, form: &Form) {
    let mut preset = Vec::new();
    if let Some(field) = spec.group.as_deref()
        && !form.column.is_empty()
    {
        preset.push((field.to_string(), Value::Str(form.column.clone())));
    }
    ws.plan_request(Action::New {
        template: template.clone(),
        folder: view_folder(spec, from),
        answers: form.answers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        preset,
    });
}

/// Fields left in a note made from a template: a small form to fill them in.
pub(crate) fn fill_widget(ui: &mut Ui, ws: &Workspace, doc: DocId, pal: &Palette) {
    let Some(d) = ws.doc(doc) else { return };
    let prompts = quire_app::templates_ui_state::pending_prompts(&d.text());
    if prompts.is_empty() {
        return;
    }
    let id = ui.scope_id().with(("quire-fill", doc.0));
    let mut answers: HashMap<String, String> = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let mut go = false;
    egui::Frame::NONE.fill(pal.panel).stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width().min(460.0));
        ui.label(
            RichText::new(format!(
                "This note has {} template field{} to fill in",
                prompts.len(),
                if prompts.len() == 1 { "" } else { "s" }
            ))
            .size(12.0)
            .color(pal.muted)
            .strong(),
        );
        go |= fields(ui, id, None, &prompts, &mut answers);
        go |= ui.button("Fill in").on_hover_text("One change you can undo").clicked();
    });
    if go {
        ws.plan_request(Action::FillTemplate { doc, answers: answers.drain().collect() });
    }
    ui.data_mut(|d| d.insert_temp(id, answers));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_notes_go_in_the_views_folder() {
        let spec = quire_app::index::views::parse("from: ./Projects/, #x\nnew: Templates/P").unwrap();
        assert_eq!(view_folder(&spec, Some(&NotePath::new("Work/Board.md"))).as_deref(), Some("Work/Projects"));
        let spec = quire_app::index::views::parse("from: Areas/\n").unwrap();
        assert_eq!(view_folder(&spec, None).as_deref(), Some("Areas"));
        assert_eq!(view_folder(&quire_app::index::views::parse("from: #x").unwrap(), None), None);
    }
}
