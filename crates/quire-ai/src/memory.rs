// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Free memory before a language model loads (`docs/design/0.14-local-ai.md` §3.5).
//!
//! Read where it is cheap: `MemAvailable` from `/proc/meminfo` on Linux, and
//! PowerShell's `FreePhysicalMemory` on Windows (the query the app already
//! makes for total memory). macOS gives `None`: there the pack dialog says how
//! much memory writing uses, and only `min_ram_gb` is checked.

/// Room needed beyond a model's `ram_mb`: the app, the index and the system.
pub const MARGIN_BYTES: u64 = 500 * 1_000_000;

/// Free memory now, in bytes; `None` where it can't be read cheaply.
pub fn available_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        parse_mem_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
    } else if cfg!(windows) {
        windows_free()
    } else {
        None
    }
}

/// `MemAvailable:` from `/proc/meminfo`, in bytes.
pub fn parse_mem_available(info: &str) -> Option<u64> {
    let kb: u64 = info.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    kb.checked_mul(1024)
}

/// PowerShell prints `FreePhysicalMemory` (KB) as a number alone.
pub fn parse_free_physical_kb(out: &str) -> Option<u64> {
    out.lines().map(str::trim).find(|l| !l.is_empty())?.parse::<u64>().ok()?.checked_mul(1024)
}

fn windows_free() -> Option<u64> {
    #[expect(clippy::disallowed_methods, reason = "reads free memory on Windows (PowerShell), with fixed arguments")]
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", "(Get-CimInstance -ClassName Win32_OperatingSystem).FreePhysicalMemory"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned()).as_deref().and_then(parse_free_physical_kb)
}

/// Decimal GB with one decimal, as the pages and the dialog show sizes.
pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Whether `free` bytes leave room for a model of `ram_mb`: `Err` with the
/// message to show when they don't. An unknown `free` passes.
pub fn check_room(model: &str, ram_mb: u64, free: Option<u64>) -> Result<(), String> {
    let need = ram_mb.saturating_mul(1_000_000);
    match free {
        Some(f) if f < need.saturating_add(MARGIN_BYTES) => Err(format!(
            "Not enough free memory for {model} (needs about {}, {} free). Close some programs and try again.",
            gb(need),
            gb(f)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_memory_is_read_and_checked() {
        let info = "MemTotal:       16303340 kB\nMemFree:          812344 kB\nMemAvailable:    2050000 kB\n";
        assert_eq!(parse_mem_available(info), Some(2_050_000 * 1024));
        assert_eq!(parse_mem_available("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_free_physical_kb("\r\n 2097152\r\n"), Some(2_097_152 * 1024));
        assert_eq!(parse_free_physical_kb("nope"), None);
        // 3.3 GB plus 0.5 GB of room.
        assert!(check_room("Qwen 3.5 4B", 3300, Some(3_900_000_000)).is_ok());
        assert_eq!(
            check_room("Qwen 3.5 4B", 3300, Some(2_100_000_000)).unwrap_err(),
            "Not enough free memory for Qwen 3.5 4B (needs about 3.3 GB, 2.1 GB free). Close some programs and try again."
        );
        assert!(check_room("Qwen 3.5 4B", 3300, Some(3_700_000_000)).is_err(), "the margin counts");
        assert!(check_room("Qwen 3.5 4B", 3300, None).is_ok(), "unknown free memory doesn't block");
    }
}
