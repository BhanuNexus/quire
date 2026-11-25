// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The peak memory of a cold load of a vault on disk (no startup cache), the
//! way the app loads (`load_cached`) or the MCP server does (`load`). Run by
//! hand, one load per process (the peak is the process's), on a vault from
//! `quire-cli bench vault <dir> <notes>`:
//! `QUIRE_PEAK_VAULT=<dir> QUIRE_PEAK_MODE=cached cargo test --release -p quire-index --test cold_load_peak -- --ignored --nocapture`.

#![allow(
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    clippy::print_stderr,
    clippy::disallowed_methods,
    reason = "a measurement run by hand: it reads its vault and /proc directly, and unwraps to fail"
)]

use std::time::Instant;

use quire_index::IndexService;
use quire_vault::Vault;
use quire_vault::changes::Manifest;

/// A line of `/proc/self/status` in MB (`VmHWM`: the peak, `VmRSS`: now).
fn status_mb(key: &str) -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<f64>().ok())
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

#[test]
#[ignore = "memory, run by hand on a vault"]
fn cold_load_peak() {
    let Ok(dir) = std::env::var("QUIRE_PEAK_VAULT") else {
        eprintln!("set QUIRE_PEAK_VAULT to a vault folder");
        return;
    };
    let cached = std::env::var("QUIRE_PEAK_MODE").is_ok_and(|m| m == "cached");
    // Cold: no startup cache from an earlier run.
    let _ = std::fs::remove_dir_all(std::path::Path::new(&dir).join(".quire/cache"));
    let vault = Vault::open_local(std::path::Path::new(&dir)).unwrap();
    let before = status_mb("VmRSS:");
    let svc = IndexService::spawn();
    let t = Instant::now();
    if cached {
        svc.load_cached(vault.clone(), Manifest::open(&vault));
    } else {
        svc.load(vault);
    }
    svc.flush();
    let keyword = t.elapsed();
    let notes = svc.read().len();
    // The meaning model's fit waits out its debounce: this peak is the keyword load's.
    eprintln!(
        "{} {notes} notes: keyword complete in {keyword:?}; {before:.0} MB before, {:.0} MB now, peak {:.0} MB",
        if cached { "load_cached" } else { "load" },
        status_mb("VmRSS:"),
        status_mb("VmHWM:")
    );
    svc.flush_semantic();
    eprintln!("  with the meaning model: {:.0} MB now, peak {:.0} MB", status_mb("VmRSS:"), status_mb("VmHWM:"));
}
