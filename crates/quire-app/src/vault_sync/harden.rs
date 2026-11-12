// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13 sync hardening for one vault (`docs/design/0.13-sync.md`): the key
//! epoch kept here, this device's own key and record, removing a device,
//! upgrading the vault, compaction.
//!
//! `.quire/sync/hardening.json` holds what this device knows (whether the
//! keychain holds a device token, how much history compaction keeps);
//! `.quire/sync/peers.json` the device keys pinned here. Secrets stay in
//! the keychain: the vault key of the newest epoch (`sync-key-<space>`, as
//! before, written `e<epoch>:<recovery key>` once the key changed) and this
//! device's X25519 secret (`sync-device-<device>`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use quire_sync::admin::{self, DeviceInfo, HARDENED, Removal};
use quire_sync::compact::Policy;
use quire_sync::folder::Compacted;
use quire_sync::keyring::{DeviceSecret, EpochRecord, KeySet, Pins};
use quire_sync::{DeviceId, LanePos, SpaceId, VaultKey};
use serde::{Deserialize, Serialize};

use super::{KDF, Link, SyncConfig, key_name, secrets, sync_dir};

/// `.quire/sync/hardening.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardening {
    /// The keychain holds this device's own server token (0.13 server).
    #[serde(default)]
    pub device_token: bool,
    /// Days of history compaction keeps (default 90).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_days: Option<u32>,
    /// When this device last tried compacting (Unix seconds).
    #[serde(default)]
    pub last_compact: i64,
}

impl Hardening {
    pub fn load(root: &Path) -> Self {
        std::fs::read(sync_dir(root).join("hardening.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> Result<(), String> {
        let dir = sync_dir(root);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        crate::atomic::write(&dir.join("hardening.json"), &serde_json::to_vec_pretty(self).unwrap_or_default(), false)
            .map_err(|e| e.to_string())
    }

    /// The history policy for compaction.
    pub fn policy(&self) -> Policy {
        Policy { keep_days: self.keep_days.unwrap_or(Policy::DEFAULT.keep_days), ..Policy::DEFAULT }
    }
}

pub(super) fn device_secret_name(device: &DeviceId) -> String {
    format!("sync-device-{device}")
}

/// This device's X25519 secret, made the first time (per device id: two
/// vault folders of one vault on this computer are two devices).
pub(super) fn device_secret(device: &DeviceId) -> Result<DeviceSecret, String> {
    let store = secrets::store();
    if let Some(s) = store.get(&device_secret_name(device)).and_then(|h| DeviceSecret::from_hex(&h)) {
        return Ok(s);
    }
    let s = DeviceSecret::generate().map_err(|e| e.to_string())?;
    store.set(&device_secret_name(device), &s.to_hex())?;
    Ok(s)
}

pub(super) fn load_pins(root: &Path) -> Pins {
    std::fs::read(sync_dir(root).join("peers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub(super) fn save_pins(root: &Path, pins: &Pins) {
    let _ = crate::atomic::write(&sync_dir(root).join("peers.json"), &serde_json::to_vec_pretty(pins).unwrap_or_default(), false);
}

/// The keys this device has: the keychain's key, of the epoch kept with it.
pub(super) fn keyset(space: &SpaceId, key: &VaultKey) -> KeySet {
    let epoch = secrets::store().get(&key_name(space)).and_then(|k| super::parse_key(&k)).map_or(0, |(e, _)| e);
    if epoch > 0 { KeySet::at(epoch, key.clone()) } else { KeySet::new(key) }
}

/// Keep the newest key and its epoch in the keychain.
pub(super) fn store_keys(space: &SpaceId, keys: &KeySet) -> Result<(), String> {
    let raw = keys.current_raw().ok_or("no key to keep")?;
    let value = match keys.current() {
        0 => raw.recovery_key().to_string(),
        e => format!("e{e}:{}", raw.recovery_key()),
    };
    secrets::store().set(&key_name(space), &value)
}

/// The removed devices and what is still taken from each, per lane.
pub(super) fn removed_lanes(r: &BTreeMap<DeviceId, quire_sync::keyring::Removed>, lane: &str) -> BTreeMap<DeviceId, LanePos> {
    r.iter().map(|(d, x)| (d.clone(), x.lanes.get(lane).copied().unwrap_or_default())).collect()
}

/// What an admin action needs: the config, a link (with a temporary
/// mirror for S3 and WebDAV, so it never races the sync worker), the keys.
struct Parts {
    cfg: SyncConfig,
    link: Link,
    keys: KeySet,
    secret: DeviceSecret,
    pins: Pins,
}

fn parts(root: &Path) -> Result<Parts, String> {
    let (_vault, cfg, token, key) = super::open_parts(root)?;
    let link = Link::new(&cfg, &token, None, None)?;
    let keys = keyset(&cfg.space, &key);
    let link = link.with_keys(&keys);
    Ok(Parts { secret: device_secret(&cfg.device)?, pins: load_pins(root), keys, link, cfg })
}

/// A device of the vault, for Settings → Sync and `quire-cli sync devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDevice {
    pub id: String,
    /// "Asha's laptop (1a2b3c4d)".
    pub label: String,
    /// The Quire it says it runs; `None`: 0.12 or older (or not synced since updating).
    pub quire: Option<String>,
    pub this: bool,
    pub removed: bool,
    /// It can be given a new key when another device is removed.
    pub has_key: bool,
}

impl From<DeviceInfo> for SyncDevice {
    fn from(d: DeviceInfo) -> Self {
        Self { label: d.label(), id: d.id.0, quire: d.quire, this: d.this, removed: d.removed, has_key: d.has_key }
    }
}

/// The vault's devices, and its level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDevices {
    pub devices: Vec<SyncDevice>,
    /// Upgraded to 0.13 sync (bound headers, removal, compaction).
    pub upgraded: bool,
    /// The key epoch this device uses.
    pub key_epoch: u32,
}

/// The vault's devices (`quire-cli sync devices`, Settings → Sync → Devices).
pub fn sync_devices_at(root: &Path) -> Result<SyncDevices, String> {
    let mut p = parts(root)?;
    let admin = p.link.admin();
    let fresh =
        admin::refresh_keys(admin, &p.cfg.space, &p.cfg.device, Some(&p.secret), &p.keys, &mut p.pins).map_err(|e| e.to_string())?;
    let removed: BTreeSet<DeviceId> = fresh.removed.keys().cloned().collect();
    let list = admin::devices(admin, &p.cfg.space, &p.cfg.device, &fresh.keys, &mut p.pins, &removed).map_err(|e| e.to_string())?;
    save_pins(root, &p.pins);
    Ok(SyncDevices {
        devices: list.into_iter().map(SyncDevice::from).collect(),
        upgraded: admin.level().map_err(|e| e.to_string())? >= HARDENED,
        key_epoch: fresh.keys.current(),
    })
}

/// What removing a device did.
#[derive(Debug, Clone)]
pub struct DeviceRemoved {
    /// The vault's new recovery key: show it once (the old one opens only
    /// what was written before).
    pub recovery_key: String,
    /// Devices that didn't get the new key (never synced since updating):
    /// they must join again.
    pub left_out: Vec<String>,
    /// What else to do (S3 and WebDAV: the storage key; a server: its token).
    pub advice: String,
}

/// Remove `device` from the vault: a new vault key for the other devices
/// (`docs/design/0.13-sync.md` §2). `passphrase` is for devices that join
/// later.
pub fn sync_remove_device_at(root: &Path, device: &str, passphrase: &str) -> Result<DeviceRemoved, String> {
    if passphrase.chars().count() < 8 {
        return Err("Use a passphrase of at least 8 characters".into());
    }
    let mut p = parts(root)?;
    let target = DeviceId(device.trim().to_string());
    let admin = p.link.admin();
    let ends = admin.lane_ends().map_err(|e| e.to_string())?;
    let mut lanes = BTreeMap::from([(String::new(), ends.get(&target).copied().unwrap_or_default())]);
    if let Some(dir) = &p.cfg.folder
        && quire_sync::folder::lane_exists(dir, &p.cfg.space, quire_sync::folder::SHARED_LANE)
    {
        let lane =
            quire_sync::folder::FolderTransport::lane(dir, &p.cfg.space, p.cfg.device.clone(), None, quire_sync::folder::SHARED_LANE)
                .with_keys(p.keys.clone());
        lanes.insert(
            quire_sync::folder::SHARED_LANE.to_string(),
            lane.lane_ends().map_err(|e| e.to_string())?.get(&target).copied().unwrap_or_default(),
        );
    }
    let known = admin.writers().map_err(|e| e.to_string())?;
    let records = admin.device_records().map_err(|e| e.to_string())?;
    if !known.contains(&target) && !records.iter().any(|r| r.device == target) {
        return Err(format!("No device {device} in this vault (see `quire-cli sync devices`)."));
    }
    let fresh =
        admin::refresh_keys(admin, &p.cfg.space, &p.cfg.device, Some(&p.secret), &p.keys, &mut p.pins).map_err(|e| e.to_string())?;
    let removed: BTreeSet<DeviceId> = fresh.removed.keys().cloned().collect();
    let r = Removal {
        space: &p.cfg.space,
        me: &p.cfg.device,
        secret: &p.secret,
        keys: &fresh.keys,
        target: &target,
        passphrase,
        kdf: KDF,
        lanes,
        removed: &removed,
    };
    let done = admin::remove_device(admin, &r, &mut p.pins).map_err(|e| e.to_string())?;
    let mut keys = fresh.keys;
    keys.insert(done.record.epoch, done.key.clone());
    store_keys(&p.cfg.space, &keys)?;
    save_pins(root, &p.pins);
    let advice = if p.cfg.remote.is_some() {
        "Every device shares the storage key: make a new one, give it to the other devices (Update credentials…), and delete the old one. With a key per device, delete the removed device's key."
    } else if p.cfg.folder.is_some() {
        "Stop sharing the sync folder with the removed device (or its account), or it can still delete files there."
    } else {
        "The server no longer takes that device's token. If it might know the server's own token, change it on the server; the other devices don't need it."
    };
    Ok(DeviceRemoved {
        recovery_key: done.key.recovery_key().to_string(),
        left_out: done.left_out.into_iter().map(|d| d.0).collect(),
        advice: advice.into(),
    })
}

/// Before upgrading: is it done, and which devices would stop syncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeInfo {
    pub upgraded: bool,
    /// Devices that wrote revisions but never said they run Quire 0.13.
    pub old_devices: Vec<String>,
}

pub fn sync_upgrade_info_at(root: &Path) -> Result<UpgradeInfo, String> {
    let p = parts(root)?;
    let admin = p.link.admin();
    let upgraded = admin.level().map_err(|e| e.to_string())? >= HARDENED;
    let old = admin::old_devices(admin, &p.cfg.space, &p.cfg.device, &p.keys).map_err(|e| e.to_string())?;
    Ok(UpgradeInfo { upgraded, old_devices: old.into_iter().map(|d| d.label()).collect() })
}

/// Upgrade the vault's sync format (`docs/design/0.13-sync.md` §3):
/// revisions bind their whole header, devices can be removed, history can
/// be compacted. Quire 0.12 and older can't sync it afterwards.
pub fn sync_upgrade_at(root: &Path) -> Result<(), String> {
    let p = parts(root)?;
    if let Link::Server(_) = &p.link
        && !Hardening::load(root).device_token
    {
        return Err("This device has no device token yet (the server needs Quire Sync Server 0.13): sync once, then upgrade.".into());
    }
    admin::upgrade(p.link.admin()).map_err(|e| e.to_string())
}

/// Compact now, with the sync lock held (`quire-cli sync compact`). The
/// app does it in its sync worker instead.
pub fn sync_compact_at(root: &Path, force: bool, keep_days: Option<u32>) -> Result<String, String> {
    let (vault, cfg, token, key) = super::open_parts(root)?;
    let lock = super::lock_sync(root).map_err(|e| match e {
        super::LockError::Busy { .. } => format!("{}: compact from the app (Settings → Sync), or stop it first.", super::ALREADY_SYNCING),
        super::LockError::Io(_) => e.to_string(),
    })?;
    if let Some(d) = keep_days {
        let mut h = Hardening::load(root);
        h.keep_days = Some(d);
        h.save(root)?;
    }
    let session = super::Session::new(root.to_path_buf(), vault, cfg, &token, key, lock)?;
    session.compact(force)
}

/// Words for what compaction did.
pub(super) fn describe(c: &Compacted, what: &str) -> String {
    match c {
        Compacted::Done(m) => format!(
            "{what}: folded {} old revision(s) ({} KB) into a snapshot; {} kept. Old files go in a week, once every device has seen it.",
            m.stats.dropped,
            m.stats.dropped_bytes / 1024,
            m.stats.kept
        ),
        Compacted::NotWorthIt(s) => format!("{what}: nothing worth compacting yet ({} old revision(s)).", s.dropped),
        Compacted::Settling { until } => {
            let when = chrono::DateTime::from_timestamp(*until, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            format!("{what}: the last compaction is still settling (until {when}).")
        }
        Compacted::NotUpgraded => format!("{what}: upgrade the vault's sync format first."),
        Compacted::Busy(why) => format!("{what}: {why}."),
    }
}

/// The joining keys: from the passphrase or a recovery key, through the
/// vault's key records.
pub(super) fn join_keys(epoch0: Option<&quire_sync::WrappedKey>, records: &[EpochRecord], secret: &str) -> Result<KeySet, String> {
    if let Ok(k) = VaultKey::from_recovery_key(secret) {
        return Ok(quire_sync::keyring::join_with_key(&k, records));
    }
    let w = epoch0.ok_or("this vault's key isn't readable")?;
    quire_sync::keyring::join_with_passphrase(w, records, secret).map_err(|e| match e {
        quire_sync::keyring::KeyError::WrongPassphrase if !records.is_empty() => {
            "Wrong passphrase. (The vault's passphrase may have changed when a device was removed.)".to_string()
        }
        e => e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::{Selection, SyncTarget, sync_create_at, sync_join_at, sync_once_at};
    use super::*;

    fn vault(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The app's own path through a 0.13 sync folder: a new vault starts
    /// upgraded, devices publish what they run, history is compacted, a
    /// device is removed and a later device joins with the new passphrase.
    #[test]
    fn a_new_vault_is_upgraded_lists_devices_compacts_and_removes_one() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (vault(dir.path(), "A"), vault(dir.path(), "B"), vault(dir.path(), "C"));
        let target = SyncTarget::Folder(dir.path().join("Dropbox"));
        std::fs::create_dir_all(dir.path().join("Dropbox")).unwrap();
        std::fs::write(a.join("Plan.md"), "one\n").unwrap();
        sync_create_at(&a, &target, "first pass phrase", "laptop", &Selection::default()).unwrap();
        sync_once_at(&a).unwrap();
        let space = super::super::sync_list(&target).unwrap()[0].space.clone();
        sync_join_at(&b, &target, &space, "first pass phrase", "phone", &Selection::default()).unwrap();
        sync_once_at(&b).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("Plan.md")).unwrap(), "one\n");
        let info = sync_upgrade_info_at(&a).unwrap();
        assert!(info.upgraded, "0.13 makes vaults in the 0.13 format");
        let list = sync_devices_at(&a).unwrap();
        assert_eq!(list.devices.len(), 2, "{list:?}");
        assert!(list.devices.iter().all(|d| d.quire.as_deref() == Some(env!("CARGO_PKG_VERSION"))));
        assert!(list.devices.iter().any(|d| d.this && d.label.starts_with("laptop")));
        // Some history, then compaction from the CLI's path (with the lock).
        for i in 0..5 {
            std::fs::write(a.join("Plan.md"), format!("one\n{i}\n")).unwrap();
            sync_once_at(&a).unwrap();
        }
        let said = sync_compact_at(&a, true, Some(0)).unwrap();
        assert!(said.contains("folded"), "{said}");
        sync_once_at(&b).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("Plan.md")).unwrap(), "one\n4\n");
        // Remove the phone.
        let phone = list.devices.iter().find(|d| !d.this).unwrap().id.clone();
        assert!(sync_remove_device_at(&a, &phone, "short").is_err());
        let r = sync_remove_device_at(&a, &phone, "second pass phrase").unwrap();
        assert!(r.left_out.is_empty());
        assert!(r.advice.contains("sync folder"));
        sync_once_at(&a).unwrap();
        let list = sync_devices_at(&a).unwrap();
        assert_eq!(list.key_epoch, 1);
        assert!(list.devices.iter().any(|d| d.id == phone && d.removed));
        std::fs::write(a.join("After.md"), "after the removal\n").unwrap();
        sync_once_at(&a).unwrap();
        // A device joining later: the old passphrase no longer opens the vault's key.
        assert!(sync_join_at(&c, &target, &space, "first pass phrase", "tablet", &Selection::default()).is_err());
        sync_join_at(&c, &target, &space, "second pass phrase", "tablet", &Selection::default()).unwrap();
        sync_once_at(&c).unwrap();
        assert_eq!(std::fs::read_to_string(c.join("After.md")).unwrap(), "after the removal\n");
        assert_eq!(std::fs::read_to_string(c.join("Plan.md")).unwrap(), "one\n4\n");
        // The recovery key now opens the new epoch, and through it every older one.
        let rk = r.recovery_key;
        let d = vault(dir.path(), "D");
        sync_join_at(&d, &target, &space, &rk, "desk", &Selection::default()).unwrap();
        sync_once_at(&d).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("After.md")).unwrap(), "after the removal\n");
    }
}
