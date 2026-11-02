// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Appearance: theme files, the accent colour and tag colours
//! (design §F.4).

use egui::{Color32, RichText, Ui};
use quire_app::Workspace;
use quire_app::themes::{self, ThemeInfo};
use quire_app::workspace::NoticeLevel;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::{self, BASE_DARK, BASE_LIGHT, Palette};

/// Accent colours offered with one click.
const ACCENTS: &[(&str, [u8; 3])] = &[
    ("Blue", [0x2A, 0x67, 0xE0]),
    ("Teal", [0x0E, 0x8A, 0x7E]),
    ("Green", [0x2E, 0x8B, 0x3E]),
    ("Amber", [0xC2, 0x7C, 0x0E]),
    ("Orange", [0xD9, 0x5A, 0x1F]),
    ("Red", [0xC8, 0x37, 0x3B]),
    ("Pink", [0xC2, 0x3A, 0x83]),
    ("Purple", [0x75, 0x47, 0xD1]),
];

/// Colours offered for tags.
pub(crate) const TAG_COLORS: &[[u8; 3]] = &[
    [0x4C, 0x8B, 0xF5],
    [0x16, 0xA0, 0x85],
    [0x3F, 0xA3, 0x4D],
    [0xD9, 0xA4, 0x0B],
    [0xE6, 0x7E, 0x22],
    [0xE0, 0x4F, 0x5F],
    [0xC2, 0x3A, 0x83],
    [0x8E, 0x5C, 0xD9],
    [0x7F, 0x8C, 0x8D],
];

/// A theme in the list, with what it looks like (for the accent in use).
struct Row {
    id: String,
    label: String,
    preview: Option<(Palette, Palette)>,
}

/// UI state of the Appearance tab.
#[derive(Default)]
pub(crate) struct ThemeUi {
    /// The themes, read once (and again after Reload), for this accent.
    rows: Option<(String, Vec<Row>)>,
    tag_input: String,
}

fn rows(accent: &str) -> Vec<Row> {
    let a = themes::parse_color(accent);
    let infos: Vec<ThemeInfo> = themes::list();
    std::iter::once((String::new(), "Quire (built in)".to_string()))
        .chain(infos.into_iter().map(|t| (t.id, t.label)))
        .map(|(id, label)| {
            let preview = themes::load(&id).ok().map(|t| {
                (theme::build(&BASE_LIGHT, t.as_ref().map(|t| &t.light), a), theme::build(&BASE_DARK, t.as_ref().map(|t| &t.dark), a))
            });
            Row { id, label, preview }
        })
        .collect()
}

pub(crate) const fn available(_ws: &Workspace) -> bool {
    true
}

/// Three dots of a palette: background, text and accent.
fn swatch(ui: &mut Ui, p: &Palette) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(46.0, 18.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 4.0, p.bg);
    ui.painter().rect_stroke(r, 4.0, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    for (i, c) in [p.text, p.accent, p.tag].into_iter().enumerate() {
        ui.painter().circle_filled(egui::pos2(r.left() + 10.0 + i as f32 * 13.0, r.center().y), 4.0, c);
    }
}

/// The theme's id is saved (by the Settings dialog); the workspace loads
/// it, and `theme::sync` applies it.
fn choose(ui: &Ui, ws: &mut Workspace, id: &str) {
    ws.settings.appearance.theme_file = id.to_string();
    themes::apply_settings(ws);
    theme::sync(ui.ctx(), ws);
}

/// How tall a settings tab may be: the Settings dialog has no scrolling of
/// its own, so a long tab scrolls inside this.
pub(crate) fn tab_height(ui: &Ui) -> f32 {
    (ui.ctx().content_rect().height() - 240.0).clamp(140.0, 560.0)
}

pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let max_h = tab_height(ui);
    egui::ScrollArea::vertical()
        .id_salt("quire-settings-appearance")
        .max_height(max_h)
        .auto_shrink([false, true])
        .show(ui, |ui| appearance(ui, ws, st, pal));
    PanelOut::default()
}

fn appearance(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) {
    let st = &mut st.themes;
    let accent = ws.settings.appearance.accent.clone();
    if st.rows.as_ref().is_none_or(|(a, _)| *a != accent) {
        st.rows = Some((accent.clone(), rows(&accent)));
    }
    ui.label(RichText::new("Theme").strong());
    ui.label(
        RichText::new(
            "Colours for light and dark mode (View: Toggle theme switches between them). Theme files are plain text you can edit.",
        )
        .size(12.0)
        .color(pal.muted),
    );
    ui.add_space(4.0);
    let current = ws.settings.appearance.theme_file.clone();
    let mut pick: Option<String> = None;
    for row in st.rows.as_ref().map(|r| r.1.as_slice()).unwrap_or_default() {
        ui.horizontal_wrapped(|ui| {
            let selected = current.eq_ignore_ascii_case(&row.id);
            match &row.preview {
                Some((l, d)) => {
                    swatch(ui, l);
                    swatch(ui, d);
                }
                None => {
                    ui.label(RichText::new("can't be read").size(12.0).color(pal.warn));
                }
            }
            if ui.radio(selected, row.label.as_str()).clicked() && !selected {
                pick = Some(row.id.clone());
            }
        });
    }
    if let Some(id) = pick {
        choose(ui, ws, &id);
    }
    if let Some(e) = &ws.features.themes.error {
        ui.label(RichText::new(format!("{e}. The built-in theme is in use.")).size(12.0).color(pal.warn));
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("New theme from these colours…").on_hover_text("Write a theme file with every colour, to edit").clicked() {
            match new_theme_file() {
                Ok(id) => {
                    st.rows = None;
                    choose(ui, ws, &id);
                    ws.notify(NoticeLevel::Info, format!("Wrote {id} in the themes folder; edit it and choose it again to see changes"));
                }
                Err(e) => ws.notify(NoticeLevel::Error, e),
            }
        }
        if let Some(dir) = themes::themes_dir()
            && ui.button("Open themes folder").clicked()
        {
            let _ = std::fs::create_dir_all(&dir);
            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("file://{}", dir.display())));
        }
        if ui.button("Reload").on_hover_text("Read the theme files again").clicked() {
            st.rows = None;
            themes::forget(ws);
            choose(ui, ws, &current);
        }
    });

    ui.add_space(12.0);
    ui.label(RichText::new("Accent colour").strong());
    ui.label(RichText::new("Links, the caret, selections and buttons.").size(12.0).color(pal.muted));
    ui.horizontal_wrapped(|ui| {
        let mut new_accent: Option<String> = ui.selectable_label(accent.is_empty(), "Theme's").clicked().then(String::new);
        for (name, rgb) in ACCENTS {
            let hex = themes::format_color([rgb[0], rgb[1], rgb[2], 255]);
            let chosen = accent.eq_ignore_ascii_case(&hex);
            let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
            ui.painter().circle_filled(r.center(), 9.0, Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
            if chosen {
                ui.painter().circle_stroke(r.center(), 11.0, egui::Stroke::new(2.0, pal.strong));
            }
            if resp.on_hover_text(*name).clicked() {
                new_accent = Some(hex);
            }
        }
        let mut c = themes::parse_color(&accent).map_or_else(|| [pal.accent.r(), pal.accent.g(), pal.accent.b()], |c| [c[0], c[1], c[2]]);
        if ui.color_edit_button_srgb(&mut c).on_hover_text("Any colour").changed() {
            new_accent = Some(themes::format_color([c[0], c[1], c[2], 255]));
        }
        if let Some(a) = new_accent {
            ws.settings.appearance.accent = a;
            themes::apply_settings(ws);
            theme::sync(ui.ctx(), ws);
        }
    });

    ui.add_space(12.0);
    tag_colors(ui, ws, &mut st.tag_input, pal);
}

/// Write the palettes in use as a new theme file; returns its id.
fn new_theme_file() -> Result<String, String> {
    let dir = themes::themes_dir().ok_or("There's no settings folder on this system")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let side = |p: &Palette| -> Vec<(&str, themes::Rgba)> {
        themes::TOKENS.iter().filter_map(|k| theme::token(p, k).map(|c| (*k, theme::rgba(c)))).collect()
    };
    let text = themes::template(&side(&theme::LIGHT), &side(&theme::DARK));
    let name =
        (1..100).map(|i| if i == 1 { "My theme.toml".to_string() } else { format!("My theme {i}.toml") }).find(|n| !dir.join(n).exists());
    let name = name.ok_or("Too many themes named “My theme”")?;
    quire_app::atomic::write(&dir.join(&name), text.as_bytes(), false).map_err(|e| e.to_string())?;
    Ok(name)
}

/// Colours for tags, kept in the vault so they travel with it.
fn tag_colors(ui: &mut Ui, ws: &mut Workspace, input: &mut String, pal: &Palette) {
    ui.label(RichText::new("Tag colours").strong());
    if ws.vault().is_none() {
        ui.label(RichText::new("Open a vault to colour its tags.").size(12.0).color(pal.muted));
        return;
    }
    ui.label(RichText::new("Shown in notes and the sidebar, on every device the vault syncs to.").size(12.0).color(pal.muted));
    let colored: Vec<(String, themes::Rgba)> = ws.features.themes.tag_colors.iter().map(|(k, v)| (k.clone(), *v)).collect();
    let mut change: Option<(String, Option<themes::Rgba>)> = None;
    for (tag, c) in &colored {
        ui.horizontal(|ui| {
            let mut rgb = [c[0], c[1], c[2]];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                change = Some((tag.clone(), Some([rgb[0], rgb[1], rgb[2], 255])));
            }
            ui.label(RichText::new(format!("#{tag}")).color(theme::color(*c)));
            if ui.small_button("Remove").clicked() {
                change = Some((tag.clone(), None));
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(egui::TextEdit::singleline(input).hint_text("tag").desired_width(110.0));
        let tag = input.trim().trim_start_matches('#').to_string();
        if ui.add_enabled(!tag.is_empty(), egui::Button::new("Add colour")).clicked() {
            let n = colored.len() % TAG_COLORS.len();
            let c = TAG_COLORS[n];
            change = Some((tag, Some([c[0], c[1], c[2], 255])));
            input.clear();
        }
    });
    if let Some((tag, c)) = change
        && let Err(e) = themes::set_tag_color(ws, &tag, c)
    {
        ws.notify(NoticeLevel::Error, format!("Couldn't save the tag colour: {e}"));
    }
}
