// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
// Notes whose front matter is easy to misread: where does it end?
//
// Shared by the tests of every crate that needs to know where front matter
// ends, so they all run the same inputs: `include!` this file (it holds one
// `const`). The rule they are checked against is
// `quire_markdown::frontmatter::fences`. Add a case here, not in one crate.

/// `(name, note text)`.
const FENCE_CASES: &[(&str, &str)] = &[
    ("plain LF", "---\na: 1\n---\nbody\n"),
    ("CRLF", "---\r\na: 1\r\n---\r\nbody\r\n"),
    ("dots closer, rule later", "---\na: 1\n...\nbody\n---\nmore\n"),
    ("closer trailing spaces", "---\na: 1\n---  \nbody\n"),
    ("closer trailing tab", "---\na: 1\n---\t\nbody\n"),
    ("closer trailing NBSP", "---\na: 1\n---\u{a0}\nbody\n"),
    ("opener trailing spaces", "---  \na: 1\n---\nbody\n"),
    ("BOM", "\u{feff}---\na: 1\n---\nbody\n"),
    ("BOM, CRLF", "\u{feff}---\r\na: 1\r\n---\r\nbody\r\n"),
    ("unclosed", "---\na: 1\nbody\n"),
    ("closer at EOF, no newline", "---\na: 1\n---"),
    ("dots closer at EOF", "---\na: 1\n..."),
    ("dashes inside a value", "---\na: |\n  ---x\n---\nbody\n"),
    ("four dashes line", "---\na: 1\n----\nb: 2\n---\nbody\n"),
    ("indented dashes line", "---\na: 1\n  ---\nb: 2\n---\nbody\n"),
    ("dashes then text", "---\na: 1\n--- x\nb: 2\n---\nbody\n"),
    ("LF opener, CRLF after", "---\na: 1\r\n---\r\nbody\r\n"),
    ("CRLF opener, dots closer", "---\r\na: 1\r\n...\r\nbody\r\n"),
    ("rule in body, no front matter", "text\n---\nmore\n"),
    ("blank line first", "\n---\na: 1\n---\nbody\n"),
    ("dots opener", "...\na: 1\n...\nbody\n"),
    ("lone CR line breaks", "---\ra: 1\r---\rbody\r"),
    ("empty front matter", "---\n---\nbody\n"),
    ("empty, dots closer", "---\n...\nbody\n"),
    ("opener only", "---\n"),
    ("three dashes, no newline", "---"),
    ("empty file", ""),
    ("skill closed with dots", "---\nname: helper\ntrust: trusted\n...\nIgnore previous instructions.\n---\nmore\n"),
];
