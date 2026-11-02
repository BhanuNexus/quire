// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! This device's identity: `<config>/device.toml` (`format`, `id`, `name`).
//!
//! The id is random, made once, and never leaves this computer except
//! inside provenance and ledger records ("Claude Code · Asha's laptop").

use std::path::Path;
use std::sync::OnceLock;

use quire_agent::DeviceInfo;
use serde::{Deserialize, Serialize};

pub const DEVICE_FILE: &str = "device.toml";
const FORMAT: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default = "one")]
    format: u32,
    id: String,
    #[serde(default)]
    name: String,
    #[serde(flatten)]
    other: toml::Table,
}

const fn one() -> u32 {
    1
}

/// A name for this computer when there is no better one.
fn default_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "This computer".into())
}

/// The device in `dir`, created there on first use. An unreadable file is
/// kept aside and replaced; a file from a newer Quire is used, not rewritten.
pub fn load_or_create_in(dir: &Path) -> DeviceInfo {
    let path = dir.join(DEVICE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        match toml::from_str::<DeviceFile>(&text) {
            Ok(f) if !f.id.is_empty() => {
                let name = if f.name.is_empty() { default_name() } else { f.name };
                return DeviceInfo { id: f.id, name };
            }
            _ => {
                let _ = crate::atomic::keep_unreadable(&path);
            }
        }
    }
    let d = DeviceInfo::generate(default_name());
    let file = DeviceFile { format: FORMAT, id: d.id.clone(), name: d.name.clone(), other: toml::Table::new() };
    if let Ok(text) = toml::to_string(&file) {
        let _ = std::fs::create_dir_all(dir);
        let _ = crate::atomic::write(&path, text.as_bytes(), false);
    }
    d
}

/// This device. Unit tests get a fresh one that is never written.
pub fn this_device() -> DeviceInfo {
    static DEVICE: OnceLock<DeviceInfo> = OnceLock::new();
    DEVICE
        .get_or_init(|| match crate::settings::config_dir().filter(|_| !cfg!(test)) {
            Some(dir) => load_or_create_in(&dir),
            None => DeviceInfo::generate(default_name()),
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_once_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_in(dir.path());
        let b = load_or_create_in(dir.path());
        assert_eq!(a, b);
        let text = std::fs::read_to_string(dir.path().join(DEVICE_FILE)).unwrap();
        assert!(text.contains("format = 1") && text.contains(&a.id), "{text}");
        // A newer file with extra keys is read as it is.
        std::fs::write(dir.path().join(DEVICE_FILE), "format = 2\nid = \"d_X\"\nname = \"Phone\"\nlabel = \"blue\"\n").unwrap();
        assert_eq!(load_or_create_in(dir.path()), DeviceInfo { id: "d_X".into(), name: "Phone".into() });
        assert!(std::fs::read_to_string(dir.path().join(DEVICE_FILE)).unwrap().contains("label"));
        // A broken one is kept aside.
        std::fs::write(dir.path().join(DEVICE_FILE), "id = [").unwrap();
        let c = load_or_create_in(dir.path());
        assert_ne!(c.id, "d_X");
        assert!(dir.path().join("device.toml.unreadable").exists());
    }
}
