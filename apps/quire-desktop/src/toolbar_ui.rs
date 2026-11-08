// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The formatting toolbar (0.11): a row of buttons above the editor and a
//! compact one next to the selection. Both are off by default (View:
//! Formatting toolbar, Settings → Toolbar). A button runs a command or a
//! macro; the app sends formatting to every cursor as one undo step.
//!
//! Keyboard: F10 (Move focus to the formatting toolbar) puts the focus on
//! the first button, even while the toolbar is off; ← → (and Tab) Home End
//! move, Enter or Space presses, Escape goes back to the note.

use egui::{Key, Modifiers, RichText, Ui};
use quire_app::Workspace;
use quire_app::keymap::{Action, Keymap};
use quire_app::settings::ToolbarItem;
use quire_app::settings::toolbar::FLOATING_ITEMS;

use crate::theme::Palette;

/// The heading menu: a toolbar-only button with H1–H6 and Paragraph.
pub(crate) const HEADING_MENU: &str = "menu:headings";

/// What the toolbar keeps between frames.
#[derive(Default)]
pub(crate) struct ToolbarUi {
    /// The button with keyboard focus (F10), while the toolbar has it.
    focus: Option<usize>,
    /// F10 moved focus here: show the bar even while it is off.
    pub keyboard: bool,
    /// Give the first button focus next frame.
    take_focus: bool,
    /// Where the floating toolbar was last drawn: (the selection's start, the bar).
    last_floating: Option<(egui::Rect, egui::Rect)>,
}

impl ToolbarUi {
    pub(crate) const fn focus_first(&mut self) {
        self.keyboard = true;
        self.take_focus = true;
    }

    /// The floating toolbar stays while a button on it is being pressed:
    /// the press takes the keyboard from the note, and the bar would go.
    pub(crate) fn floating_held(&self, ctx: &egui::Context) -> Option<egui::Rect> {
        let (sel, bar) = self.last_floating?;
        let pressing =
            ctx.input(|i| (i.pointer.any_down() || i.pointer.any_released()) && i.pointer.interact_pos().is_some_and(|p| bar.contains(p)));
        pressing.then_some(sel)
    }

    /// Whether the bar should be drawn this frame.
    pub(crate) const fn visible(&self, ws: &Workspace) -> bool {
        ws.settings.toolbar.show || self.keyboard
    }
}

/// What a click asked for.
pub(crate) enum Pressed {
    Run(Action),
    /// The keyboard left the toolbar (Escape): back to the note.
    Leave,
}

/// The default icon and how it's styled, for commands without one of their own.
/// Only glyphs the bundled fonts have (Inter and egui's emoji fonts).
fn default_icon(id: &str) -> Option<RichText> {
    let t = |s: &str| RichText::new(s.to_string());
    Some(match id {
        "Bold" => t("B").strong(),
        "Italic" => t("I").italics(),
        "Strikethrough" => t("S").strikethrough(),
        "Highlight" => t("H").underline(),
        "InlineCode" => t("</>").monospace(),
        "InsertMarkdownLink" => t("🔗"),
        "InsertLink" => t("[[ ]]"),
        "Heading1" => t("H1"),
        "Heading2" => t("H2"),
        "Heading3" => t("H3"),
        "Heading4" => t("H4"),
        "Heading5" => t("H5"),
        "Heading6" => t("H6"),
        "Paragraph" => t("¶"),
        "BulletList" => t("☰"),
        "NumberedList" => t("🔢"),
        "ToggleTask" => t("☑"),
        "Quote" => t("“"),
        "InsertCallout" => t("ℹ"),
        "InsertTable" => t("⊞"),
        "InlineMath" => t("∑"),
        "MathBlock" => t("$$"),
        "CodeBlock" => t("{ }"),
        "InsertFootnote" => t("x¹"),
        "InsertRule" => t("―"),
        "InsertAttachment" => t("📎"),
        "Undo" => t("↶"),
        "Redo" => t("↷"),
        HEADING_MENU => t("H⏷"),
        _ => return None,
    })
}

/// The icon a button shows: its own, the command's default, or the first
/// letters of its title.
pub(crate) fn icon(item: &ToolbarItem) -> RichText {
    if !item.icon.trim().is_empty() {
        return RichText::new(item.icon.trim());
    }
    if let Some(i) = default_icon(&item.command) {
        return i;
    }
    let title = title(item);
    let short: String = title.trim_start_matches("Macro: ").chars().take(10).collect();
    RichText::new(short.trim_end_matches('…').trim().to_string())
}

/// The name a button goes by: its label, or the command's title.
pub(crate) fn title(item: &ToolbarItem) -> String {
    if !item.label.trim().is_empty() {
        return item.label.trim().to_string();
    }
    if item.command == HEADING_MENU {
        return "Heading level".into();
    }
    Action::from_id(&item.command).map_or_else(|| format!("{} (not in this version)", item.command), |a| a.title())
}

/// The tooltip: what it does, and its shortcut.
fn tooltip(item: &ToolbarItem, keymap: &Keymap, mac: bool) -> String {
    let name = title(item);
    let keys = Action::from_id(&item.command).and_then(|a| keymap.keys(&a).first().map(|k| k.label(mac)));
    match keys {
        Some(k) => format!("{} ({k})", name.trim_end_matches('…')),
        None => name.trim_end_matches('…').to_string(),
    }
}

const HEADINGS: [(&str, &str); 7] = [
    ("Heading1", "Heading 1"),
    ("Heading2", "Heading 2"),
    ("Heading3", "Heading 3"),
    ("Heading4", "Heading 4"),
    ("Heading5", "Heading 5"),
    ("Heading6", "Heading 6"),
    ("Paragraph", "Paragraph"),
];

/// Draw one row of buttons. Returns what was pressed.
fn buttons(ui: &mut Ui, items: &[ToolbarItem], keymap: &Keymap, pal: &Palette, st: &mut ToolbarUi, place: &str) -> Option<Pressed> {
    let mac = cfg!(target_os = "macos");
    let mut pressed = None;
    let ids: Vec<egui::Id> = (0..items.len()).map(|i| egui::Id::unique(("quire-toolbar", place, i))).collect();
    let focusable: Vec<usize> = items.iter().enumerate().filter(|(_, it)| !it.is_separator()).map(|(i, _)| i).collect();
    // Keyboard: which button has focus, and moving between them.
    let focused = ui.memory(egui::Memory::focused).and_then(|f| ids.iter().position(|i| *i == f));
    st.focus = focused;
    if std::mem::take(&mut st.take_focus)
        && let Some(&first) = focusable.first()
    {
        ui.memory_mut(|m| m.request_focus(ids[first]));
    }
    if let Some(at) = focused {
        // ← → and Tab move between buttons (egui moves focus by direction);
        // Home and End go to the first and last.
        let (home, end, escape) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Home),
                i.consume_key(Modifiers::NONE, Key::End),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        let next = if home {
            focusable.first()
        } else if end {
            focusable.last()
        } else {
            None
        };
        if let Some(&n) = next {
            ui.memory_mut(|m| m.request_focus(ids[n]));
        }
        if escape {
            ui.memory_mut(|m| m.surrender_focus(ids[at]));
            st.keyboard = false;
            pressed = Some(Pressed::Leave);
        }
    }
    ui.spacing_mut().item_spacing.x = 2.0;
    for (i, item) in items.iter().enumerate() {
        if item.is_separator() {
            ui.add_space(4.0);
            let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, 18.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 0.0, pal.border);
            ui.add_space(4.0);
            continue;
        }
        let tip = tooltip(item, keymap, mac);
        if item.command == HEADING_MENU {
            let r = ui.push_id(ids[i], |ui| {
                ui.menu_button(icon(item).size(13.5).color(pal.text), |ui| {
                    for (id, name) in HEADINGS {
                        let keys = Action::from_id(id).and_then(|a| keymap.keys(&a).first().map(|k| k.label(mac))).unwrap_or_default();
                        if ui.add(egui::Button::new(name).shortcut_text(keys)).clicked()
                            && let Some(a) = Action::from_id(id)
                        {
                            pressed = Some(Pressed::Run(a));
                            ui.close();
                        }
                    }
                })
                .response
            });
            r.inner.on_hover_text(tip);
            continue;
        }
        let known = Action::from_id(&item.command);
        let galley = egui::WidgetText::from(icon(item).size(13.5).color(if known.is_some() { pal.text } else { pal.faint })).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Button,
        );
        let size = egui::vec2((galley.size().x + 12.0).max(26.0), 24.0);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let resp = ui.interact(rect, ids[i], if known.is_some() { egui::Sense::click() } else { egui::Sense::hover() });
        if ui.is_rect_visible(rect) {
            if resp.is_pointer_button_down_on() {
                ui.painter().rect_filled(rect, 4.0, pal.active_bg);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 4.0, pal.hover);
            }
            if resp.has_focus() {
                ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.5, pal.accent), egui::StrokeKind::Inside);
            }
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, pal.text);
        }
        // The keyboard's button shows its tooltip too.
        if resp.has_focus() {
            resp.show_tooltip_text(&tip);
        }
        let by_keyboard = resp.has_focus();
        let resp = resp.on_hover_text(tip);
        if resp.clicked()
            && let Some(a) = known
        {
            if by_keyboard {
                // Enter or Space pressed the button: the note doesn't get them too
                // (it takes the keyboard back in this frame).
                ui.input_mut(|i| {
                    i.events.retain(|e| !matches!(e, egui::Event::Text(_) | egui::Event::Key { key: Key::Enter | Key::Space, .. }));
                });
            }
            pressed = Some(Pressed::Run(a));
        }
    }
    pressed
}

/// The toolbar above the editor.
pub(crate) fn bar(ui: &mut Ui, ws: &Workspace, keymap: &Keymap, pal: &Palette, st: &mut ToolbarUi) -> Option<Pressed> {
    let items = ws.settings.toolbar.items();
    let pressed = ui.horizontal_wrapped(|ui| buttons(ui, &items, keymap, pal, st, "bar")).inner;
    if matches!(pressed, Some(Pressed::Run(_))) {
        st.keyboard = false;
    }
    pressed
}

/// The compact toolbar above the selection (`at` is the selection's start).
pub(crate) fn floating(ctx: &egui::Context, at: egui::Rect, keymap: &Keymap, pal: &Palette, st: &mut ToolbarUi) -> Option<Pressed> {
    let items: Vec<ToolbarItem> = FLOATING_ITEMS.iter().map(|c| ToolbarItem::new(c)).collect();
    let width = 32.0 * items.len() as f32;
    let screen = ctx.content_rect();
    let x = at.left().clamp(screen.left() + 4.0, (screen.right() - width - 8.0).max(screen.left() + 4.0));
    let above = at.top() - 40.0;
    let y = if above < screen.top() + 4.0 { at.bottom() + 6.0 } else { above };
    let shown = egui::Area::new(egui::Id::unique("quire-floating-toolbar"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(x, y))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::symmetric(4, 2))
                .show(ui, |ui| ui.horizontal(|ui| buttons(ui, &items, keymap, pal, st, "floating")).inner)
                .inner
        });
    st.last_floating = Some((at, shown.response.rect));
    shown.inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_button_has_an_icon_and_a_title() {
        for c in quire_app::settings::toolbar::DEFAULT_ITEMS.iter().chain(FLOATING_ITEMS) {
            let item = ToolbarItem::new(c);
            if item.is_separator() {
                continue;
            }
            assert!(default_icon(c).is_some(), "{c} has no icon");
            assert!(*c == HEADING_MENU || Action::from_id(c).is_some(), "{c} is a command");
            assert!(!title(&item).contains("not in this version"));
        }
        let custom = ToolbarItem { command: "Save".into(), icon: "💾".into(), label: "Save now".into(), ..Default::default() };
        assert_eq!(icon(&custom).text(), "💾");
        assert_eq!(title(&custom), "Save now");
        assert_eq!(icon(&ToolbarItem::new("Save")).text(), "Save");
        let km = Keymap::new(&quire_app::settings::CommandSettings::default());
        assert_eq!(tooltip(&ToolbarItem::new("Bold"), &km, false), "Bold (Ctrl+B)");
    }
}
