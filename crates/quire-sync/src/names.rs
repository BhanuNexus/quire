// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Whether a note's name can be a file on this computer (0.13.2).
//!
//! Another device can sync a note whose name this one's disk refuses: one
//! made on Linux or macOS with `:` or `?` in it reaching Windows, or two
//! notes whose names differ only in capital letters reaching a disk that
//! doesn't tell them apart (Windows, and macOS by default). The engine
//! skips such a note on this device, keeps syncing the rest, and says so
//! once ([`crate::LocalStore::unstorable`]). These are the rules, as pure
//! functions, so they can be tested on any system.

/// Characters Windows refuses in a file or folder name (besides `/`, which
/// separates folders everywhere, and control characters).
const WINDOWS_REFUSES: &[char] = &['<', '>', ':', '"', '\\', '|', '?', '*'];

/// Names Windows keeps for devices, with or without an extension.
const WINDOWS_DEVICES: &[&str] = &["con", "prn", "aux", "nul", "conin$", "conout$"];

/// The longest name of one file or folder, as most disks count it (bytes
/// on Linux and macOS, UTF-16 units on Windows).
const MAX_NAME: usize = 255;

/// Why `path` (vault-relative, `/` between folders) can't be a file on
/// Windows, in words; `None` when it can.
pub fn windows_problem(path: &str) -> Option<String> {
    for part in path.split('/') {
        if let Some(c) = part.chars().find(|c| WINDOWS_REFUSES.contains(c) || c.is_control()) {
            let shown = if c.is_control() { "a control character".to_string() } else { format!("“{c}”") };
            return Some(format!("Windows doesn't allow {shown} in a name"));
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Some("Windows doesn't allow a name that ends with a dot or a space".into());
        }
        // The part before the first dot, as Windows compares it.
        let stem = part.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_lowercase();
        let device = WINDOWS_DEVICES.contains(&stem.as_str())
            || ((stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.chars().nth(3).is_some_and(|d| matches!(d, '1'..='9' | '¹' | '²' | '³'))
                && stem.chars().count() == 4);
        if device {
            return Some(format!("“{stem}” is a name Windows keeps for itself"));
        }
        if part.encode_utf16().count() > MAX_NAME {
            return Some("the name is too long for this computer".into());
        }
    }
    None
}

/// Why `path` can't be a file on Linux or macOS; `None` when it can.
pub fn unix_problem(path: &str) -> Option<String> {
    for part in path.split('/') {
        if part.contains('\0') {
            return Some("a name can't contain a null character".into());
        }
        if part.len() > MAX_NAME {
            return Some("the name is too long for this computer".into());
        }
    }
    None
}

/// Why `path` can't be a file on the system this runs on.
pub fn problem_here(path: &str) -> Option<String> {
    if cfg!(windows) { windows_problem(path) } else { unix_problem(path) }
}

/// `path` as a disk that ignores capital letters compares it: each
/// character lower-cased where that gives one character.
pub fn fold_case(path: &str) -> String {
    path.chars()
        .map(|c| {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(one), None) => one,
                _ => c,
            }
        })
        .collect()
}

/// Whether two paths name the same file on a disk that ignores capital letters.
pub fn same_ignoring_case(a: &str, b: &str) -> bool {
    a == b || fold_case(a) == fold_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_refuses_these_names() {
        for bad in [
            "a:b.md",
            "Why?.md",
            "x*y.md",
            "<tag>.md",
            "a|b.md",
            "say \"hi\".md",
            "back\\slash.md",
            "Folder./Note.md",
            "Note.md.",
            "Note.md ",
            "trailing /Note.md",
            "aux.md",
            "AUX.md",
            "con",
            "Notes/nul.txt",
            "com1.md",
            "LPT9.md",
            "com¹.md",
            "aux .md",
            "tab\there.md",
        ] {
            assert!(windows_problem(bad).is_some(), "{bad} should be refused");
        }
    }

    #[test]
    fn windows_takes_these_names() {
        for good in [
            "Plan.md",
            "Notes/Plan.md",
            "auxiliary.md",
            "console.md",
            "com10.md",
            "com.md",
            "lpt.md",
            "a.b.c.md",
            "Résumé.md",
            "日本語.md",
            ".hidden.md",
        ] {
            assert_eq!(windows_problem(good), None, "{good}");
        }
    }

    #[test]
    fn long_names_are_refused_by_length_per_system() {
        let ascii = format!("{}.md", "a".repeat(252));
        assert_eq!(windows_problem(&ascii), None);
        assert_eq!(unix_problem(&ascii), None);
        // 100 characters of three bytes each: fine in UTF-16, too long in bytes.
        let wide = format!("{}.md", "語".repeat(100));
        assert_eq!(windows_problem(&wide), None);
        assert!(unix_problem(&wide).is_some());
        assert!(windows_problem(&format!("{}.md", "a".repeat(253))).is_some());
    }

    #[test]
    fn case_folding_matches_names_that_differ_only_in_capitals() {
        assert!(same_ignoring_case("Plan.md", "plan.md"));
        assert!(same_ignoring_case("Notes/PLAN.md", "notes/plan.md"));
        assert!(same_ignoring_case("Ärger.md", "ärger.md"));
        assert!(!same_ignoring_case("Plan.md", "Plan 2.md"));
        assert_eq!(fold_case("İ.md"), "İ.md", "a letter that lowers to two characters is kept");
    }

    #[cfg(windows)]
    #[test]
    fn this_windows_refuses_what_the_rules_say() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["Why?.md", "x*y.md", "<tag>.md", "a|b.md", "say \"hi\".md"] {
            assert!(windows_problem(bad).is_some());
            assert!(std::fs::write(dir.path().join(bad), "x").is_err(), "{bad} was written");
        }
        assert!(problem_here("a:b.md").is_some());
        // Names that differ only in capitals are one file here.
        std::fs::write(dir.path().join("Plan.md"), "one").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("plan.md")).unwrap(), "one");
    }
}
