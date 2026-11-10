// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The linter's settings: `vault.toml` `[lint.format]`.

use serde::{Deserialize, Serialize};

macro_rules! choice {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $var:ident = $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vm])* #[serde(rename = $text)] $var),+
        }
        impl $name {
            /// Every choice with the text it has in `vault.toml`.
            pub const ALL: &'static [(Self, &'static str)] = &[$((Self::$var, $text)),+];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$var => $text),+ }
            }
        }
    };
}

choice! {
    /// The bullet used for every unordered list.
    ListMarker { #[default] Dash = "-", Star = "*", Plus = "+", Keep = "keep" }
}
choice! {
    /// How far each nesting level of a list is indented.
    ListIndent { Two = "2", Four = "4", Tab = "tab", #[default] Keep = "keep" }
}
choice! {
    /// Which mark makes emphasis (`*x*` or `_x_`; `**x**` or `__x__`).
    Emphasis { Asterisk = "asterisk", Underscore = "underscore", #[default] Keep = "keep" }
}
choice! {
    /// Setext headings (`Title` over `=====`) become `# Title`.
    HeadingStyle { #[default] Atx = "atx", Keep = "keep" }
}
choice! {
    /// What to do with two spaces at the end of a line (a hard line break).
    TwoSpace { #[default] Keep = "keep", Strip = "strip" }
}
choice! {
    LineEndings { #[default] Keep = "keep", Lf = "lf", Crlf = "crlf" }
}
choice! {
    /// How `tags:` is written in front matter.
    TagsFormat { List = "list", Flow = "flow", #[default] Keep = "keep" }
}
choice! {
    /// The order of front-matter keys that `front-matter-order` doesn't name.
    Sort { #[default] Keep = "keep", Alpha = "alpha" }
}

/// `[lint.format]`. Every key is optional; a missing key has its default here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Options {
    /// Tidy a note when it is saved by hand (Save), as one undo step. Off by default.
    pub on_save: bool,
    /// Folders left alone when linting a folder or on save.
    pub exclude: Vec<String>,

    /// A heading is at most one level deeper than the one before it.
    pub heading_increment: bool,
    /// The first heading is at most this level (0: any).
    pub heading_start: u8,
    pub heading_style: HeadingStyle,
    pub trailing_spaces: bool,
    pub two_space_linebreak: TwoSpace,
    pub list_marker: ListMarker,
    pub list_indent: ListIndent,
    /// A blank line around headings, lists and code, and at most `max-blank-lines` in a row.
    pub blank_lines: bool,
    pub max_blank_lines: u8,
    /// The file ends with exactly one line ending.
    pub final_newline: bool,
    /// Tidy front matter: `key: value` spacing and no trailing spaces.
    pub front_matter: bool,
    /// Front-matter keys in this order, then the rest.
    pub front_matter_order: Vec<String>,
    pub front_matter_sort: Sort,
    pub tags_format: TagsFormat,
    /// `lower` or `keep`.
    pub tags_case: TagsCase,
    /// Set a "date modified" property.
    pub date_modified: bool,
    pub date_modified_key: String,
    /// `strftime` format of its value.
    pub date_format: String,
    /// `[[ Note | alias ]]` becomes `[[Note|alias]]`.
    pub link_spacing: bool,
    pub emphasis: Emphasis,
    pub strong: Emphasis,
    pub line_endings: LineEndings,
}

choice! {
    TagsCase { #[default] Keep = "keep", Lower = "lower" }
}

impl Options {
    /// Whether `format` is fit to build the "date modified" value from: it
    /// has text, no line break or other control character (which could write
    /// more front matter lines), and doesn't open with `:`.
    pub fn date_format_ok(format: &str) -> bool {
        !format.trim().is_empty() && !format.chars().any(char::is_control) && !format.trim_start().starts_with(':')
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            on_save: false,
            exclude: Vec::new(),
            heading_increment: true,
            heading_start: 0,
            heading_style: HeadingStyle::Atx,
            trailing_spaces: true,
            two_space_linebreak: TwoSpace::Keep,
            list_marker: ListMarker::Dash,
            list_indent: ListIndent::Keep,
            blank_lines: true,
            max_blank_lines: 1,
            final_newline: true,
            front_matter: true,
            front_matter_order: Vec::new(),
            front_matter_sort: Sort::Keep,
            tags_format: TagsFormat::Keep,
            tags_case: TagsCase::Keep,
            date_modified: false,
            date_modified_key: "modified".into(),
            date_format: "%Y-%m-%dT%H:%M".into(),
            link_spacing: true,
            emphasis: Emphasis::Keep,
            strong: Emphasis::Keep,
            line_endings: LineEndings::Keep,
        }
    }
}

/// A rule for lists and docs: its id, name and one line of help.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub help: &'static str,
}

/// The rules, in the order they run.
pub const RULES: &[Rule] = &[
    Rule { id: "front-matter", title: "Front matter", help: "Tidy `key: value` spacing, order keys, write tags the same way" },
    Rule { id: "headings", title: "Headings", help: "No skipped levels, a first level, `#` headings instead of underlines" },
    Rule { id: "list-marker", title: "List markers", help: "The same bullet in every unordered list" },
    Rule { id: "list-indent", title: "List indentation", help: "The same indent for each nesting level" },
    Rule { id: "emphasis", title: "Emphasis", help: "`*italic*` or `_italic_`, `**bold**` or `__bold__`" },
    Rule { id: "link-spacing", title: "Link spacing", help: "`[[ Note | alias ]]` becomes `[[Note|alias]]`" },
    Rule { id: "trailing-spaces", title: "Trailing spaces", help: "No spaces at the end of a line (two spaces for a line break can stay)" },
    Rule { id: "blank-lines", title: "Blank lines", help: "A blank line around headings, lists and code; no runs of blank lines" },
    Rule { id: "final-newline", title: "Final newline", help: "The file ends with one line ending" },
    Rule { id: "date-modified", title: "Date modified", help: "Set a property to the time of the change" },
    Rule { id: "line-endings", title: "Line endings", help: "Keep the file's own (CRLF stays CRLF), or convert all lines" },
];
