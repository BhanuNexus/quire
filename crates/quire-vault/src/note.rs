// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::fmt;

/// A vault-relative path with `/` separators, e.g. `Projects/Quire.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotePath(String);

impl NotePath {
    pub fn new(p: impl Into<String>) -> Self {
        let s: String = p.into().replace('\\', "/");
        Self(s.trim_start_matches("./").trim_start_matches('/').to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// File name without extension — the note's title and wikilink name.
    pub fn stem(&self) -> &str {
        let name = self.file_name();
        name.rsplit_once('.').map_or(name, |(s, _)| s)
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Parent folder, `""` for the vault root.
    pub fn folder(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(d, _)| d)
    }

    /// Path without `.md`, as used by path-style wikilinks.
    pub fn without_ext(&self) -> &str {
        self.0.strip_suffix(".md").unwrap_or(&self.0)
    }

    pub fn is_markdown(&self) -> bool {
        self.0.to_ascii_lowercase().ends_with(".md")
    }

    pub fn join(folder: &str, file: &str) -> Self {
        if folder.is_empty() { Self::new(file) } else { Self::new(format!("{}/{file}", folder.trim_end_matches('/'))) }
    }

    /// Same folder, different stem.
    #[must_use]
    pub fn with_stem(&self, stem: &str) -> Self {
        Self::join(self.folder(), &format!("{stem}.md"))
    }
}

impl fmt::Display for NotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NotePath {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// Strip characters that are invalid in file names on any major OS or that
/// would break wikilinks (`[]#|^`).
pub fn sanitize_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '[' | ']' | '#' | '^') { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_matches('.').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_parts() {
        let p = NotePath::new("./Projects\\Quire Plan.md");
        assert_eq!(p.as_str(), "Projects/Quire Plan.md");
        assert_eq!(p.stem(), "Quire Plan");
        assert_eq!(p.folder(), "Projects");
        assert_eq!(p.without_ext(), "Projects/Quire Plan");
        assert_eq!(p.with_stem("Roadmap").as_str(), "Projects/Roadmap.md");
    }

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize_title("  What? A/B: [test] #1  "), "What A B test 1");
        assert_eq!(sanitize_title(".."), "");
    }
}
