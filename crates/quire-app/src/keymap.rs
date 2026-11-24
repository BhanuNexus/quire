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

/// Defaults that differ on one platform, because the old key can't fire there
/// (the system keeps it) and the new one can. `old` is what every other
/// platform still has. Quire 0.14.
struct Moved {
    command: Command,
    platform: Platform,
    old: KeyCombo,
    new: KeyCombo,
}

const fn combo(cmd: bool, shift: bool, alt: bool, key: &'static str) -> KeyCombo {
    KeyCombo { cmd, shift, alt, key }
}

const MOVED: &[Moved] = &[
    // ⌘Tab and ⌘` switch apps and windows. ⌘⌥PageDown/PageUp: next and
    // previous, the system has no use for them, and ⌘⇧[ is taken by folding.
    Moved {
        command: Command::NextTab,
        platform: Platform::Mac,
        old: combo(true, false, false, "Tab"),
        new: combo(true, false, true, "PageDown"),
    },
    Moved {
        command: Command::PrevTab,
        platform: Platform::Mac,
        old: combo(true, true, false, "Tab"),
        new: combo(true, false, true, "PageUp"),
    },
    // ⇧⌘C: C for code (⌘⇧` also switches windows).
    Moved {
        command: Command::InlineCode,
        platform: Platform::Mac,
        old: combo(true, false, false, "Backtick"),
        new: combo(true, true, false, "C"),
    },
    // ⌘H hides the app. ⌘R pairs with ⌘⇧R, replace in all notes, as ⌘F does with ⌘⇧F.
    Moved {
        command: Command::FindReplace,
        platform: Platform::Mac,
        old: combo(true, false, false, "H"),
        new: combo(true, false, false, "R"),
    },
    // Ctrl+Alt+arrows switch workspaces on GNOME, KDE and others.
    Moved {
        command: Command::AddCursorAbove,
        platform: Platform::Linux,
        old: combo(true, false, true, "ArrowUp"),
        new: combo(false, true, true, "ArrowUp"),
    },
    Moved {
        command: Command::AddCursorBelow,
        platform: Platform::Linux,
        old: combo(true, false, true, "ArrowDown"),
        new: combo(false, true, true, "ArrowDown"),
    },
];

/// `k` as `command` has it on `platform`: the platform's own default where
/// `k` is the old one that can't fire there.
fn on_platform(command: Command, k: KeyCombo, platform: Platform) -> KeyCombo {
    MOVED.iter().find(|m| m.command == command && m.platform == platform && same_keys(&m.old, &k)).map_or(k, |m| m.new)
}

/// Every shortcut in effect.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<(KeyCombo, Action)>,
}

impl Keymap {
    pub fn new(settings: &CommandSettings) -> Self {
        Self::for_platform(settings, Platform::current())
    }

    /// The shortcuts in effect on `platform`. A stored list (settings.toml
    /// keeps only what a person changed, but Settings also writes a command's
    /// own defaults into it when they add a key beside them or another
    /// command takes one) that has the old default of a `MOVED` row gets the
    /// platform's default in its place: that key never fired there. Keys a
    /// person chose themselves are used as they are.
    pub fn for_platform(settings: &CommandSettings, platform: Platform) -> Self {
        let mut bindings = Vec::new();
        for info in COMMANDS {
            let keys: Vec<KeyCombo> = match settings.keys.get(info.id) {
                Some(specs) => specs.iter().filter_map(|s| KeyCombo::parse(s)).collect(),
                None => info.keys.to_vec(),
            };
            let mut mine: Vec<KeyCombo> = Vec::new();
            for k in keys {
                let k = on_platform(info.command, k, platform);
                if !mine.contains(&k) {
                    mine.push(k);
                }
            }
            bindings.extend(mine.into_iter().map(|k| (k, Action::Command(info.command))));
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

    /// Every built-in default shortcut, checked by the same checker Settings
    /// uses for a person's own keys, for all three platforms at once.
    ///
    /// No default may be blocked on the platform that has it (taken by another
    /// command or kept by the system): where a key can't fire on one platform,
    /// that platform gets its own default (`MOVED`). The warnings below were
    /// already flagged when this test was written (0.14): each is listed with
    /// what the checker says. Any other default must have no conflict at all,
    /// so a new command's default key fails here unless it works everywhere.
    /// A listed default that no longer has that conflict fails too: take its
    /// row out.
    #[test]
    fn every_default_shortcut_passes_the_conflict_checker() {
        use Command as C;
        // (command, keys, what the checker says).
        // Warnings: the shortcut works, but where AltGr (Windows) or Option
        // (macOS) with the key types a character, the app lets typing win
        // (`altgr_typed`) or drops the character.
        let warned: &[(Command, &str, &str)] = &[
            (C::DailyNote, "Alt+D", "Option"),
            (C::Scratchpad, "Alt+S", "Option"),
            (C::ShowTasks, "Alt+T", "Option"),
            (C::OpenInbox, "Alt+I", "Option"),
            (C::NextHunk, "Alt+CloseBracket", "Option"),
            (C::PrevHunk, "Alt+OpenBracket", "Option"),
            (C::ShowAgenda, "Alt+A", "Option"),
            (C::ExportPdf, "Ctrl+Alt+E", "AltGr"),
            (C::ShowHistory, "Ctrl+Alt+H", "AltGr"),
            (C::NewProject, "Ctrl+Alt+N", "AltGr"),
            (C::CopyParagraphLink, "Ctrl+Alt+L", "AltGr"),
            (C::FoldAll, "Ctrl+Alt+OpenBracket", "AltGr"),
            (C::UnfoldAll, "Ctrl+Alt+CloseBracket", "AltGr"),
            (C::SplitEditor, "Ctrl+Alt+Backslash", "AltGr"),
            (C::AcceptHunk, "Ctrl+Alt+Y", "AltGr"),
            (C::AcceptAllInFile, "Ctrl+Alt+Shift+Y", "AltGr"),
            (C::StartReview, "Ctrl+Alt+R", "AltGr"),
            (C::NewCanvas, "Ctrl+Alt+C", "AltGr"),
        ];
        let said = |c: &Conflict| match c {
            Conflict::Taken(a) => format!("taken by {}", a.id()),
            Conflict::Reserved { platform, why } => format!("{}: {why}", platform.name()),
            Conflict::AltGr => "AltGr".into(),
            Conflict::MacOption => "Option".into(),
        };
        let mut wrong = Vec::new();
        let mut seen = Vec::new();
        for platform in Platform::ALL {
            let km = Keymap::for_platform(&CommandSettings::default(), platform);
            let at = platform.name();
            assert!(km.bindings().len() >= 80, "every default of COMMANDS is bound on {at}: {}", km.bindings().len());
            for (combo, action) in km.bindings() {
                let Action::Command(cmd) = action else { panic!("the defaults bind no macro") };
                // The checker also reports the keys other platforms keep, for
                // settings copied between computers; a default only has to work
                // on the platform that has it.
                let mut conflicts = km.conflicts(combo, action);
                conflicts.retain(|c| !matches!(c, Conflict::Reserved { platform: p, .. } if *p != platform));
                let got: Vec<String> = conflicts.iter().map(said).collect();
                let listed = warned.iter().find(|(c, k, _)| c == cmd && *k == combo.spec());
                let want: Vec<String> = listed.map(|(_, _, w)| vec![(*w).to_string()]).unwrap_or_default();
                if got != want {
                    wrong.push(format!("{at}: {cmd:?} {}: the checker says {got:?}, the table {want:?}", combo.spec()));
                }
                if conflicts.iter().any(Conflict::blocks) {
                    wrong.push(format!("{at}: {cmd:?} {}: blocked, so it can't fire", combo.spec()));
                }
                seen.push((*cmd, combo.spec()));
            }
        }
        for (c, k, _) in warned {
            if !seen.iter().any(|(sc, sk)| sc == c && sk == k) {
                wrong.push(format!("{c:?} {k}: no longer a default; take its row out"));
            }
        }
        assert!(wrong.is_empty(), "a default shortcut must pass `Keymap::conflicts` on every platform:\n{}", wrong.join("\n"));
    }

    fn on(settings: &CommandSettings, p: Platform, cmd: Command) -> Vec<String> {
        Keymap::for_platform(settings, p).keys_of(cmd).iter().map(KeyCombo::spec).collect()
    }

    #[test]
    fn six_defaults_differ_on_the_platform_where_the_old_key_cannot_fire() {
        let none = CommandSettings::default();
        for (cmd, p, old, new) in [
            (Command::NextTab, Platform::Mac, "Ctrl+Tab", "Ctrl+Alt+PageDown"),
            (Command::PrevTab, Platform::Mac, "Ctrl+Shift+Tab", "Ctrl+Alt+PageUp"),
            (Command::InlineCode, Platform::Mac, "Ctrl+Backtick", "Ctrl+Shift+C"),
            (Command::FindReplace, Platform::Mac, "Ctrl+H", "Ctrl+R"),
            (Command::AddCursorAbove, Platform::Linux, "Ctrl+Alt+ArrowUp", "Alt+Shift+ArrowUp"),
            (Command::AddCursorBelow, Platform::Linux, "Ctrl+Alt+ArrowDown", "Alt+Shift+ArrowDown"),
        ] {
            assert_eq!(on(&none, p, cmd), vec![new], "{cmd:?} on {}", p.name());
            for other in Platform::ALL.into_iter().filter(|o| *o != p) {
                assert_eq!(on(&none, other, cmd), vec![old], "{cmd:?} keeps its default on {}", other.name());
            }
        }
    }

    #[test]
    fn a_stored_old_default_moves_but_a_persons_own_keys_stay() {
        let mut s = CommandSettings::default();
        // Settings wrote the default beside a key the person added.
        s.keys.insert("FindReplace".into(), vec!["Ctrl+H".into(), "F6".into()]);
        assert_eq!(on(&s, Platform::Mac, Command::FindReplace), vec!["Ctrl+R", "F6"]);
        assert_eq!(on(&s, Platform::Windows, Command::FindReplace), vec!["Ctrl+H", "F6"], "other platforms are untouched");
        // Rebound, unbound, or bound to something else: as chosen.
        s.keys.insert("FindReplace".into(), vec!["Ctrl+Alt+F".into()]);
        assert_eq!(on(&s, Platform::Mac, Command::FindReplace), vec!["Ctrl+Alt+F"]);
        s.keys.insert("FindReplace".into(), vec![]);
        assert!(on(&s, Platform::Mac, Command::FindReplace).is_empty(), "unbound stays unbound");
        s.keys.insert("AddCursorAbove".into(), vec!["Ctrl+Alt+ArrowUp".into(), "Alt+Shift+ArrowUp".into()]);
        assert_eq!(on(&s, Platform::Linux, Command::AddCursorAbove), vec!["Alt+Shift+ArrowUp"], "no key twice");
        // A key the person gave another command is theirs.
        let mut t = CommandSettings::default();
        t.keys.insert("Bold".into(), vec!["Ctrl+H".into()]);
        assert_eq!(on(&t, Platform::Mac, Command::Bold), vec!["Ctrl+H"]);
    }

    #[test]
    fn actions_round_trip_through_ids() {
        for a in [Action::Command(Command::InsertTable), Action::Macro("Clean up".into())] {
            assert_eq!(Action::from_id(&a.id()), Some(a));
        }
        assert_eq!(Action::from_id("Nope"), None);
    }
}
