// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The XML an S3-compatible or WebDAV server answers with (listings and errors):
//! a hostile or broken server must get an error, never a panic or a hang.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quire_sync::remote::xml::{dav_list, s3_error, s3_list};

fuzz_target!(|data: &[u8]| {
    let _ = s3_error(data);
    let _ = s3_list(data, "vault/quire-sync/");
    let _ = dav_list(data, "dav.example", "/dav/vault/");
});
