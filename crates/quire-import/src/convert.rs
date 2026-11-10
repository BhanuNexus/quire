// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small conversions the importers share: times, tag and page names, and
//! rewriting text outside code.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use quire_vault::sanitize_title;

/// `2026-11-01T12:00:00Z` for unix seconds.
pub(crate) fn iso(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0).single().map_or_else(String::new, |d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Unix seconds of an ISO 8601 / RFC 3339 time (`2020-01-02T03:04:05.678Z`) or a plain date.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp());
    }
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0).map(|d| d.and_utc().timestamp())
}

/// A tag Quire can use: no `#`, no spaces.
pub(crate) fn tag_name(t: &str) -> String {
    let t: String = t
        .trim()
        .trim_start_matches('#')
        .chars()
        .map(|c| if c.is_whitespace() || matches!(c, ',' | '[' | ']' | '#' | '|' | '"' | '\'') { '-' } else { c })
        .collect();
    t.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

/// A page's name as a file path: `a/b` is a namespace (folders), every part
/// a valid file name. At most 8 parts; empty parts go. `None` if nothing is left.
pub(crate) fn page_path(title: &str) -> Option<String> {
    let parts: Vec<String> = title.split('/').map(sanitize_title).filter(|p| !p.is_empty()).take(8).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Apply `f` to the parts of `s` that aren't code (`` `…` `` spans and
/// fenced blocks); code is copied as it is.
pub(crate) fn outside_code(s: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let Some(i) = rest.find('`') else {
            out.push_str(&f(rest));
            break;
        };
        out.push_str(&f(&rest[..i]));
        let run = rest[i..].bytes().take_while(|b| *b == b'`').count();
        let fence = &rest[i..i + run];
        let after = &rest[i + run..];
        let Some(e) = after.find(fence) else {
            out.push_str(&rest[i..]);
            break;
        };
        out.push_str(&rest[i..i + run + e + run]);
        rest = &after[e + run..];
    }
    out
}

/// `dir/../x` → `x`: a path relative to a page's folder, made plain.
pub(crate) fn join_rel(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Cut `s` to at most `n` characters at a word end, without a trailing dot.
pub(crate) fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.trim().to_string();
    }
    let cut: String = s.chars().take(n).collect();
    let cut = cut.rfind(' ').filter(|i| *i > n / 2).map_or(cut.as_str(), |i| &cut[..i]);
    cut.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_names() {
        assert_eq!(iso(1_714_979_290), "2024-05-06T07:08:10Z");
        assert_eq!(parse_time("2024-05-06T07:08:10.250Z"), Some(1_714_979_290));
        assert_eq!(parse_time("2024-05-06"), Some(1_714_953_600));
        assert_eq!(parse_time("soon"), None);
        assert_eq!(tag_name("#Big idea, 2"), "Big-idea-2");
        assert_eq!(page_path("a/../b/ /c?"), Some("a/b/c".into()));
        assert_eq!(page_path("../.."), None);
        assert_eq!(shorten("a very long title indeed", 12), "a very long");
    }

    #[test]
    fn code_is_left_alone() {
        let up = |s: &str| s.to_uppercase();
        assert_eq!(outside_code("a `b c` d ```x\ny``` e", up), "A `b c` D ```x\ny``` E");
        assert_eq!(outside_code("open ` never closed", up), "OPEN ` never closed");
    }
}
