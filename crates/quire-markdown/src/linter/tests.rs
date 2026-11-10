// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Examples for each rule, then property tests over generated notes:
//! linting twice is linting once, no rule ever needs the safety net, code
//! and math and unreadable front matter come out as they went in, and CRLF
//! stays CRLF.

use super::*;

fn only(f: impl FnOnce(&mut Options)) -> Options {
    let mut o = Options {
        heading_increment: false,
        heading_style: HeadingStyle::Keep,
        trailing_spaces: false,
        list_marker: ListMarker::Keep,
        blank_lines: false,
        final_newline: false,
        front_matter: false,
        link_spacing: false,
        ..Options::default()
    };
    f(&mut o);
    o
}

fn run(text: &str, o: &Options) -> String {
    let out = lint(text, o, &Cx { stamp: "2026-11-10T09:30", edited: false });
    assert!(out.reverted.is_empty(), "reverted {:?} on {text:?}", out.reverted);
    out.text
}

#[test]
fn trailing_spaces_keep_line_breaks_and_leave_code() {
    let o = only(|o| o.trailing_spaces = true);
    assert_eq!(run("a  \nb   \nc\t\n", &o), "a  \nb  \nc\n");
    assert_eq!(run("a   \n\n  \nb\n", &o), "a\n\n\nb\n");
    assert_eq!(run("```\ncode   \n```\ntext  \n", &o), "```\ncode   \n```\ntext\n");
    let strip = only(|o| {
        o.trailing_spaces = true;
        o.two_space_linebreak = TwoSpace::Strip;
    });
    let out = lint("a  \nb\n", &strip, &Cx::default());
    // Taking a hard break away changes the note: the safety net refuses it.
    assert_eq!(out.text, "a  \nb\n");
    assert_eq!(out.reverted, vec!["trailing-spaces"]);
}

#[test]
fn headings_never_skip_a_level_and_keep_the_outline() {
    let o = only(|o| o.heading_increment = true);
    assert_eq!(run("# A\n\n### B\n\n#### C\n\n### D\n", &o), "# A\n\n## B\n\n### C\n\n## D\n");
    assert_eq!(run("# A\n## B\n", &o), "# A\n## B\n");
    let o = only(|o| o.heading_start = 2);
    assert_eq!(run("#### A\n\n##### B\n\n#### C\n", &o), "## A\n\n### B\n\n## C\n");
    // Headings in code stay.
    assert_eq!(run("# A\n\n```\n#### x\n```\n", &only(|o| o.heading_increment = true)), "# A\n\n```\n#### x\n```\n");
}

#[test]
fn setext_and_closed_headings_become_atx() {
    let o = only(|o| o.heading_style = HeadingStyle::Atx);
    assert_eq!(run("Title\n=====\n\nSub\n---\n\ntext\n", &o), "# Title\n\n## Sub\n\ntext\n");
    assert_eq!(run("##   Closed ##  \n#tag stays\n# C#\n", &o), "## Closed\n#tag stays\n# C#\n");
}

#[test]
fn list_markers_are_the_same_but_adjacent_lists_are_not_merged() {
    let o = only(|o| o.list_marker = ListMarker::Dash);
    assert_eq!(run("* a\n* b\n  + c\n", &o), "- a\n- b\n  - c\n");
    // Two bullet lists side by side stay two lists.
    assert_eq!(run("- a\n\n* b\n", &o), "- a\n\n* b\n");
    assert_eq!(run("1. a\n2. b\n", &o), "1. a\n2. b\n");
    assert_eq!(run("* * *\n", &o), "* * *\n");
}

#[test]
fn list_indent_follows_each_level() {
    let o = only(|o| o.list_indent = ListIndent::Two);
    assert_eq!(run("- a\n    - b\n        - c\n", &o), "- a\n  - b\n    - c\n");
    // Under a numbered item a child needs the number's width.
    assert_eq!(run("1. a\n    - b\n", &o), "1. a\n   - b\n");
    // Continuation lines move with their item.
    assert_eq!(run("- a\n    - b\n      more\n", &o), "- a\n  - b\n    more\n");
    let o = only(|o| o.list_indent = ListIndent::Four);
    assert_eq!(run("- a\n  - b\n    text\n", &o), "- a\n    - b\n      text\n");
    let o = only(|o| o.list_indent = ListIndent::Tab);
    assert_eq!(run("- a\n  - b\n    - c\n", &o), "- a\n\t- b\n\t\t- c\n");
}

#[test]
fn list_indent_leaves_code_in_items_alone() {
    let o = only(|o| o.list_indent = ListIndent::Two);
    let src = "- a\n    - b\n      ```\n      x   y\n      ```\n";
    assert_eq!(run(src, &o), "- a\n  - b\n    ```\n    x   y\n    ```\n");
}

#[test]
fn emphasis_style_changes_only_real_emphasis() {
    let o = only(|o| {
        o.emphasis = Emphasis::Underscore;
        o.strong = Emphasis::Underscore;
    });
    assert_eq!(run("*a* and **b** and snake_case and a*b*c\n", &o), "_a_ and __b__ and snake_case and a*b*c\n");
    let o = only(|o| {
        o.emphasis = Emphasis::Asterisk;
        o.strong = Emphasis::Asterisk;
    });
    assert_eq!(run("_a_ and __b__ and `_c_` and 2 * 3 * 4\n", &o), "*a* and **b** and `_c_` and 2 * 3 * 4\n");
    assert_eq!(run("***both***\n", &o), "***both***\n");
}

#[test]
fn link_spacing_tidies_wikilinks_not_code() {
    let o = only(|o| o.link_spacing = true);
    assert_eq!(run("[[ Note | alias ]] ![[ img.png ]] [[Note # Head]]\n", &o), "[[Note|alias]] ![[img.png]] [[Note#Head]]\n");
    assert_eq!(run("`[[ a ]]`\n\n```\n[[ b ]]\n```\n", &o), "`[[ a ]]`\n\n```\n[[ b ]]\n```\n");
}

#[test]
fn blank_lines_around_blocks_and_in_runs() {
    let o = only(|o| o.blank_lines = true);
    assert_eq!(
        run("text\n# H\ntext\n- a\n- b\ntext2\n```\ncode\n```\nafter\n", &o),
        "text\n\n# H\n\ntext\n\n- a\n- b\ntext2\n\n```\ncode\n```\n\nafter\n"
    );
    assert_eq!(run("\n\na\n\n\n\nb\n", &o), "a\n\nb\n");
    // Blank lines in code stay; a lazy line after a list item stays part of it.
    assert_eq!(run("```\na\n\n\n\nb\n```\n", &o), "```\na\n\n\n\nb\n```\n");
    assert_eq!(run("- a\nlazy\n", &o), "- a\nlazy\n");
    // Right after front matter nothing is added.
    assert_eq!(run("---\na: 1\n---\n# H\n", &o), "---\na: 1\n---\n# H\n");
}

#[test]
fn final_newline_is_one() {
    let o = only(|o| o.final_newline = true);
    assert_eq!(run("a", &o), "a\n");
    assert_eq!(run("a\n\n\n", &o), "a\n");
    assert_eq!(run("a\r\nb", &o), "a\r\nb\r\n");
    assert_eq!(run("", &o), "");
}

#[test]
fn line_endings_stay_unless_asked() {
    let all = Options::default();
    let src = "# A\r\ntext  \r\n* b\r\n* c\r\n";
    let out = run(src, &all);
    assert_eq!(out, "# A\r\n\r\ntext\r\n\r\n- b\r\n- c\r\n");
    let lf = only(|o| o.line_endings = LineEndings::Lf);
    assert_eq!(run(src, &lf), "# A\ntext  \n* b\n* c\n");
    let crlf = only(|o| o.line_endings = LineEndings::Crlf);
    assert_eq!(run("a\nb\n", &crlf), "a\r\nb\r\n");
    // Lines added to a CRLF file are CRLF too.
    assert_eq!(run("a\r\n# H\r\nb\r\n", &Options::default()), "a\r\n\r\n# H\r\n\r\nb\r\n");
}

#[test]
fn front_matter_is_tidied_ordered_and_left_alone_when_odd() {
    let o = only(|o| {
        o.front_matter = true;
        o.front_matter_order = vec!["title".into(), "tags".into()];
    });
    let src = "---\nzeta:   9  \nalias: x\n# why\ntags: [b, a]\ntitle:  \"T\"\n---\nbody\n";
    assert_eq!(run(src, &o), "---\ntitle: \"T\"\n# why\ntags: [b, a]\nzeta: 9\nalias: x\n---\nbody\n");
    // Unreadable front matter is untouched, whatever the rules say.
    let odd = "---\n: broken\n\t- x   \n---\n\n\n\nbody  \n";
    let out = lint(odd, &Options::default(), &Cx::default());
    assert!(out.text.starts_with("---\n: broken\n\t- x   \n---\n"), "{:?}", out.text);
    assert!(!out.notes.is_empty());
    // A block scalar keeps its trailing spaces.
    let sc = "---\nnote: |\n  keep   \n  this\nb: 1  \n---\n";
    assert_eq!(run(sc, &only(|o| o.front_matter = true)), "---\nnote: |\n  keep   \n  this\nb: 1\n---\n");
}

#[test]
fn tags_are_written_one_way() {
    let o = only(|o| {
        o.tags_format = TagsFormat::List;
        o.tags_case = TagsCase::Lower;
    });
    assert_eq!(run("---\ntags: Alpha, beta gamma, alpha\n---\n", &o), "---\ntags:\n  - alpha\n  - beta-gamma\n---\n");
    // `#Alpha` after `tags:` is a YAML comment, not a tag: left alone.
    let comment = "---\ntags: #Alpha, beta gamma, alpha\n---\n";
    assert_eq!(run(comment, &o), comment);
    let flow = only(|o| o.tags_format = TagsFormat::Flow);
    assert_eq!(run("---\ntags:\n  - a\n  - \"#b\"\n---\n", &flow), "---\ntags: [a, b]\n---\n");
    assert_eq!(run("---\ntags: []\n---\n", &flow), "---\ntags: []\n---\n");
}

#[test]
fn date_modified_is_set_when_the_note_changes_or_is_edited() {
    let o = only(|o| {
        o.date_modified = true;
        o.trailing_spaces = true;
    });
    let cx = Cx { stamp: "2026-11-10T09:30", edited: false };
    // Nothing else changed: the note is left as it is.
    let same = lint("a\n", &o, &cx);
    assert_eq!(same.text, "a\n");
    // Something changed: the property is added, and a second run adds nothing.
    let one = lint("a   \n", &o, &cx);
    assert_eq!(one.text, "---\nmodified: 2026-11-10T09:30\n---\n\na\n");
    assert_eq!(lint(&one.text, &o, &cx).text, one.text);
    // Edited by hand: set even if nothing else changed; an existing key is updated in place.
    let edited = lint("---\nmodified: 2020-01-01\ntitle: T\n---\nx\n", &o, &Cx { edited: true, ..cx });
    assert_eq!(edited.text, "---\nmodified: 2026-11-10T09:30\ntitle: T\n---\nx\n");
}

#[test]
fn math_html_and_comments_are_never_touched() {
    let src = "$$\n*a*   \n$$\n\n<div>\n* x   \n</div>\n\n<!-- *c*   -->\n\ntext $a *b* c$ end  \n";
    let o = Options { emphasis: Emphasis::Underscore, list_marker: ListMarker::Dash, ..Options::default() };
    let out = run(src, &o);
    assert!(out.contains("$$\n*a*   \n$$"));
    assert!(out.contains("<div>\n* x   \n</div>"));
    assert!(out.contains("<!-- *c*   -->"));
    assert!(out.contains("$a *b* c$"));
}

#[test]
fn a_lone_cr_file_is_left_alone() {
    let out = lint("a\rb\r", &Options::default(), &Cx::default());
    assert_eq!(out.text, "a\rb\r");
    assert!(!out.notes.is_empty());
}

// ---------------------------------------------------------------- properties

/// A small deterministic generator (xorshift), so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

const PIECES: &[&str] = &[
    "# Title",
    "## Sub  ",
    "### Deep",
    "###### Deepest",
    "#### Skipped",
    "Setext\n======",
    "Setext two\n---",
    "##   Closed ##",
    "#tag and text",
    "plain text",
    "text with trailing   ",
    "hard  \nbreak",
    "three   \nspaces",
    "* star",
    "- dash",
    "+ plus",
    "  - nested",
    "    * deeper nested",
    "1. one",
    "2. two",
    "   - under ordered",
    "\t- tabbed",
    "- a\n    - b\n        - c",
    "- a\n    - b\n      more text",
    "1. a\n    - b\n    - c",
    "- [ ] task",
    "- [x] done",
    "```\ncode   \n  # not a heading\n* not a list\n```",
    "~~~rust\nfn x() {}   \n~~~",
    "```\nunclosed code  \n",
    "    indented code  ",
    "$$\nx *  y_1   \n$$",
    "text with $a *b* c$ math  ",
    "<div>\n*html*  \n</div>",
    "<!-- a *comment*  -->",
    "| a | b |\n|---|---|\n| 1 | 2  |",
    "> quote  ",
    "> [!note] Callout\n> * item",
    "[[ Note | alias ]]",
    "![[ img.png ]] and [[Note # Heading]]",
    "[[Note#^block]] [[a]]  [[ b ]]",
    "*em* and __strong__ and _it_ and **bold**",
    "snake_case_word and 2 * 3 * 4",
    "`code *x* ` and `[[ a ]]` after  ",
    "***both*** and _**mixed**_",
    "---",
    "***",
    "* * *",
    "[link *x*](http://a.b/_c_) and <b>x</b>",
    "",
    "",
    "   ",
    "\t",
    "text\nlazy\n- item\nlazy again",
    "- a\n\n* b",
    "Term\n: definition",
    "[^1]: footnote  ",
    "line with ünïcode  and emoji 😀  ",
    "---\n\n---\n```md\n---\nfoo: bar\n---\n```",
    "text\n---\nnot front matter: x\n---\n```\n---\nk: v\n---\n```",
    "$$\n---\nx *  y\n---\n$$",
];

const FRONTS: &[&str] = &[
    "",
    "",
    "",
    "---\ntitle:   Note  \ntags: #A, b c\nzeta: 1\nalpha: 2   \n---",
    "---\n# comment\nstatus: draft\n\ntags:\n  - x\n  - Y\n---",
    "---\nnote: |\n  keep   \n  this\nb: 1\n---",
    "---\n: broken\n\t- x   \n---",
    "---\nauthor:\n  name: Ada  \n  tags: [a]\ntags: []\n---",
    "---\nunclosed: 1\n",
    "---\na: 1\na: 2\n---",
    "---\ntags: [A, b]  # keep me\ntitle: \"x: y\"\n---",
    "---\ntags:\n  - A # first\n  - \"true\"\nz: &x [1, 2]\na: *x\n---",
    "---\ntags: \"quoted, one\"\nnote: >\n  folded   \n  text\n---",
];

fn make_note(r: &mut Rng) -> String {
    let mut s = String::new();
    s.push_str(r.pick(FRONTS));
    if !s.is_empty() && r.chance(80) {
        s.push('\n');
    }
    for _ in 0..=r.below(12) {
        s.push_str(r.pick(PIECES));
        s.push('\n');
        if r.chance(40) {
            s.push('\n');
        }
    }
    if r.chance(10) {
        s.pop();
    }
    if r.chance(35) {
        s = s.replace('\n', "\r\n");
    } else if r.chance(5) {
        // Mixed endings.
        s = s.replacen('\n', "\r\n", 2);
    }
    s
}

fn make_options(r: &mut Rng) -> Options {
    Options {
        heading_increment: r.chance(60),
        heading_start: *r.pick(&[0, 0, 1, 2, 3]),
        heading_style: if r.chance(70) { HeadingStyle::Atx } else { HeadingStyle::Keep },
        trailing_spaces: r.chance(70),
        two_space_linebreak: TwoSpace::Keep,
        list_marker: *r.pick(&[ListMarker::Dash, ListMarker::Star, ListMarker::Plus, ListMarker::Keep]),
        list_indent: *r.pick(&[ListIndent::Two, ListIndent::Four, ListIndent::Tab, ListIndent::Keep]),
        blank_lines: r.chance(70),
        max_blank_lines: *r.pick(&[1, 1, 2]),
        final_newline: r.chance(70),
        front_matter: r.chance(70),
        front_matter_order: if r.chance(40) { vec!["title".into(), "tags".into()] } else { Vec::new() },
        front_matter_sort: if r.chance(30) { Sort::Alpha } else { Sort::Keep },
        tags_format: *r.pick(&[TagsFormat::List, TagsFormat::Flow, TagsFormat::Keep]),
        tags_case: if r.chance(30) { TagsCase::Lower } else { TagsCase::Keep },
        date_modified: r.chance(30),
        link_spacing: r.chance(70),
        emphasis: *r.pick(&[Emphasis::Asterisk, Emphasis::Underscore, Emphasis::Keep]),
        strong: *r.pick(&[Emphasis::Asterisk, Emphasis::Underscore, Emphasis::Keep]),
        line_endings: if r.chance(15) {
            LineEndings::Lf
        } else if r.chance(10) {
            LineEndings::Crlf
        } else {
            LineEndings::Keep
        },
        ..Options::default()
    }
}

const CASES: u64 = 4000;

fn cx(edited: bool) -> Cx<'static> {
    Cx { stamp: "2026-11-10T09:30", edited }
}

#[test]
fn linting_twice_is_linting_once() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..CASES {
        let (note, o) = (make_note(&mut r), make_options(&mut r));
        let edited = r.chance(50);
        let one = lint(&note, &o, &cx(edited));
        let two = lint(&one.text, &o, &cx(edited));
        assert_eq!(two.text, one.text, "case {case}: not idempotent\noptions {o:?}\nnote {note:?}\nonce {:?}", one.text);
    }
}

#[test]
fn no_rule_needs_the_safety_net() {
    let mut r = Rng(0xD1B5_4A32_D192_ED03);
    for case in 0..CASES {
        let (note, o) = (make_note(&mut r), make_options(&mut r));
        let out = lint(&note, &o, &cx(true));
        assert!(out.reverted.is_empty(), "case {case}: {:?} was undone\noptions {o:?}\nnote {note:?}", out.reverted);
    }
}

/// Only whitespace and markup change: the parser's view of the note, with
/// text compared without its whitespace and code and math compared exactly,
/// is the same before and after (front matter and heading levels aside).
#[test]
fn only_whitespace_and_markup_change() {
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..CASES {
        let (note, o) = (make_note(&mut r), make_options(&mut r));
        let out = lint(&note, &o, &cx(true));
        let levels_change = o.heading_increment || o.heading_start > 0 || o.heading_style == HeadingStyle::Atx;
        assert_eq!(
            signature(&note, !levels_change),
            signature(&out.text, !levels_change),
            "case {case}\noptions {o:?}\nnote {note:?}\nout {:?}",
            out.text
        );
    }
}

/// Code, math and HTML come out byte for byte (list re-indenting aside,
/// which moves a whole item, code included, and is covered by the check
/// above, since code is compared exactly there).
#[test]
fn code_math_and_html_are_untouched() {
    let blocks = ["```\ncode   \n  # not heading\n* not list\n[[ a ]] *b*\n```", "$$\nx *  y_1   \n$$", "<div>\n* x   \n*y*\n</div>"];
    let mut r = Rng(0xCAFE_F00D_1234_5678);
    for case in 0..1500 {
        let mut note = String::new();
        let mut kept: Vec<&str> = Vec::new();
        for _ in 0..=r.below(6) {
            // An unclosed fence would swallow what follows it.
            let piece = loop {
                let p = *r.pick(PIECES);
                if !p.contains("unclosed") {
                    break p;
                }
            };
            note.push_str(piece);
            note.push_str("\n\n");
            if r.chance(50) {
                let b = *r.pick(&blocks);
                kept.push(b);
                note.push_str(b);
                note.push_str("\n\n");
            }
        }
        // Top-level blocks only: list re-indenting is checked above.
        let mut o = make_options(&mut r);
        o.list_indent = ListIndent::Keep;
        let out = lint(&note, &o, &cx(false));
        let text = out.text.replace("\r\n", "\n");
        for b in &kept {
            assert!(text.contains(b), "case {case}: {b:?} changed\nnote {note:?}\nout {text:?}");
        }
    }
}

#[test]
fn crlf_stays_crlf() {
    let mut r = Rng(0x0123_4567_89AB_CDEF);
    for case in 0..CASES {
        let note = make_note(&mut r).replace("\r\n", "\n").replace('\n', "\r\n");
        let mut o = make_options(&mut r);
        o.line_endings = LineEndings::Keep;
        if !note.contains("\r\n") {
            continue;
        }
        let out = lint(&note, &o, &cx(true));
        assert!(!out.text.replace("\r\n", "").contains('\n'), "case {case}: a bare LF appeared\nnote {note:?}\nout {:?}", out.text);
    }
}

#[test]
fn unreadable_front_matter_is_untouched() {
    let odd = "---\n: broken\n\t- x   \n---";
    let mut r = Rng(0xFEED_FACE_0BAD_BEEF);
    for case in 0..1000 {
        let mut note = format!("{odd}\n");
        for _ in 0..=r.below(6) {
            note.push_str(r.pick(PIECES));
            note.push('\n');
        }
        let mut o = make_options(&mut r);
        // Changing every line ending is the one thing asked for by name.
        o.line_endings = LineEndings::Keep;
        let out = lint(&note, &o, &cx(true));
        assert!(out.text.starts_with(&format!("{odd}\n")), "case {case}: front matter changed\n{:?}", out.text);
    }
}

#[test]
fn nothing_panics_on_junk() {
    let alphabet: Vec<char> = "*_#[]()`$<>-+|!\\\n\r\t \"'~=:.0123456789aZ é😀".chars().collect();
    let mut r = Rng(0xABCD_EF01_2345_6789);
    for _ in 0..3000 {
        let n = r.below(120);
        let s: String = (0..n).map(|_| *r.pick(&alphabet)).collect();
        let o = make_options(&mut r);
        let _ = lint(&s, &o, &cx(true));
    }
}

#[test]
fn tags_with_comments_flow_syntax_or_lookalikes_are_not_corrupted() {
    let o = only(|o| {
        o.tags_format = TagsFormat::List;
        o.tags_case = TagsCase::Lower;
    });
    for src in [
        "---\ntags: [a, b]  # c\n---\n",
        "---\ntags: a # c\n---\n",
        "---\ntags:\n  - A # first\n  - b\n---\n",
        "---\ntags: [A: b]\n---\n",
        "---\ntags: &t [A, b]\nother: *t\n---\n",
    ] {
        assert_eq!(run(src, &o), src, "{src:?}");
    }
    // A quoted `"true"` stays text; so do numbers, dates and words with a comma.
    let list = only(|o| o.tags_format = TagsFormat::List);
    assert_eq!(
        run("---\ntags: [\"true\", \"123\", \"2024-01-01\", \"a,b\", \"null\"]\n---\n", &list),
        "---\ntags:\n  - \"true\"\n  - \"123\"\n  - \"2024-01-01\"\n  - \"a,b\"\n  - \"null\"\n---\n"
    );
    let flow = only(|o| o.tags_format = TagsFormat::Flow);
    assert_eq!(run("---\ntags:\n  - \"a,b\"\n  - c\n---\n", &flow), "---\ntags: [\"a,b\", c]\n---\n");
    // A real tag list of booleans is not text.
    let src = "---\ntags: [true, x]\n---\n";
    assert_eq!(run(src, &list), src);
}

#[test]
fn front_matter_that_is_not_yaml_is_left_alone() {
    let src = "---\ntitle: Foo: bar\nb:   1   \n---\nbody  \n";
    let out = lint(src, &only(|o| o.front_matter = true), &Cx::default());
    assert_eq!(out.text, src);
    assert!(!out.notes.is_empty());
}

#[test]
fn sorting_never_moves_an_alias_before_its_anchor() {
    let o = only(|o| o.front_matter_sort = Sort::Alpha);
    let src = "---\nz: &x [1, 2]\na: *x\n---\n";
    assert_eq!(run(src, &o), src);
}

#[test]
fn a_dash_line_near_the_start_does_not_make_code_editable() {
    let src = "---\n\n---\n```md\n---\nfoo: bar\n---\n```\n";
    let out = lint(src, &Options::default(), &Cx::default());
    assert_eq!(out.text, src);
}

#[test]
fn emphasis_rule_leaves_wikilink_targets_alone() {
    let o = only(|o| o.emphasis = Emphasis::Underscore);
    let src = "see [[a *b* c]] and *d*\n";
    assert_eq!(run(src, &o), "see [[a *b* c]] and _d_\n");
}

// ---- front matter grammar: the YAML means the same after tidying

fn yaml_value(r: &mut Rng, anchors: &mut Vec<&'static str>) -> String {
    let plain = ["x", "Hello world", "2026-11-05", "42", "true", "null", "~", "a:b", "it's", "[[Note]]"];
    match r.below(12) {
        0 => (*r.pick(&plain)).to_string(),
        1 => "\"quoted: yes # not a comment\"".to_string(),
        2 => "'it''s'".to_string(),
        3 => "[a, b]  # trailing comment".to_string(),
        4 => "[a, \"b c\", 3]".to_string(),
        5 => "\n  - one # why\n  # between\n  - \"two\"\n  - 3".to_string(),
        6 => "\n  - [nested, list]\n  - k: v".to_string(),
        7 => "|\n  literal   \n  text".to_string(),
        8 => ">\n  folded\n  text".to_string(),
        9 if !anchors.is_empty() => format!("*{}", r.pick(anchors)),
        9 => "{a: 1, b: [2]}".to_string(),
        10 => {
            let name = *r.pick(&["p", "q"]);
            if anchors.contains(&name) {
                "plain".to_string()
            } else {
                anchors.push(name);
                format!("&{name} [1, 2]")
            }
        }
        _ => "\"true\"".to_string(),
    }
}

fn yaml_note(r: &mut Rng) -> String {
    let keys = ["zeta", "alpha", "title", "tags", "status", "up", "k b", "mid"];
    let mut anchors = Vec::new();
    let mut s = String::from("---\n");
    let eol = if r.chance(30) { "\r\n" } else { "\n" };
    let mut used: Vec<&str> = Vec::new();
    for _ in 0..=r.below(6) {
        let k = *r.pick(&keys);
        if used.contains(&k) {
            continue;
        }
        used.push(k);
        if r.chance(25) {
            s.push_str("# a comment  \n");
        }
        let v = if k == "tags" && r.chance(70) {
            (*r.pick(&["[A, b]", "[a, b]  # c", "a, B", "\n  - A\n  - \"true\"", "\n  - a # c", "[\"x,y\", Z]", "[true, a]", "\"123\""]))
                .to_string()
        } else {
            yaml_value(r, &mut anchors)
        };
        s.push_str(k);
        s.push(':');
        if !v.starts_with('\n') {
            s.push(' ');
        }
        s.push_str(&v);
        if r.chance(30) && !v.contains('\n') {
            s.push_str("   ");
        }
        s.push('\n');
    }
    s.push_str("---\n");
    let mut note = s;
    for _ in 0..=r.below(4) {
        note.push_str(r.pick(PIECES));
        note.push('\n');
    }
    if eol == "\r\n" { note.replace('\n', "\r\n") } else { note }
}

fn real_front(text: &str) -> Option<Vec<(String, yaml_rust2::Yaml)>> {
    let t = text.replace("\r\n", "\n");
    let rest = t.strip_prefix("---\n")?;
    let end = rest.find("\n---\n").map(|i| i + 1).or_else(|| rest.starts_with("---\n").then_some(0))?;
    let docs = yaml_rust2::YamlLoader::load_from_str(&rest[..end]).ok()?;
    match docs.first() {
        Some(yaml_rust2::Yaml::Hash(h)) => Some(h.iter().map(|(k, v)| (format!("{k:?}"), v.clone())).collect()),
        _ => Some(Vec::new()),
    }
}

fn flat_tags(v: &yaml_rust2::Yaml) -> Vec<String> {
    use yaml_rust2::Yaml;
    let mut out: Vec<String> = match v {
        Yaml::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(|x| format!("S:{}", x.trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join("-").to_lowercase()))
            .collect(),
        Yaml::Array(items) => items
            .iter()
            .map(|i| match i {
                Yaml::String(s) => {
                    format!("S:{}", s.trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join("-").to_lowercase())
                }
                other => format!("{other:?}"),
            })
            .collect(),
        other => vec![format!("{other:?}")],
    };
    out.sort();
    out.dedup();
    out
}

#[test]
fn yaml_means_the_same_after_tidying() {
    let mut r = Rng(0x5EED_1234_ABCD_9876);
    for case in 0..CASES {
        let note = yaml_note(&mut r);
        let o = make_options(&mut r);
        let out = lint(&note, &o, &cx(false));
        let two = lint(&out.text, &o, &cx(false));
        assert_eq!(two.text, out.text, "case {case}: not idempotent\noptions {o:?}\nnote {note:?}");
        let before = real_front(&note);
        let after = real_front(&out.text);
        if before.is_none() {
            // Not valid YAML to begin with: the front matter is left as it was.
            assert!(out.text.starts_with(note.split("---").take(2).collect::<Vec<_>>().join("---").as_str()) || out.text == note);
            continue;
        }
        let (Some(mut b), Some(mut a)) = (before, after) else {
            panic!("case {case}: YAML stopped parsing\noptions {o:?}\nnote {note:?}\nout {:?}", out.text)
        };
        let tags_rule = o.tags_format != TagsFormat::Keep || o.tags_case != TagsCase::Keep;
        let key = format!("{:?}", yaml_rust2::Yaml::String("tags".into()));
        let modified = format!("{:?}", yaml_rust2::Yaml::String(o.date_modified_key.clone()));
        let tags = |v: &[(String, yaml_rust2::Yaml)]| v.iter().find(|(k, _)| *k == key).map(|(_, y)| flat_tags(y));
        if tags_rule {
            assert_eq!(tags(&b), tags(&a), "case {case}: tags changed\noptions {o:?}\nnote {note:?}\nout {:?}", out.text);
            b.retain(|(k, _)| *k != key);
            a.retain(|(k, _)| *k != key);
        }
        if o.date_modified {
            b.retain(|(k, _)| *k != modified);
            a.retain(|(k, _)| *k != modified);
        }
        b.sort_by(|x, y| x.0.cmp(&y.0));
        a.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(b, a, "case {case}: YAML changed\noptions {o:?}\nnote {note:?}\nout {:?}", out.text);
    }
}

#[test]
fn a_date_format_cannot_inject_front_matter() {
    assert!(Options::date_format_ok("%Y-%m-%dT%H:%M"));
    assert!(!Options::date_format_ok("%Y\nevil: 1"));
    assert!(!Options::date_format_ok(":x"));
    assert!(!Options::date_format_ok("  "));
    let o = only(|o| o.date_modified = true);
    let out = lint("a\n", &o, &Cx { stamp: "2026\nevil: 1", edited: true });
    assert!(!out.text.contains("evil"), "{:?}", out.text);
    // A stamp with a colon and a space is quoted.
    let out = lint("a\n", &o, &Cx { stamp: "x: y", edited: true });
    assert!(out.text.contains("modified: \"x: y\""), "{:?}", out.text);
}
