// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The spell checker the editor uses: loaded in the background, with the
//! personal word list in the config folder (`dictionary.txt`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quire_spell::{Speller, UserDictionary};

/// Words common in notes and in Quire itself that the bundled English word
/// list doesn't know. Ignored for the session, not added to your list.
const EVERYDAY: &[&str] = &[
    "ok",
    "okay",
    "todo",
    "todos",
    "backlink",
    "backlinks",
    "callout",
    "callouts",
    "markdown",
    "frontmatter",
    "wikilink",
    "wikilinks",
    "app",
    "apps",
    "email",
    "emails",
    "online",
    "offline",
    "website",
    "websites",
    "login",
    "logout",
    "username",
    "inbox",
    "plugin",
    "plugins",
    "checklist",
    "checklists",
    "kanban",
    "dropdown",
    "screenshot",
    "screenshots",
    "repo",
    "repos",
    "dev",
    "devs",
    "config",
    "async",
    "localhost",
    "url",
    "urls",
    "json",
    "yaml",
    "api",
    "apis",
    "ui",
    "ux",
    "vs",
    "etc",
    "eg",
    "ie",
    "blog",
    "blogs",
    "podcast",
    "podcasts",
    "wifi",
    "smartphone",
    "workflow",
    "workflows",
    "timestamp",
    "timestamps",
    "deadline",
    "standup",
    "standups",
    "retro",
    "roadmap",
    "roadmaps",
    "brainstorm",
    "brainstorming",
    "Quire",
    "Obsidian",
    "Logseq",
    "Notion",
    "Dropbox",
    "iCloud",
    "OneDrive",
    "Syncthing",
    "GitHub",
    "macOS",
    "iOS",
    "Linux",
];

enum Slot {
    Loading,
    Ready(Arc<Speller>),
    Failed,
}

fn registry() -> &'static Mutex<HashMap<String, Slot>> {
    static SLOTS: std::sync::OnceLock<Mutex<HashMap<String, Slot>>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn user_dictionary_path() -> Option<std::path::PathBuf> {
    quire_app::settings::config_dir().map(|d| d.join("dictionary.txt"))
}

/// The speller for `lang`, or `None` while it loads (the UI is woken when
/// it's ready) or if that language isn't available.
pub(crate) fn speller(lang: &str, ctx: &egui::Context) -> Option<Arc<Speller>> {
    let mut slots = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match slots.get(lang) {
        Some(Slot::Ready(s)) => return Some(Arc::clone(s)),
        Some(Slot::Loading | Slot::Failed) => return None,
        None => {}
    }
    slots.insert(lang.to_string(), Slot::Loading);
    let (lang, ctx) = (lang.to_string(), ctx.clone());
    std::thread::spawn(move || {
        // A bad dictionary file on the system mustn't leave spelling stuck "loading".
        let loaded = std::panic::catch_unwind(|| Speller::load(&lang))
            .unwrap_or_else(|_| Err(quire_spell::SpellError::UnknownLanguage(lang.clone())));
        let slot = match loaded {
            Ok(s) => {
                if let Some(path) = user_dictionary_path()
                    && path.parent().is_none_or(|d| std::fs::create_dir_all(d).is_ok())
                    && let Ok(dict) = UserDictionary::load(path)
                {
                    s.set_user_dictionary(dict);
                }
                for w in EVERYDAY {
                    s.ignore(w);
                }
                Slot::Ready(Arc::new(s))
            }
            Err(_) => Slot::Failed,
        };
        registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(lang, slot);
        ctx.request_repaint();
    });
    None
}

/// Languages offered in Settings: the bundled ones, then any installed on the system.
pub(crate) fn languages() -> Vec<String> {
    let mut v: Vec<String> = quire_spell::available_dictionaries().into_iter().map(|d| d.lang).collect();
    v.dedup();
    v
}

/// Add `word` to the personal list (saved), in every loaded language.
pub(crate) fn add_word(word: &str) {
    let slots = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for slot in slots.values() {
        if let Slot::Ready(s) = slot {
            let _ = s.add_user_word(word);
        }
    }
}

/// Stop flagging `word` until Quire restarts.
pub(crate) fn ignore_word(word: &str) {
    let slots = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for slot in slots.values() {
        if let Slot::Ready(s) = slot {
            s.ignore(word);
        }
    }
}
