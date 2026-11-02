// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Callouts (`> [!note] Title`) and paragraph ids (`… ^id`), as Obsidian
//! writes them.
//!
//! A callout is a block quote whose first line starts with `[!type]`,
//! optionally followed by `+` or `-` (foldable, open or closed) and a title.
//! Types are case-insensitive and have aliases (`[!tldr]` is an abstract);
//! unknown types look like a note, titled with the type's name.
//!
//! A paragraph id is ` ^id` at the end of a paragraph or list item; a link to
//! `[[Note#^id]]` goes to that paragraph.

use std::ops::Range;

/// One kind of callout and how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalloutKind {
    /// Canonical type name (lowercase).
    pub id: &'static str,
    /// Title shown when the callout has none.
    pub label: &'static str,
    /// Accent colour, sRGB.
    pub color: [u8; 3],
    /// A one-character icon, drawn on the accent colour.
    pub glyph: char,
    /// Other names for the same kind.
    pub aliases: &'static [&'static str],
}

/// Every built-in kind, as in Obsidian.
pub const KINDS: &[CalloutKind] = &[
    CalloutKind { id: "note", label: "Note", color: [0x44, 0x8a, 0xff], glyph: 'i', aliases: &[] },
    CalloutKind { id: "abstract", label: "Abstract", color: [0x00, 0xb0, 0xc8], glyph: '¶', aliases: &["summary", "tldr"] },
    CalloutKind { id: "info", label: "Info", color: [0x44, 0x8a, 0xff], glyph: 'i', aliases: &[] },
    CalloutKind { id: "todo", label: "Todo", color: [0x44, 0x8a, 0xff], glyph: '○', aliases: &[] },
    CalloutKind { id: "tip", label: "Tip", color: [0x00, 0xa8, 0x96], glyph: '★', aliases: &["hint", "important"] },
    CalloutKind { id: "success", label: "Success", color: [0x2e, 0xa0, 0x43], glyph: '✓', aliases: &["check", "done"] },
    CalloutKind { id: "question", label: "Question", color: [0xe0, 0x8e, 0x0b], glyph: '?', aliases: &["help", "faq"] },
    CalloutKind { id: "warning", label: "Warning", color: [0xe8, 0x7a, 0x00], glyph: '!', aliases: &["caution", "attention"] },
    CalloutKind { id: "failure", label: "Failure", color: [0xe5, 0x48, 0x4d], glyph: '✗', aliases: &["fail", "missing"] },
    CalloutKind { id: "danger", label: "Danger", color: [0xe5, 0x48, 0x4d], glyph: '!', aliases: &["error"] },
    CalloutKind { id: "bug", label: "Bug", color: [0xe5, 0x48, 0x4d], glyph: '●', aliases: &[] },
    CalloutKind { id: "example", label: "Example", color: [0x8b, 0x5c, 0xf6], glyph: '◆', aliases: &[] },
    CalloutKind { id: "quote", label: "Quote", color: [0x8a, 0x8f, 0x98], glyph: '“', aliases: &["cite"] },
    // Quire's own: marks text an AI wrote (saved answers, ingest summaries).
    CalloutKind { id: "ai", label: "AI", color: [0x7c, 0x5c, 0xd6], glyph: '✦', aliases: &[] },
];

/// The kind for a type name; unknown names look like a note.
pub fn kind(name: &str) -> &'static CalloutKind {
    let n = name.to_ascii_lowercase();
    KINDS.iter().find(|k| k.id == n || k.aliases.contains(&n.as_str())).unwrap_or(&KINDS[0])
}

/// The first line of a callout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutHeader<'a> {
    pub kind: &'static CalloutKind,
    /// The type as written (`tldr`, `Warning`, `my-custom`).
    pub type_name: &'a str,
    /// The title as written, trimmed; empty if none.
    pub title: &'a str,
    /// `Some(true)` for `+` (folded open), `Some(false)` for `-` (folded shut).
    pub fold: Option<bool>,
    /// Byte range of the `>` markers, `[!type]`, fold sign and the spaces after it.
    pub marker: Range<usize>,
    /// Byte range of the title within the line (empty if none).
    pub title_range: Range<usize>,
}

impl CalloutHeader<'_> {
    /// What to show as the heading: the title, or the kind's label (for an
    /// unknown type, its name with a capital).
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.to_string();
        }
        let known = KINDS
            .iter()
            .any(|k| k.id.eq_ignore_ascii_case(self.type_name) || k.aliases.iter().any(|a| a.eq_ignore_ascii_case(self.type_name)));
        if known {
            return self.kind.label.to_string();
        }
        let mut c = self.type_name.chars();
        c.next().map_or_else(String::new, |f| {
            f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect::<String>().replace(['-', '_'], " ")
        })
    }
}

/// Parse a line as a callout's first line: `> [!type]±? title`. Nested
/// quotes (`> > [!note]`) count; the line must start with `>`.
pub fn parse_header(line: &str) -> Option<CalloutHeader<'_>> {
    let bytes = line.as_bytes();
    let mut i = 0;
    // Up to three spaces of indent, then one or more `>` (each optionally
    // followed by a space).
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    let mut quoted = false;
    while i < bytes.len() && bytes[i] == b'>' {
        quoted = true;
        i += 1;
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
    }
    if !quoted || !line[i..].starts_with("[!") {
        return None;
    }
    let close = line[i..].find(']')? + i;
    let type_name = &line[i + 2..close];
    if type_name.is_empty() || !type_name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let mut j = close + 1;
    let fold = match bytes.get(j) {
        Some(b'+') => {
            j += 1;
            Some(true)
        }
        Some(b'-') => {
            j += 1;
            Some(false)
        }
        _ => None,
    };
    // A title needs a space before it; `[!note]x` isn't a callout.
    if j < bytes.len() && !bytes[j].is_ascii_whitespace() {
        return None;
    }
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    let end = line.trim_end().len().max(j);
    Some(CalloutHeader { kind: kind(type_name), type_name, title: &line[j..end], fold, marker: 0..j, title_range: j..end })
}

/// The ` ^id` at the end of a line, if any: (byte range including the space
/// before it, the id). Ids are letters, digits and `-`.
pub fn block_id(line: &str) -> Option<(Range<usize>, &str)> {
    let t = line.trim_end();
    let caret = t.rfind('^')?;
    let id = &t[caret + 1..];
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    // Must stand alone: start of line or after whitespace (not `x^2`).
    let before = &t[..caret];
    if !(before.is_empty() || before.ends_with([' ', '\t'])) {
        return None;
    }
    let start = before.trim_end().len();
    Some((start..t.len(), id))
}

/// Byte ranges where a trailing `^x` isn't a paragraph id: fenced and
/// indented code, front matter and display math (`$$ x ^2 $$`).
pub fn non_prose(src: &str) -> Vec<Range<usize>> {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
    let mut out = crate::blocks::verbatim_blocks(src);
    if src.contains("$$") || src.contains("\n    ") || src.contains("\n\t") {
        let mut indented: Option<usize> = None;
        for (e, r) in Parser::new_ext(src, crate::options()).into_offset_iter() {
            match e {
                Event::DisplayMath(_) => out.push(r),
                Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => indented = Some(r.start),
                Event::End(TagEnd::CodeBlock) => {
                    if let Some(s) = indented.take() {
                        out.push(s..r.end);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Where paragraph `id` is in `src`: the byte range of its block (the
/// paragraph or list item that ends with ` ^id`), outside code, math and front matter.
pub fn find_block(src: &str, id: &str) -> Option<Range<usize>> {
    let verbatim = non_prose(src);
    let mut at = 0;
    let mut para_start = 0;
    for line in src.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            para_start = at;
            continue;
        }
        if verbatim.iter().any(|r| r.start < start + text.len() && start < r.end) {
            para_start = at;
            continue;
        }
        let item = {
            let t = text.trim_start();
            t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("+ ")
                || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if item {
            para_start = start;
        }
        if let Some((_, found)) = block_id(text)
            && found == id
        {
            // A line that's only `^id` names the block above it.
            let from = if text.trim().starts_with('^') && start > 0 { prev_block_start(src, start) } else { para_start };
            return Some(from..start + text.len());
        }
    }
    None
}

/// Start of the non-blank block that ends just before `line_start`
/// (blank lines may hold spaces; line ends may be `\r\n`).
fn prev_block_start(src: &str, line_start: usize) -> usize {
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    for l in src[..line_start].split_inclusive('\n') {
        starts.push(at);
        at += l.len();
    }
    let blank = |i: usize| {
        let end = starts.get(i + 1).copied().unwrap_or(line_start);
        src[starts[i]..end].trim().is_empty()
    };
    // Skip the blank lines right above, then take the block above them.
    let mut i = starts.len();
    while i > 0 && blank(i - 1) {
        i -= 1;
    }
    while i > 0 && !blank(i - 1) {
        i -= 1;
    }
    starts.get(i).copied().unwrap_or(0)
}

/// A new paragraph id: six lowercase letters and digits, not already in `src`.
pub fn new_block_id(src: &str, seed: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut x = seed | 1;
    loop {
        let mut id = String::with_capacity(6);
        for _ in 0..6 {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            id.push(char::from(ALPHABET[(x % ALPHABET.len() as u64) as usize]));
        }
        if !src.contains(&format!("^{id}")) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_callout_kind() {
        let h = parse_header("> [!ai] Written by Claude from 3 notes").unwrap();
        assert_eq!(h.kind.id, "ai");
        assert_eq!(h.kind.label, "AI");
    }

    #[test]
    fn parses_callout_headers() {
        let h = parse_header("> [!warning] Mind the gap").unwrap();
        assert_eq!((h.kind.id, h.type_name, h.title, h.fold), ("warning", "warning", "Mind the gap", None));
        assert_eq!(&"> [!warning] Mind the gap"[h.marker], "> [!warning] ");
        let h = parse_header(">[!TLDR]-").unwrap();
        assert_eq!((h.kind.id, h.title, h.fold), ("abstract", "", Some(false)));
        assert_eq!(h.display_title(), "Abstract");
        let h = parse_header("> > [!my-thing]+ ").unwrap();
        assert_eq!((h.kind.id, h.fold, h.display_title().as_str()), ("note", Some(true), "My thing"));
        assert!(parse_header("[!note] not quoted").is_none());
        assert!(parse_header("> [!note]x").is_none());
        assert!(parse_header("> [!] empty").is_none());
        assert!(parse_header("> [!no te] space").is_none());
        assert!(parse_header("> plain quote").is_none());
        assert_eq!(kind("Error").id, "danger");
    }

    #[test]
    fn finds_paragraph_ids() {
        assert_eq!(block_id("Some text ^abc-1"), Some((9..16, "abc-1")));
        assert_eq!(block_id("^solo"), Some((0..5, "solo")));
        assert!(block_id("x^2").is_none());
        assert!(block_id("trailing ^").is_none());
        assert!(block_id("odd ^id!").is_none());
        let src = "# T\n\nFirst line\nsecond line ^p1\n\n- item one ^li\n- item two\n\nPara\n\n^after\n\n```\ncode ^nope\n```\n";
        assert_eq!(&src[find_block(src, "p1").unwrap()], "First line\nsecond line ^p1");
        assert_eq!(&src[find_block(src, "li").unwrap()], "- item one ^li");
        assert_eq!(&src[find_block(src, "after").unwrap()], "Para\n\n^after");
        assert!(find_block(src, "nope").is_none());
        // Windows line ends and a blank line with spaces in it.
        let crlf = "# T\r\n\r\nPara one\r\n  \r\nPara two\r\n^after";
        assert_eq!(&crlf[find_block(crlf, "after").unwrap()], "Para two\r\n^after");
        // Display math isn't a paragraph.
        assert!(find_block("$$\nx = y ^2\n$$\n", "2").is_none());
        assert!(find_block(src, "missing").is_none());
        let id = new_block_id(src, 42);
        assert_eq!(id.len(), 6);
        assert!(!src.contains(&format!("^{id}")));
    }
}
