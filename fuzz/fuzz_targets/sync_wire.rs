// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The sync wire format: the JSON a server, a bucket or another device's folder
//! hands back (a revision, a snapshot manifest) and the names of its folders and
//! parts. Reading any bytes returns; what is read once writes back and reads again
//! the same. (The sealed payload inside a revision is checked by its tag, not
//! parsed: `Payload::open` needs the vault's keys.)
#![no_main]

use libfuzzer_sys::fuzz_target;
use quire_sync::compact::{Manifest, parse_name, parse_part};
use quire_sync::protocol::Revision;

fuzz_target!(|data: &[u8]| {
    if let Ok(r) = serde_json::from_slice::<Revision>(data) {
        let again = serde_json::to_vec(&r).expect("a revision serialises");
        let back = serde_json::from_slice::<Revision>(&again).expect("what was written reads back");
        assert_eq!(back, r);
    }
    if let Ok(m) = serde_json::from_slice::<Manifest>(data) {
        let again = serde_json::to_vec(&m).expect("a manifest serialises");
        assert_eq!(serde_json::from_slice::<Manifest>(&again).expect("what was written reads back"), m);
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = parse_name(s);
        let _ = parse_part(s);
    }
});
