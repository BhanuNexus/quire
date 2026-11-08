// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The shortcuts in effect (0.11): each command's defaults from
//! [`COMMANDS`], replaced where Settings → Commands says so, plus macros.
//!
//! Conflicts are checked for every platform, because settings are often
//! copied between computers: the same keys can be taken by another
//! command, kept by the system (⌘Q on macOS, Alt+F4 on Windows), or typed
//! as a character (Ctrl+Alt is `AltGr` on Windows, Option types characters
//! on macOS).

use crate::commands::{COMMANDS, Command, KeyCombo};
use crate::settings::commands::{CommandSettings, macro_id};

/// What a shortcut, a toolbar button or a pinned palette entry runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Command(Command),
    /// A macro, by name.
    Macro(String),
}

impl Action {
    /// From a settings id: `"Bold"` or `"macro:Tidy"`. `None` for ids this
    /// version doesn't know.
    pub fn from_id(id: &str) -> Option<Self> {
        match id.strip_prefix("macro:") {
            Some(name) => Some(Self::Macro(name.to_string())),
            None => Command::from_id(id).map(Self::Command),
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Command(c) => c.id().to_string(),
            Self::Macro(name) => macro_id(name),
        }
    }

    /// What people see: the command's title or "Macro: name".
    pub fn title(&self) -> String {
        match self {
            Self::Command(c) => c.info().title.to_string(),
            Self::Macro(name) => format!("Macro: {name}"),
        }
    }
}

/// The platforms whose shortcuts are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Mac,
    Linux,
}

impl Platform {
    pub const ALL: [Self; 3] = [Self::Windows, Self::Mac, Self::Linux];

    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Mac => "macOS",
            Self::Linux => "Linux",
        }
    }
}

/// Why a shortcut may not work as hoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Another command or macro has the same keys (on every platform).
    Taken(Action),
    /// The system or the text field keeps these keys on this platform.
    Reserved { platform: Platform, why: &'static str },
    /// Ctrl+Alt is `AltGr` on Windows: on keyboards where `AltGr` with this key
    /// types a character, typing wins.
    AltGr,
    /// Option with a letter, digit or sign types a character on macOS;
    /// the shortcut still runs, and the character is dropped.
    MacOption,
}

impl Conflict {
    /// One sentence for Settings.
    pub fn describe(&self) -> String {
        match self {
            Self::Taken(a) => format!("Also the shortcut of “{}”", a.title().trim_end_matches('…')),
            Self::Reserved { platform, why } => format!("On {}: {why}", platform.name()),
            Self::AltGr => "On Windows, Ctrl+Alt is AltGr: where AltGr with this key types a character (é, ł, @, €), typing wins".into(),
            Self::MacOption => "On macOS, Option with this key types a character; Quire runs the shortcut and drops the character".into(),
        }
    }

    /// Whether the shortcut can't work at all (as opposed to a warning).
    pub const fn blocks(&self) -> bool {
        matches!(self, Self::Taken(_) | Self::Reserved { .. })
    }
}

/// Keys the system or every text field keeps, per platform.
fn reserved(k: &KeyCombo, p: Platform) -> Option<&'static str> {
    let plain_cmd = k.cmd && !k.alt && !k.shift;
    match (p, k.key) {
        (_, "C" | "V" | "X") if plain_cmd => Some("copy, paste and cut"),
        (_, "Tab") if !k.cmd && k.alt && !k.shift => Some("switching windows"),
        (Platform::Windows | Platform::Linux, "F4") if k.alt && !k.cmd => Some("closes the window"),
        (Platform::Windows, "Escape") if k.cmd && !k.alt => Some("the Start menu"),
        (Platform::Windows, "Delete") if k.cmd && k.alt => Some("the security screen"),
        (Platform::Linux, "T") if k.cmd && k.alt && !k.shift => Some("opens a terminal on many desktops"),
        (Platform::Linux, "Delete") if k.cmd && k.alt => Some("logs out on many desktops"),
        (Platform::Linux, "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown") if k.cmd && k.alt && !k.shift => {
            Some("switches workspaces on many desktops")
        }
        (Platform::Mac, "Q") if plain_cmd => Some("⌘Q quits"),
        (Platform::Mac, "H") if plain_cmd => Some("⌘H hides the app"),
        (Platform::Mac, "M") if plain_cmd => Some("⌘M minimizes the window"),
        (Platform::Mac, "Space") if k.cmd && !k.alt => Some("Spotlight and input sources"),
        (Platform::Mac, "Tab" | "Backtick") if k.cmd && !k.alt => Some("switching apps and windows"),
        (Platform::Mac, "Escape") if k.cmd && k.alt => Some("Force Quit"),
        (Platform::Mac, "3" | "4" | "5") if k.cmd && k.shift && !k.alt => Some("screenshots"),
        _ => None,
    }
}

/// Keys that type a character with Option (macOS) or `AltGr` (Windows).
fn types_with_alt(key: &str) -> bool {
    key.len() == 1
        || matches!(
            key,
            "Comma"
                | "Minus"
                | "Period"
                | "Plus"
                | "Equals"
                | "Semicolon"
                | "Colon"
                | "Backslash"
                | "Slash"
                | "OpenBracket"
                | "CloseBracket"
                | "Backtick"
                | "Quote"
        )
}

/// Every shortcut in effect.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<(KeyCombo, Action)>,
}

impl Keymap {
    pub fn new(settings: &CommandSettings) -> Self {
        let mut bindings = Vec::new();
        for info in COMMANDS {
            match settings.keys.get(info.id) {
                Some(specs) => bindings.extend(specs.iter().filter_map(|s| KeyCombo::parse(s)).map(|k| (k, Action::Command(info.command)))),
                None => bindings.extend(info.keys.iter().map(|k| (*k, Action::Command(info.command)))),
            }
        }
        for m in &settings.macros {
            if let Some(specs) = settings.keys.get(&macro_id(&m.name)) {
                bindings.extend(specs.iter().filter_map(|s| KeyCombo::parse(s)).map(|k| (k, Action::Macro(m.name.clone()))));
            }
        }
        Self { bindings }
    }

    pub fn bindings(&self) -> &[(KeyCombo, Action)] {
        &self.bindings
    }

    /// The keys of `action`, first is the one to show.
    pub fn keys(&self, action: &Action) -> Vec<KeyCombo> {
        self.bindings.iter().filter(|(_, a)| a == action).map(|(k, _)| *k).collect()
    }

    pub fn keys_of(&self, cmd: Command) -> Vec<KeyCombo> {
        self.keys(&Action::Command(cmd))
    }

    /// The first shortcut of `cmd` as the platform writes it, or "".
    pub fn label(&self, cmd: Command, mac: bool) -> String {
        self.keys_of(cmd).first().map(|k| k.label(mac)).unwrap_or_default()
    }

    /// What may stop `combo` from working for `action`.
    pub fn conflicts(&self, combo: &KeyCombo, action: &Action) -> Vec<Conflict> {
        let mut out: Vec<Conflict> =
            self.bindings.iter().filter(|(k, a)| a != action && same_keys(k, combo)).map(|(_, a)| Conflict::Taken(a.clone())).collect();
        out.dedup();
        for p in Platform::ALL {
            if let Some(why) = reserved(combo, p) {
                out.push(Conflict::Reserved { platform: p, why });
            }
        }
        if combo.cmd && combo.alt && types_with_alt(combo.key) {
            out.push(Conflict::AltGr);
        }
        if combo.alt && !combo.cmd && types_with_alt(combo.key) {
            out.push(Conflict::MacOption);
        }
        out
    }

    /// Every shortcut that is taken twice: (keys, the actions).
    pub fn duplicates(&self) -> Vec<(KeyCombo, Vec<Action>)> {
        let mut out: Vec<(KeyCombo, Vec<Action>)> = Vec::new();
        for (k, a) in &self.bindings {
            match out.iter_mut().find(|(o, _)| same_keys(o, k)) {
                Some((_, v)) if !v.contains(a) => v.push(a.clone()),
                Some(_) => {}
                None => out.push((*k, vec![a.clone()])),
            }
        }
        out.retain(|(_, v)| v.len() > 1);
        out
    }
}

/// Keys equal on every platform (Ctrl is ⌘ on macOS for both).
fn same_keys(a: &KeyCombo, b: &KeyCombo) -> bool {
    a.cmd == b.cmd && a.alt == b.alt && a.shift == b.shift && a.key == b.key
}

/// Set the shortcuts of `action` (replacing its defaults). `None` puts the
/// defaults back.
pub fn set_keys(settings: &mut CommandSettings, action: &Action, keys: Option<&[KeyCombo]>) {
    let id = action.id();
    match keys {
        Some(k) => {
            settings.keys.insert(id, k.iter().map(KeyCombo::spec).collect());
        }
        None => {
            settings.keys.remove(&id);
        }
    }
}

/// Whether `action`'s shortcuts differ from the defaults.
pub fn is_customized(settings: &CommandSettings, action: &Action) -> bool {
    settings.keys.contains_key(&action.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Macro;

    fn k(spec: &str) -> KeyCombo {
        KeyCombo::parse(spec).unwrap()
    }

    #[test]
    fn settings_replace_defaults_and_bind_macros() {
        let mut s = CommandSettings::default();
        let km = Keymap::new(&s);
        assert_eq!(km.label(Command::Bold, false), "Ctrl+B");
        set_keys(&mut s, &Action::Command(Command::Bold), Some(&[k("Ctrl+Shift+B")]));
        set_keys(&mut s, &Action::Command(Command::Italic), Some(&[]));
        s.macros.push(Macro { name: "Tidy".into(), steps: vec!["FormatTable".into(), "Save".into()], ..Default::default() });
        set_keys(&mut s, &Action::Macro("Tidy".into()), Some(&[k("Alt+F9")]));
        let km = Keymap::new(&s);
        assert_eq!(km.label(Command::Bold, false), "Ctrl+Shift+B");
        assert!(km.keys_of(Command::Italic).is_empty(), "unbound");
        assert_eq!(km.keys(&Action::Macro("Tidy".into())), vec![k("Alt+F9")]);
        set_keys(&mut s, &Action::Command(Command::Bold), None);
        assert_eq!(Keymap::new(&s).label(Command::Bold, true), "⌘B", "reset");
        assert!(!is_customized(&s, &Action::Command(Command::Bold)));
        // Unknown and unreadable entries are skipped, not fatal.
        s.keys.insert("FutureCommand".into(), vec!["F8".into()]);
        s.keys.insert("Save".into(), vec!["Ctrl+Nope".into(), "Ctrl+Alt+S".into()]);
        assert_eq!(Keymap::new(&s).keys_of(Command::Save), vec![k("Ctrl+Alt+S")]);
    }

    #[test]
    fn conflicts_on_every_platform() {
        let km = Keymap::new(&CommandSettings::default());
        let bold = Action::Command(Command::Bold);
        assert_eq!(km.conflicts(&k("Ctrl+I"), &bold), vec![Conflict::Taken(Action::Command(Command::Italic))]);
        assert!(km.conflicts(&k("Ctrl+B"), &bold).is_empty(), "its own keys");
        assert!(km.conflicts(&k("Ctrl+V"), &bold).iter().any(Conflict::blocks));
        let q = km.conflicts(&k("Cmd+Q"), &bold);
        assert_eq!(q, vec![Conflict::Reserved { platform: Platform::Mac, why: "⌘Q quits" }]);
        assert!(km.conflicts(&k("Ctrl+Alt+Q"), &bold).contains(&Conflict::AltGr), "AltGr+Q is @ on German keyboards");
        assert!(!km.conflicts(&k("Ctrl+Alt+F5"), &bold).contains(&Conflict::AltGr));
        assert!(km.conflicts(&k("Alt+E"), &bold).contains(&Conflict::MacOption));
        assert!(km.conflicts(&k("Alt+F4"), &bold).iter().any(|c| matches!(c, Conflict::Reserved { platform: Platform::Windows, .. })));
        assert!(km.duplicates().is_empty(), "the defaults have no duplicates");
    }

    /// 0.11 review L2: no default shortcut is one the checker says can't
    /// work on some platform, and the 0.11 ones have no conflict at all
    /// (Ctrl+Alt+T opens a terminal on Linux, Option+N types ñ on a Mac,
    /// AltGr+Shift+L types Ł in Poland).
    #[test]
    fn defaults_work_everywhere() {
        let km = Keymap::new(&CommandSettings::default());
        // Older defaults the checker flags on one platform, kept as they were.
        let known = [
            Command::AddCursorAbove,
            Command::AddCursorBelow,
            Command::NextTab,
            Command::PrevTab,
            Command::InlineCode,
            Command::FindReplace,
        ];
        let flagged: Vec<String> = km
            .bindings()
            .iter()
            .filter(|(combo, action)| km.conflicts(combo, action).iter().any(Conflict::blocks))
            .filter(|(_, action)| !matches!(action, Action::Command(c) if known.contains(c)))
            .map(|(combo, action)| format!("{} for {action:?}", combo.label(false)))
            .collect();
        assert!(flagged.is_empty(), "{flagged:?}");
        for cmd in [Command::NewTask, Command::StartStopTimer, Command::LockNow] {
            let keys = km.keys_of(cmd);
            assert!(!keys.is_empty(), "{cmd:?} has a key");
            for combo in keys {
                assert_eq!(km.conflicts(&combo, &Action::Command(cmd)), vec![], "{cmd:?}: {}", combo.label(false));
            }
        }
    }

    #[test]
    fn actions_round_trip_through_ids() {
        for a in [Action::Command(Command::InsertTable), Action::Macro("Clean up".into())] {
            assert_eq!(Action::from_id(&a.id()), Some(a));
        }
        assert_eq!(Action::from_id("Nope"), None);
    }
}
