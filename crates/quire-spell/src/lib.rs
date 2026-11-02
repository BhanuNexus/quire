// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown-aware spell checking.
//!
//! - [`Speller`] wraps a Hunspell dictionary (via the pure-Rust `spellbook`
//!   crate) plus a personal [`UserDictionary`] and a per-session ignore list.
//! - English (US and GB) is bundled; other languages are picked up from
//!   Hunspell dictionaries installed on the system ([`system_dictionaries`]).
//! - [`Speller::misspellings`] scans one line of Markdown at a time, carrying
//!   block state (code fences, front matter, math) in a [`LineContext`], and
//!   returns byte ranges of misspelled words for the editor to underline.
//!
//! ```
//! use quire_spell::{LineContext, Speller};
//!
//! let speller = Speller::english();
//! let mut ctx = LineContext::new();
//! let line = "Some mispeled words, `cde` and [lnk text](https://example.org/pathh).";
//! let bad: Vec<&str> = speller.misspellings(line, &mut ctx).into_iter().map(|r| &line[r]).collect();
//! assert_eq!(bad, ["mispeled", "lnk"]);
//! assert_eq!(speller.suggest("mispeled", 3)[0], "misspelled");
//! ```

mod cache;
mod scan;
mod speller;
mod system;
mod user_dict;

use std::path::PathBuf;

/// Copyright and licence notices of the bundled word lists (SCOWL and the
/// sources it draws on), to show to users as their licences ask.
pub const DICTIONARY_NOTICES: &[(&str, &str)] = &[
    ("English (US) word list", include_str!("../dictionaries/en_US.LICENSE")),
    ("English (UK) word list", include_str!("../dictionaries/en_GB.LICENSE")),
];

pub use scan::LineContext;
pub use speller::Speller;
pub use system::{BUNDLED_LANGUAGES, DictionaryInfo, DictionarySource, available_dictionaries, bundled_dictionaries, system_dictionaries};
pub use user_dict::UserDictionary;

/// Errors from loading dictionaries and saving the personal word list.
#[derive(Debug, thiserror::Error)]
pub enum SpellError {
    #[error("no spelling dictionary for {0:?}")]
    UnknownLanguage(String),
    #[error("can't access {}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{}: unsupported text encoding (expected UTF-8 or ISO-8859-1/15)", path.display())]
    Encoding { path: PathBuf },
    #[error("invalid {lang} dictionary: {message}")]
    Parse { lang: String, message: String },
}
