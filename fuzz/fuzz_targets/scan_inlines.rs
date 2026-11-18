// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The inline scanners (wikilinks, tags, highlights, citations) and the
//! verbatim-range finder: they finish, and their ranges are in order, separate
//! and on character boundaries, for any text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quire_markdown::{cite, scan_inlines, verbatim_ranges};

fn check(src: &str, ranges: impl Iterator<Item = std::ops::Range<usize>>, what: &str) {
    let mut end = 0;
    for r in ranges {
        assert!(r.start >= end && r.end > r.start && src.get(r.clone()).is_some(), "{what}: bad range {r:?} after {end}");
        end = r.end;
    }
}

fuzz_target!(|data: &str| {
    check(data, scan_inlines(data).iter().map(quire_markdown::Inline::range), "scan_inlines");
    check(data, cite::scan(data).into_iter().map(|c| c.range), "cite::scan");
    check(data, verbatim_ranges(data).into_iter(), "verbatim_ranges");
});
