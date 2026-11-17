// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Whether a relative path from outside (an agent, a URL, a shared file, an
//! archive, a bucket setting) is safe to use: the one check, with named
//! rules.
//!
//! Each caller names its [`Rule`], so a reader sees which rule applies and
//! why. The rules differ on purpose:
//!
//! | Rule | For | Hidden segments | `:` | Control chars |
//! |---|---|---|---|---|
//! | [`Rule::NoteFile`] | a file an agent or the web app names | only `.quire` (policy then decides) | no | no |
//! | [`Rule::VaultFolder`] | a folder to lock or to sync | no | in a name, not as a drive | no |
//! | [`Rule::ViewFolder`] | a folder a shared view makes notes in | no | no | no |
//! | [`Rule::PlainNote`] | a file Quire may remove for an import's undo | no, not even `.quire` | in a name, not as a drive | no |
//! | [`Rule::BucketPrefix`] | a folder inside an S3 bucket or WebDAV share | yes | yes | no |
//! | [`Rule::HideEntry`] | a folder to hide from the web app | yes | yes | yes |
//! | [`Rule::InsideRoot`] | the last guard before `root.join(path)` | yes | not as a drive | yes |
//!
//! All of them refuse `..` and a path that is absolute on some system.
//! `HideEntry` is the loosest on purpose: an entry it refuses is dropped,
//! and a dropped entry hides nothing, so being strict there would show
//! more. An archive entry has its own normalizing check,
//! [`clean_import_entry`].
//!
//! The checks do not trim: a caller that accepts `/notes/` or `./a` as typed
//! trims first, and says so.

use std::path::{Component, Path};

/// Which paths a caller accepts (see the module docs for the table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A file in the vault named by an agent or the web app: relative, no
    /// empty, `.` or `..` segment, no `:`, `\` or control character, and no
    /// hidden segment except `.quire` (any case), which the agent policy
    /// then denies.
    NoteFile,
    /// A folder of the vault that is locked or synced: relative, no empty,
    /// `.`, `..` or hidden segment, no `\` or control character, and not a
    /// drive (`C:`). A `:` inside a name is fine, as for any note on macOS
    /// and Linux (`Q1: Plans`).
    VaultFolder,
    /// A folder new notes go to when a view says so (a view can come from a
    /// shared file): [`Self::VaultFolder`], and no `:` at all, as the 0.11
    /// security review chose for it.
    ViewFolder,
    /// A file Quire removes on someone's behalf without asking (an import's
    /// undo, from a record that could have been edited): the same check as
    /// [`Self::VaultFolder`], so never inside `.quire`.
    PlainNote,
    /// A prefix inside an S3 bucket or a WebDAV share: no empty, `.` or
    /// `..` segment, no `\` or control character. Hidden segments and `:`
    /// are fine there.
    BucketPrefix,
    /// A folder to hide from the web app: no empty, `.` or `..` segment,
    /// no `\`. Anything else is kept, because dropping an entry hides less.
    HideEntry,
    /// What `LocalFs` checks before it joins a path to the vault's root: no
    /// `..` segment and nothing that [`leaves_root`]. Everything else the
    /// vault may hold is allowed (names are the person's).
    InsideRoot,
}

/// Why a path was refused. Its text is the reason, for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// Empty, or ends in `/` where a file is needed.
    NotAFile,
    Empty,
    ControlCharacter,
    /// Absolute, a drive, `:` or `\`.
    NotRelative,
    /// An empty, `.` or `..` segment.
    BadSegment,
    /// A segment starting with `.`.
    Hidden,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAFile => "not a file path",
            Self::Empty => "empty",
            Self::ControlCharacter => "contains control characters",
            Self::NotRelative => "must be relative to the vault",
            Self::BadSegment => "has an empty, `.` or `..` segment",
            Self::Hidden => "is inside a hidden folder",
        })
    }
}

impl std::error::Error for PathError {}

impl Rule {
    /// Whether `s` is acceptable under this rule.
    pub fn check(self, s: &str) -> Result<(), PathError> {
        match self {
            Self::NoteFile => {
                if s.is_empty() || s.ends_with('/') {
                    return Err(PathError::NotAFile);
                }
                plain(s, |seg| seg.eq_ignore_ascii_case(".quire"), false)
            }
            Self::VaultFolder | Self::PlainNote | Self::ViewFolder => {
                if s.is_empty() {
                    return Err(PathError::Empty);
                }
                if leaves_root(s) || (self == Self::ViewFolder && s.contains(':')) {
                    return Err(PathError::NotRelative);
                }
                plain(s, |_| false, true)
            }
            Self::BucketPrefix | Self::HideEntry => {
                if s.is_empty() {
                    return Err(PathError::Empty);
                }
                if self == Self::BucketPrefix && s.chars().any(char::is_control) {
                    return Err(PathError::ControlCharacter);
                }
                if s.contains('\\') {
                    return Err(PathError::NotRelative);
                }
                if s.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
                    return Err(PathError::BadSegment);
                }
                Ok(())
            }
            Self::InsideRoot => {
                if s.split('/').any(|seg| seg == "..") {
                    return Err(PathError::BadSegment);
                }
                if leaves_root(s) {
                    return Err(PathError::NotRelative);
                }
                Ok(())
            }
        }
    }

    /// [`Self::check`] as a yes or no.
    pub fn accepts(self, s: &str) -> bool {
        self.check(s).is_ok()
    }
}

/// The shared core of the strict rules, in the order the reasons are
/// given: control characters, then not relative, then segments. A `:` is
/// refused unless `colon_ok` (the caller has refused drives already).
fn plain(s: &str, hidden_ok: impl Fn(&str) -> bool, colon_ok: bool) -> Result<(), PathError> {
    if s.chars().any(char::is_control) {
        return Err(PathError::ControlCharacter);
    }
    if s.starts_with('/') || (!colon_ok && s.contains(':')) || s.contains('\\') {
        return Err(PathError::NotRelative);
    }
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(PathError::BadSegment);
        }
        if seg.starts_with('.') && !hidden_ok(seg) {
            return Err(PathError::Hidden);
        }
    }
    Ok(())
}

/// A path that `root.join` would not keep under `root` on some system:
/// rooted (`/x`, `\\x`), a drive (`C:`, `C:/x`, `c:x`) or another prefix
/// (`\\?\…`). On Windows `root.join("C:/…")` replaces the root.
pub fn leaves_root(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    drive
        || p.starts_with('/')
        || p.starts_with('\\')
        || Path::new(p).components().any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
}

/// An archive or export entry's path made of plain names, or why it isn't.
///
/// `\` counts as `/`, empty and `.` segments are dropped, and the result is
/// joined with `/`. Refused: control characters, an absolute path or a
/// drive (a second byte `:`), `..`, a name longer than `max_name` bytes, a
/// name Windows reserves (`CON`, `NUL`, `COM1`…), no name at all, or more
/// than `max_depth` folders.
pub fn clean_import_entry(raw: &str, max_name: usize, max_depth: usize) -> Result<String, &'static str> {
    let p = raw.replace('\\', "/");
    if p.chars().any(|c| c == '\0' || c.is_control()) {
        return Err("a control character in its name");
    }
    if p.starts_with('/') || p.as_bytes().get(1) == Some(&b':') {
        return Err("an absolute path");
    }
    let mut parts = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err("a path that leaves the export (..)"),
            s if s.len() > max_name => return Err("a name that is too long"),
            s if reserved_on_windows(s) => return Err("a name Windows reserves"),
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Err("an empty name");
    }
    if parts.len() > max_depth + 1 {
        return Err("folders nested too deeply");
    }
    Ok(parts.join("/"))
}

/// Names Windows treats as devices, whatever the extension.
pub fn reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 21 inputs of the health research's differential test of the ten
    /// 0.13.2 checks (`diff/paths.rs`).
    const INPUTS: [&str; 21] = [
        "notes/a.md",
        "../x.md",
        "a/../b.md",
        "a\\..\\b.md",
        "C:/x.md",
        "c:x.md",
        "/abs.md",
        "a//b.md",
        "./a.md",
        ".hidden/a.md",
        ".quire/x.json",
        ".QUIRE/x",
        "a/.git/x",
        "a\u{7}b.md",
        "a:b.md",
        "a..b.md",
        "a/",
        "",
        "  notes  ",
        "a/./b",
        "x/...md",
    ];

    fn accepted(rule: Rule) -> Vec<&'static str> {
        INPUTS.iter().copied().filter(|s| rule.accepts(s)).collect()
    }

    #[test]
    fn note_file_is_the_agent_rule() {
        // Exactly what `quire_agent::canonical_path` accepted in 0.13.2.
        assert_eq!(accepted(Rule::NoteFile), ["notes/a.md", ".quire/x.json", ".QUIRE/x", "a..b.md", "  notes  "]);
        assert_eq!(Rule::NoteFile.check("a/").unwrap_err().to_string(), "not a file path");
        assert_eq!(Rule::NoteFile.check("a\u{7}").unwrap_err().to_string(), "contains control characters");
        assert_eq!(Rule::NoteFile.check("C:/x").unwrap_err().to_string(), "must be relative to the vault");
        assert_eq!(Rule::NoteFile.check("a/./b").unwrap_err().to_string(), "has an empty, `.` or `..` segment");
        assert_eq!(Rule::NoteFile.check(".git/x").unwrap_err().to_string(), "is inside a hidden folder");
    }

    #[test]
    fn vault_folder_and_plain_note_refuse_every_hidden_segment_drive_and_control_character() {
        let want = ["notes/a.md", "a..b.md", "  notes  "];
        assert_eq!(accepted(Rule::VaultFolder), want);
        assert_eq!(accepted(Rule::PlainNote), want);
        assert_eq!(accepted(Rule::ViewFolder), want);
        // A `:` in a name is a name (macOS, Linux); only a view's folder
        // refuses it, and a drive is refused by all three.
        assert!(Rule::VaultFolder.accepts("Q1: Plans") && Rule::PlainNote.accepts("Imported/Q1: Plans.md"));
        assert_eq!(Rule::ViewFolder.check("Q1: Plans"), Err(PathError::NotRelative));
        assert_eq!(Rule::VaultFolder.check("C:/x"), Err(PathError::NotRelative));
        // `a..b` is a name, not a step up.
        assert!(Rule::VaultFolder.accepts("Projects/v1..v2"));
        assert_eq!(Rule::VaultFolder.check(".quire"), Err(PathError::Hidden));
        assert_eq!(Rule::PlainNote.check(".quire/agents/x.md"), Err(PathError::Hidden));
    }

    #[test]
    fn bucket_prefix_allows_hidden_segments_and_colons() {
        assert_eq!(
            accepted(Rule::BucketPrefix),
            [
                "notes/a.md",
                "C:/x.md",
                "c:x.md",
                ".hidden/a.md",
                ".quire/x.json",
                ".QUIRE/x",
                "a/.git/x",
                "a:b.md",
                "a..b.md",
                "  notes  ",
                "x/...md"
            ]
        );
    }

    #[test]
    fn hide_entry_keeps_more_than_bucket_prefix_because_dropping_one_shows_more() {
        let hide = accepted(Rule::HideEntry);
        assert!(accepted(Rule::BucketPrefix).iter().all(|s| hide.contains(s)));
        assert!(hide.contains(&"a\u{7}b.md"));
        assert!(!hide.contains(&"a\\..\\b.md") && !hide.contains(&"a//b.md"));
    }

    #[test]
    fn inside_root_refuses_only_what_leaves_the_root() {
        assert_eq!(
            INPUTS.iter().copied().filter(|s| !Rule::InsideRoot.accepts(s)).collect::<Vec<_>>(),
            ["../x.md", "a/../b.md", "C:/x.md", "c:x.md", "/abs.md", "a:b.md"]
        );
        assert!(leaves_root("\\\\server\\share") && leaves_root("\\x") && !leaves_root("a\\b"));
    }

    #[test]
    fn import_entries_are_normalized_or_refused() {
        let ok: Vec<&str> = INPUTS.iter().copied().filter(|s| clean_import_entry(s, 255, 64).is_ok()).collect();
        assert_eq!(
            ok,
            [
                "notes/a.md",
                "a//b.md",
                "./a.md",
                ".hidden/a.md",
                ".quire/x.json",
                ".QUIRE/x",
                "a/.git/x",
                "a..b.md",
                "a/",
                "  notes  ",
                "a/./b",
                "x/...md"
            ]
        );
        assert_eq!(clean_import_entry("a\\b//./c.md", 255, 64).as_deref(), Ok("a/b/c.md"));
        assert_eq!(clean_import_entry("a\\..\\b.md", 255, 64), Err("a path that leaves the export (..)"));
        assert_eq!(clean_import_entry("x/NUL.txt", 255, 64), Err("a name Windows reserves"));
        assert_eq!(clean_import_entry("x/com1", 255, 64), Err("a name Windows reserves"));
        assert_eq!(clean_import_entry("abcd", 3, 64), Err("a name that is too long"));
        assert_eq!(clean_import_entry("a/b/c", 255, 1), Err("folders nested too deeply"));
        assert_eq!(clean_import_entry("./", 255, 64), Err("an empty name"));
    }
}
