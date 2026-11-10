// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Suggestions while typing: `[[` offers notes to link, `#` offers tags, and
//! `/` at the start of a word offers blocks to insert (headings, lists,
//! tables, views, templates…).
//!
//! The frontend asks [`Workspace::suggestions`] each frame and shows a small
//! list at the caret; [`Workspace::accept_suggestion`] makes the edit.

use quire_text::{EditKind, Selection, Transaction};
use quire_vault::{NotePath, TemplateVars, expand_template};

use crate::workspace::Workspace;

/// What was typed that opened the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuggestKind {
    /// `[[query` → a note to link.
    Link,
    /// `#query` → a tag.
    Tag,
    /// `/query` → a block to insert.
    Slash,
    /// `[@query` → a work in the bibliography (WS-G, `citations.rs`).
    Cite,
}

/// Where the trigger is: chars `start..end` (end is the caret) get replaced.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SuggestContext {
    pub kind: SuggestKind,
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestItem {
    pub label: String,
    /// Shown dimmed after the label (a folder, a count, an example).
    pub detail: String,
    /// What replaces the trigger. `{|}` marks where the caret goes (the end
    /// if absent).
    pub insert: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestions {
    pub context: SuggestContext,
    pub items: Vec<SuggestItem>,
}

const CARET: &str = "{|}";
const MAX_ITEMS: usize = 8;

/// The trigger right before the caret on this line, if any. `before` is the
/// line's text up to the caret; returns the kind, the char column where the
/// trigger starts, and the query typed after it.
pub fn detect(before: &str) -> Option<(SuggestKind, usize, String)> {
    let chars: Vec<char> = before.chars().collect();
    let n = chars.len();
    // [[query — no closing brackets, alias or heading part yet.
    if let Some(open) = before.rfind("[[") {
        let query = &before[open + 2..];
        if !query.contains(['[', ']', '|', '#']) && query.chars().count() <= 80 {
            return Some((SuggestKind::Link, before[..open].chars().count(), query.to_string()));
        }
    }
    // Walk back over the word being typed.
    let mut i = n;
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    let boundary = |at: usize| at == 0 || chars[at - 1].is_whitespace() || chars[at - 1] == '(';
    // #tag (not `##`, not inside a word like `C#`).
    if i > 0 && chars[i - 1] == '#' && boundary(i - 1) {
        let query: String = chars[i..].iter().collect();
        return Some((SuggestKind::Tag, i - 1, query));
    }
    // /command, optionally with spaces ("/heading 2"), at a word start.
    let mut j = n;
    while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == ' ') {
        j -= 1;
    }
    if j > 0 && chars[j - 1] == '/' && boundary(j - 1) {
        let query: String = chars[j..].iter().collect();
        if !query.starts_with(' ') && !query.contains("  ") && query.chars().count() <= 24 {
            return Some((SuggestKind::Slash, j - 1, query));
        }
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// How well `label` matches `query`: every query word must start a word in
/// the label, or appear in it. Lower is better.
fn rank(label: &str, query: &str) -> Option<usize> {
    let label = label.to_lowercase();
    let mut score = 0;
    for q in query.to_lowercase().split_whitespace() {
        if label.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(q)) {
            continue;
        }
        score += 1 + label.find(q)?;
    }
    Some(score)
}

/// What the slash menu inserts. `own_line` ones start on a line of their own.
struct SlashItem {
    label: &'static str,
    keywords: &'static str,
    insert: &'static str,
    /// What it looks like, shown beside the label.
    hint: &'static str,
    own_line: bool,
}

const SLASH_ITEMS: &[SlashItem] = &[
    SlashItem { label: "Heading 1", keywords: "h1 title", insert: "# ", hint: "#", own_line: true },
    SlashItem { label: "Heading 2", keywords: "h2 section", insert: "## ", hint: "##", own_line: true },
    SlashItem { label: "Heading 3", keywords: "h3", insert: "### ", hint: "###", own_line: true },
    SlashItem { label: "Bulleted list", keywords: "bullet unordered ul", insert: "- ", hint: "-", own_line: true },
    SlashItem { label: "Numbered list", keywords: "ordered ol", insert: "1. ", hint: "1.", own_line: true },
    SlashItem { label: "Checklist", keywords: "task todo checkbox", insert: "- [ ] ", hint: "- [ ]", own_line: true },
    SlashItem { label: "Quote", keywords: "blockquote", insert: "> ", hint: ">", own_line: true },
    SlashItem { label: "Code block", keywords: "fence snippet", insert: "```\n{|}\n```", hint: "```", own_line: true },
    SlashItem { label: "Table", keywords: "grid columns", insert: "| {|} |  |\n| --- | --- |\n|  |  |", hint: "2 × 2", own_line: true },
    SlashItem { label: "Divider", keywords: "rule hr line separator", insert: "---\n", hint: "---", own_line: true },
    SlashItem { label: "Link to note", keywords: "wikilink reference", insert: "[[{|}]]", hint: "[[ ]]", own_line: false },
    SlashItem { label: "Embed note", keywords: "transclude include", insert: "![[{|}]]", hint: "![[ ]]", own_line: false },
    SlashItem { label: "Tag", keywords: "label", insert: "#", hint: "#", own_line: false },
    SlashItem {
        label: "Board view",
        keywords: "kanban query projects",
        insert: "```quire-view\nfrom: \"#{|}\"\nlayout: kanban\ngroup: status\n```",
        hint: "quire-view",
        own_line: true,
    },
    SlashItem {
        label: "Table view",
        keywords: "query database properties",
        insert: "```quire-view\nfrom: \"#{|}\"\nlayout: table\nfields: [title, status, due, tags]\n```",
        hint: "quire-view",
        own_line: true,
    },
    SlashItem {
        label: "Calendar view",
        keywords: "month dates query",
        insert: "```quire-view\nfrom: \"#{|}\"\nlayout: calendar\ndate: due\n```",
        hint: "quire-view",
        own_line: true,
    },
];

impl Workspace {
    /// What to offer at the caret right now, if anything.
    pub fn suggestions(&self) -> Option<Suggestions> {
        let doc = self.active_doc()?;
        let sel = doc.selection();
        if doc.large || !sel.is_empty() {
            return None;
        }
        let buf = doc.buffer();
        let line = buf.char_to_line(sel.head);
        let line_start = buf.line_to_char(line);
        let before = buf.slice(line_start..sel.head);
        // WS-G: `[@` completes citation keys from the bibliography.
        if let Some((col, query, items)) = crate::citations::suggestions(self, &before)
            && !in_code(&doc.text(), line)
        {
            return Some(Suggestions {
                context: SuggestContext { kind: SuggestKind::Cite, start: line_start + col, end: sel.head, query },
                items,
            });
        }
        // 0.12: after `parent:: ` (a field that holds links), notes to link.
        if let Some((field, col, query)) = crate::links_state::field_value_query(&before)
            && self.is_link_field(&field)
            && !in_code(&doc.text(), line)
        {
            let items = self.link_items(&query);
            let context = SuggestContext { kind: SuggestKind::Link, start: line_start + col, end: sel.head, query };
            return (!items.is_empty()).then_some(Suggestions { context, items });
        }
        let (kind, col, query) = detect(&before)?;
        if in_code(&doc.text(), line) {
            return None;
        }
        let at_line_start = before.chars().take(col).all(char::is_whitespace);
        // A link the user already closed (or that came with "]]") is replaced whole.
        let after = buf.slice(sel.head..buf.line_range(line).end);
        let end = if kind == SuggestKind::Link && after.starts_with("]]") { sel.head + 2 } else { sel.head };
        let context = SuggestContext { kind, start: line_start + col, end, query };
        let items = match kind {
            // 0.12: `[[` at the start of a line also offers the vault's typed fields (`up:: [[…]]`).
            SuggestKind::Link if at_line_start && context.query.is_empty() => {
                let mut items = self.link_items("");
                items.extend(self.typed_field_items());
                items
            }
            SuggestKind::Link => self.link_items(&context.query),
            SuggestKind::Tag => self.tag_items(&context.query),
            SuggestKind::Slash => self.slash_items(&context.query, at_line_start),
            SuggestKind::Cite => Vec::new(),
        };
        (!items.is_empty()).then_some(Suggestions { context, items })
    }

    fn link_items(&self, query: &str) -> Vec<SuggestItem> {
        // With display titles on, a note whose title differs from its file
        // name is linked as `[[name|Title]]`, so the text reads as it shows.
        let titled = self.settings.editing.display_titles;
        let item = |p: &NotePath, title: &str| SuggestItem {
            label: if title.is_empty() { p.stem().to_string() } else { title.to_string() },
            detail: if title.is_empty() || title == p.stem() { p.folder().to_string() } else { p.without_ext().to_string() },
            insert: if titled && !title.trim().is_empty() && title.trim() != p.stem() && !title.contains([']', '|']) {
                format!("[[{}|{}]]", p.stem(), title.trim())
            } else {
                format!("[[{}]]", p.stem())
            },
        };
        let ix = self.index().read();
        let active = self.active_doc().and_then(|d| d.path.clone());
        let mut items: Vec<SuggestItem> = if query.trim().is_empty() {
            // Recent notes first, then any others.
            let mut paths = self.recent_notes(MAX_ITEMS);
            let more: Vec<NotePath> = ix.quick_open("", MAX_ITEMS * 2).into_iter().map(|h| h.path).collect();
            for p in more {
                if paths.len() < MAX_ITEMS && Some(&p) != active.as_ref() && !paths.contains(&p) {
                    paths.push(p);
                }
            }
            paths.iter().filter_map(|p| ix.get(p).map(|r| item(p, &r.title))).collect()
        } else {
            ix.quick_open(query, MAX_ITEMS + 1)
                .into_iter()
                .filter(|h| Some(&h.path) != active.as_ref())
                .take(MAX_ITEMS)
                .map(|h| item(&h.path, &h.title))
                .collect()
        };
        let q = query.trim();
        let exists = ix.records().any(|r| r.path.stem().eq_ignore_ascii_case(q) || r.title.eq_ignore_ascii_case(q));
        if !q.is_empty() && !exists {
            items.push(SuggestItem {
                label: format!("New note “{q}”"),
                detail: "link now, create on click".into(),
                insert: format!("[[{q}]]"),
            });
        }
        items
    }

    /// `up:: [[…]]` for the typed fields this vault uses, most used first.
    fn typed_field_items(&self) -> Vec<SuggestItem> {
        let ix = self.index().read();
        ix.relations()
            .fields()
            .into_iter()
            .take(4)
            .map(|(f, n)| SuggestItem {
                label: format!("{f}:: [[…]]"),
                detail: format!("typed link · {n} in this vault"),
                insert: format!("{f}:: [[{CARET}]]"),
            })
            .collect()
    }

    fn tag_items(&self, query: &str) -> Vec<SuggestItem> {
        let q = query.to_lowercase();
        let mut tags: Vec<(usize, String, usize)> = self
            .index()
            .read()
            .tags()
            .into_iter()
            .filter(|(t, _)| *t != q)
            .filter_map(|(t, n)| {
                let at = if q.is_empty() { Some(0) } else { t.find(&q) }?;
                // Prefix matches first, then the most used.
                Some((usize::from(at > 0), t, n))
            })
            .collect();
        tags.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)).then(a.1.cmp(&b.1)));
        tags.into_iter()
            .take(MAX_ITEMS)
            .map(|(_, t, n)| SuggestItem {
                detail: if n == 1 { "1 note".into() } else { format!("{n} notes") },
                insert: format!("#{t} "),
                label: format!("#{t}"),
            })
            .collect()
    }

    fn slash_items(&self, query: &str, at_line_start: bool) -> Vec<SuggestItem> {
        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        let date = if self.settings.dates_as_links { format!("[[{today}]]") } else { today };
        let mut out: Vec<(usize, SuggestItem)> = Vec::new();
        let mut offer = |label: String, keywords: &str, detail: String, insert: String| {
            let r = rank(&format!("{label} {keywords}"), query);
            if let Some(r) = r {
                out.push((r, SuggestItem { label, detail, insert }));
            }
        };
        for b in SLASH_ITEMS {
            let insert = if b.own_line && !at_line_start { format!("\n{}", b.insert) } else { b.insert.to_string() };
            offer(b.label.to_string(), b.keywords, b.hint.to_string(), insert);
        }
        offer("Today's date".into(), "date now day", date.clone(), date);
        offer("Current time".into(), "time now clock", now.format("%H:%M").to_string(), now.format("%H:%M").to_string());
        // The vault's templates, expanded in place.
        for t in self.templates() {
            let Some(text) = self.vault().and_then(|v| v.read(&t).ok()) else { continue };
            let title = self.active_doc().map(crate::document::Document::title).unwrap_or_default();
            let (body, cursor) = expand_template(&text, &TemplateVars { title, now });
            let insert = match cursor {
                Some(c) => {
                    let at = body.char_indices().nth(c).map_or(body.len(), |(b, _)| b);
                    format!("{}{CARET}{}", &body[..at], &body[at..])
                }
                None => body,
            };
            let insert = if at_line_start { insert } else { format!("\n{insert}") };
            offer(format!("Template: {}", t.stem()), "template insert", t.folder().to_string(), insert);
        }
        out.sort_by_key(|(r, _)| *r);
        out.into_iter().map(|(_, i)| i).take(MAX_ITEMS + 4).collect()
    }

    /// Replace the trigger with item `i`. Returns false if the text moved on.
    pub fn accept_suggestion(&mut self, s: &Suggestions, i: usize) -> bool {
        let Some(item) = s.items.get(i) else { return false };
        let still_there = self.suggestions().is_some_and(|now| now.context == s.context);
        let Some(doc) = self.active_doc_mut() else { return false };
        if !still_there {
            return false;
        }
        let (text, caret) = match item.insert.find(CARET) {
            Some(b) => (item.insert.replacen(CARET, "", 1), item.insert[..b].chars().count()),
            None => (item.insert.clone(), item.insert.chars().count()),
        };
        let at = s.context.start + caret;
        let tx = Transaction::replace(s.context.start, s.context.end, text).with_selection(Selection::cursor(at));
        doc.apply(tx, EditKind::Other).is_ok()
    }
}

/// Whether `line` is inside a fenced code block.
fn in_code(text: &str, line: usize) -> bool {
    let mut open = false;
    for l in text.lines().take(line) {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            open = !open;
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use quire_vault::Vault;

    #[test]
    fn triggers() {
        use SuggestKind::{Link, Slash, Tag};
        assert_eq!(detect("See [[Rop"), Some((Link, 4, "Rop".into())));
        assert_eq!(detect("See [[Rope]] and"), None);
        assert_eq!(detect("[[Rope|alias"), None);
        assert_eq!(detect("tagged #pro"), Some((Tag, 7, "pro".into())));
        assert_eq!(detect("#"), Some((Tag, 0, String::new())));
        assert_eq!(detect("## Head"), None);
        assert_eq!(detect("C#"), None);
        assert_eq!(detect("/"), Some((Slash, 0, String::new())));
        assert_eq!(detect("- /head"), Some((Slash, 2, "head".into())));
        assert_eq!(detect("/heading 2"), Some((Slash, 0, "heading 2".into())));
        assert_eq!(detect("and/or"), None);
        assert_eq!(detect("https://x"), None);
        assert_eq!(detect("/ x"), None);
    }

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(files));
        w.index().flush();
        w
    }

    fn type_at_end(w: &mut Workspace, text: &str) {
        let d = w.active_doc_mut().unwrap();
        let end = d.buffer().len_chars();
        d.set_selection(Selection::cursor(end));
        w.insert_text(text);
    }

    #[test]
    fn typed_fields_complete_notes_and_are_offered_after_brackets() {
        let mut w = ws(&[("Rope.md", "# Rope\n"), ("A.md", "up:: [[Rope]]\n"), ("B.md", "# B\n")]);
        w.open_note(&NotePath::new("B.md"));
        type_at_end(&mut w, "parent:: Ro");
        let s = w.suggestions().expect("notes after parent::");
        assert_eq!(s.context.kind, SuggestKind::Link);
        assert_eq!(s.items[0].insert, "[[Rope]]");
        assert!(w.accept_suggestion(&s, 0));
        assert!(w.active_doc().unwrap().text().ends_with("parent:: [[Rope]]"));
        // A field that holds no links in this vault offers nothing.
        type_at_end(&mut w, "\nstatus:: dr");
        assert!(w.suggestions().is_none());
        type_at_end(&mut w, "\n[[");
        let s = w.suggestions().expect("links and typed fields");
        let up = s.items.iter().position(|i| i.label == "up:: [[…]]").expect("the vault's typed field");
        assert!(w.accept_suggestion(&s, up));
        let d = w.active_doc().unwrap();
        assert!(d.text().ends_with("\nup:: [[]]"));
        assert_eq!(d.selection().head, d.buffer().len_chars() - 2, "caret inside the brackets");
    }

    #[test]
    fn links_complete_to_note_names_and_offer_new_ones() {
        let mut w = ws(&[("Rope Notes.md", "# Rope Notes\n"), ("Projects/Ropeway.md", "# Ropeway\n"), ("Today.md", "# Today\n")]);
        w.open_note(&NotePath::new("Today.md"));
        type_at_end(&mut w, "See [[rope");
        let s = w.suggestions().unwrap();
        assert_eq!(s.context.kind, SuggestKind::Link);
        assert_eq!(s.items[0].insert, "[[Rope Notes]]");
        assert!(s.items.iter().any(|i| i.insert == "[[Ropeway]]" && i.detail.contains("Projects")));
        assert!(s.items.last().unwrap().label.starts_with("New note"));
        assert!(w.accept_suggestion(&s, 0));
        assert_eq!(w.active_doc().unwrap().text().as_ref(), "# Today\nSee [[Rope Notes]]");
        assert!(w.suggestions().is_none());
        // Undo brings back what was typed.
        w.active_doc_mut().unwrap().undo();
        assert!(w.active_doc().unwrap().text().ends_with("[[rope"));
    }

    #[test]
    fn slash_inserts_blocks_and_puts_the_caret_inside() {
        let mut w = ws(&[("N.md", "# N\n"), ("Other.md", "# Other\n")]);
        w.open_note(&NotePath::new("N.md"));
        type_at_end(&mut w, "/code");
        let s = w.suggestions().unwrap();
        assert_eq!(s.items[0].label, "Code block");
        w.accept_suggestion(&s, 0);
        let d = w.active_doc().unwrap();
        assert_eq!(d.text().as_ref(), "# N\n```\n\n```");
        assert_eq!(d.selection().head, "# N\n```\n".chars().count());

        // Mid-line, a block starts on a new line; "/link" then offers notes.
        type_at_end(&mut w, "\ntext /check");
        let s = w.suggestions().unwrap();
        assert_eq!(s.items[0].label, "Checklist");
        w.accept_suggestion(&s, 0);
        assert!(w.active_doc().unwrap().text().ends_with("text \n- [ ] "));
        type_at_end(&mut w, "/link");
        let s = w.suggestions().unwrap();
        w.accept_suggestion(&s, 0);
        assert_eq!(w.suggestions().map(|s| s.context.kind), Some(SuggestKind::Link));
    }

    #[test]
    fn tags_complete_from_the_vault_and_not_in_code() {
        let mut w = ws(&[("A.md", "#project #proposal\n"), ("B.md", "#project\n"), ("N.md", "x\n")]);
        w.open_note(&NotePath::new("N.md"));
        type_at_end(&mut w, "#pro");
        let s = w.suggestions().unwrap();
        assert_eq!(s.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["#project", "#proposal"]);
        assert_eq!(s.items[0].detail, "2 notes");
        w.accept_suggestion(&s, 0);
        assert!(w.active_doc().unwrap().text().ends_with("#project "));
        type_at_end(&mut w, "\n```\n#pro");
        assert!(w.suggestions().is_none(), "no suggestions inside code blocks");
    }
}
