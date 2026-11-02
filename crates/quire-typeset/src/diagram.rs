// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Mermaid -> SVG with `mermaid-rs-renderer` (pure Rust).

use std::panic::{AssertUnwindSafe, catch_unwind};

use mermaid_rs_renderer::{RenderOptions, Theme, render_strict};

use crate::TypesetError;
use crate::cache::{Cache, hash_of};

/// Longest diagram source we try to lay out.
const MAX_SOURCE_LEN: usize = 100_000;

static DIAGRAM_CACHE: Cache<(String, bool), Result<String, TypesetError>> = Cache::new();

/// Renders a Mermaid diagram to SVG. `dark` picks Mermaid's dark palette
/// (keeping the Inter-first font stack of the light theme). Results,
/// including errors, are cached.
pub fn diagram_svg(source: &str, dark: bool) -> Result<String, TypesetError> {
    let hash = hash_of(&(source, dark));
    if let Some(hit) = DIAGRAM_CACHE.get(hash, |k| k.0 == source && k.1 == dark) {
        return hit;
    }
    let result = render(source, dark);
    DIAGRAM_CACHE.insert(hash, (source.to_owned(), dark), result.clone());
    result
}

fn render(source: &str, dark: bool) -> Result<String, TypesetError> {
    if source.trim().is_empty() {
        return Err(TypesetError::Diagram("the diagram is empty".into()));
    }
    if source.len() > MAX_SOURCE_LEN {
        return Err(TypesetError::Diagram("the diagram is too large".into()));
    }
    let mut options = RenderOptions::modern();
    if dark {
        let font_family = options.theme.font_family.clone();
        let font_size = options.theme.font_size;
        options.theme = Theme { font_family, font_size, ..Theme::dark() };
    }
    match catch_unwind(AssertUnwindSafe(|| render_strict(source, options))) {
        Ok(Ok(svg)) => Ok(svg),
        Ok(Err(e)) => Err(TypesetError::Diagram(short(&e.to_string()))),
        Err(_) => Err(TypesetError::Diagram("the renderer could not handle this diagram".into())),
    }
}

/// The first line of an error message, capped in length.
fn short(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message).trim();
    if line.chars().count() > 160 {
        let cut: String = line.chars().take(157).collect();
        format!("{cut}...")
    } else {
        line.to_owned()
    }
}
