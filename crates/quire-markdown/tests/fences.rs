// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where does front matter end? A differential test of the seven places that
//! answered that question at 3f33a14 (v0.13.2), on the cases in
//! `fences/cases.rs`.
//!
//! The reader (`frontmatter::parse`) and the editor's block scan
//! (`blocks::verbatim_blocks`) run live. The other five were private to
//! their crates, so their logic is copied here as it was at 3f33a14, and
//! stays as the record of what each one did before they all moved to one
//! rule. pulldown-cmark's metadata block (syntax colours, extraction) is a
//! separate parser and is shown for comparison.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::fmt::Write as _;

use pulldown_cmark::{Event, Parser, Tag};
use quire_markdown::blocks::verbatim_blocks;
use quire_markdown::frontmatter;

include!("fences/cases.rs");

/// Each finder's answer: where the body starts, or `None` for "no front matter".
type Finder = fn(&str) -> Option<usize>;

/// The seven finders as they were at 3f33a14.
mod at_3f33a14 {
    /// `quire-markdown/src/linter/front.rs` `locate` (it returned the lines
    /// between the fences; here, where the body starts).
    pub(crate) fn linter_front(text: &str) -> Option<usize> {
        let first = text.split_inclusive('\n').next()?;
        if first.trim_end() != "---" {
            return None;
        }
        let mut pos = first.len();
        for line in text[pos..].split_inclusive('\n') {
            let t = line.trim_end();
            if t == "---" || t == "..." {
                return Some(pos + line.len());
            }
            pos += line.len();
        }
        None
    }

    /// `quire-index/src/derived.rs` `body_start` (0 meant no front matter).
    pub(crate) fn index_derived(src: &str) -> Option<usize> {
        if !(src.starts_with("---\n") || src.starts_with("---\r\n")) {
            return None;
        }
        let mut pos = src.find('\n').map_or(src.len(), |i| i + 1);
        for line in src[pos..].split_inclusive('\n') {
            pos += line.len();
            if matches!(line.trim_end(), "---" | "...") {
                return Some(pos);
            }
        }
        None
    }

    /// `quire-spell/src/scan.rs` `LineContext::advance`, run over the
    /// document's lines as `Speller::check_document` did. An unclosed block
    /// ran to the end of the file.
    pub(crate) fn spell_scan(src: &str) -> Option<usize> {
        let (mut in_fm, mut pos, mut first) = (false, 0, true);
        for raw in src.split_inclusive('\n') {
            let mut line = raw.strip_suffix('\n').unwrap_or(raw);
            line = line.strip_suffix('\r').unwrap_or(line);
            if first {
                line = line.strip_prefix('\u{feff}').unwrap_or(line);
            }
            pos += raw.len();
            if in_fm {
                let t = line.trim_end();
                if t == "---" || t == "..." {
                    return Some(pos);
                }
                continue;
            }
            if first && line.trim_end() == "---" {
                in_fm = true;
                first = false;
                continue;
            }
            return None;
        }
        in_fm.then_some(src.len())
    }

    /// `quire-agent/src/contract.rs` `body_of` (it returned the whole text
    /// when it found no closer).
    pub(crate) fn agent_contract(text: &str) -> Option<usize> {
        let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))?;
        rest.find("\n---").map(|i| {
            let after = &rest[i + 4..];
            let after = after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")).unwrap_or(after);
            text.len() - after.len()
        })
    }

    /// `apps/quire-cli/src/commands/agent.rs` `mark_untrusted`: the lines it
    /// took as front matter (and stripped `trust:` from) ended where the
    /// next `---` line begins; with none, it took no lines at all and the
    /// body started right after the opener. Here, where its body starts:
    /// after that `---` line.
    pub(crate) fn cli_mark_untrusted(text: &str) -> Option<usize> {
        let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let rest = text.strip_prefix("---\r\n").or_else(|| text.strip_prefix("---\n"))?;
        let open = text.len() - rest.len();
        Some(rest.find(&format!("{nl}---")).map_or(open, |i| {
            let line = open + i + nl.len();
            line + text[line..].find('\n').map_or(text.len() - line, |n| n + 1)
        }))
    }
}

fn reader(src: &str) -> Option<usize> {
    frontmatter::parse(src).map(|f| f.range.end)
}

fn blocks(src: &str) -> Option<usize> {
    // Front matter is the first verbatim block when it starts the note.
    verbatim_blocks(src).first().filter(|r| r.start == 0 && src.starts_with("---")).map(|r| r.end)
}

/// pulldown-cmark's metadata block: where the body starts (its range stops
/// before the closing line's line break).
fn pulldown(src: &str) -> Option<usize> {
    let opts = pulldown_cmark::Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let end = Parser::new_ext(src, opts)
        .into_offset_iter()
        .find_map(|(e, r)| matches!(e, Event::Start(Tag::MetadataBlock(_))).then_some(r.end))?;
    let rest = &src[end..];
    Some(end + if rest.starts_with("\r\n") { 2 } else { usize::from(rest.starts_with('\n')) })
}

const FINDERS: &[(&str, Finder)] = &[
    ("parse", reader),
    ("blocks", blocks),
    ("linter", at_3f33a14::linter_front),
    ("index", at_3f33a14::index_derived),
    ("spell", at_3f33a14::spell_scan),
    ("contract", at_3f33a14::agent_contract),
    ("mark_untr", at_3f33a14::cli_mark_untrusted),
    ("pulldown", pulldown),
];

fn cell(x: Option<usize>) -> String {
    x.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// One row per case: `!` marks a case where the seven do not all agree
/// (pulldown-cmark is not counted).
fn table(finders: &[(&str, Finder)]) -> String {
    let mut s = format!("{:<32}", "case");
    for (name, _) in finders {
        let _ = write!(s, "{name:>10}");
    }
    s.push('\n');
    for (name, src) in FENCE_CASES {
        let got: Vec<Option<usize>> = finders.iter().map(|(_, f)| f(src)).collect();
        let agree = got[..7].iter().all(|g| *g == got[0]);
        let _ = write!(s, "{}{name:<30}", if agree { "  " } else { "! " });
        for g in got {
            let _ = write!(s, "{:>10}", cell(g));
        }
        s.push('\n');
    }
    s
}

/// What each finder answered at 3f33a14. Read across a row: `-` is "no
/// front matter", a number is where the body starts.
const AT_3F33A14: &str = r"case                                 parse    blocks    linter     index     spell  contract mark_untr  pulldown
  plain LF                              13        13        13        13        13        13        13        13
  CRLF                                  16        16        16        16        16        16        16        16
! dots closer, rule later               13        13        13        13        13        22        22        13
! closer trailing spaces                15        15        15        15        15        12        15        15
! closer trailing tab                   14        14        14        14        14        12        14         -
! closer trailing NBSP                  15        15        15        15        15        12        15         -
! opener trailing spaces                 -         -        15         -        15         -         -        15
! BOM                                    -         -         -         -        16         -         -         -
! BOM, CRLF                              -         -         -         -        19         -         -         -
! unclosed                               -         -         -         -        14         -         4         -
  closer at EOF, no newline             12        12        12        12        12        12        12        12
! dots closer at EOF                    12        12        12        12        12         -         4        12
  dashes inside a value                 20        20        20        20        20        20        20        20
! four dashes line                      23        23        23        23        23        12        14        23
  indented dashes line                  24        24        24        24        24        24        24        24
! dashes then text                      24        24        24        24        24        12        15        24
  LF opener, CRLF after                 15        15        15        15        15        15        15        15
! CRLF opener, dots closer              16        16        16        16        16         -         5        16
  rule in body, no front matter          -         -         -         -         -         -         -         -
  blank line first                       -         -         -         -         -         -         -        14
  dots opener                            -         -         -         -         -         -         -         -
  lone CR line breaks                    -         -         -         -         -         -         -         -
! empty front matter                     8         8         8         8         8         -         4         -
! empty, dots closer                     8         8         8         8         8         -         4         -
! opener only                            -         -         -         -         4         -         4         -
! three dashes, no newline               -         -         -         -         3         -         -         -
  empty file                             -         -         -         -         -         -         -         -
! skill closed with dots                36        36        36        36        36        70        70        36
";

#[test]
fn the_seven_finders_at_3f33a14() {
    let got = table(FINDERS);
    println!("{got}");
    assert_eq!(got, AT_3F33A14, "\n{got}");
}

/// The one rule: `frontmatter::fences` answers what the reader answered at
/// 3f33a14 on every case, and the live finders in this crate agree with it.
#[test]
fn one_rule_is_the_readers() {
    let header: Vec<&str> = AT_3F33A14.lines().next().unwrap_or_default().split_whitespace().collect();
    let col = header.iter().position(|h| *h == "parse").expect("a parse column");
    for ((name, src), row) in FENCE_CASES.iter().zip(AT_3F33A14.lines().skip(1)) {
        let cells: Vec<&str> = row.split_whitespace().collect();
        let was = cells[cells.len() - (header.len() - col)];
        let now = frontmatter::fences(src).map(|f| f.body_start());
        assert_eq!(cell(now), was, "{name}: {src:?}");
        assert_eq!(reader(src), now, "{name}");
        assert_eq!(blocks(src), now, "{name}");
        if let Some(f) = frontmatter::fences(src) {
            assert!(frontmatter::is_opening_line(&src[f.open.clone()]), "{name}");
            assert!(frontmatter::is_closing_line(&src[f.close.clone()]), "{name}");
            assert_eq!(f.nl, if src.starts_with("---\r\n") { "\r\n" } else { "\n" });
        }
    }
}
