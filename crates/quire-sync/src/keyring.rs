// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Key epochs and device keys (0.13, `docs/design/0.13-sync.md` §2).
//!
//! A vault starts with one key, epoch 0 (the 0.5–0.12 vault key). Removing
//! a device writes an [`EpochRecord`]: a new random key, sealed for each
//! remaining device's X25519 key (and wrapped by the sync passphrase, for
//! devices that join later). New revisions are sealed under the newest
//! epoch, so the removed device, which only has the older keys, can't read
//! them. It still reads what it had.
//!
//! Every 0.13 device has a [`DeviceSecret`] (in the keychain) and publishes
//! a [`DeviceRecord`] with its public key, authenticated with the vault key.
//! Public keys are pinned the first time they're seen ([`Pins`]).
//!
//! An epoch's key also opens the one before it (`prev`), so the newest key
//! is enough to read every older revision: it's the recovery key.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::crypto::{
    CryptoError, KdfParams, Keys, VaultKey, WrappedKey, derive32, hex, hex_bytes, open_with, random_bytes, seal_with, unhex,
};
use crate::protocol::{DeviceId, LanePos, SpaceId};

/// The vault's keys for every epoch this device has, and the newest, which
/// seals new revisions.
#[derive(Clone)]
pub struct KeySet {
    current: u32,
    keys: BTreeMap<u32, Keys>,
    raw: BTreeMap<u32, VaultKey>,
}

impl std::fmt::Debug for KeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeySet(epochs {:?}, current {})", self.keys.keys().collect::<Vec<_>>(), self.current)
    }
}

impl From<Keys> for KeySet {
    fn from(k: Keys) -> Self {
        Self { current: 0, keys: BTreeMap::from([(0, k)]), raw: BTreeMap::new() }
    }
}

impl KeySet {
    /// The keys of a vault that never changed its key.
    pub fn new(epoch0: &VaultKey) -> Self {
        let mut s = Self { current: 0, keys: BTreeMap::new(), raw: BTreeMap::new() };
        s.insert(0, epoch0.clone());
        s
    }

    /// The keys of a vault from the key of `epoch` alone (older epochs
    /// come from the key records, see [`resolve`]).
    pub fn at(epoch: u32, key: VaultKey) -> Self {
        let mut s = Self { current: 0, keys: BTreeMap::new(), raw: BTreeMap::new() };
        s.insert(epoch, key);
        s
    }

    /// Add the key of `epoch`; the newest epoch is the current one.
    pub fn insert(&mut self, epoch: u32, key: VaultKey) {
        self.keys.insert(epoch, key.keys());
        self.raw.insert(epoch, key);
        self.current = self.current.max(epoch);
    }

    pub const fn current(&self) -> u32 {
        self.current
    }

    pub fn current_keys(&self) -> &Keys {
        self.keys.get(&self.current).unwrap_or_else(|| unreachable!("the current epoch always has keys"))
    }

    pub fn get(&self, epoch: u32) -> Option<&Keys> {
        self.keys.get(&epoch)
    }

    /// The raw key of `epoch` (to wrap it, or show it as the recovery key).
    pub fn raw(&self, epoch: u32) -> Option<&VaultKey> {
        self.raw.get(&epoch)
    }

    pub fn current_raw(&self) -> Option<&VaultKey> {
        self.raw.get(&self.current)
    }

    pub fn epochs(&self) -> Vec<u32> {
        self.keys.keys().copied().collect()
    }
}

/// A device's own X25519 secret. Wiped from memory when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DeviceSecret([u8; 32]);

impl std::fmt::Debug for DeviceSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceSecret(…)")
    }
}

impl DeviceSecret {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self(random_bytes()?))
    }

    /// From the hex kept in the keychain.
    pub fn from_hex(s: &str) -> Option<Self> {
        let mut b = unhex(s.trim())?;
        let arr: [u8; 32] = b.as_slice().try_into().ok()?;
        b.zeroize();
        Some(Self(arr))
    }

    /// For the keychain.
    pub fn to_hex(&self) -> zeroize::Zeroizing<String> {
        zeroize::Zeroizing::new(hex(&self.0))
    }

    fn secret(&self) -> x25519_dalek::StaticSecret {
        x25519_dalek::StaticSecret::from(self.0)
    }

    pub fn public(&self) -> [u8; 32] {
        x25519_dalek::PublicKey::from(&self.secret()).to_bytes()
    }
}

/// Both Diffie-Hellman results must be contributory (no low-order points).
fn agree(me: &x25519_dalek::StaticSecret, them: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    let ss = me.diffie_hellman(&x25519_dalek::PublicKey::from(*them));
    if !ss.was_contributory() {
        return Err(CryptoError::Tampered);
    }
    Ok(ss.to_bytes())
}

fn box_key(eph_ss: &[u8; 32], static_ss: &[u8; 32], eph: &[u8; 32], to: &[u8; 32], maker: &[u8; 32], context: &[u8]) -> [u8; 32] {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(eph_ss);
    ikm[32..].copy_from_slice(static_ss);
    let mut info = Vec::with_capacity(96 + context.len());
    info.extend_from_slice(eph);
    info.extend_from_slice(to);
    info.extend_from_slice(maker);
    info.extend_from_slice(context);
    let k = derive32("quire-sync key box v1", &ikm, &info);
    ikm.zeroize();
    k
}

/// Seal `plain` for the device whose public key is `to`.
///
/// Only it can open it, and it knows `maker` sealed it (an ephemeral and a
/// static X25519 agreement, HKDF-SHA256, XChaCha20-Poly1305). Returns the
/// ephemeral public key and the sealed bytes.
pub fn seal_to(maker: &DeviceSecret, to: &[u8; 32], plain: &[u8], context: &[u8]) -> Result<([u8; 32], Vec<u8>), CryptoError> {
    let eph = x25519_dalek::StaticSecret::from(random_bytes::<32>()?);
    let eph_pub = x25519_dalek::PublicKey::from(&eph).to_bytes();
    let (mut a, mut b) = (agree(&eph, to)?, agree(&maker.secret(), to)?);
    let mut k = box_key(&a, &b, &eph_pub, to, &maker.public(), context);
    let sealed = seal_with(&k, plain, context);
    a.zeroize();
    b.zeroize();
    k.zeroize();
    Ok((eph_pub, sealed?))
}

/// Open what [`seal_to`] sealed for this device. Fails unless it was sealed
/// by the holder of `maker`'s secret, for this device, with this context.
pub fn open_from(me: &DeviceSecret, maker: &[u8; 32], eph: &[u8; 32], sealed: &[u8], context: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let s = me.secret();
    let (mut a, mut b) = (agree(&s, eph)?, agree(&s, maker)?);
    let mut k = box_key(&a, &b, eph, &me.public(), maker, context);
    let out = open_with(&k, sealed, context).map_err(|_| CryptoError::Tampered);
    a.zeroize();
    b.zeroize();
    k.zeroize();
    out
}

/// Device public keys as first seen (trust on first use). A later record
/// that names another key for a device is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pins(pub BTreeMap<DeviceId, String>);

impl Pins {
    /// Pin `key` for `device` unless another key is pinned: whether `key`
    /// is the device's key now.
    pub fn pin(&mut self, device: &DeviceId, key: &[u8; 32]) -> bool {
        let h = hex(key);
        self.0.entry(device.clone()).or_insert_with(|| h.clone()) == &h
    }

    pub fn get(&self, device: &DeviceId) -> Option<[u8; 32]> {
        unhex(self.0.get(device)?)?.as_slice().try_into().ok()
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What a 0.13 device says about itself, in every transport: its public
/// key, the Quire it runs and its name (sealed). Authenticated with the
/// vault key of `epoch`, so the storage can't forge one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub format: u32,
    pub space: SpaceId,
    pub device: DeviceId,
    /// Its X25519 public key, hex.
    pub x25519: String,
    /// The Quire version it runs (`0.13.0`).
    pub quire: String,
    /// Its name, sealed with the vault key.
    #[serde(with = "hex_bytes")]
    pub name: Vec<u8>,
    pub epoch: u32,
    /// Unix seconds.
    pub made: i64,
    /// Where this device's own lanes stood when it made this record (`""`
    /// for notes, `"shared"`): it vouches that everything up to there is
    /// its own. After that, its lines are sealed under `epoch` or a newer
    /// key, and (when `bound`) with their header bound. Readers refuse
    /// anything else there, which is what keeps a removed device, which
    /// still has the older keys, from writing into this device's lane.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub since: BTreeMap<String, LanePos>,
    /// It seals with the whole header bound (`v3`) after `since`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bound: bool,
    #[serde(default)]
    pub mac: String,
}

const DEVICE_MAC: &str = "quire-sync/device-record/v1";

impl DeviceRecord {
    pub fn make(space: &SpaceId, device: &DeviceId, secret: &DeviceSecret, name: &str, keys: &KeySet) -> Result<Self, CryptoError> {
        Self::make_at(space, device, secret, name, keys, BTreeMap::new(), false)
    }

    /// [`Self::make`], vouching for this device's lanes up to `since`.
    pub fn make_at(
        space: &SpaceId,
        device: &DeviceId,
        secret: &DeviceSecret,
        name: &str,
        keys: &KeySet,
        since: BTreeMap<String, LanePos>,
        bound: bool,
    ) -> Result<Self, CryptoError> {
        let epoch = keys.current();
        let aad = format!("quire-sync/device-name/{space}/{device}");
        let name = keys.current_keys().seal(name.as_bytes(), aad.as_bytes())?;
        let mut r = Self {
            format: 1,
            space: space.clone(),
            device: device.clone(),
            x25519: hex(&secret.public()),
            quire: env!("CARGO_PKG_VERSION").to_string(),
            name,
            epoch,
            made: unix_now(),
            since,
            bound,
            mac: String::new(),
        };
        r.mac = hex(&keys.current_keys().mac(DEVICE_MAC, &r.body()));
        Ok(r)
    }

    fn body(&self) -> Vec<u8> {
        serde_json::to_vec(&Self { mac: String::new(), ..self.clone() }).unwrap_or_default()
    }

    /// Its public key and name, if it is authentic for `space` and `device`
    /// (the folder or object it was found in).
    pub fn verify(&self, keys: &KeySet, space: &SpaceId, device: &DeviceId) -> Option<([u8; 32], String)> {
        if self.space != *space || self.device != *device {
            return None;
        }
        let k = keys.get(self.epoch)?;
        if !k.verify_mac(DEVICE_MAC, &self.body(), &unhex(&self.mac)?) {
            return None;
        }
        let aad = format!("quire-sync/device-name/{space}/{device}");
        let name = k.open(&self.name, aad.as_bytes()).ok().and_then(|b| String::from_utf8(b).ok())?;
        let key: [u8; 32] = unhex(&self.x25519)?.as_slice().try_into().ok()?;
        Some((key, name))
    }
}

/// A device removed by an [`EpochRecord`], and the last of its revisions
/// the other devices still take, per lane (`""` for notes, `"shared"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub device: DeviceId,
    #[serde(default)]
    pub lanes: BTreeMap<String, LanePos>,
}

/// The new key, sealed for one remaining device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The device's public key it was sealed for, hex.
    pub key: String,
    /// The ephemeral public key, hex.
    pub eph: String,
    #[serde(with = "hex_bytes")]
    pub sealed: Vec<u8>,
}

/// A change of the vault's key: written when a device is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochRecord {
    pub format: u32,
    pub space: SpaceId,
    pub epoch: u32,
    /// The device that made it, and its public key (hex).
    pub maker: DeviceId,
    pub maker_key: String,
    /// Unix seconds.
    pub made: i64,
    pub removed: Vec<Removed>,
    /// Every device that keeps access, with the new key sealed for it.
    pub members: BTreeMap<DeviceId, Member>,
    /// The new key wrapped by the sync passphrase, for devices that join later.
    pub wrapped: WrappedKey,
    /// The previous epoch's key, sealed under this one.
    #[serde(with = "hex_bytes")]
    pub prev: Vec<u8>,
    /// Where every device's lanes stood as the maker saw them: lines
    /// sealed under an older key past there are refused, unless that
    /// device vouches for them in its own record.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<DeviceId, BTreeMap<String, LanePos>>,
    /// The snapshots in use when it was made (lane, name, SHA-256 of the
    /// manifest): the only snapshots under an older key still taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<(String, String, String)>,
    /// MAC of the record with this epoch's key: lets a device that got the
    /// key from the passphrase or the recovery key check the rest.
    #[serde(default)]
    pub check: String,
}

/// Why a key change can't be taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("this device was removed from the sync vault by another device")]
    Removed,
    #[error("this device doesn't have the vault's new key")]
    NotMember,
    #[error(
        "two devices changed the vault's key at the same moment and one removed the other ({0}); choose the one you trust: `quire-cli sync trust-key`"
    )]
    Conflict(String),
    #[error("a key change in the sync vault doesn't check out: {0}")]
    Invalid(String),
    #[error("wrong passphrase")]
    WrongPassphrase,
}

const RECORD_MAC: &str = "quire-sync/epoch-record/v1";

impl EpochRecord {
    /// What every member's sealed key is bound to: everything public in the
    /// record except the sealed keys themselves.
    fn context(&self) -> Vec<u8> {
        let members: Vec<(&DeviceId, &str)> = self.members.iter().map(|(d, m)| (d, m.key.as_str())).collect();
        let head = serde_json::json!({
            "format": self.format, "space": self.space, "epoch": self.epoch, "maker": self.maker,
            "maker_key": self.maker_key, "made": self.made, "removed": self.removed, "members": members,
        });
        let mut ctx = b"quire-sync/epoch/v1\0".to_vec();
        ctx.extend_from_slice(&Sha256::digest(serde_json::to_vec(&head).unwrap_or_default()));
        ctx
    }

    fn body(&self) -> Vec<u8> {
        serde_json::to_vec(&Self { check: String::new(), ..self.clone() }).unwrap_or_default()
    }

    fn prev_aad(space: &SpaceId, epoch: u32) -> Vec<u8> {
        format!("quire-sync/epoch-prev/{space}/{epoch}").into_bytes()
    }

    /// A new epoch after `keys`' current one: a new key sealed for each of
    /// `members` (device, public key), wrapped by `passphrase`.
    #[allow(clippy::too_many_arguments, reason = "everything a key change needs, each once")]
    pub fn make(
        space: &SpaceId,
        keys: &KeySet,
        maker: &DeviceId,
        maker_secret: &DeviceSecret,
        members: &[(DeviceId, [u8; 32])],
        removed: Vec<Removed>,
        passphrase: &str,
        kdf: KdfParams,
    ) -> Result<(Self, VaultKey), CryptoError> {
        let prev = keys.current_raw().ok_or_else(|| CryptoError::UnknownEpoch(keys.current()))?;
        let new = VaultKey::generate()?;
        let epoch = keys.current() + 1;
        let new_keys = new.keys();
        let prev_sealed = seal_with(&new_bytes(&new), prev.as_bytes(), &Self::prev_aad(space, epoch))?;
        let mut r = Self {
            format: 1,
            space: space.clone(),
            epoch,
            maker: maker.clone(),
            maker_key: hex(&maker_secret.public()),
            made: unix_now(),
            removed,
            members: members.iter().map(|(d, k)| (d.clone(), Member { key: hex(k), eph: String::new(), sealed: Vec::new() })).collect(),
            wrapped: WrappedKey::wrap(&new, passphrase, kdf)?,
            prev: prev_sealed,
            seen: BTreeMap::new(),
            snapshots: Vec::new(),
            check: String::new(),
        };
        let ctx = r.context();
        for (d, k) in members {
            let (eph, sealed) = seal_to(maker_secret, k, new.as_bytes(), &ctx)?;
            if let Some(m) = r.members.get_mut(d) {
                m.eph = hex(&eph);
                m.sealed = sealed;
            }
        }
        r.check = hex(&new_keys.mac(RECORD_MAC, &r.body()));
        Ok((r, new))
    }

    /// Add where every device's lanes stood and the snapshots in use, and
    /// authenticate the record again with its key.
    pub fn with_context(
        mut self,
        key: &VaultKey,
        seen: BTreeMap<DeviceId, BTreeMap<String, LanePos>>,
        snapshots: Vec<(String, String, String)>,
    ) -> Self {
        self.seen = seen;
        self.snapshots = snapshots;
        self.check = hex(&key.keys().mac(RECORD_MAC, &self.body()));
        self
    }

    /// Whether `key` is this epoch's key and the record is as its maker wrote it.
    pub fn verify(&self, key: &VaultKey) -> bool {
        unhex(&self.check).is_some_and(|tag| key.keys().verify_mac(RECORD_MAC, &self.body(), &tag))
    }

    fn checked(&self, key: VaultKey) -> Result<VaultKey, KeyError> {
        if self.verify(&key) { Ok(key) } else { Err(KeyError::Invalid("its check doesn't match".into())) }
    }

    /// This epoch's key, for device `me`. `maker_key` is the maker's
    /// public key as pinned here; it must be the one the record names.
    pub fn open_as(&self, me: &DeviceId, secret: &DeviceSecret, maker_key: &[u8; 32]) -> Result<VaultKey, KeyError> {
        if self.removed.iter().any(|r| r.device == *me) {
            return Err(KeyError::Removed);
        }
        let Some(m) = self.members.get(me) else { return Err(KeyError::NotMember) };
        if m.key != hex(&secret.public()) {
            return Err(KeyError::NotMember);
        }
        if self.maker_key != hex(maker_key) {
            return Err(KeyError::Invalid("its maker's key isn't the one this device knows".into()));
        }
        let eph: [u8; 32] = unhex(&m.eph).and_then(|e| e.as_slice().try_into().ok()).ok_or_else(|| KeyError::Invalid("bad key".into()))?;
        let mut bytes = open_from(secret, maker_key, &eph, &m.sealed, &self.context())
            .map_err(|_| KeyError::Invalid("its key for this device doesn't open".into()))?;
        let arr: Result<[u8; 32], _> = bytes.as_slice().try_into();
        bytes.zeroize();
        self.checked(VaultKey::from_bytes(arr.map_err(|_| KeyError::Invalid("bad key".into()))?))
    }

    /// This epoch's key, from the sync passphrase.
    pub fn open_with_passphrase(&self, passphrase: &str) -> Result<VaultKey, KeyError> {
        let k = self.wrapped.unwrap(passphrase).map_err(|_| KeyError::WrongPassphrase)?;
        self.checked(k)
    }

    /// The previous epoch's key, from this one's.
    pub fn prev_key(&self, key: &VaultKey) -> Result<VaultKey, KeyError> {
        let mut bytes = open_with(&new_bytes(key), &self.prev, &Self::prev_aad(&self.space, self.epoch))
            .map_err(|_| KeyError::Invalid("the previous key doesn't open".into()))?;
        let arr: Result<[u8; 32], _> = bytes.as_slice().try_into();
        bytes.zeroize();
        Ok(VaultKey::from_bytes(arr.map_err(|_| KeyError::Invalid("bad key".into()))?))
    }

    pub fn removes(&self, d: &DeviceId) -> bool {
        self.removed.iter().any(|r| r.device == *d)
    }
}

/// A key's own bytes, to seal with directly (the previous key is sealed
/// with the new raw key, not a subkey, so the chain needs nothing else).
fn new_bytes(k: &VaultKey) -> [u8; 32] {
    *k.as_bytes()
}

/// The records of one epoch in the order they're tried.
///
/// Two devices may remove a device at the same moment (a synced folder has no
/// compare-and-swap): every device puts the candidates in the same order,
/// from what anyone can see: a record whose maker another candidate removes
/// goes last, then by maker id. Each device takes the first that opens for
/// it, which is the same one on every device that can open both.
pub fn ordered(cands: &[EpochRecord]) -> Vec<&EpochRecord> {
    let removed: BTreeSet<&DeviceId> = cands.iter().flat_map(|c| c.removed.iter().map(|r| &r.device)).collect();
    let mut v: Vec<&EpochRecord> = cands.iter().collect();
    v.sort_by(|a, b| (removed.contains(&a.maker), &a.maker, &a.check).cmp(&(removed.contains(&b.maker), &b.maker, &b.check)));
    v
}

/// Where a device stands after reading a vault's key records.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub keys: KeySet,
    /// The record taken for each epoch above 0.
    pub records: BTreeMap<u32, EpochRecord>,
    /// Every device removed so far, with what is still taken from it.
    pub removed: BTreeMap<DeviceId, Removed>,
    /// Why a newer epoch couldn't be taken (this device was removed, or
    /// isn't in it).
    pub stuck: Option<(u32, KeyError)>,
    /// What each device vouches for in its record (filled by
    /// [`crate::admin::refresh_keys`]): the epoch that authenticates the
    /// record, where its lanes stood, and whether it binds headers.
    pub vouched: BTreeMap<DeviceId, (u32, BTreeMap<String, LanePos>, bool)>,
}

impl Resolved {
    /// What the lines and snapshots of lane `lane` (`""` for notes) must
    /// be to be taken, from the key changes taken and the devices' records.
    pub fn rules(&self, lane: &str) -> LaneRules {
        LaneRules {
            changes: self
                .records
                .iter()
                // A record made before 0.13.1 says nothing about lanes.
                .filter(|(_, r)| !r.seen.is_empty())
                .map(|(e, r)| (*e, r.seen.iter().filter_map(|(d, l)| Some((d.clone(), *l.get(lane)?))).collect()))
                .collect(),
            vouched: self.vouched.iter().filter_map(|(d, (e, since, bound))| Some((d.clone(), (*e, *since.get(lane)?, *bound)))).collect(),
            snapshots: self
                .records
                .values()
                .next_back()
                .map(|r| r.snapshots.iter().filter(|(l, _, _)| l == lane).map(|(_, n, h)| (n.clone(), h.clone())).collect())
                .unwrap_or_default(),
        }
    }
}

/// What a lane's lines and snapshots must be, once the vault's key has
/// changed (`docs/design/0.13-sync.md` §2). A removed device still has the
/// older keys; these rules keep what it writes with them from being taken.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneRules {
    /// Each key change taken (with its epoch), and where each device's
    /// lane stood as its maker read it: past there, a line must be sealed
    /// under that epoch or a newer one.
    pub changes: Vec<(u32, BTreeMap<DeviceId, LanePos>)>,
    /// What each device says in its own record, authenticated under the
    /// epoch given: its lane up to the position is its own (lines sealed
    /// under older keys there are taken), and, when the flag is set, it
    /// binds headers (`v3`) after it.
    pub vouched: BTreeMap<DeviceId, (u32, LanePos, bool)>,
    /// Snapshots made under an older key that the newest key change names
    /// (name, SHA-256 of the manifest): the only such snapshots taken.
    pub snapshots: Vec<(String, String)>,
}

impl LaneRules {
    /// Whether device `dev`'s line at `(seg, line)`, numbered `n`, sealed
    /// as `sealed`, may be taken. A line refused here may become fine
    /// later, when its device vouches for it.
    pub fn allows(&self, dev: &DeviceId, seg: u32, line: u64, n: u64, sealed: &[u8]) -> bool {
        let epoch = crate::crypto::sealed_epoch(sealed).unwrap_or(0);
        let v3 = sealed.first() == Some(&crate::crypto::SEALED_V2);
        let own = self.vouched.get(dev);
        // Only a record under the newest key says where binding starts
        // (a removed device could write one under an older key).
        let top = self.changes.iter().map(|c| c.0).max().unwrap_or(0);
        if let Some((ve, since, true)) = own
            && *ve >= top
            && !v3
            && !since.covers(seg, line, n)
        {
            return false;
        }
        self.changes.iter().all(|(e, seen)| {
            epoch >= *e
                || seen.get(dev).is_some_and(|p| p.covers(seg, line, n))
                || own.is_some_and(|(ve, since, _)| *ve >= *e && since.covers(seg, line, n))
        })
    }

    /// Whether a snapshot authenticated under `epoch` may be taken.
    pub fn takes_snapshot(&self, current: u32, epoch: u32, name: &str, manifest_sha256: &str) -> bool {
        epoch == current || self.snapshots.iter().any(|(n, h)| n == name && h == manifest_sha256)
    }
}

/// Bring `keys` up to date with the `records` found: take each newer epoch
/// this device is in, and work out older keys from the newest one.
/// `pins` holds the device keys known here (makers must be in it).
pub fn resolve(keys: &KeySet, records: &[EpochRecord], me: &DeviceId, secret: Option<&DeviceSecret>, pins: &Pins) -> Resolved {
    resolve_trusting(keys, records, me, secret, pins, &BTreeMap::new())
}

/// [`resolve`], with the person's choice for epochs whose candidates
/// conflict: `trust[e]` is the maker whose record of epoch `e` to take.
///
/// Candidates conflict when they remove each other's makers (two devices
/// removing each other, or a removed device, which still has the old key,
/// removing the device that removed it). No rule can tell which one is
/// honest, so none is taken until the person chooses.
pub fn resolve_trusting(
    keys: &KeySet,
    records: &[EpochRecord],
    me: &DeviceId,
    secret: Option<&DeviceSecret>,
    pins: &Pins,
    trust: &BTreeMap<u32, DeviceId>,
) -> Resolved {
    let mut by_epoch: BTreeMap<u32, Vec<EpochRecord>> = BTreeMap::new();
    for r in records {
        by_epoch.entry(r.epoch).or_default().push(r.clone());
    }
    let mut out = keys.clone();
    let mut taken: BTreeMap<u32, EpochRecord> = BTreeMap::new();
    // Older epochs: the record whose `check` the key we have opens.
    for e in (1..=keys.current()).rev() {
        let Some(k) = out.raw(e).cloned() else { continue };
        if let Some(r) = by_epoch.get(&e).and_then(|c| c.iter().find(|r| r.verify(&k))) {
            if out.get(e - 1).is_none()
                && let Ok(p) = r.prev_key(&k)
            {
                out.insert(e - 1, p);
            }
            taken.insert(e, r.clone());
        }
    }
    let mut stuck = None;
    let mut e = out.current() + 1;
    while let Some(cands) = by_epoch.get(&e) {
        // A maker must be a device of the epoch before it (or any device, for epoch 1).
        let prev_members = taken.get(&(e - 1)).map(|r| &r.members);
        let eligible: Vec<&EpochRecord> = ordered(cands)
            .into_iter()
            .filter(|r| {
                prev_members.is_none_or(|m| m.contains_key(&r.maker))
                    && !taken.values().any(|t| t.removes(&r.maker))
                    // Its maker is a device known here, with the key it names.
                    && pins.get(&r.maker).is_some_and(|k| hex(&k) == r.maker_key)
            })
            .collect();
        let conflict = eligible.iter().any(|a| eligible.iter().any(|b| a.maker != b.maker && a.removes(&b.maker) && b.removes(&a.maker)));
        let chosen: Vec<&EpochRecord> = match (conflict, trust.get(&e)) {
            (false, _) => eligible,
            (true, Some(m)) => eligible.into_iter().filter(|r| r.maker == *m).collect(),
            (true, None) => {
                let mut makers: Vec<String> = eligible.iter().map(|r| r.maker.0.get(..8).unwrap_or(&r.maker.0).to_string()).collect();
                makers.sort();
                makers.dedup();
                stuck = Some((e, KeyError::Conflict(makers.join(", "))));
                break;
            }
        };
        let mut got = None;
        let mut why = KeyError::NotMember;
        for r in chosen {
            let Some(secret) = secret else { break };
            let Some(maker_key) = pins.get(&r.maker) else {
                why = KeyError::Invalid("its maker's device key isn't known here yet".into());
                continue;
            };
            match r.open_as(me, secret, &maker_key) {
                Ok(k) => {
                    // The chain must lead back to the key this device has.
                    if r.prev_key(&k).ok().as_ref().map(VaultKey::as_bytes) == out.raw(e - 1).map(VaultKey::as_bytes) {
                        got = Some((r.clone(), k));
                        break;
                    }
                    why = KeyError::Invalid("it doesn't follow the vault's key".into());
                }
                Err(KeyError::Removed) => why = KeyError::Removed,
                Err(err) if why != KeyError::Removed => why = err,
                Err(_) => {}
            }
        }
        let Some((r, k)) = got else {
            stuck = Some((e, why));
            break;
        };
        out.insert(e, k);
        taken.insert(e, r);
        e += 1;
    }
    let mut removed = BTreeMap::new();
    for r in taken.values() {
        for x in &r.removed {
            removed.insert(x.device.clone(), x.clone());
        }
    }
    Resolved { keys: out, records: taken, removed, stuck, vouched: BTreeMap::new() }
}

/// The keys of a vault for someone joining with its passphrase.
///
/// The newest epoch the passphrase opens, and every older one through
/// `prev`; an error when it opens none (a wrong one). `epoch0` is the vault's
/// first wrapped key (`space.json`, the server's key).
pub fn join_with_passphrase(epoch0: &WrappedKey, records: &[EpochRecord], passphrase: &str) -> Result<KeySet, KeyError> {
    let mut best: Option<(u32, VaultKey)> = None;
    for r in records {
        if best.as_ref().is_some_and(|(e, _)| *e >= r.epoch) {
            continue;
        }
        if let Ok(k) = r.open_with_passphrase(passphrase) {
            best = Some((r.epoch, k));
        }
    }
    let Some((epoch, key)) = best else {
        // No key change opens with it: the first key, if no change was ever made.
        if !records.is_empty() {
            return Err(KeyError::WrongPassphrase);
        }
        return epoch0.unwrap(passphrase).map(|k| KeySet::new(&k)).map_err(|_| KeyError::WrongPassphrase);
    };
    Ok(walk_down(epoch, key, records))
}

/// The keys of a vault from a recovery key (the key of some epoch).
pub fn join_with_key(key: &VaultKey, records: &[EpochRecord]) -> KeySet {
    match records.iter().filter(|r| r.verify(key)).max_by_key(|r| r.epoch) {
        Some(r) => walk_down(r.epoch, key.clone(), records),
        None => KeySet::new(key),
    }
}

fn walk_down(epoch: u32, key: VaultKey, records: &[EpochRecord]) -> KeySet {
    let mut set = KeySet { current: epoch, keys: BTreeMap::new(), raw: BTreeMap::new() };
    set.insert(epoch, key.clone());
    let mut cur = (epoch, key);
    while cur.0 > 0 {
        let Some(r) = records.iter().find(|r| r.epoch == cur.0 && r.verify(&cur.1)) else { break };
        let Ok(p) = r.prev_key(&cur.1) else { break };
        set.insert(cur.0 - 1, p.clone());
        cur = (cur.0 - 1, p);
    }
    set.current = epoch;
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> (DeviceId, DeviceSecret) {
        (DeviceId::random().unwrap(), DeviceSecret::generate().unwrap())
    }

    #[test]
    fn boxes_open_only_for_their_device_and_maker() {
        let (maker, other, to) = (DeviceSecret::generate().unwrap(), DeviceSecret::generate().unwrap(), DeviceSecret::generate().unwrap());
        let (eph, sealed) = seal_to(&maker, &to.public(), b"secret", b"ctx").unwrap();
        assert_eq!(open_from(&to, &maker.public(), &eph, &sealed, b"ctx").unwrap(), b"secret");
        // Another device, another claimed maker, another context: no.
        assert!(open_from(&other, &maker.public(), &eph, &sealed, b"ctx").is_err());
        assert!(open_from(&to, &other.public(), &eph, &sealed, b"ctx").is_err());
        assert!(open_from(&to, &maker.public(), &eph, &sealed, b"other").is_err());
        // A low-order point is refused.
        assert!(open_from(&to, &[0; 32], &eph, &sealed, b"ctx").is_err());
    }

    #[test]
    fn removing_a_device_gives_the_new_key_to_the_others_only() {
        let space = SpaceId::random().unwrap();
        let k0 = VaultKey::generate().unwrap();
        let keys = KeySet::new(&k0);
        let ((a, sa), (b, sb), (r, sr)) = (dev(), dev(), dev());
        let mut pins = Pins::default();
        for (d, s) in [(&a, &sa), (&b, &sb), (&r, &sr)] {
            assert!(pins.pin(d, &s.public()));
        }
        let removed = vec![Removed { device: r.clone(), lanes: BTreeMap::new() }];
        let (rec, k1) = EpochRecord::make(
            &space,
            &keys,
            &a,
            &sa,
            &[(a.clone(), sa.public()), (b.clone(), sb.public())],
            removed,
            "new pass",
            KdfParams::TEST,
        )
        .unwrap();
        // B takes it; the removed device can't.
        let rb = resolve(&keys, std::slice::from_ref(&rec), &b, Some(&sb), &pins);
        assert_eq!(rb.keys.current(), 1);
        assert_eq!(rb.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
        assert!(rb.removed.contains_key(&r));
        let rr = resolve(&keys, std::slice::from_ref(&rec), &r, Some(&sr), &pins);
        assert_eq!(rr.keys.current(), 0);
        assert_eq!(rr.stuck.unwrap().1, KeyError::Removed);
        // The passphrase and the new recovery key open everything.
        let wrapped0 = WrappedKey::wrap(&k0, "old pass", KdfParams::TEST).unwrap();
        let j = join_with_passphrase(&wrapped0, std::slice::from_ref(&rec), "new pass").unwrap();
        assert_eq!((j.current(), j.raw(0).unwrap().as_bytes()), (1, k0.as_bytes()));
        assert!(join_with_passphrase(&wrapped0, std::slice::from_ref(&rec), "old pass").is_err());
        let j = join_with_key(&k1, std::slice::from_ref(&rec));
        assert_eq!(j.raw(0).unwrap().as_bytes(), k0.as_bytes());
        // A changed record doesn't open.
        let mut bad = rec;
        bad.removed.clear();
        assert_eq!(resolve(&keys, &[bad], &b, Some(&sb), &pins).keys.current(), 0);
    }

    #[test]
    fn a_removed_device_cant_win_a_race_with_its_own_record() {
        let space = SpaceId::random().unwrap();
        let keys = KeySet::new(&VaultKey::generate().unwrap());
        let ((a, sa), (b, sb), (r, sr)) = (dev(), dev(), dev());
        let mut pins = Pins::default();
        for (d, s) in [(&a, &sa), (&b, &sb), (&r, &sr)] {
            pins.pin(d, &s.public());
        }
        let all = |x: &[(&DeviceId, &DeviceSecret)]| x.iter().map(|(d, s)| ((*d).clone(), s.public())).collect::<Vec<_>>();
        let (real, k1) = EpochRecord::make(
            &space,
            &keys,
            &a,
            &sa,
            &all(&[(&a, &sa), (&b, &sb)]),
            vec![Removed { device: r.clone(), lanes: BTreeMap::new() }],
            "p",
            KdfParams::TEST,
        )
        .unwrap();
        // The removed device, offline, makes its own epoch 1 with a key it knows.
        let (rogue, _) =
            EpochRecord::make(&space, &keys, &r, &sr, &all(&[(&a, &sa), (&b, &sb), (&r, &sr)]), vec![], "p", KdfParams::TEST).unwrap();
        for order in [vec![real.clone(), rogue.clone()], vec![rogue.clone(), real.clone()]] {
            let got = resolve(&keys, &order, &b, Some(&sb), &pins);
            assert_eq!(got.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
        }
        // The rogue record also removes the device that removed it: no
        // rule can tell which is honest, so neither is taken until the
        // person chooses.
        let (rogue, k_rogue) = EpochRecord::make(
            &space,
            &keys,
            &r,
            &sr,
            &all(&[(&b, &sb), (&r, &sr)]),
            vec![Removed { device: a.clone(), lanes: BTreeMap::new() }],
            "p",
            KdfParams::TEST,
        )
        .unwrap();
        for order in [vec![real.clone(), rogue.clone()], vec![rogue.clone(), real.clone()]] {
            let got = resolve(&keys, &order, &b, Some(&sb), &pins);
            assert_eq!(got.keys.current(), 0);
            assert!(matches!(got.stuck, Some((1, KeyError::Conflict(_)))), "{:?}", got.stuck);
            let trust = BTreeMap::from([(1, a.clone())]);
            let got = resolve_trusting(&keys, &order, &b, Some(&sb), &pins, &trust);
            assert_eq!(got.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
            let trust = BTreeMap::from([(1, r.clone())]);
            let got = resolve_trusting(&keys, &order, &b, Some(&sb), &pins, &trust);
            assert_eq!(got.keys.raw(1).unwrap().as_bytes(), k_rogue.as_bytes(), "the person's choice, even a bad one");
        }
        // A maker this device never pinned (a device the removed one made
        // up) isn't a candidate at all.
        let (m, sm) = dev();
        let (fake, _) = EpochRecord::make(
            &space,
            &keys,
            &m,
            &sm,
            &all(&[(&b, &sb), (&m, &sm)]),
            vec![Removed { device: a.clone(), lanes: BTreeMap::new() }],
            "p",
            KdfParams::TEST,
        )
        .unwrap();
        for order in [vec![real.clone(), fake.clone()], vec![fake.clone(), real.clone()]] {
            let got = resolve(&keys, &order, &b, Some(&sb), &pins);
            assert_eq!(got.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
        }
    }

    #[test]
    fn device_records_are_authentic_or_ignored() {
        let space = SpaceId::random().unwrap();
        let keys = KeySet::new(&VaultKey::generate().unwrap());
        let (d, s) = dev();
        let rec = DeviceRecord::make(&space, &d, &s, "Asha's laptop", &keys).unwrap();
        assert_eq!(rec.verify(&keys, &space, &d), Some((s.public(), "Asha's laptop".to_string())));
        assert!(!serde_json::to_string(&rec).unwrap().contains("Asha"), "the name is sealed");
        assert!(rec.verify(&keys, &space, &DeviceId::random().unwrap()).is_none(), "found in another device's folder");
        let mut forged = rec;
        forged.x25519 = hex(&DeviceSecret::generate().unwrap().public());
        assert!(forged.verify(&keys, &space, &d).is_none());
    }
}
