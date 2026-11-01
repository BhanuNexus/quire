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
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub(crate) const LIGHT: Palette = Palette {
    dark: false,
    bg: hex(0x00FB_FAF7),
    panel: hex(0x00F3_F1EC),
    text: hex(0x002A_2D33),
    strong: hex(0x0011_1318),
    muted: hex(0x0080_858E),
    faint: hex(0x00B9_BDC4),
    accent: hex(0x002F_6FEB),
    tag: hex(0x007C_4DDB),
    tag_bg: hex(0x00EF_E8FC),
    code_bg: hex(0x00ED_EAE3),
    mark_bg: hex(0x00FF_F0A0),
    border: hex(0x00E3_DFD7),
    selection: hex(0x00CF_E0FF),
    warn: hex(0x00B7_791F),
    danger: hex(0x00C9_3C37),
};

pub(crate) const DARK: Palette = Palette {
    dark: true,
    bg: hex(0x0016_181D),
    panel: hex(0x001C_1F25),
    text: hex(0x00DD_E0E5),
    strong: hex(0x00F4_F5F7),
    muted: hex(0x007F_8793),
    faint: hex(0x004A_505B),
    accent: hex(0x0074_A9FF),
    tag: hex(0x00C2_9BFF),
    tag_bg: hex(0x002C_2540),
    code_bg: hex(0x0023_2730),
    mark_bg: hex(0x005C_4B10),
    border: hex(0x002A_2E36),
    selection: hex(0x0026_4477),
    warn: hex(0x00E3_B341),
    danger: hex(0x00F0_716B),
};

pub(crate) fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.panel;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.code_bg;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v
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

/// Use a nicer UI/editor font when the system has one, add a bold face for
/// headings and strong text, and fallbacks for scripts egui's bundled fonts lack.
pub(crate) fn install_fonts(ctx: &egui::Context) -> FontSupport {
    let mut defs = FontDefinitions::default();
    let mut support = FontSupport::default();

    let regular = first_existing(&[
        "/usr/share/fonts/opentype/inter/Inter-Regular.otf",
        "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
        "/usr/share/fonts/inter/Inter-Regular.otf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "/Library/Fonts/Inter-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ]);
    let bold = first_existing(&[
        "/usr/share/fonts/opentype/inter/Inter-SemiBold.otf",
        "/usr/share/fonts/truetype/inter/Inter-SemiBold.ttf",
        "/usr/share/fonts/inter/Inter-SemiBold.otf",
        "C:\\Windows\\Fonts\\seguisb.ttf",
        "/Library/Fonts/Inter-SemiBold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]);
    let mono = first_existing(&[
        "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "C:\\Windows\\Fonts\\consola.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ]);
    // Coverage fallbacks (kept small to protect start-up time).
    let fallbacks: Vec<Vec<u8>> = [
        &["/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf", "C:\\Windows\\Fonts\\Nirmala.ttf"][..],
        &["/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", "C:\\Windows\\Fonts\\msyh.ttc", "/System/Library/Fonts/PingFang.ttc"][..],
    ]
    .iter()
    .filter_map(|c| first_existing(c))
    .filter(|b| b.len() < 24 * 1024 * 1024)
    .collect();

    let prop = FontFamily::Proportional;
    if let Some(r) = regular {
        defs.font_data.insert("quire-regular".into(), Arc::new(FontData::from_owned(r)));
        defs.families.entry(prop.clone()).or_default().insert(0, "quire-regular".into());
    }
    if let Some(m) = mono {
        defs.font_data.insert("quire-mono".into(), Arc::new(FontData::from_owned(m)));
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "quire-mono".into());
    }
    for (i, f) in fallbacks.into_iter().enumerate() {
        let name = format!("quire-fallback-{i}");
        defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(f)));
        defs.families.entry(prop.clone()).or_default().push(name.clone());
        defs.families.entry(FontFamily::Monospace).or_default().push(name);
    }
    if let Some(b) = bold {
        defs.font_data.insert(BOLD.into(), Arc::new(FontData::from_owned(b)));
        let mut chain = vec![BOLD.to_string()];
        chain.extend(defs.families.get(&prop).cloned().unwrap_or_default());
        defs.families.insert(FontFamily::Name(BOLD.into()), chain);
        support.bold = true;
    }
    ctx.set_fonts(defs);
    support
}
