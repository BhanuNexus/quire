// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Colours, fonts and egui visuals.
//!
//! The palettes are built from the built-in colours, the chosen theme file
//! and the accent colour (`quire_app::themes`). [`LIGHT`] and [`DARK`] always
//! hold the palettes in use; [`set_palettes`] swaps them when the theme changes.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Color32, FontData, FontDefinitions, FontFamily, Stroke, Visuals};
use quire_app::Workspace;
use quire_app::themes::{Rgba, Theme};

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
    /// Links in notes.
    pub link: Color32,
    /// Quoted text.
    pub quote: Color32,
    /// Conflict markers.
    pub conflict: Color32,
    /// Headings, level 1 to 6.
    pub h: [Color32; 6],
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// The built-in light colours.
pub(crate) const BASE_LIGHT: Palette = Palette {
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
    link: hex(0x002A_67E0),
    quote: hex(0x0076_7B85),
    conflict: hex(0x00B0_6F12),
    h: [hex(0x0011_1318); 6],
};

/// The built-in dark colours.
pub(crate) const BASE_DARK: Palette = Palette {
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
    link: hex(0x0078_A8FA),
    quote: hex(0x0082_8994),
    conflict: hex(0x00E0_B046),
    h: [hex(0x00F3_F4F6); 6],
};

/// A palette that can change while the app runs. It reads like a
/// `&'static Palette` (`&LIGHT` coerces to one); [`set_palettes`] replaces it.
pub(crate) struct Live(Mutex<&'static Palette>);

impl Live {
    const fn new(p: &'static Palette) -> Self {
        Self(Mutex::new(p))
    }

    fn set(&self, p: Palette) {
        // Palettes are tiny and change only when you pick another theme.
        let leaked: &'static Palette = Box::leak(Box::new(p));
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = leaked;
    }
}

impl Deref for Live {
    type Target = Palette;

    fn deref(&self) -> &Palette {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The light palette in use.
pub(crate) static LIGHT: Live = Live::new(&BASE_LIGHT);
/// The dark palette in use.
pub(crate) static DARK: Live = Live::new(&BASE_DARK);
/// The theme generation (`ws.features.themes.generation`) last applied.
static APPLIED: AtomicU64 = AtomicU64::new(0);
/// The theme sets a font for note text (family [`TEXT`]).
static TEXT_FONT: AtomicBool = AtomicBool::new(false);
/// The family note text uses when the theme sets one.
pub(crate) const TEXT: &str = "quire-text";

pub(crate) fn color(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

pub(crate) fn rgba(c: Color32) -> Rgba {
    c.to_srgba_unmultiplied()
}

/// A token of the palette by its theme-file name.
pub(crate) fn token(p: &Palette, name: &str) -> Option<Color32> {
    Some(match name {
        "bg" => p.bg,
        "panel" => p.panel,
        "raised" => p.raised,
        "text" => p.text,
        "strong" => p.strong,
        "muted" => p.muted,
        "faint" => p.faint,
        "accent" => p.accent,
        "link" => p.link,
        "tag" => p.tag,
        "tag_bg" => p.tag_bg,
        "border" => p.border,
        "selection" => p.selection,
        "hover" => p.hover,
        "active_bg" => p.active_bg,
        "mark_bg" => p.mark_bg,
        "code_bg" => p.code_bg,
        "quote" => p.quote,
        "conflict" => p.conflict,
        "warn" => p.warn,
        "danger" => p.danger,
        h if h.len() == 2 && h.starts_with('h') => p.h[h[1..].parse::<usize>().ok().filter(|n| (1..=6).contains(n))? - 1],
        _ => return None,
    })
}

fn set_token(p: &mut Palette, name: &str, c: Color32) {
    let slot = match name {
        "bg" => &mut p.bg,
        "panel" => &mut p.panel,
        "raised" => &mut p.raised,
        "text" => &mut p.text,
        "strong" => &mut p.strong,
        "muted" => &mut p.muted,
        "faint" => &mut p.faint,
        "accent" => &mut p.accent,
        "link" => &mut p.link,
        "tag" => &mut p.tag,
        "tag_bg" => &mut p.tag_bg,
        "border" => &mut p.border,
        "selection" => &mut p.selection,
        "hover" => &mut p.hover,
        "active_bg" => &mut p.active_bg,
        "mark_bg" => &mut p.mark_bg,
        "code_bg" => &mut p.code_bg,
        "quote" => &mut p.quote,
        "conflict" => &mut p.conflict,
        "warn" => &mut p.warn,
        "danger" => &mut p.danger,
        h if h.len() == 2 && h.starts_with('h') => match h[1..].parse::<usize>() {
            Ok(n @ 1..=6) => &mut p.h[n - 1],
            _ => return,
        },
        _ => return,
    };
    *slot = c;
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// The palette for one side: the built-in colours, the theme's tokens on
/// top, then the accent colour (which also tints selections).
pub(crate) fn build(base: &Palette, theme: Option<&std::collections::BTreeMap<String, Rgba>>, accent: Option<Rgba>) -> Palette {
    let mut p = *base;
    if let Some(t) = theme {
        // A theme that sets the accent but not links: links follow the accent.
        if t.contains_key("accent") && !t.contains_key("link") {
            p.link = color(t["accent"]);
        }
        for (k, v) in t {
            set_token(&mut p, k, color(*v));
        }
    }
    if let Some(a) = accent {
        let a = color(a);
        if p.link == p.accent {
            p.link = a;
        }
        p.accent = a;
        p.active_bg = mix(p.bg, a, if p.dark { 0.24 } else { 0.13 });
        p.selection = mix(p.bg, a, if p.dark { 0.34 } else { 0.22 });
    }
    p
}

/// Use `theme` (or the built-in colours) and `accent` from now on.
pub(crate) fn set_palettes(theme: Option<&Theme>, accent: &str) {
    let accent = quire_app::themes::parse_color(accent);
    LIGHT.set(build(&BASE_LIGHT, theme.map(|t| &t.light), accent));
    DARK.set(build(&BASE_DARK, theme.map(|t| &t.dark), accent));
}

/// Make egui's widgets match the palettes in use.
pub(crate) fn refresh_visuals(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT));
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK));
    ctx.request_repaint();
}

/// Follow the theme the workspace loaded (`ws.features.themes`): rebuild
/// the palettes and fonts when it changed. Cheap to call every frame.
pub(crate) fn sync(ctx: &egui::Context, ws: &Workspace) {
    let st = &ws.features.themes;
    if APPLIED.swap(st.generation, Ordering::Relaxed) == st.generation {
        return;
    }
    set_palettes(st.current.as_deref(), &ws.settings.appearance.accent);
    let fonts = st.current.as_ref().map(|t| t.fonts.clone()).unwrap_or_default();
    install_fonts_with(ctx, &fonts);
    refresh_visuals(ctx);
}

/// Whether note text uses the theme's text font ([`TEXT`]).
pub(crate) fn text_font() -> bool {
    TEXT_FONT.load(Ordering::Relaxed)
}

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
    // Also egui's "strong text" colour (headings in the preview, bold labels).
    w.active.fg_stroke = Stroke::new(1.5, p.strong);
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
/// plus system fallbacks for scripts they don't cover. At start-up this also
/// puts the chosen theme's colours (and fonts) in place.
pub(crate) fn install_fonts(ctx: &egui::Context) -> FontSupport {
    let settings = quire_app::Settings::load();
    let theme = quire_app::themes::load(&settings.appearance.theme_file).ok().flatten();
    set_palettes(theme.as_ref(), &settings.appearance.accent);
    let fonts = theme.map(|t| t.fonts).unwrap_or_default();
    install_fonts_with(ctx, &fonts)
}

/// A font file from the fonts folder (`Inter.ttf`, or `Inter` with the
/// extension left out).
fn user_font(name: &str) -> Option<Vec<u8>> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) {
        return None;
    }
    let dir = quire_app::themes::fonts_dir()?;
    [name.to_string(), format!("{name}.ttf"), format!("{name}.otf"), format!("{name}-Regular.ttf"), format!("{name}-Regular.otf")]
        .iter()
        .find_map(|f| std::fs::read(dir.join(f)).ok())
        .filter(|b| b.len() < 32 * 1024 * 1024)
}

/// A newer monochrome emoji font put in the fonts folder
/// (`NotoEmoji-Regular.ttf`); egui's bundled one covers icons otherwise.
fn emoji_font() -> Option<Vec<u8>> {
    let dir = quire_app::themes::fonts_dir()?;
    std::fs::read(dir.join("NotoEmoji-Regular.ttf")).ok().filter(|b| b.len() < 24 * 1024 * 1024)
}

fn install_fonts_with(ctx: &egui::Context, theme_fonts: &quire_app::themes::ThemeFonts) -> FontSupport {
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
    let mut fallbacks = fallbacks;
    if let Some(e) = emoji_font() {
        fallbacks.push(e);
    }
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
    // The theme's fonts, when they can be read.
    if let Some(b) = user_font(&theme_fonts.ui) {
        defs.font_data.insert("theme-ui".into(), Arc::new(FontData::from_owned(b)));
        defs.families.entry(prop.clone()).or_default().insert(0, "theme-ui".into());
    }
    if let Some(b) = user_font(&theme_fonts.mono) {
        defs.font_data.insert("theme-mono".into(), Arc::new(FontData::from_owned(b)));
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "theme-mono".into());
    }
    let mut text_chain = defs.families.get(&prop).cloned().unwrap_or_default();
    let text = user_font(&theme_fonts.text);
    TEXT_FONT.store(text.is_some(), Ordering::Relaxed);
    if let Some(b) = text {
        defs.font_data.insert("theme-text".into(), Arc::new(FontData::from_owned(b)));
        text_chain.insert(0, "theme-text".into());
    }
    defs.families.insert(FontFamily::Name(TEXT.into()), text_chain);
    ctx.set_fonts(defs);
    FontSupport { bold: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_switches_every_token() {
        let (_, text) = quire_app::themes::BUNDLED.iter().find(|(n, _)| *n == "Nord").unwrap();
        let t = quire_app::themes::parse("Nord", text).unwrap();
        for (base, side) in [(&BASE_LIGHT, &t.light), (&BASE_DARK, &t.dark)] {
            let p = build(base, Some(side), None);
            for name in quire_app::themes::TOKENS {
                assert_eq!(token(&p, name), Some(color(side[*name])), "{name}");
            }
        }
        // Tokens a theme leaves out keep the built-in colour.
        let mut partial = std::collections::BTreeMap::new();
        partial.insert("bg".to_string(), [1, 2, 3, 255]);
        let p = build(&BASE_DARK, Some(&partial), None);
        assert_eq!((p.bg, p.text), (Color32::from_rgb(1, 2, 3), BASE_DARK.text));
        // Every theme-file token is a palette token, both ways.
        for name in quire_app::themes::TOKENS {
            assert!(token(&BASE_LIGHT, name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_accent_tints_links_and_selections() {
        let p = build(&BASE_LIGHT, None, Some([0xE0, 0x40, 0x10, 255]));
        assert_eq!((p.accent, p.link), (Color32::from_rgb(0xE0, 0x40, 0x10), Color32::from_rgb(0xE0, 0x40, 0x10)));
        assert_ne!(p.selection, BASE_LIGHT.selection);
        assert_eq!(build(&BASE_LIGHT, None, None), BASE_LIGHT, "nothing chosen: the built-in colours");
    }
}
