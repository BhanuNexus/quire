// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a device does to its sync vault besides syncing (0.13).
//!
//! Publish its device record, take key changes, list and remove devices,
//! upgrade the vault. The same for every transport, through [`SpaceAdmin`].

use std::collections::{BTreeMap, BTreeSet};

use crate::crypto::{CryptoError, KdfParams, VaultKey};
use crate::keyring::{DeviceRecord, DeviceSecret, EpochRecord, KeyError, KeySet, Pins, Removed, Resolved, resolve};
use crate::protocol::{DeviceId, LanePos, SpaceId};
use crate::transport::TransportError;

/// The minimum reader of an upgraded vault, in each transport's numbering
/// (folders and storage say 4, a server's protocol 3); see [`SpaceAdmin::level`].
pub const HARDENED: u32 = 4;

/// The vault-level operations of a transport.
pub trait SpaceAdmin {
    /// [`HARDENED`] once the vault is upgraded (or made by 0.13), less before.
    fn level(&self) -> Result<u32, TransportError>;
    /// Raise the vault to [`HARDENED`]: Quire before 0.13 can't sync it any more.
    fn upgrade(&self) -> Result<(), TransportError>;
    fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError>;
    /// Store a key record. A server takes only the next epoch (compare-and-swap).
    fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError>;
    fn device_records(&self) -> Result<Vec<DeviceRecord>, TransportError>;
    fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError>;
    /// Devices that wrote revisions.
    fn writers(&self) -> Result<Vec<DeviceId>, TransportError>;
    /// How far each device's revisions have been read here.
    fn lane_ends(&self) -> Result<BTreeMap<DeviceId, LanePos>, TransportError>;
    /// Ignore what removed devices write after their cut.
    fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>);
    fn set_keys(&self, keys: &KeySet);
    /// A server stops taking `device`'s token. Other transports: nothing to do.
    fn shut_out(&self, device: &DeviceId) -> Result<(), TransportError> {
        let _ = device;
        Ok(())
    }
}

/// A device of the vault, for Settings → Sync → Devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    /// Its name, as it says (sealed in its record); `None` without one.
    pub name: Option<String>,
    /// The Quire it runs, as it says; `None`: 0.12 or older, or not synced since updating.
    pub quire: Option<String>,
    pub this: bool,
    pub removed: bool,
    /// Its key is known here, so it can be given a new vault key.
    pub has_key: bool,
}

impl DeviceInfo {
    /// "Asha's laptop (1a2b3c4d)".
    pub fn label(&self) -> String {
        let short = self.id.0.get(..8).unwrap_or(&self.id.0);
        match &self.name {
            Some(n) if !n.trim().is_empty() => format!("{} ({short})", n.trim()),
            _ => format!("device {short}"),
        }
    }
}

/// Pin the keys of authentic device records; returns `(device, key, name,
/// quire)` for each one whose key is the pinned one. Once the vault's key
/// has changed, a record authenticated only with an older key (which a
/// removed device has) counts only for a device the newest key change
/// names, with the same key: a removed device can't slip in a device of
/// its own.
fn verified(
    space: &SpaceId,
    keys: &KeySet,
    records: &[DeviceRecord],
    pins: &mut Pins,
    latest: Option<&EpochRecord>,
) -> Vec<(DeviceId, [u8; 32], String, String)> {
    let mut out = Vec::new();
    for r in records {
        let Some((k, name)) = r.verify(keys, space, &r.device) else { continue };
        let fresh = latest.is_none_or(|l| r.epoch >= l.epoch || l.members.get(&r.device).is_some_and(|m| m.key == crate::crypto::hex(&k)));
        if fresh && pins.pin(&r.device, &k) {
            out.push((r.device.clone(), k, name, r.quire.clone()));
        }
    }
    out
}

/// Publish this device's record when it has none for the current key epoch.
pub fn publish_self(
    admin: &dyn SpaceAdmin,
    space: &SpaceId,
    me: &DeviceId,
    secret: &DeviceSecret,
    name: &str,
    keys: &KeySet,
) -> Result<(), TransportError> {
    let records = admin.device_records()?;
    // A record stays as long as it's right: one made under an older key
    // still counts while this device is in the newest key change.
    let current = records.iter().any(|r| {
        r.device == *me
            && r.quire == env!("CARGO_PKG_VERSION")
            && r.verify(keys, space, me).is_some_and(|(k, n)| k == secret.public() && n == name)
    });
    if current {
        return Ok(());
    }
    let rec = DeviceRecord::make(space, me, secret, name, keys).map_err(|e| TransportError::Refused(e.to_string()))?;
    admin.put_device_record(&rec)
}

/// Read the key records and take any newer epoch this device is in.
///
/// Pins device keys from their records first (makers must be known). The caller
/// stores the new keys and pins, and gives the result's `removed` to the
/// transports.
pub fn refresh_keys(
    admin: &dyn SpaceAdmin,
    space: &SpaceId,
    me: &DeviceId,
    secret: Option<&DeviceSecret>,
    keys: &KeySet,
    pins: &mut Pins,
) -> Result<Resolved, TransportError> {
    let records = admin.key_records()?;
    if records.is_empty() {
        return Ok(Resolved { keys: keys.clone(), records: BTreeMap::new(), removed: BTreeMap::new(), stuck: None });
    }
    // Device records may be authenticated with any epoch this device has,
    // and a maker's record with the new one only (after its removal):
    // pins come from what's verifiable now.
    let devs = admin.device_records()?;
    let before = resolve(keys, &records, me, None, pins);
    verified(space, keys, &devs, pins, before.records.values().next_back());
    let r = resolve(keys, &records, me, secret, pins);
    if r.keys.current() > keys.current() {
        verified(space, &r.keys, &devs, pins, r.records.values().next_back());
    }
    admin.set_keys(&r.keys);
    Ok(r)
}

/// The vault's devices: those that wrote revisions and those with a record.
pub fn devices(
    admin: &dyn SpaceAdmin,
    space: &SpaceId,
    me: &DeviceId,
    keys: &KeySet,
    pins: &mut Pins,
    removed: &BTreeSet<DeviceId>,
) -> Result<Vec<DeviceInfo>, TransportError> {
    let records = admin.device_records()?;
    let latest = resolve(keys, &admin.key_records()?, me, None, pins).records.into_values().next_back();
    let good = verified(space, keys, &records, pins, latest.as_ref());
    let mut ids: BTreeSet<DeviceId> = admin.writers()?.into_iter().collect();
    ids.extend(good.iter().map(|g| g.0.clone()));
    ids.insert(me.clone());
    let mut out: Vec<DeviceInfo> = ids
        .into_iter()
        .map(|id| {
            let g = good.iter().find(|g| g.0 == id);
            DeviceInfo {
                this: id == *me,
                removed: removed.contains(&id),
                name: g.map(|g| g.2.clone()),
                quire: g.map(|g| g.3.clone()),
                has_key: g.is_some(),
                id,
            }
        })
        .collect();
    out.sort_by_key(|d| (!d.this, d.removed, d.label()));
    Ok(out)
}

/// Devices that wrote revisions but never said they run 0.13: they can't
/// sync once the vault is upgraded.
pub fn old_devices(admin: &dyn SpaceAdmin, space: &SpaceId, me: &DeviceId, keys: &KeySet) -> Result<Vec<DeviceInfo>, TransportError> {
    let mut pins = Pins::default();
    Ok(devices(admin, space, me, keys, &mut pins, &BTreeSet::new())?.into_iter().filter(|d| !d.this && d.quire.is_none()).collect())
}

/// Upgrade the vault (`docs/design/0.13-sync.md` §3). Idempotent.
pub fn upgrade(admin: &dyn SpaceAdmin) -> Result<(), TransportError> {
    if admin.level()? >= HARDENED {
        return Ok(());
    }
    admin.upgrade()
}

/// What removing a device needs from the caller.
#[derive(Debug)]
pub struct Removal<'a> {
    pub space: &'a SpaceId,
    pub me: &'a DeviceId,
    pub secret: &'a DeviceSecret,
    pub keys: &'a KeySet,
    pub target: &'a DeviceId,
    /// The passphrase for devices that join later (a new one, unless only
    /// a device was lost and nobody else knows the old).
    pub passphrase: &'a str,
    pub kdf: KdfParams,
    /// The last of the target's revisions still taken, per lane.
    pub lanes: BTreeMap<String, LanePos>,
    /// Devices removed before, which stay removed.
    pub removed: &'a BTreeSet<DeviceId>,
}

/// What a removal did.
#[derive(Debug)]
pub struct Removed1 {
    pub record: EpochRecord,
    pub key: VaultKey,
    /// Devices that keep access.
    pub members: Vec<DeviceId>,
    /// Devices left out because their key isn't known here (they never
    /// published one: not synced since updating to 0.13). They must join again.
    pub left_out: Vec<DeviceId>,
}

/// Remove a device: a new vault key for every other device whose key is
/// known, the passphrase and the recovery key (`docs/design/0.13-sync.md` §2).
pub fn remove_device(admin: &dyn SpaceAdmin, r: &Removal<'_>, pins: &mut Pins) -> Result<Removed1, TransportError> {
    if admin.level()? < HARDENED {
        return Err(TransportError::Refused("Upgrade the vault's sync format first (Settings → Sync).".into()));
    }
    if r.target == r.me {
        return Err(TransportError::Refused("To remove this device, turn sync off here (or remove it from another device).".into()));
    }
    // The newest key first: a removal is made on top of it.
    let fresh = refresh_keys(admin, r.space, r.me, Some(r.secret), r.keys, pins)?;
    if let Some((e, why)) = &fresh.stuck {
        return Err(TransportError::Refused(format!("This device can't take the vault's key {e}: {why}")));
    }
    let keys = fresh.keys;
    let records = admin.device_records()?;
    let good = verified(r.space, &keys, &records, pins, fresh.records.values().next_back());
    let mut members: Vec<(DeviceId, [u8; 32])> = good
        .iter()
        .filter(|g| g.0 != *r.target && !r.removed.contains(&g.0) && !fresh.removed.contains_key(&g.0))
        .map(|g| (g.0.clone(), g.1))
        .collect();
    if !members.iter().any(|(d, _)| d == r.me) {
        members.push((r.me.clone(), r.secret.public()));
    }
    let writers = admin.writers()?;
    let left_out: Vec<DeviceId> = writers
        .into_iter()
        .filter(|w| w != r.target && !r.removed.contains(w) && !fresh.removed.contains_key(w) && !members.iter().any(|(d, _)| d == w))
        .collect();
    let removed = vec![Removed { device: r.target.clone(), lanes: r.lanes.clone() }];
    let (record, key) = EpochRecord::make(r.space, &keys, r.me, r.secret, &members, removed, r.passphrase, r.kdf)
        .map_err(|e: CryptoError| TransportError::Refused(e.to_string()))?;
    admin.put_key_record(&record)?;
    admin.shut_out(r.target)?;
    Ok(Removed1 { record, key, members: members.into_iter().map(|(d, _)| d).collect(), left_out })
}

/// Words for a [`KeyError`] that stops sync on this device.
pub fn stuck_notice(why: &KeyError) -> String {
    match why {
        KeyError::Removed => {
            "This device was removed from the sync vault on another device. It keeps the notes it has; to sync again, turn sync off and join the vault again with its passphrase.".into()
        }
        KeyError::NotMember => {
            "The vault's key changed and this device wasn't given the new one (it hadn't synced since updating). Turn sync off and join again with the passphrase.".into()
        }
        other => format!("Sync stopped: {other}."),
    }
}

impl SpaceAdmin for crate::folder::FolderTransport {
    fn level(&self) -> Result<u32, TransportError> {
        self.read_level()
    }
    fn upgrade(&self) -> Result<(), TransportError> {
        self.raise_level(crate::folder::LEVEL_HARDENED)
    }
    fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError> {
        Ok(Self::key_records(self))
    }
    fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        Self::put_key_record(self, r)
    }
    fn device_records(&self) -> Result<Vec<DeviceRecord>, TransportError> {
        Ok(Self::device_records(self))
    }
    fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        Self::put_device_record(self, r)
    }
    fn writers(&self) -> Result<Vec<DeviceId>, TransportError> {
        Ok(Self::writers(self))
    }
    fn lane_ends(&self) -> Result<BTreeMap<DeviceId, LanePos>, TransportError> {
        Self::lane_ends(self)
    }
    fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>) {
        Self::set_removed(self, removed);
    }
    fn set_keys(&self, keys: &KeySet) {
        Self::set_keys(self, keys.clone());
    }
}

#[cfg(feature = "remote")]
impl SpaceAdmin for crate::remote::RemoteTransport {
    fn level(&self) -> Result<u32, TransportError> {
        crate::remote::level(self.store(), self.space())
    }
    fn upgrade(&self) -> Result<(), TransportError> {
        self.raise_level(crate::remote::LEVEL_HARDENED)
    }
    fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError> {
        Self::key_records(self)
    }
    fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        Self::put_key_record(self, r)
    }
    fn device_records(&self) -> Result<Vec<DeviceRecord>, TransportError> {
        Self::device_records(self)
    }
    fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        Self::put_device_record(self, r)
    }
    fn writers(&self) -> Result<Vec<DeviceId>, TransportError> {
        Self::writers(self)
    }
    fn lane_ends(&self) -> Result<BTreeMap<DeviceId, LanePos>, TransportError> {
        Self::lane_ends(self)
    }
    fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>) {
        Self::set_removed(self, removed);
    }
    fn set_keys(&self, keys: &KeySet) {
        Self::set_keys(self, keys.clone());
    }
}

#[cfg(feature = "http")]
impl SpaceAdmin for crate::http::HttpTransport {
    fn level(&self) -> Result<u32, TransportError> {
        Ok(if self.min_protocol()? >= crate::protocol::HARDENED_PROTOCOL { HARDENED } else { 1 })
    }
    fn upgrade(&self) -> Result<(), TransportError> {
        self.set_min_protocol(crate::protocol::HARDENED_PROTOCOL)?;
        self.min_protocol().map(|_| ())
    }
    fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError> {
        Self::key_records(self)
    }
    fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        Self::put_key_record(self, r)
    }
    fn device_records(&self) -> Result<Vec<DeviceRecord>, TransportError> {
        Ok(self
            .device_rows()?
            .into_iter()
            .filter_map(|row| {
                let r: DeviceRecord = serde_json::from_value(row.record?).ok()?;
                (r.device.0 == row.device).then_some(r)
            })
            .collect())
    }
    fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        Self::put_device_record(self, r)
    }
    fn writers(&self) -> Result<Vec<DeviceId>, TransportError> {
        Ok(self
            .device_rows()?
            .into_iter()
            .filter(|r| r.last_seq > 0 && crate::folder::is_device_name(&r.device))
            .map(|r| DeviceId(r.device))
            .collect())
    }
    fn lane_ends(&self) -> Result<BTreeMap<DeviceId, LanePos>, TransportError> {
        Ok(self.device_rows()?.into_iter().map(|r| (DeviceId(r.device), LanePos { n: r.last_seq, ..LanePos::default() })).collect())
    }
    fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>) {
        Self::set_removed(self, removed);
    }
    fn set_keys(&self, _keys: &KeySet) {}
    fn shut_out(&self, device: &DeviceId) -> Result<(), TransportError> {
        self.remove_device(device)
    }
}
