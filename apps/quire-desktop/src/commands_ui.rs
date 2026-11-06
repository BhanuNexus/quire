// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Commands and Settings → Toolbar (0.11).
//!
//! Commands: search every command, change its shortcuts (conflicts are
//! checked for Windows, macOS and Linux, `AltGr` included), reset one or all,
//! pin commands to the top of the palette, and make macros: a name and a
//! list of commands run in order (no scripting), which can have shortcuts,
//! be pinned and be toolbar buttons. Toolbar: the buttons, their order,
//! icons and labels. Everything is stored in the device's settings.

use egui::{Key, RichText, Ui};
use quire_app::keymap::{self, Action, Conflict, Keymap};
use quire_app::settings::commands::macro_id;
use quire_app::settings::toolbar::SEPARATOR;
use quire_app::settings::{Macro, ToolbarItem};
use quire_app::{COMMANDS, KeyCombo, Workspace};

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// Settings → Commands state.
#[derive(Default)]
pub(crate) struct CommandsUi {
    query: String,
    /// Waiting for keys for this action; `add` keeps its other shortcuts.
    recording: Option<(Action, bool)>,
    /// Keys pressed that conflict with something: assign anyway or cancel.
    pending: Option<(Action, KeyCombo, bool, Vec<Conflict>)>,
    /// Settings → Toolbar: what to look for among the commands to add.
    add_pick: String,
}

const RECORDING: &str = "quire-recording-keys";

/// Whether Settings → Commands is waiting for a shortcut (this frame or
/// the last): Escape then cancels the recording instead of closing
/// Settings, and Ctrl+C/V/X arrive as keys.
pub(crate) fn recording(ctx: &egui::Context) -> bool {
    let now = ctx.cumulative_frame_nr();
    ctx.data(|d| d.get_temp::<u64>(egui::Id::unique(RECORDING))).is_some_and(|f| now.saturating_sub(f) <= 1)
}

pub(crate) const fn available(_ws: &Workspace) -> bool {
    true
}

fn note(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(pal.muted)).wrap());
}

/// The shortcut being pressed now, if a key (not just a modifier) went down.
fn pressed_combo(ui: &Ui) -> Option<Result<KeyCombo, String>> {
    ui.input(|i| {
        i.events.iter().find_map(|e| match e {
            // Modifiers alone (Ctrl, Shift, Alt, Super) wait for the key they go with.
            egui::Event::Key {
                key:
                    Key::ShiftLeft
                    | Key::ShiftRight
                    | Key::ControlLeft
                    | Key::ControlRight
                    | Key::AltLeft
                    | Key::AltRight
                    | Key::SuperLeft
                    | Key::SuperRight,
                ..
            } => None,
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                if modifiers.ctrl && !modifiers.command {
                    return Some(Err("Control (⌃) shortcuts aren't supported on macOS: use ⌘".into()));
                }
                let Some(name) = KeyCombo::key_name(key.name()) else {
                    return Some(Err(format!("“{}” can't be part of a shortcut", key.name())));
                };
                Some(Ok(KeyCombo { cmd: modifiers.command, shift: modifiers.shift, alt: modifiers.alt, key: name }))
            }
            _ => None,
        })
    })
}

/// Settings → Commands.
pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    commands_tab(ui, ws, &mut st.commands, pal);
    PanelOut::default()
}

fn key_chips(ui: &mut Ui, keys: &[KeyCombo], pal: &Palette, mac: bool) {
    if keys.is_empty() {
        ui.label(RichText::new("no shortcut").size(12.0).color(pal.faint));
    }
    for k in keys {
        ui.label(RichText::new(k.label(mac)).monospace().size(12.0).color(pal.accent));
    }
}

/// The recording, conflict and buttons part of one command's (or macro's) row.
fn shortcut_controls(ui: &mut Ui, ws: &mut Workspace, st: &mut CommandsUi, keymap: &Keymap, action: &Action, pal: &Palette) {
    let mac = cfg!(target_os = "macos");
    let keys = keymap.keys(action);
    let recording_this = st.recording.as_ref().is_some_and(|(a, _)| a == action);
    if recording_this {
        let frame = ui.ctx().cumulative_frame_nr();
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::unique(RECORDING), frame));
        ui.label(RichText::new("Press the keys… (Escape cancels)").size(12.0).color(pal.accent));
        let escape = ui.input(|i| {
            i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Escape, pressed: true, modifiers, .. } if modifiers.is_none()))
        });
        if ui.small_button("Cancel").clicked() || escape {
            st.recording = None;
        } else if let Some(got) = pressed_combo(ui) {
            let add = st.recording.take().is_some_and(|(_, add)| add);
            match got {
                Ok(combo) => {
                    let conflicts = keymap.conflicts(&combo, action);
                    ui.ctx().request_repaint();
                    if conflicts.iter().any(Conflict::blocks) {
                        st.pending = Some((action.clone(), combo, add, conflicts));
                    } else {
                        assign(ws, keymap, action, combo, add, false);
                        for c in conflicts {
                            ws.notify(quire_app::workspace::NoticeLevel::Info, c.describe());
                        }
                    }
                }
                Err(why) => ws.notify(quire_app::workspace::NoticeLevel::Warn, why),
            }
        }
        return;
    }
    key_chips(ui, &keys, pal, mac);
    if ui.small_button("Change").on_hover_text("Replace this command's shortcuts with new keys").clicked() {
        st.recording = Some((action.clone(), false));
        st.pending = None;
        ui.ctx().request_repaint();
    }
    if !keys.is_empty() && ui.small_button("+").on_hover_text("Add another shortcut").clicked() {
        st.recording = Some((action.clone(), true));
        st.pending = None;
        ui.ctx().request_repaint();
    }
    if !keys.is_empty() && ui.small_button("×").on_hover_text("Remove the shortcuts").clicked() {
        keymap::set_keys(&mut ws.settings.commands, action, Some(&[]));
    }
    if keymap::is_customized(&ws.settings.commands, action)
        && matches!(action, Action::Command(_))
        && ui.small_button("Reset").on_hover_text("Back to Quire's shortcut").clicked()
    {
        keymap::set_keys(&mut ws.settings.commands, action, None);
    }
}

/// Give `action` the keys `combo` (added to its others, or instead of
/// them). With `steal`, other commands lose those keys.
fn assign(ws: &mut Workspace, keymap: &Keymap, action: &Action, combo: KeyCombo, add: bool, steal: bool) {
    let mut keys = if add { keymap.keys(action) } else { Vec::new() };
    if !keys.contains(&combo) {
        keys.push(combo);
    }
    if steal {
        for (k, other) in keymap.bindings() {
            if *k == combo && other != action {
                let left: Vec<KeyCombo> = keymap.keys(other).into_iter().filter(|o| *o != combo).collect();
                keymap::set_keys(&mut ws.settings.commands, other, Some(&left));
            }
        }
    }
    keymap::set_keys(&mut ws.settings.commands, action, Some(&keys));
}

fn pin_button(ui: &mut Ui, ws: &mut Workspace, id: &str, pal: &Palette) {
    let pinned = ws.settings.commands.pinned.iter().any(|p| p == id);
    let label = RichText::new("📌").size(12.0).color(if pinned { pal.accent } else { pal.faint });
    let hint = if pinned { "Pinned to the top of the command palette: click to unpin" } else { "Pin to the top of the command palette" };
    if ui.add(egui::Button::new(label).frame_when_inactive(false)).on_hover_text(hint).clicked() {
        if pinned {
            ws.settings.commands.pinned.retain(|p| p != id);
        } else {
            ws.settings.commands.pinned.push(id.to_string());
        }
    }
}

fn commands_tab(ui: &mut Ui, ws: &mut Workspace, st: &mut CommandsUi, pal: &Palette) {
    let mac = cfg!(target_os = "macos");
    let keymap = Keymap::new(&ws.settings.commands);
    note(
        ui,
        pal,
        "Change any command's shortcut, pin commands to the top of the command palette (📌), and make macros. \
         Shortcuts are checked against other commands and the keys Windows, macOS and Linux keep. Ctrl means ⌘ on macOS.",
    );
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let w = ui.available_width().min(240.0);
        ui.add(egui::TextEdit::singleline(&mut st.query).hint_text("Search commands").desired_width(w));
        let customized = !ws.settings.commands.keys.is_empty();
        if ui.add_enabled(customized, egui::Button::new("Reset all shortcuts")).clicked() {
            ws.settings.commands.keys.clear();
            st.pending = None;
            st.recording = None;
        }
    });
    let dups = keymap.duplicates();
    for (k, actions) in &dups {
        let names: Vec<String> = actions.iter().map(Action::title).collect();
        ui.label(RichText::new(format!("{} runs only the first of: {}", k.label(mac), names.join(", "))).size(12.0).color(pal.warn));
    }
    // A shortcut that conflicts: assign anyway, or not.
    if let Some((action, combo, add, conflicts)) = st.pending.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new(format!("{} for “{}”", combo.label(mac), action.title().trim_end_matches('…'))).strong());
            for c in &conflicts {
                note(ui, pal, &c.describe());
            }
            ui.horizontal_wrapped(|ui| {
                let taken = conflicts.iter().any(|c| matches!(c, Conflict::Taken(_)));
                if taken && ui.button("Use it here and remove it there").clicked() {
                    assign(ws, &keymap, &action, combo, add, true);
                    st.pending = None;
                }
                if ui.button("Use it anyway").clicked() {
                    assign(ws, &keymap, &action, combo, add, false);
                    st.pending = None;
                }
                if ui.button("Cancel").clicked() {
                    st.pending = None;
                }
            });
        });
    }
    ui.add_space(6.0);
    macros(ui, ws, st, &keymap, pal);
    ui.add_space(8.0);
    ui.label(RichText::new("COMMANDS").size(11.0).color(pal.muted).strong());
    let q = st.query.trim().to_lowercase();
    let mut list: Vec<_> = COMMANDS
        .iter()
        .filter(|c| ws.command_available(c.command))
        .filter(|c| q.is_empty() || format!("{} {} {}", c.category, c.title, c.id).to_lowercase().contains(&q))
        .collect();
    list.sort_by_key(|c| (c.category, c.title));
    if list.is_empty() {
        ui.label(RichText::new("No matching commands").color(pal.muted));
    }
    for c in list {
        let action = Action::Command(c.command);
        ui.horizontal_wrapped(|ui| {
            pin_button(ui, ws, c.id, pal);
            ui.label(RichText::new(c.title.trim_end_matches('…')).color(pal.text));
            ui.label(RichText::new(c.category).size(11.5).color(pal.faint));
            shortcut_controls(ui, ws, st, &keymap, &action, pal);
        });
    }
}

/// The first matches of a search as "+ name" buttons: the id of the one
/// clicked. With no search, `hint` says how to find one.
fn adder(ui: &mut Ui, matching: &[(String, String)], empty: bool, hint: &str, pal: &Palette) -> Option<String> {
    if empty {
        note(ui, pal, hint);
        return None;
    }
    if matching.is_empty() {
        note(ui, pal, "No matching commands");
        return None;
    }
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for (id, label) in matching.iter().take(8) {
            if ui.small_button(format!("+ {label}")).clicked() {
                picked = Some(id.clone());
            }
        }
        if matching.len() > 8 {
            ui.label(RichText::new(format!("and {} more: type more of the name", matching.len() - 8)).size(12.0).color(pal.muted));
        }
    });
    picked
}

/// Rename every reference to a macro (keys, pins, toolbar buttons).
fn rename_macro(ws: &mut Workspace, old: &str, new: &str) {
    let (old_id, new_id) = (macro_id(old), macro_id(new));
    let s = &mut ws.settings;
    if let Some(k) = s.commands.keys.remove(&old_id) {
        s.commands.keys.insert(new_id.clone(), k);
    }
    for p in &mut s.commands.pinned {
        if *p == old_id {
            p.clone_from(&new_id);
        }
    }
    if let Some(items) = &mut s.toolbar.items {
        for it in items.iter_mut().filter(|i| i.command == old_id) {
            it.command.clone_from(&new_id);
        }
    }
}

/// Every command, for pickers: (id, "Category: Title").
fn command_choices(ws: &Workspace) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = COMMANDS
        .iter()
        .filter(|c| ws.command_available(c.command))
        .map(|c| (c.id.to_string(), format!("{}: {}", c.category, c.title.trim_end_matches('…'))))
        .collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

fn choice_label(choices: &[(String, String)], id: &str) -> String {
    choices.iter().find(|(i, _)| i == id).map_or_else(|| id.to_string(), |(_, l)| l.clone())
}

fn macros(ui: &mut Ui, ws: &mut Workspace, st: &mut CommandsUi, keymap: &Keymap, pal: &Palette) {
    ui.label(RichText::new("MACROS").size(11.0).color(pal.muted).strong());
    note(
        ui,
        pal,
        "A macro runs commands one after another, each its own undo step. It stops at a command that opens a dialog. \
         Type in the search box to narrow the list of commands to add.",
    );
    // The step picker lists what the search box matches (everything when it's empty).
    let q = st.query.trim().to_lowercase();
    let choices: Vec<(String, String)> = command_choices(ws)
        .into_iter()
        .filter(|(id, l)| q.is_empty() || l.to_lowercase().contains(&q) || id.to_lowercase().contains(&q))
        .collect();
    let all_choices = command_choices(ws);
    let n = ws.settings.commands.macros.len();
    let mut remove = None;
    for i in 0..n {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let old = ws.settings.commands.macros[i].name.clone();
            ui.horizontal_wrapped(|ui| {
                let mut name = old.clone();
                let w = ui.available_width().min(200.0);
                ui.add(egui::TextEdit::singleline(&mut name).hint_text("Macro name").desired_width(w));
                if name != old && !name.trim().is_empty() {
                    rename_macro(ws, &old, &name);
                    ws.settings.commands.macros[i].name = name;
                }
                if ui.small_button("Delete macro").clicked() {
                    remove = Some(i);
                }
            });
            let name = ws.settings.commands.macros[i].name.clone();
            let action = Action::Macro(name.clone());
            ui.horizontal_wrapped(|ui| {
                pin_button(ui, ws, &macro_id(&name), pal);
                let on_bar = ws.settings.toolbar.items().iter().any(|t| t.command == macro_id(&name));
                if !on_bar && ui.small_button("Add to toolbar").clicked() {
                    let mut items = ws.settings.toolbar.items();
                    items.push(ToolbarItem::new(&macro_id(&name)));
                    ws.settings.toolbar.items = Some(items);
                }
                shortcut_controls(ui, ws, st, keymap, &action, pal);
            });
            let steps = ws.settings.commands.macros[i].steps.clone();
            let mut op: Option<(usize, i32)> = None;
            for (j, step) in steps.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{}.", j + 1)).color(pal.faint));
                    ui.label(choice_label(&all_choices, step));
                    if j > 0 && ui.small_button("↑").clicked() {
                        op = Some((j, -1));
                    }
                    if j + 1 < steps.len() && ui.small_button("↓").clicked() {
                        op = Some((j, 1));
                    }
                    if ui.small_button("×").clicked() {
                        op = Some((j, 0));
                    }
                });
            }
            if let Some((j, d)) = op {
                let s = &mut ws.settings.commands.macros[i].steps;
                match d {
                    0 => {
                        s.remove(j);
                    }
                    -1 => s.swap(j, j - 1),
                    _ => s.swap(j, j + 1),
                }
            }
            if let Some(id) = adder(ui, &choices, q.is_empty(), "Type in the search box above to find a command to add.", pal) {
                ws.settings.commands.macros[i].steps.push(id);
            }
        });
    }
    if let Some(i) = remove {
        let name = ws.settings.commands.macros.remove(i).name;
        let id = macro_id(&name);
        ws.settings.commands.keys.remove(&id);
        ws.settings.commands.pinned.retain(|p| *p != id);
        if let Some(items) = &mut ws.settings.toolbar.items {
            items.retain(|t| t.command != id);
        }
    }
    if ui.button("New macro").clicked() {
        let mut n = ws.settings.commands.macros.len() + 1;
        let taken = |n: usize, ws: &Workspace| ws.settings.commands.macros.iter().any(|m| m.name == format!("Macro {n}"));
        while taken(n, ws) {
            n += 1;
        }
        ws.settings.commands.macros.push(Macro { name: format!("Macro {n}"), ..Macro::default() });
    }
}

// ------------------------------------------------------------ Settings → Toolbar

/// Settings → Toolbar.
pub(crate) fn toolbar_tab(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    toolbar_settings(ui, ws, &mut st.commands, pal);
    PanelOut::default()
}

fn toolbar_settings(ui: &mut Ui, ws: &mut Workspace, st: &mut CommandsUi, pal: &Palette) {
    let t = &mut ws.settings.toolbar;
    ui.checkbox(&mut t.show, RichText::new("Formatting toolbar above the editor").strong());
    note(ui, pal, "Also View: Formatting toolbar. F10 moves the keyboard to it (← → to move, Enter to press, Escape to go back).");
    ui.checkbox(&mut t.floating, RichText::new("Small toolbar next to selected text").strong());
    note(ui, pal, "Bold, italic, strikethrough, highlight, code, math and links, shown while text is selected.");
    ui.add_space(8.0);
    ui.label(RichText::new("BUTTONS").size(11.0).color(pal.muted).strong());
    note(ui, pal, "Any command or macro can be a button. The icon can be an emoji or a few letters; the label is the tooltip.");
    let mut items = ws.settings.toolbar.items();
    let before = items.clone();
    let mut op: Option<(usize, i32)> = None;
    let n = items.len();
    for (i, item) in items.iter_mut().enumerate() {
        ui.horizontal_wrapped(|ui| {
            if item.is_separator() {
                ui.label(RichText::new("— separator —").color(pal.faint));
            } else {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 22.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 4.0, pal.hover);
                let g = egui::WidgetText::from(crate::toolbar_ui::icon(item).size(13.0).color(pal.text)).into_galley(
                    ui,
                    Some(egui::TextWrapMode::Truncate),
                    34.0,
                    egui::TextStyle::Button,
                );
                ui.painter().galley(rect.center() - g.size() / 2.0, g, pal.text);
                ui.add(egui::Label::new(crate::toolbar_ui::title(item)).truncate());
            }
            if i > 0 && ui.small_button("↑").clicked() {
                op = Some((i, -1));
            }
            if i + 1 < n && ui.small_button("↓").clicked() {
                op = Some((i, 1));
            }
            if ui.small_button("×").on_hover_text("Remove").clicked() {
                op = Some((i, 0));
            }
        });
        if !item.is_separator() {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(38.0);
                ui.label(RichText::new("Icon").size(12.0).color(pal.muted));
                ui.add(egui::TextEdit::singleline(&mut item.icon).desired_width(48.0).hint_text("auto"));
                ui.label(RichText::new("Label").size(12.0).color(pal.muted));
                let w = ui.available_width().clamp(60.0, 180.0);
                ui.add(egui::TextEdit::singleline(&mut item.label).desired_width(w).hint_text("the command's name"));
            });
        }
    }
    if let Some((i, d)) = op {
        match d {
            0 => {
                items.remove(i);
            }
            -1 => items.swap(i, i - 1),
            _ => items.swap(i, i + 1),
        }
    }
    ui.add_space(6.0);
    let mut choices = vec![
        (SEPARATOR.to_string(), "Separator".to_string()),
        (crate::toolbar_ui::HEADING_MENU.to_string(), "Heading level menu".to_string()),
    ];
    choices.extend(ws.settings.commands.macros.iter().map(|m| (macro_id(&m.name), format!("Macro: {}", m.name))));
    choices.extend(command_choices(ws));
    let w = ui.available_width().min(240.0);
    ui.add(egui::TextEdit::singleline(&mut st.add_pick).hint_text("Find a command to add").desired_width(w));
    let q = st.add_pick.trim().to_lowercase();
    let matching: Vec<(String, String)> =
        choices.into_iter().filter(|(id, l)| !q.is_empty() && (l.to_lowercase().contains(&q) || id.to_lowercase().contains(&q))).collect();
    if let Some(id) = adder(ui, &matching, q.is_empty(), "Separators, the heading menu, macros and every command can be buttons.", pal) {
        items.push(ToolbarItem::new(&id));
    }
    if items != before {
        ws.settings.toolbar.items = Some(items);
    }
    ui.add_space(6.0);
    if ws.settings.toolbar.items.is_some() && ui.button("Reset to the default buttons").clicked() {
        ws.settings.toolbar.items = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_app::Command;

    #[test]
    fn assigning_a_taken_shortcut_can_take_it_away() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        let km = Keymap::new(&ws.settings.commands);
        let ctrl_i = KeyCombo::parse("Ctrl+I").unwrap();
        assign(&mut ws, &km, &Action::Command(Command::Bold), ctrl_i, false, true);
        let km = Keymap::new(&ws.settings.commands);
        assert_eq!(km.keys_of(Command::Bold), vec![ctrl_i]);
        assert!(km.keys_of(Command::Italic).is_empty(), "taken from Italic");
        assert!(km.duplicates().is_empty());
    }

    #[test]
    fn renaming_a_macro_keeps_its_keys_pins_and_button() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.settings.commands.macros.push(Macro { name: "A".into(), steps: vec!["Save".into()], ..Macro::default() });
        ws.settings.commands.keys.insert("macro:A".into(), vec!["F9".into()]);
        ws.settings.commands.pinned.push("macro:A".into());
        ws.settings.toolbar.items = Some(vec![ToolbarItem::new("macro:A")]);
        rename_macro(&mut ws, "A", "B");
        ws.settings.commands.macros[0].name = "B".into();
        assert_eq!(Keymap::new(&ws.settings.commands).keys(&Action::Macro("B".into())), vec![KeyCombo::parse("F9").unwrap()]);
        assert_eq!(ws.settings.commands.pinned, ["macro:B"]);
        assert_eq!(ws.settings.toolbar.items()[0].command, "macro:B");
    }
}
