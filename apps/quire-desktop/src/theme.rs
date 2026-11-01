// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Colours, fonts and egui visuals.

use std::sync::Arc;

use egui::{Color32, FontData, FontDefinitions, FontFamily, Stroke, Visuals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub text: Color32,
    pub strong: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub tag: Color32,
    pub tag_bg: Color32,
    pub code_bg: Color32,
    pub mark_bg: Color32,
    pub border: Color32,
    pub selection: Color32,
    pub warn: Color32,
    pub danger: Color32,
    /// Fill under a row or button the pointer is over.
    pub hover: Color32,
    /// Fill under the selected row (the open note, the chosen item).
    pub active_bg: Color32,
    /// Popups, menus and dialogs: one step above the panels.
    pub raised: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub(crate) const LIGHT: Palette = Palette {
    dark: false,
    bg: hex(0x00FD_FCFA),
    panel: hex(0x00F6_F5F2),
    text: hex(0x0026_292F),
    strong: hex(0x0011_1318),
    muted: hex(0x0076_7B85),
    faint: hex(0x00B4_B8BF),
    accent: hex(0x002A_67E0),
    tag: hex(0x0075_47D1),
    tag_bg: hex(0x00F0_EBFC),
    code_bg: hex(0x00F2_F0EC),
    mark_bg: hex(0x00FF_EFA3),
    border: hex(0x00E6_E3DD),
    selection: hex(0x00D3_E2FE),
    warn: hex(0x00B0_6F12),
    danger: hex(0x00C4_3A35),
    hover: hex(0x00EC_EAE6),
    active_bg: hex(0x00E2_EAFB),
    raised: hex(0x00FF_FFFF),
};

pub(crate) const DARK: Palette = Palette {
    dark: true,
    bg: hex(0x0015_171B),
    panel: hex(0x001A_1C21),
    text: hex(0x00D9_DCE1),
    strong: hex(0x00F3_F4F6),
    muted: hex(0x0082_8994),
    faint: hex(0x004B_515C),
    accent: hex(0x0078_A8FA),
    tag: hex(0x00C0_9EFA),
    tag_bg: hex(0x002B_2540),
    code_bg: hex(0x0021_242B),
    mark_bg: hex(0x0056_4712),
    border: hex(0x0029_2C33),
    selection: hex(0x0024_3E6B),
    warn: hex(0x00E0_B046),
    danger: hex(0x00EE_726C),
    hover: hex(0x0024_272E),
    active_bg: hex(0x0022_3150),
    raised: hex(0x001F_2228),
};

pub(crate) fn visuals(p: &Palette) -> Visuals {
    use egui::{CornerRadius, Shadow};
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(6);
    v.panel_fill = p.panel;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.code_bg;
    v.code_bg_color = p.code_bg;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.active_bg;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let shade = if p.dark { 110 } else { 28 };
    v.window_shadow = Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(shade) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(shade) };
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.indent_has_left_vline = false;
    v.striped = false;

    let w = &mut v.widgets;
    // Labels, separators, frames.
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    // Buttons and fields at rest: a soft fill, no outline.
    w.inactive.bg_fill = p.hover;
    w.inactive.weak_bg_fill = p.hover;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;
    let deeper = if p.dark { p.hover.gamma_multiply(1.35) } else { Color32::from_rgb(0xE2, 0xE0, 0xDB) };
    w.hovered.bg_fill = deeper;
    w.hovered.weak_bg_fill = deeper;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5, p.strong);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.active_bg;
    w.active.weak_bg_fill = p.active_bg;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.accent);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.bg_fill = deeper;
    w.open.weak_bg_fill = deeper;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, p.strong);
    w.open.corner_radius = r;
    v
}

/// Spacing and sizes shared by every screen.
pub(crate) fn style(s: &mut egui::Style) {
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.button_padding = egui::vec2(10.0, 4.0);
    sp.interact_size.y = 24.0;
    sp.menu_margin = egui::Margin::same(6);
    sp.window_margin = egui::Margin::same(18);
    sp.indent = 16.0;
    sp.icon_width = 15.0;
    sp.icon_width_inner = 9.0;
    sp.combo_height = 260.0;
    sp.text_edit_width = 260.0;
    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 8.0;
    scroll.floating_width = 3.0;
    scroll.floating_allocated_width = 0.0;
    scroll.dormant_handle_opacity = 0.0;
    scroll.active_handle_opacity = 0.55;
    scroll.interact_handle_opacity = 0.8;
    sp.scroll = scroll;
    s.interaction.selectable_labels = false;
}

/// A full-width list row (sidebar note, palette item): fills on hover, and
/// with the accent tint when `selected`. Returns the row's response.
pub(crate) fn row(
    ui: &mut egui::Ui,
    selected: bool,
    height: f32,
    p: &Palette,
    paint: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            p.active_bg
        } else if resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        if fill != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 6.0, fill);
        }
        paint(ui.painter(), rect);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// Which optional font families were found on this system.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FontSupport {
    pub bold: bool,
}

pub(crate) const BOLD: &str = "quire-bold";

fn first_existing(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

/// The bundled faces (SIL Open Font License; see `fonts/`), so every
/// platform gets the same type instead of whatever the system falls back to.
const INTER_REGULAR: &[u8] = include_bytes!("../fonts/Inter-Regular.ttf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../fonts/Inter-SemiBold.ttf");
const INTER_ITALIC: &[u8] = include_bytes!("../fonts/Inter-Italic.ttf");
const JETBRAINS_MONO: &[u8] = include_bytes!("../fonts/JetBrainsMonoNL-Regular.ttf");

pub(crate) const ITALIC: &str = "quire-italic";

/// Inter for text, `JetBrains Mono` for code, a semibold and a true italic,
/// plus system fallbacks for scripts they don't cover.
pub(crate) fn install_fonts(ctx: &egui::Context) -> FontSupport {
    let mut defs = FontDefinitions::default();
    let prop = FontFamily::Proportional;
    let add = |defs: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        defs.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut defs, "quire-regular", INTER_REGULAR);
    add(&mut defs, BOLD, INTER_SEMIBOLD);
    add(&mut defs, ITALIC, INTER_ITALIC);
    add(&mut defs, "quire-mono", JETBRAINS_MONO);
    defs.families.entry(prop.clone()).or_default().insert(0, "quire-regular".into());
    defs.families.entry(FontFamily::Monospace).or_default().insert(0, "quire-mono".into());

    // Coverage fallbacks (kept small to protect start-up time).
    let fallbacks: Vec<Vec<u8>> = [
        &["/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf", "C:\\Windows\\Fonts\\Nirmala.ttf"][..],
        &["/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", "C:\\Windows\\Fonts\\msyh.ttc", "/System/Library/Fonts/PingFang.ttc"][..],
    ]
    .iter()
    .filter_map(|c| first_existing(c))
    .filter(|b| b.len() < 24 * 1024 * 1024)
    .collect();
    for (i, f) in fallbacks.into_iter().enumerate() {
        let name = format!("quire-fallback-{i}");
        defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(f)));
        defs.families.entry(prop.clone()).or_default().push(name.clone());
        defs.families.entry(FontFamily::Monospace).or_default().push(name);
    }
    let rest = defs.families.get(&prop).cloned().unwrap_or_default();
    for face in [BOLD, ITALIC] {
        let mut chain = vec![face.to_string()];
        chain.extend(rest.iter().cloned());
        defs.families.insert(FontFamily::Name(face.into()), chain);
    }
    ctx.set_fonts(defs);
    FontSupport { bold: true }
}
