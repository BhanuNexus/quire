// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeMap, HashMap};

use quire_markdown::{Extracted, Heading, Link, Stats, Task, Value, extract};
use quire_vault::NotePath;

use crate::terms::TermId;
use crate::tokenize::FastHash;

/// Everything derived from one note's source. Rebuildable at any time.
#[derive(Debug, Clone)]
pub struct NoteRecord {
    pub path: NotePath,
    /// File stem — the name wikilinks use.
    pub name: String,
    /// Front-matter title or first H1, falling back to the name.
    pub title: String,
    pub aliases: Vec<String>,
    pub headings: Vec<Heading>,
    pub links: Vec<Link>,
    pub tags: Vec<String>,
    pub tasks: Vec<Task>,
    pub stats: Stats,
    /// Front-matter fields, for the properties panel and views.
    pub properties: BTreeMap<String, Value>,
    /// Prose without markup, for snippets and summaries.
    pub plain: String,
    /// Content term frequencies (stopwords removed) for intelligence
    /// features, sorted by id. The ids are the keyword index's
    /// ([`crate::Index::term`] gives a term's text); empty until the record is
    /// in an index (`docs/design/0.14-index-startup.md` §3.2).
    pub terms: Vec<(TermId, u32)>,
    /// The content terms as text, from parsing until the index interns them.
    pub(crate) content: Vec<(String, u32)>,
    /// BM25 term weights and document length, precomputed off the index
    /// lock; emptied once the keyword index has them (`Index::insert_record`).
    pub(crate) search_terms: (Vec<(String, f32)>, f32),
}

impl NoteRecord {
    pub fn from_source(path: NotePath, src: &str) -> Self {
        let ex: Extracted = extract(src);
        let name = path.stem().to_string();
        let title = ex.title().map_or_else(|| name.clone(), str::to_string);
        let aliases = ex.front_matter.as_ref().map(quire_markdown::FrontMatter::aliases).unwrap_or_default();
        // Content terms and search terms in one pass over the words.
        let mut terms: HashMap<String, u32, FastHash> = HashMap::default();
        let search_terms = crate::fulltext::note_terms(&ex.plain, &name, &title, &aliases, &ex.tags, Some(&mut terms));
        let properties = ex.front_matter.as_ref().map(|f| f.fields.clone()).unwrap_or_default();
        let mut content: Vec<(String, u32)> = terms.into_iter().collect();
        content.sort_unstable();
        // In term order, not hash order: the same note gives the same record
        // on every build (the cache's golden test hashes it).
        let mut search_terms = search_terms;
        search_terms.0.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Self {
            properties,
            search_terms,
            path,
            name,
            title,
            aliases,
            headings: ex.headings,
            links: ex.links,
            tags: ex.tags,
            tasks: ex.tasks,
            stats: ex.stats,
            plain: ex.plain,
            terms: Vec::new(),
            content,
        }
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        let tag = tag.trim_start_matches('#');
        self.tags.iter().any(|t| {
            t.eq_ignore_ascii_case(tag)
                || (t.len() > tag.len() && t[..tag.len()].eq_ignore_ascii_case(tag) && t.as_bytes()[tag.len()] == b'/')
        })
    }
}
