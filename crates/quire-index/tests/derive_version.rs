// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The golden test of `quire_index::store::DERIVE_VERSION`
//! (`docs/design/0.14-index-startup.md` §3.6, versioning).
//!
//! The startup cache keeps what `NoteRecord::from_source` and `chunk_note`
//! derive from a note. If that changes (extract, the tokenizer, the stemmer,
//! the passages, a new field), a cache written by an older build would hand
//! out the old result. This test hashes both over a fixed corpus of 100
//! notes (`tests/fixtures/derive-corpus.md`) and fails until the version and
//! the hash below are bumped together.

#![expect(clippy::tests_outside_test_module, reason = "an integration test: its test sits at the top level")]

use quire_index::NoteRecord;
use quire_index::chunk::chunk_note;
use quire_index::store::DERIVE_VERSION;
use quire_vault::NotePath;

/// The hash of everything derived from the corpus at [`DERIVE_VERSION`].
const GOLDEN: (u32, u64) = (1, 1_459_444_412_393_386_129);

/// FNV-1a, 64 bits: stable across Rust releases (unlike `DefaultHasher`).
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

#[test]
fn derive_version_matches_what_is_derived() {
    let corpus = include_str!("fixtures/derive-corpus.md");
    let notes: Vec<&str> = corpus.split("\n<!-- next note -->\n").collect();
    assert_eq!(notes.len(), 100);
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for (i, src) in notes.iter().enumerate() {
        let path = NotePath::new(format!("Folder {}/Note {i}.md", i % 7));
        h = fnv(h, format!("{:?}", NoteRecord::from_source(path.clone(), src)).as_bytes());
        h = fnv(h, format!("{:?}", chunk_note(&path, src)).as_bytes());
    }
    assert_eq!(
        (DERIVE_VERSION, h),
        GOLDEN,
        "what the index derives from a note changed: bump `DERIVE_VERSION` (crates/quire-index/src/store.rs) and set this hash"
    );
}
