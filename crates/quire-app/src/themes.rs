// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Theme files, accent colour, icons and tag colours (design §F.4).
//!
//! A theme is data: a TOML file with colour tokens for light and dark and
//! optional fonts (docs/formats/themes.md). No expressions, no imports.
//! Unknown keys are ignored and a missing token keeps the built-in colour, so
//! a theme written for a newer Quire still works. A theme that can't be read
//! leaves the built-in theme in place and says why once.
//!
//! Folder icons and tag colours live in the vault (`.quire/shared/vault.toml`,
//! `[icons]` and `[tags.colors]`) so they travel with it; note icons come
//! from front matter (`icon: 📘`).
//!
//! Owned by WS-F; kept in `ws.features.themes`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::workspace::{NoticeLevel, Workspace};

/// An sRGB colour with alpha.
pub type Rgba = [u8; 4];

/// Every colour token a theme may set, in the order Settings lists them.
pub const TOKENS: &[&str] = &[
    "bg",
    "panel",
    "raised",
    "text",
    "strong",
    "muted",
    "faint",
    "accent",
    "link",
    "tag",
    "tag_bg",
    "border",
    "selection",
    "hover",
    "active_bg",
    "mark_bg",
    "code_bg",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "quote",
    "conflict",
    "warn",
    "danger",
];

/// The newest theme format this version reads fully.
pub const THEME_FORMAT: i64 = 1;

/// Themes that come with Quire: (name, file).
pub const BUNDLED: &[(&str, &str)] = &[
    ("Paper", include_str!("../themes/paper.toml")),
    ("Nord", include_str!("../themes/nord.toml")),
    ("Solarized", include_str!("../themes/solarized.toml")),
    ("High contrast", include_str!("../themes/high-contrast.toml")),
];

/// Fonts a theme asks for, by file name in `<config>/fonts/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeFonts {
    pub ui: String,
    pub text: String,
    pub mono: String,
}

/// A parsed theme: the tokens it sets for each side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub light: BTreeMap<String, Rgba>,
    pub dark: BTreeMap<String, Rgba>,
    pub fonts: ThemeFonts,
    /// What couldn't be used (a bad colour, a token on the wrong side).
    pub warnings: Vec<String>,
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA`.
pub fn parse_color(s: &str) -> Option<Rgba> {
    let h = s.trim().strip_prefix('#')?;
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        3 => {
            let n = |i: usize| u8::from_str_radix(&h[i..=i], 16).ok().map(|v| v * 17);
            Some([n(0)?, n(1)?, n(2)?, 255])
        }
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

/// `#RRGGBB` (or `#RRGGBBAA` when not opaque).
pub fn format_color(c: Rgba) -> String {
    if c[3] == 255 {
        format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", c[0], c[1], c[2], c[3])
    }
}

fn side(t: Option<&toml::Value>, which: &str, warnings: &mut Vec<String>) -> BTreeMap<String, Rgba> {
    let mut out = BTreeMap::new();
    let Some(t) = t else { return out };
    let Some(table) = t.as_table() else {
        warnings.push(format!("[{which}] isn't a table"));
        return out;
    };
    for (k, v) in table {
        if !TOKENS.contains(&k.as_str()) {
            continue; // a token from a newer Quire, or a typo: ignored
        }
        match v.as_str().and_then(parse_color) {
            Some(c) => {
                out.insert(k.clone(), c);
            }
            None => warnings.push(format!("{which}.{k} isn't a colour like \"#4C8BF5\"")),
        }
    }
    out
}

/// Read a theme file. Errors only when nothing usable is in it.
pub fn parse(name: &str, text: &str) -> Result<Theme, String> {
    let v: toml::Table = toml::from_str(text).map_err(|e| format!("{name} can't be read: {}", e.message()))?;
    let mut warnings = Vec::new();
    let format = v.get("format").and_then(toml::Value::as_integer).unwrap_or(1);
    if format > THEME_FORMAT {
        warnings.push(format!("{name} was made for a newer Quire; using the colours this version knows"));
    }
    let light = side(v.get("light"), "light", &mut warnings);
    let dark = side(v.get("dark"), "dark", &mut warnings);
    let f = v.get("fonts").and_then(toml::Value::as_table);
    let font = |k: &str| f.and_then(|t| t.get(k)).and_then(toml::Value::as_str).unwrap_or_default().trim().to_string();
    let fonts = ThemeFonts { ui: font("ui"), text: font("text"), mono: font("mono") };
    if light.is_empty() && dark.is_empty() && fonts == ThemeFonts::default() {
        return Err(match warnings.first() {
            Some(w) => w.clone(),
            None => format!("{name} has no [light] or [dark] colours"),
        });
    }
    Ok(Theme { name: name.to_string(), light, dark, fonts, warnings })
}

/// `<config>/themes/`.
pub fn themes_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join("themes"))
}

/// `<config>/fonts/`.
pub fn fonts_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join("fonts"))
}

/// A theme that can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    /// What `appearance.theme_file` holds: a bundled theme's name, or a file
    /// name in the themes folder (`my theme.toml`).
    pub id: String,
    pub label: String,
    pub bundled: bool,
}

/// Bundled themes, then the files in `dir`.
pub fn list_in(dir: Option<&Path>) -> Vec<ThemeInfo> {
    let mut out: Vec<ThemeInfo> =
        BUNDLED.iter().map(|(n, _)| ThemeInfo { id: (*n).to_string(), label: (*n).to_string(), bundled: true }).collect();
    let mut files: Vec<ThemeInfo> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let label = name.strip_suffix(".toml")?.to_string();
            Some(ThemeInfo { id: name, label, bundled: false })
        })
        .collect();
    files.sort_by_key(|t| t.label.to_lowercase());
    out.extend(files);
    out
}

pub fn list() -> Vec<ThemeInfo> {
    list_in(themes_dir().as_deref())
}

/// Load a theme by its id (see [`ThemeInfo::id`]); `""` is the built-in theme.
pub fn load_in(dir: Option<&Path>, id: &str) -> Result<Option<Theme>, String> {
    let id = id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    if let Some((name, text)) = BUNDLED.iter().find(|(n, _)| n.eq_ignore_ascii_case(id)) {
        return parse(name, text).map(Some);
    }
    if id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(format!("Theme “{id}” must be a file in the themes folder"));
    }
    let Some(dir) = dir else { return Err(format!("Theme “{id}” wasn't found")) };
    let has_ext = Path::new(id).extension().is_some_and(|e| e.eq_ignore_ascii_case("toml"));
    let file = if has_ext { id.to_string() } else { format!("{id}.toml") };
    let text = std::fs::read_to_string(dir.join(&file)).map_err(|e| format!("Theme “{id}” can't be opened: {e}"))?;
    let name = Path::new(&file).file_stem().map_or_else(|| file.clone(), |s| s.to_string_lossy().into_owned());
    parse(&name, &text).map(Some)
}

pub fn load(id: &str) -> Result<Option<Theme>, String> {
    load_in(themes_dir().as_deref(), id)
}

/// Write the built-in colours as a theme file to start from.
pub fn template(light: &[(&str, Rgba)], dark: &[(&str, Rgba)]) -> String {
    let mut s = String::from(
        "# A Quire theme. Colours are #RRGGBB (or #RRGGBBAA). Leave a token out to keep\n\
         # the built-in colour. Fonts are TTF or OTF files in the fonts folder.\n\
         format = 1\n\n[light]\n",
    );
    for (k, c) in light {
        let _ = writeln!(s, "{k} = \"{}\"", format_color(*c));
    }
    s.push_str("\n[dark]\n");
    for (k, c) in dark {
        let _ = writeln!(s, "{k} = \"{}\"", format_color(*c));
    }
    s.push_str("\n[fonts]\n# ui = \"Inter.ttf\"\n# text = \"Literata.ttf\"\n# mono = \"JetBrainsMono.ttf\"\n");
    s
}

// ------------------------------------------------------------ vault: icons and tag colours

/// `[tags]` in vault.toml (`[tags.colors] project = "#4C8BF5"`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TagsPrefs {
    pub colors: BTreeMap<String, String>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// `[icons]` in vault.toml: folder (`"Projects/"`) → emoji.
pub type IconPrefs = BTreeMap<String, String>;

/// State for this feature.
#[derive(Debug, Default)]
pub struct ThemesState {
    /// The theme in use (`None`: built-in).
    pub current: Option<Arc<Theme>>,
    /// Why the chosen theme isn't in use.
    pub error: Option<String>,
    /// The `appearance.theme_file` the above is for.
    loaded_for: Option<String>,
    /// Bumped when the theme or accent changes: the frontend rebuilds colours.
    pub generation: u64,
    accent_for: Option<String>,
    /// Tag → colour, from the vault.
    pub tag_colors: BTreeMap<String, Rgba>,
    /// Folder (`"Projects/"`) → icon, from the vault.
    pub folder_icons: IconPrefs,
}

impl ThemesState {
    /// The colour for a tag (`project/x` uses `project`'s if it has none).
    pub fn tag_color(&self, tag: &str) -> Option<Rgba> {
        let tag = tag.trim_start_matches('#').to_lowercase();
        let mut t = tag.as_str();
        loop {
            if let Some(c) = self.tag_colors.get(t) {
                return Some(*c);
            }
            t = t.rsplit_once('/')?.0;
        }
    }

    /// The icon for a folder (`"Projects/"` or `"Projects"`).
    pub fn folder_icon(&self, folder: &str) -> Option<&str> {
        let key = format!("{}/", folder.trim_end_matches('/'));
        self.folder_icons.get(&key).map(String::as_str).filter(|s| !s.is_empty())
    }
}

/// Load the chosen theme when the setting changes; say once if it can't be used.
pub(crate) fn tick(ws: &mut Workspace) -> bool {
    let want = ws.settings.appearance.theme_file.clone();
    let accent = ws.settings.appearance.accent.clone();
    let mut changed = false;
    if ws.features.themes.loaded_for.as_deref() != Some(want.as_str()) {
        let st = &mut ws.features.themes;
        st.loaded_for = Some(want.clone());
        st.generation += 1;
        changed = true;
        match load(&want) {
            Ok(t) => {
                let warn = t.as_ref().and_then(|t| t.warnings.first().cloned());
                st.current = t.map(Arc::new);
                st.error = None;
                if let Some(w) = warn {
                    ws.notify(NoticeLevel::Warn, format!("Theme: {w}"));
                }
            }
            Err(e) => {
                st.current = None;
                st.error = Some(e.clone());
                ws.notify(NoticeLevel::Warn, format!("{e}. Using the built-in theme."));
            }
        }
    }
    if ws.features.themes.accent_for.as_deref() != Some(accent.as_str()) {
        let st = &mut ws.features.themes;
        st.accent_for = Some(accent.clone());
        st.generation += 1;
        changed = true;
        if !accent.is_empty() && parse_color(&accent).is_none() {
            ws.notify(NoticeLevel::Warn, format!("Accent colour “{accent}” isn't like #4C8BF5; using the theme's"));
        }
    }
    changed
}

/// Load the theme and accent the settings name now (Settings calls this).
pub fn apply_settings(ws: &mut Workspace) -> bool {
    tick(ws)
}

/// Read the theme file again next time (after it was edited).
pub fn forget(ws: &mut Workspace) {
    ws.features.themes.loaded_for = None;
}

/// Read `[icons]` and `[tags.colors]` from the vault's preferences.
pub(crate) fn read_prefs(ws: &mut Workspace, prefs: &quire_vault::VaultPrefs) {
    let tags: TagsPrefs = prefs.section("tags");
    let st = &mut ws.features.themes;
    st.tag_colors = tags.colors.iter().filter_map(|(k, v)| Some((k.trim_start_matches('#').to_lowercase(), parse_color(v)?))).collect();
    st.folder_icons = prefs.section("icons");
}

/// Set (or with `None` clear) a tag's colour in vault.toml.
pub fn set_tag_color(ws: &mut Workspace, tag: &str, color: Option<Rgba>) -> Result<(), String> {
    let tag = tag.trim_start_matches('#').to_lowercase();
    crate::nav::edit_prefs(ws, |p| {
        let mut t: TagsPrefs = p.section("tags");
        match color {
            Some(c) => t.colors.insert(tag.clone(), format_color(c)),
            None => t.colors.remove(&tag),
        };
        p.set_section("tags", &t)
    })
}

/// Set (or with an empty icon clear) a folder's icon in vault.toml.
pub fn set_folder_icon(ws: &mut Workspace, folder: &str, icon: &str) -> Result<(), String> {
    let key = format!("{}/", folder.trim_end_matches('/'));
    let icon = icon.trim().to_string();
    crate::nav::edit_prefs(ws, |p| {
        let mut m: IconPrefs = p.section("icons");
        if icon.is_empty() {
            m.remove(&key);
        } else {
            m.insert(key.clone(), icon.clone());
        }
        p.set_section("icons", &m)
    })
}

/// A note's icon: `icon:` in its front matter.
pub fn note_icon(ws: &Workspace, path: &quire_vault::NotePath) -> Option<String> {
    let ix = ws.index().read();
    let rec = ix.get(path)?;
    let v = rec.properties.iter().find(|(k, _)| k.eq_ignore_ascii_case("icon"))?.1;
    match v {
        quire_markdown::Value::Str(s) if !s.trim().is_empty() && s.chars().count() <= 8 => Some(s.trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_and_print() {
        assert_eq!(parse_color("#4C8BF5"), Some([0x4C, 0x8B, 0xF5, 255]));
        assert_eq!(parse_color("#fff"), Some([255, 255, 255, 255]));
        assert_eq!(parse_color(" #00000080 "), Some([0, 0, 0, 0x80]));
        for bad in ["4C8BF5", "#4C8BF", "#ggg", "", "#12345"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        assert_eq!(format_color([0x4C, 0x8B, 0xF5, 255]), "#4C8BF5");
        assert_eq!(format_color([0, 0, 0, 0x80]), "#00000080");
    }

    #[test]
    fn themes_are_data_and_forgiving() {
        let t = parse("t", "format = 1\n[light]\nbg = \"#ffffff\"\nsparkle = \"#000\"\n[dark]\ntext = \"blue\"\naccent = \"#123\"\n[fonts]\nmono = \"Iosevka.ttf\"\n").unwrap();
        assert_eq!(t.light.get("bg"), Some(&[255, 255, 255, 255]));
        assert!(!t.light.contains_key("sparkle"), "unknown tokens are ignored");
        assert_eq!(t.dark.len(), 1);
        assert_eq!(t.warnings, vec!["dark.text isn't a colour like \"#4C8BF5\"".to_string()]);
        assert_eq!(t.fonts.mono, "Iosevka.ttf");
        let newer = parse("n", "format = 7\n[light]\nbg = \"#fff\"\n").unwrap();
        assert!(newer.warnings[0].contains("newer Quire"));
        assert!(parse("x", "[light\nbg").is_err(), "broken TOML");
        assert!(parse("x", "format = 1\n").is_err(), "nothing to use");
        assert!(parse("x", "[light]\nbg = 3\n").is_err());
    }

    #[test]
    fn bundled_themes_set_every_token_on_both_sides() {
        for (name, text) in BUNDLED {
            let t = parse(name, text).unwrap();
            assert!(t.warnings.is_empty(), "{name}: {:?}", t.warnings);
            for k in TOKENS {
                assert!(t.light.contains_key(*k) && t.dark.contains_key(*k), "{name} lacks {k}");
            }
        }
    }

    #[test]
    fn user_themes_load_from_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Mine.toml"), "[dark]\nbg = \"#000000\"\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a theme").unwrap();
        let all = list_in(Some(dir.path()));
        assert_eq!(all.len(), BUNDLED.len() + 1);
        assert_eq!(all.last().map(|t| (t.id.as_str(), t.label.as_str(), t.bundled)), Some(("Mine.toml", "Mine", false)));
        assert_eq!(load_in(Some(dir.path()), "Mine.toml").unwrap().unwrap().dark.len(), 1);
        assert_eq!(load_in(Some(dir.path()), "nord").unwrap().unwrap().name, "Nord");
        assert!(load_in(Some(dir.path()), "").unwrap().is_none());
        assert!(load_in(Some(dir.path()), "Gone.toml").is_err());
        assert!(load_in(Some(dir.path()), "../settings.toml").is_err(), "only files in the themes folder");
    }

    #[test]
    fn tag_colours_cover_nested_tags() {
        let mut st = ThemesState::default();
        st.tag_colors.insert("project".into(), [1, 2, 3, 255]);
        assert_eq!(st.tag_color("#Project/alpha"), Some([1, 2, 3, 255]));
        assert_eq!(st.tag_color("other"), None);
        st.folder_icons.insert("Projects/".into(), "🗂".into());
        assert_eq!(st.folder_icon("Projects"), Some("🗂"));
    }

    #[test]
    fn a_broken_theme_falls_back_with_a_notice() {
        let mut ws = Workspace::new(crate::Settings::default());
        ws.settings.appearance.theme_file = "Nord".into();
        assert!(tick(&mut ws));
        assert_eq!(ws.features.themes.current.as_ref().map(|t| t.name.as_str()), Some("Nord"));
        ws.settings.appearance.theme_file = "Missing.toml".into();
        let g = ws.features.themes.generation;
        assert!(tick(&mut ws));
        assert!(ws.features.themes.current.is_none() && ws.features.themes.error.is_some());
        assert!(ws.features.themes.generation > g);
        assert!(ws.notices.last().is_some_and(|n| n.text.contains("built-in theme")));
        assert!(!tick(&mut ws), "said once");
    }
}
