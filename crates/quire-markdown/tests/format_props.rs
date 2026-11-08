// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Property test for the formatting toggles (0.11 review H3): random notes
//! built from Markdown pieces, random multi-cursor selections, every
//! toggle. The only text a toggle may add or take away is markers and
//! whole fence lines, and toggling twice gives the original back for the
//! simple toggles.

use quire_markdown::format::{self, Edit, Inline, List};
use quire_text::{Buffer, Selection};

/// A small seeded generator (xorshift), so failures replay.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }
}

const PIECES: &[&str] = &[
    "a",
    "bc",
    "word",
    " ",
    " ",
    "**",
    "*",
    "_",
    "~~",
    "==",
    "`",
    "$",
    "2*3",
    "\\*",
    "`a**b`",
    "$a*b*c$",
    "**bold**",
    "_it_",
    "*em*",
    "~~gone~~",
    "==mark==",
    "`code`",
    "$x^2$",
    "[t](u*r*l)",
    "[[Note]]",
    "#tag",
    "é",
    "日本語",
    "👍🏽",
    "\n",
    "\n",
    "\r\n",
    "- ",
    "1. ",
    "- [ ] ",
    "# ",
    "> ",
    "```\n",
    "```js\n",
    "$$\n",
    "$$a$$ text",
    "snake_case",
];

/// Well-formed pieces only (no lone markers), for "toggling twice gives
/// the original back".
const WELL_FORMED: &[&str] = &[
    "a",
    "bc",
    "word",
    " ",
    " ",
    " 2*3 ",
    " **bold** ",
    " _it_ ",
    " ~~gone~~ ",
    " ==mark== ",
    " `code` ",
    " $x^2$ ",
    " [t](u) ",
    " [[Note]] ",
    " #tag ",
    "é",
    "日本語",
    "👍🏽",
    "\n",
    "\n\n",
    "\r\n",
    "- ",
    "1. ",
    "- [ ] ",
    "# ",
    "> ",
    " snake_case ",
];

fn doc(rng: &mut Rng) -> String {
    doc_of(rng, PIECES)
}

fn doc_of(rng: &mut Rng, pieces: &[&str]) -> String {
    let n = 1 + rng.below(14);
    let crlf = rng.below(3) == 0;
    let mut s = String::new();
    for _ in 0..n {
        let p = pieces[rng.below(pieces.len())];
        if crlf && p.ends_with('\n') && !p.ends_with("\r\n") {
            s.push_str(&p.replace('\n', "\r\n"));
        } else {
            s.push_str(p);
        }
    }
    s
}

fn selections(rng: &mut Rng, len: usize) -> Vec<Selection> {
    let k = 1 + rng.below(3);
    (0..k)
        .map(|_| {
            let a = rng.below(len + 1);
            if rng.below(2) == 0 { Selection::cursor(a) } else { Selection::new(a, rng.below(len + 1)) }
        })
        .collect()
}

/// A fence line as Quire writes them (```` ``` ```` with an info string, or
/// `$$`). (`~~~` lines are all markers anyway.)
fn is_fence(line: &str) -> bool {
    // At the start of the line, as Quire writes them and the pieces make them.
    let t = line.trim_end_matches('\r');
    t == "$$" || (t.starts_with("```") && !t[3..].contains('`'))
}

/// The text a toggle must keep: lines (with their `\r`) minus fence lines,
/// minus marker characters.
fn kept(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| if is_fence(l) { String::new() } else { l.trim_end_matches('\r').chars().filter(|c| !"*_~=`$".contains(*c)).collect() })
        .filter(|l| !l.is_empty())
        .collect()
}

/// A note whose line breaks are all `\r\n` keeps them so.
fn crlf_kept(text: &str, out: &str) -> bool {
    let lone_lf = |t: &str| t.replace("\r\n", "").contains('\n');
    !text.contains('\n') || lone_lf(text) || !lone_lf(out)
}

fn apply(text: &str, edit: Option<Edit>) -> (String, Option<Vec<Selection>>) {
    let mut buf = Buffer::from(text);
    match edit {
        Some((tx, sels)) => {
            tx.apply(&mut buf).expect("a toggle's edit applies");
            (buf.to_string(), Some(sels))
        }
        None => (text.to_string(), None),
    }
}

type Toggle = (&'static str, fn(&Buffer, &[Selection]) -> Option<Edit>);

const INLINE: &[Toggle] = &[
    ("bold", |b, s| format::toggle_inline(b, s, Inline::Bold)),
    ("italic", |b, s| format::toggle_inline(b, s, Inline::Italic)),
    ("strike", |b, s| format::toggle_inline(b, s, Inline::Strike)),
    ("highlight", |b, s| format::toggle_inline(b, s, Inline::Highlight)),
    ("code", |b, s| format::toggle_inline(b, s, Inline::Code)),
    ("math", |b, s| format::toggle_inline(b, s, Inline::Math)),
    ("code block", format::toggle_code_block),
    ("math block", format::toggle_math_block),
];

const OTHERS: &[Toggle] = &[
    ("heading", |b, s| format::set_heading(b, s, 2)),
    ("bullets", |b, s| format::toggle_list(b, s, List::Bullet)),
    ("numbers", |b, s| format::toggle_list(b, s, List::Numbered)),
    ("quote", format::toggle_quote),
    ("task", format::toggle_task),
    ("callout", |b, s| format::toggle_callout(b, s, "note")),
    ("link", format::toggle_link),
    ("wikilink", format::toggle_wikilink),
    ("footnote", format::insert_footnote),
    ("rule", format::insert_rule),
    ("table", format::insert_table),
];

#[test]
fn toggles_only_add_or_remove_markup() {
    let mut rng = Rng(0x5eed_0011);
    for case in 0..10_000 {
        let text = doc(&mut rng);
        let buf = Buffer::from(text.as_str());
        let sels = selections(&mut rng, buf.len_chars() + 2);
        for (name, f) in INLINE {
            let (out, _) = apply(&text, f(&buf, &sels));
            assert_eq!(kept(&out), kept(&text), "case {case}: {name} on {text:?} with {sels:?} gave {out:?}");
            assert!(crlf_kept(&text, &out), "case {case}: {name} on {text:?} with {sels:?} broke CRLF: {out:?}");
        }
        for (name, f) in OTHERS {
            // No panic, and every letter of the note is still there, in
            // order (list numbers and an unlinked URL may go).
            let (out, _) = apply(&text, f(&buf, &sels));
            if *name == "link" || *name == "numbers" || *name == "bullets" {
                continue;
            }
            let mut got = out.chars().filter(|c| c.is_alphabetic());
            assert!(
                text.chars().filter(|c| c.is_alphabetic()).all(|c| got.any(|g| g == c)),
                "case {case}: {name} lost text from {text:?} ({sels:?}): {out:?}"
            );
        }
    }
}

#[test]
fn toggling_twice_gives_the_original_back() {
    let mut rng = Rng(0x0011_7066);
    let mut checked = 0;
    for case in 0..20_000 {
        let text = doc_of(&mut rng, WELL_FORMED);
        let buf = Buffer::from(text.as_str());
        let chars: Vec<char> = text.chars().collect();
        // The simple case: a caret in a word, or the whole word selected.
        let words: Vec<(usize, usize)> = {
            let mut v = Vec::new();
            let mut i = 0;
            while i < chars.len() {
                if chars[i].is_alphanumeric() {
                    let s = i;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    v.push((s, i));
                } else {
                    i += 1;
                }
            }
            v
        };
        if words.is_empty() {
            continue;
        }
        let (ws, we) = words[rng.below(words.len())];
        // Next to other markers (`~~word~~~~`) Markdown can read them either way.
        let marker = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| "*_~=`$\\[]()".contains(*c));
        if marker(ws.checked_sub(1)) || marker(Some(we)) {
            continue;
        }
        let sels = if rng.below(2) == 0 { vec![Selection::cursor(ws + rng.below(we - ws + 1))] } else { vec![Selection::new(ws, we)] };
        for (name, f) in INLINE.iter().chain(&OTHERS[..1]).chain(&OTHERS[3..4]) {
            let (once, after) = apply(&text, f(&buf, &sels));
            let Some(after) = after else { continue };
            // Only when the first press added markup (wrapped); an unwrap
            // of `__x__` comes back as `**x**`.
            if once.chars().count() <= text.chars().count() {
                continue;
            }
            // A heading's level is set, not toggled: `#` → `##` → none.
            if *name == "heading" && buf.line(buf.char_to_line(ws)).starts_with('#') {
                continue;
            }
            // A fence added to a note with fences of its own may pair with one of those.
            if name.ends_with("block") && text.split('\n').any(is_fence) {
                continue;
            }
            let (twice, _) = apply(&once, f(&Buffer::from(once.as_str()), &after));
            assert_eq!(twice, text, "case {case}: {name} twice on {text:?} with {sels:?} (once: {once:?}, {after:?})");
            checked += 1;
        }
    }
    assert!(checked > 10_000, "only {checked} cases were checked");
}
