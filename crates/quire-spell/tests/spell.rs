// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Behaviour tests for the Markdown-aware spell checker.

use std::time::{Duration, Instant};

use quire_spell::{LineContext, SpellError, Speller, UserDictionary};

/// Misspelled words in a single line, as text.
fn bad(line: &str) -> Vec<&str> {
    let mut ctx = LineContext::new();
    // Start past line 0 so a leading `---` is not front matter.
    ctx.advance("");
    Speller::english().misspellings(line, &mut ctx).into_iter().map(|r| &line[r]).collect()
}

/// Misspelled words in a document, as text.
fn bad_doc(doc: &str) -> Vec<&str> {
    let lines: Vec<&str> = doc.split('\n').collect();
    Speller::english().check_document(doc).into_iter().map(|(n, r)| &lines[n][r]).collect()
}

// ------------------------------------------------------------ dictionary

#[test]
fn correct_words_pass() {
    let s = Speller::english();
    for w in ["the", "receive", "definitely", "Necessary", "running", "colors", "it's", "don't", "I'm", "you’re", "wouldn’t"] {
        assert!(s.check(w), "{w} should be correct");
    }
    assert!(bad("The quick brown fox jumps over the lazy dog, doesn't it?").is_empty());
}

#[test]
fn common_misspellings_flagged_with_suggestions() {
    let s = Speller::english();
    for (wrong, right) in
        [("recieve", "receive"), ("teh", "the"), ("definately", "definitely"), ("seperate", "separate"), ("thier", "their")]
    {
        assert!(!s.check(wrong), "{wrong} should be flagged");
        let sugg = s.suggest(wrong, 5);
        assert_eq!(sugg.first().map(String::as_str), Some(right), "suggestions for {wrong}: {sugg:?}");
    }
    assert_eq!(s.suggest("Teh", 3)[0], "The", "capitalization is kept");
    assert_eq!(s.suggest("dont", 3)[0], "don't");
    assert_eq!(s.suggest("dont\u{2019}", 0), Vec::<String>::new());
    assert!(s.suggest("teh", 2).len() <= 2);
    assert_eq!(bad("I will recieve teh package definately."), ["recieve", "teh", "definately"]);
}

#[test]
fn british_dictionary() {
    let gb = Speller::load("en-GB").unwrap();
    assert_eq!(gb.lang(), "en_GB");
    assert!(gb.check("colour") && gb.check("organise"));
    let us = Speller::load("en").unwrap();
    assert!(!us.check("colour") && us.check("color"));
}

#[test]
fn unknown_language() {
    assert!(matches!(Speller::load("xx_NOPE"), Err(SpellError::UnknownLanguage(_))));
}

#[test]
fn speller_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Speller>();
    let s = std::sync::Arc::new(Speller::load("en_US").unwrap());
    let t = std::thread::spawn({
        let s = std::sync::Arc::clone(&s);
        move || s.check("thread")
    });
    assert!(t.join().unwrap());
}

// ------------------------------------------------------------ inline skip rules

#[test]
fn skips_inline_code() {
    assert_eq!(bad("Run `cargo biuld` or ``a `nested` codde`` now wrng"), ["wrng"]);
}

#[test]
fn skips_inline_math() {
    assert_eq!(bad("Let $\\alpha + xyzq$ be $$\\frac{ab}{cd}$$ fine"), Vec::<&str>::new());
    // Prices are not math.
    assert_eq!(bad("It costs $5 and $10 for teh lot"), ["teh"]);
}

#[test]
fn skips_urls_and_domains() {
    assert!(bad("Visit https://exmaple.com/pathh?qury=1 or www.foobarr.org today").is_empty());
    assert!(bad("Mirror at ftp://fles.example.net and gooogle.com/serch.").is_empty());
}

#[test]
fn skips_email_addresses() {
    assert!(bad("Mail jhon.doe@exmaple.com or <foo@barr.io>").is_empty());
}

#[test]
fn wikilink_target_skipped_alias_checked() {
    assert!(bad("See [[Projcet Notse]] and ![[diagrm.png]]").is_empty());
    assert_eq!(bad("See [[Projcet Notse|my projetc]]"), ["projetc"]);
    assert_eq!(bad("See [[Projcet#Hedding|the notse]]"), ["notse"]);
}

#[test]
fn markdown_links_check_text_not_url() {
    assert_eq!(bad("Read [the manul](https://exmaple.com/docz) and ![a diagrm](imgs/pictre.png)"), ["manul", "diagrm"]);
    assert_eq!(bad("Ref [the docs][refrence-labl] here"), Vec::<&str>::new());
    assert_eq!(bad("Odd [text](unclosed/pathh more"), Vec::<&str>::new());
}

#[test]
fn skips_html_tags_and_entities() {
    assert!(bad("<span class=\"hihglight\">fine</span> &nbsp; &mdash; <br/> <!-- a commnt -->").is_empty());
    // Text between tags is still prose.
    assert_eq!(bad("<b>wrng</b>"), ["wrng"]);
}

#[test]
fn skips_tags() {
    assert!(bad("Filed under #projcet/subtpic and #wrk-in-progess").is_empty());
    assert_eq!(bad("# Heding"), ["Heding"], "a heading is not a tag");
}

#[test]
fn skips_mentions() {
    assert!(bad("Thanks @jhondoe and @mary_smth!").is_empty());
}

#[test]
fn skips_words_with_digits() {
    assert!(bad("Use abc123 and 2nd and x86ish and v2beta").is_empty());
}

#[test]
fn skips_all_caps() {
    assert!(bad("The NASAA and WHTEVER APIs").is_empty());
}

#[test]
fn skips_camel_and_pascal_case() {
    assert!(bad("Call getUsrName on MyWidgt from iPhonne").is_empty());
}

#[test]
fn skips_file_names() {
    assert!(bad("Edit main.rs, READMEE.md and confg.toml in ./srcc/libb or /usr/shre/hunspel").is_empty());
}

#[test]
fn skips_snake_case() {
    assert!(bad("Set max_lne_lenght to ten").is_empty());
}

#[test]
fn skips_footnote_refs() {
    assert_eq!(bad("A claim[^1] and anothr[^notte-two]."), ["anothr"]);
    assert_eq!(bad("[^notte]: The sourse."), ["sourse"]);
    assert_eq!(bad("[^not a footnoet]"), ["footnoet"]);
}

#[test]
fn skips_block_ids() {
    assert!(bad("A paragraph ^abc123").is_empty());
    assert!(bad("A paragraph ^blokid").is_empty());
}

#[test]
fn skips_callout_types() {
    assert_eq!(bad("> [!infoo]- A titel"), ["titel"]);
    assert!(bad("> [!note] Notes").is_empty());
    assert_eq!(bad("Not a box: [!wrng words]"), ["wrng"]);
}

#[test]
fn skips_escapes_and_latex_commands() {
    assert!(bad("Literal \\*stars\\* and \\textbf outside math").is_empty());
}

#[test]
fn skips_single_letters_and_task_boxes() {
    assert!(bad("- [x] done and - [ ] next, plan b").is_empty());
}

// ------------------------------------------------------------ words

#[test]
fn apostrophes() {
    assert!(bad("Don't panic, it's the users' choice and the dog’s bowl.").is_empty());
    assert_eq!(bad("It'ss wrng"), ["It'ss", "wrng"]);
    // Curly apostrophes give curly suggestions.
    let s = Speller::english();
    assert!(s.suggest("dont\u{2019}", 3).is_empty() || !s.check("dont\u{2019}"));
    assert!(s.suggest("wouldn\u{2019}tt", 3).iter().any(|w| w == "wouldn\u{2019}t"));
}

#[test]
fn hyphenated_words_check_each_part() {
    assert!(bad("A well-known, state-of-the-art tool").is_empty());
    assert_eq!(bad("A well-knwon tool"), ["knwon"]);
}

#[test]
fn accented_words() {
    // SCOWL's English list strips accents, so accented spellings are folded.
    assert!(bad("A naïve café résumé with a façade, also cafe\u{301}.").is_empty());
    assert_eq!(bad("A naïvee café"), ["naïvee"]);
}

#[test]
fn other_scripts_are_not_flagged() {
    assert!(bad("नमस्ते दुनिया and Привет мир and 你好世界 and Ελληνικά").is_empty());
    assert_eq!(bad("Mixed नमस्ते wrng"), ["wrng"]);
}

// ------------------------------------------------------------ block state

#[test]
fn fenced_code_across_lines() {
    let doc = "Intro tekst\n```rust\nlet wrng = recieve();\n```\nAfter fense\n~~~\n```\nstil code\n~~~\n> ```\n> quoted codde\n> ```\nend";
    assert_eq!(bad_doc(doc), ["tekst", "fense"]);
    // A fence only closes with the same character and at least the same length.
    assert_eq!(bad_doc("````\n```\nwrng\n````\nwrng"), ["wrng"]);
}

#[test]
fn front_matter_only_at_top() {
    let doc = "---\ntitel: Somthing\ntags: [wrk]\n---\nBody wrng\n\n---\nnot frnt matter\n";
    assert_eq!(bad_doc(doc), ["wrng", "frnt"]);
    // With a byte-order mark too.
    assert_eq!(bad_doc("\u{feff}---\nkee: vaule\n---\nfine"), Vec::<&str>::new());
}

#[test]
fn math_blocks_across_lines() {
    let doc = "Befor\n$$\n\\mathrm{abcd} = xyzq\n$$\nAftr\n$$ \\inlinee $$\nend";
    assert_eq!(bad_doc(doc), ["Befor", "Aftr"]);
}

#[test]
fn html_comments_across_lines() {
    assert_eq!(bad_doc("<!--\nhiden notte\n-->\nshwn"), ["shwn"]);
}

#[test]
fn indented_code_blocks() {
    let doc = "Paragrph text\n    continuation lne\n\n    let indnted = code;\n\n- list itme\n    nested contnt\n";
    assert_eq!(bad_doc(doc), ["Paragrph", "lne", "itme", "contnt"]);
}

#[test]
fn context_is_reusable_per_line() {
    let s = Speller::english();
    let mut ctx = LineContext::new();
    assert!(s.misspellings("```", &mut ctx).is_empty());
    assert!(ctx.in_block());
    let saved = ctx;
    assert!(s.misspellings("wrng", &mut ctx).is_empty());
    let mut again = saved;
    assert!(s.misspellings("wrng", &mut again).is_empty(), "cached result honours the context");
    assert_eq!(again, ctx);
    s.misspellings("```", &mut ctx);
    assert_eq!(s.misspellings("wrng", &mut ctx), vec![std::ops::Range { start: 0, end: 4 }]);
}

#[test]
fn crlf_lines() {
    assert_eq!(bad_doc("```\r\nwrng\r\n```\r\nwrng\r\n"), ["wrng"]);
}

// ------------------------------------------------------------ personal lists

#[test]
fn user_dictionary_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("words.txt");
    let mut d = UserDictionary::load(&path).unwrap();
    assert!(d.is_empty());
    assert!(d.add("zebrafish"));
    assert!(d.add("Quire"));
    assert!(d.add("anthropo\u{2019}s"));
    d.save().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Quire\nanthropo's\nzebrafish\n");

    let mut loaded = UserDictionary::load(&path).unwrap();
    assert_eq!(loaded, d);
    assert!(loaded.contains("Quire") && !loaded.contains("quire"));
    assert!(loaded.remove("zebrafish"));
    assert!(!loaded.remove("zebrafish"));
    loaded.save().unwrap();
    let reloaded = UserDictionary::load(&path).unwrap();
    assert_eq!(reloaded.words().collect::<Vec<_>>(), ["Quire", "anthropo's"]);
    // No temp files left behind by the atomic write.
    assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn speller_consults_personal_lists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("words.txt");
    std::fs::write(&path, "# my words\nquirk\nflurbish\n\n").unwrap();
    let s = Speller::load("en_US").unwrap();
    s.set_user_dictionary(UserDictionary::load(&path).unwrap());

    let mut ctx = LineContext::new();
    let line = "Flurbish the zorblax and FLURBISH's";
    let flagged: Vec<&str> = s.misspellings(line, &mut ctx).into_iter().map(|r| &line[r]).collect();
    assert_eq!(flagged, ["zorblax"], "user words match case-insensitively");

    assert!(s.add_user_word("zorblax").unwrap());
    assert!(!s.add_user_word("zorblax").unwrap());
    let mut ctx = LineContext::new();
    assert!(s.misspellings(line, &mut ctx).is_empty(), "cache is invalidated by word-list changes");
    assert!(std::fs::read_to_string(&path).unwrap().contains("zorblax\n"), "saved on add");
    assert_eq!(s.suggest("zorblx", 3)[0], "zorblax", "user words are suggested");

    assert!(s.remove_user_word("zorblax").unwrap());
    assert!(!s.check("zorblax"));
    assert!(!s.user_dictionary().contains("zorblax"));

    s.ignore("Snorkack");
    assert!(s.check("snorkack") && s.check("SNORKACK's"));
    s.clear_ignored();
    assert!(!s.check("snorkack"));
}

// ------------------------------------------------------------ system dictionaries

#[test]
fn system_dictionaries_never_fail() {
    for d in quire_spell::system_dictionaries() {
        assert!(!d.lang.is_empty());
    }
    let all = quire_spell::available_dictionaries();
    assert_eq!(all[0].lang, "en_US");
    assert_eq!(all[1].lang, "en_GB");
}

#[test]
fn loads_dictionary_files_including_latin1() {
    let dir = tempfile::tempdir().unwrap();
    let aff = dir.path().join("xx_TEST.aff");
    let dic = dir.path().join("xx_TEST.dic");
    std::fs::write(&aff, b"SET ISO8859-1\nTRY esianrtolcdugmphbyfvkwz\n\nSFX S Y 1\nSFX S 0 s .\n").unwrap();
    std::fs::write(&dic, b"2\ncaf\xe9/S\nbanana/S\n").unwrap();
    let s = Speller::from_files(&aff, &dic).unwrap();
    assert_eq!(s.lang(), "xx_TEST");
    assert!(s.check("café") && s.check("cafés") && s.check("bananas"));
    assert!(!s.check("bananna"));
    assert!(matches!(Speller::from_files(dir.path().join("nope.aff"), &dic), Err(SpellError::Io { .. })));
}

// ------------------------------------------------------------ robustness

#[test]
fn ranges_are_valid_for_odd_input() {
    let s = Speller::load("en_US").unwrap();
    let pieces = [
        "`",
        "``",
        "$",
        "$$",
        "\\",
        "[",
        "]",
        "[[",
        "]]",
        "](",
        "(",
        ")",
        "<",
        ">",
        "<!--",
        "-->",
        "&",
        "&#",
        ";",
        "#",
        "@",
        "^",
        "|",
        "'",
        "’",
        "-",
        "_",
        "*",
        "~",
        ".",
        "/",
        ":",
        "://",
        "www.",
        "a",
        "wrng",
        "é",
        "e\u{301}",
        "न",
        "्",
        "你",
        "🦀",
        "\u{200d}",
        "\t",
        " ",
        "\r",
        "---",
        "```",
        "~~~",
        "    ",
        "> ",
        "- ",
        "1. ",
        "Ünïcödé",
        "x@y.z",
        "[^",
        "[!",
        "\u{feff}",
        "\0",
    ];
    // Deterministic pseudo-random lines (xorshift), plus every pair.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut lines = Vec::new();
    for a in pieces {
        for b in pieces {
            lines.push(format!("{a}{b}wrd{a}"));
        }
    }
    for _ in 0..3000 {
        let mut line = String::new();
        for _ in 0..(seed % 12) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            line.push_str(pieces[(seed % pieces.len() as u64) as usize]);
        }
        lines.push(line);
    }
    let mut ctx = LineContext::new();
    for line in &lines {
        for r in s.misspellings(line, &mut ctx) {
            assert!(r.start < r.end && r.end <= line.len(), "{r:?} in {line:?}");
            assert!(line.is_char_boundary(r.start) && line.is_char_boundary(r.end), "{r:?} in {line:?}");
        }
    }
    let doc = lines.join("\n");
    for (n, r) in s.check_document(&doc) {
        let line = doc.split('\n').nth(n).unwrap();
        assert!(line.get(r).is_some());
    }
}

// ------------------------------------------------------------ performance

const PROSE: [&str; 10] = [
    "Quire keeps your notes as plain Markdown files in a folder you choose, so they stay yours.",
    "Links between notes are written as [[wikilinks]] and show up in the linked mentions panel automatically.",
    "When you rename a note, every link that points to it is updated across the whole vault.",
    "The editor supports **bold**, _italic_, `inline code`, tables, task lists and footnotes[^1].",
    "Search is instant because the index is rebuilt incrementally whenever a file changes on disk.",
    "You can sync a vault between devices through any folder, such as a network share or a USB drive.",
    "- [ ] Write the quarterly report and send it to the team before the meeting on Thursday.",
    "> Good writing is clear thinking made visible, and a good tool should get out of the way.",
    "See https://example.com/docs for details, or email support@example.com if something breaks.",
    "Each recieve operation is idempotent, which makes retries safe even when the network is unreliable.",
];

/// Release builds must meet the budget; debug builds (CI's `cargo test`)
/// only get a sanity bound.
fn budget(release: Duration) -> Duration {
    if cfg!(debug_assertions) { release * 15 } else { release }
}

#[test]
fn dictionary_load_is_fast() {
    let start = Instant::now();
    let s = Speller::load("en_US").unwrap();
    let elapsed = start.elapsed();
    println!("en_US load: {elapsed:?}");
    assert!(s.check("hello"));
    assert!(elapsed < budget(Duration::from_millis(300)), "load took {elapsed:?}");
}

#[test]
fn checking_200_lines_is_fast() {
    let s = Speller::load("en_US").unwrap();
    // Distinct lines so the cache does not help.
    let doc: Vec<String> = (0..200).map(|i| format!("{} {i}", PROSE[i % PROSE.len()])).collect();
    let start = Instant::now();
    let mut ctx = LineContext::new();
    let mut flagged = 0;
    for line in &doc {
        flagged += s.misspellings(line, &mut ctx).len();
    }
    let cold = start.elapsed();
    let start = Instant::now();
    let mut ctx = LineContext::new();
    for line in &doc {
        s.misspellings(line, &mut ctx);
    }
    let warm = start.elapsed();
    println!("200 lines: {cold:?} uncached, {warm:?} cached; {flagged} flagged");
    assert_eq!(flagged, 20, "only the 'recieve' lines are flagged");
    assert!(cold < budget(Duration::from_millis(20)), "checking took {cold:?}");
}

#[test]
fn suggestions_are_fast() {
    let s = Speller::load("en_US").unwrap();
    let mut worst = Duration::ZERO;
    for w in ["recieve", "teh", "definately", "seperate", "accomodate", "wierd", "untill", "becuase", "langauge", "managment"] {
        let start = Instant::now();
        assert!(!s.suggest(w, 5).is_empty());
        worst = worst.max(start.elapsed());
    }
    println!("slowest typical suggestion: {worst:?}");
    assert!(worst < budget(Duration::from_millis(20)), "suggest took {worst:?}");
}
