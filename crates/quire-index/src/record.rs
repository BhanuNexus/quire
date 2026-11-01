// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeMap, HashMap};

use quire_markdown::{Extracted, Heading, Link, Stats, Task, Value, extract};
use quire_vault::NotePath;

use crate::tokenize::content_tokens;

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
    /// Content term frequencies (stopwords removed) for intelligence features.
    pub terms: HashMap<String, u32>,
    /// BM25 term weights and document length, precomputed off the index lock.
    pub(crate) search_terms: (Vec<(String, f32)>, f32),
}

impl NoteRecord {
    pub fn from_source(path: NotePath, src: &str) -> Self {
        let ex: Extracted = extract(src);
        let name = path.stem().to_string();
        let title = ex.title().map(str::to_string).unwrap_or_else(|| name.clone());
        let aliases = ex.front_matter.as_ref().map(|f| f.aliases()).unwrap_or_default();
        let mut terms: HashMap<String, u32> = HashMap::new();
        for t in content_tokens(&ex.plain).chain(content_tokens(&name)) {
            *terms.entry(t).or_default() += 1;
        }
        let search_terms = crate::fulltext::search_terms(&ex.plain, &name, &title, &aliases, &ex.tags);
        let properties = ex.front_matter.as_ref().map(|f| f.fields.clone()).unwrap_or_default();
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
            terms,
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
