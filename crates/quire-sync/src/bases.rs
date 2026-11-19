// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the engine keeps of a synced file's contents, and where the merge
//! bases live (`docs/design/0.14-sync-speed.md` §3.1, §3.2).
//!
//! * [`Print`]: 32 bytes that stand for a file's synced text. The engine
//!   only compares prints, except in a three-way merge.
//! * [`Stamp`]: what the store says about the bytes on disk (the vault
//!   manifest's hash, opaque here). [`crate::Tracked::disk`] is the stamp of
//!   the bytes last known equal to the base, so a pass can tell a file is
//!   unchanged without reading it.
//! * [`Bases`]: the texts behind the prints, for merges. [`MemoryBases`]
//!   keeps them in memory (tests); the app keeps them in pack files beside
//!   its sync state.
//! * [`State13`]: the engine state as Quire 0.13 saved it (texts inline),
//!   to migrate from.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::engine::{DeleteMark, KeptCopy, LeftCopy, Skipped, SyncState, Tracked, is_attachment};
use crate::protocol::{FileId, RevId};

/// 32 bytes written and read as 64 lower-case hex digits.
fn ser_hex<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&quire_base::enc::hex(b))
}

fn de_hex<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
    let s = String::deserialize(d)?;
    parse_hex(&s).ok_or_else(|| serde::de::Error::custom("expected 64 hex digits"))
}

fn parse_hex(s: &str) -> Option<[u8; 32]> {
    quire_base::enc::unhex(s)?.try_into().ok()
}

/// A synced file's text as the engine remembers it: SHA-256 of the text
/// (for an attachment, of its base64 text, which is the hash inside the
/// 0.13 `sha256:` fingerprint).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Print(#[serde(serialize_with = "ser_hex", deserialize_with = "de_hex")] pub [u8; 32]);

impl Print {
    /// The print of `text` (a note's text, or an attachment's base64).
    pub fn of(text: &str) -> Self {
        Self(Sha256::digest(text.as_bytes()).into())
    }

    /// Lower-case hex, 64 digits.
    pub fn to_hex(self) -> String {
        quire_base::enc::hex(&self.0)
    }

    /// The print in 64 hex digits, if it is one.
    pub fn from_hex(s: &str) -> Option<Self> {
        parse_hex(s).map(Self)
    }
}

impl std::fmt::Debug for Print {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Print({})", &self.to_hex()[..12])
    }
}

/// What a store says about the bytes of a file on disk.
///
/// Opaque to the engine (the app's is the vault manifest's SHA-256 of the
/// stored bytes). Equal stamps mean equal bytes; a store that can't say
/// gives none.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp(#[serde(serialize_with = "ser_hex", deserialize_with = "de_hex")] pub [u8; 32]);

impl std::fmt::Debug for Stamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stamp({})", &quire_base::enc::hex(&self.0)[..12])
    }
}

/// The texts behind note prints: the common ancestors of three-way merges.
///
/// Only notes are put: attachments, settings files and encrypted files are
/// never merged, so their print is all the engine keeps. A base that
/// [`Self::get`] doesn't have (a damaged pack) costs a merge: both versions
/// are kept, never one lost. One that can't be read now is an error, and
/// the pass is tried again.
pub trait Bases: Send {
    /// The text whose print is `p`, if it is kept.
    fn get(&self, p: &Print) -> Result<Option<String>, String>;
    /// Keep `text` (whose print is `p`).
    fn put(&mut self, p: Print, text: &str) -> Result<(), String>;
}

/// [`Bases`] in memory, for tests and simulations. Never forgets a text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryBases(pub BTreeMap<Print, String>);

impl Bases for MemoryBases {
    fn get(&self, p: &Print) -> Result<Option<String>, String> {
        Ok(self.0.get(p).cloned())
    }

    fn put(&mut self, p: Print, text: &str) -> Result<(), String> {
        self.0.entry(p).or_insert_with(|| text.to_string());
        Ok(())
    }
}

/// The engine state as Quire 0.13 saved it (`state.json`): each base is a
/// note's whole text, or an attachment's `sha256:` fingerprint. Unknown
/// fields are ignored, as 0.13 ignored them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State13 {
    pub cursor: u64,
    pub files: BTreeMap<FileId, Tracked13>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub left: BTreeMap<String, LeftCopy13>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped: BTreeMap<FileId, Skipped>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deletes_sent: Vec<DeleteMark>,
}

/// [`Tracked`] as 0.13 saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked13 {
    pub path: String,
    pub synced_path: String,
    pub head: RevId,
    pub head_seq: u64,
    pub base: String,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absorbed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<KeptCopy13>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub up: Vec<RevId>,
}

/// [`KeptCopy`] as 0.13 saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptCopy13 {
    pub path: String,
    pub base: String,
}

/// [`LeftCopy`] as 0.13 saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftCopy13 {
    pub file: FileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// The print of a 0.13 base at `path`, putting a note's text into `bases`.
fn print_13(path: &str, base: &str, bases: &mut dyn Bases) -> Result<Print, String> {
    if is_attachment(path) {
        // `sha256:<hex>` holds the print itself; anything else (an empty
        // base 0.13 kept for a note edited here and deleted there) is
        // taken as text.
        if let Some(p) = base.strip_prefix("sha256:").and_then(Print::from_hex) {
            return Ok(p);
        }
        return Ok(Print::of(base));
    }
    let p = Print::of(base);
    bases.put(p, base)?;
    Ok(p)
}

impl State13 {
    /// This state with each base as a print; note texts go into `bases`.
    /// No file's stamp is known yet ([`Tracked::disk`] empty), so the first
    /// pass reads each note once.
    pub fn into_state(self, bases: &mut dyn Bases) -> Result<SyncState, String> {
        let mut files = BTreeMap::new();
        for (id, t) in self.files {
            let kept = match t.kept {
                Some(k) => Some(KeptCopy { base: print_13(&k.path, &k.base, bases)?, path: k.path }),
                None => None,
            };
            files.insert(
                id,
                Tracked {
                    base: print_13(&t.path, &t.base, bases)?,
                    path: t.path,
                    synced_path: t.synced_path,
                    head: t.head,
                    head_seq: t.head_seq,
                    deleted: t.deleted,
                    outside: t.outside,
                    absorbed: t.absorbed,
                    kept,
                    up: t.up,
                    disk: None,
                },
            );
        }
        let mut left = BTreeMap::new();
        for (path, l) in self.left {
            let base = match l.base {
                Some(b) => Some(print_13(&path, &b, bases)?),
                None => None,
            };
            left.insert(path, LeftCopy { file: l.file, base });
        }
        Ok(SyncState { cursor: self.cursor, files, left, skipped: self.skipped, deletes_sent: self.deletes_sent })
    }

    /// `state` as 0.13 saves it, with note texts from `bases`. `None` when a
    /// note's text isn't there (or can't be read): 0.13 would take an empty
    /// base for "nothing synced yet" and copy the note aside.
    pub fn from_state(state: &SyncState, bases: &dyn Bases) -> Option<Self> {
        let text = |path: &str, p: &Print| -> Option<String> {
            if is_attachment(path) {
                Some(format!("sha256:{}", p.to_hex()))
            } else if *p == Print::of("") {
                Some(String::new())
            } else {
                bases.get(p).ok().flatten()
            }
        };
        let mut files = BTreeMap::new();
        for (id, t) in &state.files {
            let kept = match &t.kept {
                Some(k) => Some(KeptCopy13 { path: k.path.clone(), base: text(&k.path, &k.base)? }),
                None => None,
            };
            files.insert(
                id.clone(),
                Tracked13 {
                    path: t.path.clone(),
                    synced_path: t.synced_path.clone(),
                    head: t.head.clone(),
                    head_seq: t.head_seq,
                    base: text(&t.path, &t.base)?,
                    deleted: t.deleted,
                    outside: t.outside,
                    absorbed: t.absorbed,
                    kept,
                    up: t.up.clone(),
                },
            );
        }
        let mut left = BTreeMap::new();
        for (path, l) in &state.left {
            let base = match &l.base {
                Some(b) => Some(text(path, b)?),
                None => None,
            };
            left.insert(path.clone(), LeftCopy13 { file: l.file.clone(), base });
        }
        Some(Self { cursor: state.cursor, files, left, skipped: state.skipped.clone(), deletes_sent: state.deletes_sent.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_print_is_the_hash_inside_the_old_fingerprint() {
        let b64 = "aGVsbG8=";
        let old = crate::engine::fingerprint("pic.png", b64);
        assert_eq!(old, format!("sha256:{}", Print::of(b64).to_hex()));
        assert_eq!(Print::from_hex(&Print::of("x").to_hex()), Some(Print::of("x")));
        assert_eq!(Print::from_hex("zz"), None);
        let json = serde_json::to_string(&Print::of("x")).unwrap();
        assert_eq!(json, format!("\"{}\"", Print::of("x").to_hex()));
        assert!(serde_json::from_str::<Print>("\"abc\"").is_err());
    }

    #[test]
    fn a_013_state_turns_into_prints_and_back() {
        let json = r#"{"cursor":7,"files":{
            "f1":{"path":"Plan.md","synced_path":"Plan.md","head":"r1","head_seq":3,"base":"one\n","deleted":false,
                  "kept":{"path":"Old/Plan.md","base":"zero\n"},"up":["r0"]},
            "f2":{"path":"pic.png","synced_path":"pic.png","head":"r2","head_seq":4,"base":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","deleted":false},
            "f3":{"path":"Gone.md","synced_path":"Gone.md","head":"r3","head_seq":5,"base":"","deleted":true,"future":1}},
            "left":{"Away.md":{"file":"f1","base":"away\n"}},
            "deletes_sent":[{"at":1,"held":2,"sent":3}]}"#;
        let old: State13 = serde_json::from_str(json).unwrap();
        let mut bases = MemoryBases::default();
        let state = old.clone().into_state(&mut bases).unwrap();
        assert_eq!(state.cursor, 7);
        let f1 = &state.files[&FileId("f1".into())];
        assert_eq!((f1.base, f1.disk), (Print::of("one\n"), None));
        assert_eq!(bases.get(&f1.base).unwrap().as_deref(), Some("one\n"));
        assert_eq!(bases.get(&f1.kept.as_ref().unwrap().base).unwrap().as_deref(), Some("zero\n"));
        let png = &state.files[&FileId("f2".into())];
        assert_eq!(png.base, Print::of("hello"), "the fingerprint's hash is the print");
        assert!(bases.get(&png.base).unwrap().is_none(), "attachments keep no text");
        assert_eq!(bases.get(&state.left["Away.md"].base.unwrap()).unwrap().as_deref(), Some("away\n"));
        assert_eq!(State13::from_state(&state, &bases), Some(old));
        assert_eq!(State13::from_state(&state, &MemoryBases::default()), None, "a missing text is never saved as empty");
    }
}
