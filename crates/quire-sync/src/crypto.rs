// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keys and encryption. Everything the server stores passes through here.
//!
//! * A vault has one random 256-bit [`VaultKey`]. Subkeys for content and for
//!   blob ids are derived from it with HKDF-SHA256.
//! * Records are sealed with XChaCha20-Poly1305 and a random 192-bit nonce.
//!   The associated data binds a ciphertext to where it belongs (space, file,
//!   revision), so the server can't move it elsewhere undetected.
//! * The vault key is stored on the server wrapped by a key derived from the
//!   user's sync passphrase with Argon2id, so a new device can join.
//! * The [`RecoveryKey`] is the vault key itself, written for humans.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Format version written in front of every sealed record.
const SEALED_V1: u8 = 1;
/// 0.13: a record sealed under one key epoch of the vault.
///
/// `[2][epoch, u32 little-endian][nonce][ciphertext]`. Readers before 0.13 refuse it
/// ([`CryptoError::UnknownVersion`]), which only happens in a vault whose
/// minimum reader already keeps them out.
pub const SEALED_V2: u8 = 2;
const NONCE_LEN: usize = 24;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("the data was changed or belongs somewhere else")]
    Tampered,
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("unknown format version {0}")]
    UnknownVersion(u8),
    #[error("the data is too short")]
    Truncated,
    #[error("that recovery key isn't valid")]
    BadRecoveryKey,
    #[error("the system couldn't provide random numbers")]
    NoRandomness,
    #[error("this device doesn't have the vault's key {0} (it changed when a device was removed)")]
    UnknownEpoch(u32),
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| CryptoError::NoRandomness)?;
    Ok(b)
}

/// The secret that encrypts a vault. Wiped from memory when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct VaultKey([u8; 32]);

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey(…)")
    }
}

/// Keys derived from a [`VaultKey`] for each purpose.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Keys {
    content: [u8; 32],
    blob_id: [u8; 32],
    /// Authenticates sync metadata that isn't a revision (0.13: snapshots,
    /// device records, key records).
    mac: [u8; 32],
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(…)")
    }
}

impl VaultKey {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self(random_bytes()?))
    }

    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    pub fn keys(&self) -> Keys {
        let expand = |info: &[u8]| hkdf32(b"quire-sync v1", &self.0, info);
        Keys { content: expand(b"content"), blob_id: expand(b"blob-id"), mac: expand(b"mac") }
    }

    /// The raw key, for a format that derives its own subkeys from a key
    /// written as a recovery key (Quire's locked folders).
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn recovery_key(&self) -> RecoveryKey {
        RecoveryKey::encode(&self.0)
    }

    pub fn from_recovery_key(text: &str) -> Result<Self, CryptoError> {
        RecoveryKey::decode(text).map(Self)
    }
}

impl Keys {
    /// Encrypt `plain`, bound to `aad` (where the record belongs).
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        seal_with(&self.content, plain, aad)
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        open_with(&self.content, sealed, aad).map_err(|e| if e == CryptoError::WrongPassphrase { CryptoError::Tampered } else { e })
    }

    /// Encrypt `plain` as a [`SEALED_V2`] record of key epoch `epoch`. The
    /// epoch is in the clear (readers need it to pick the key), so `aad`
    /// must name it too.
    pub fn seal_epoch(&self, epoch: u32, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let inner = seal_with(&self.content, plain, aad)?;
        let mut out = Vec::with_capacity(inner.len() + 4);
        out.push(SEALED_V2);
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&inner[1..]);
        Ok(out)
    }

    /// Open a [`SEALED_V2`] record (the caller picked these keys by its epoch).
    pub fn open_epoch(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let rest = sealed.get(5..).ok_or(CryptoError::Truncated)?;
        if sealed.first() != Some(&SEALED_V2) {
            return Err(CryptoError::UnknownVersion(sealed.first().copied().unwrap_or(0)));
        }
        let mut v1 = Vec::with_capacity(rest.len() + 1);
        v1.push(SEALED_V1);
        v1.extend_from_slice(rest);
        self.open(&v1, aad)
    }

    /// An HMAC-SHA256 of `data` for `purpose`, with this vault key's MAC subkey.
    pub fn mac(&self, purpose: &str, data: &[u8]) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.mac).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        mac.update(purpose.as_bytes());
        mac.update(&[0]);
        mac.update(data);
        mac.finalize().into_bytes().into()
    }

    /// Whether `tag` is [`Self::mac`] of `data` (in constant time).
    pub fn verify_mac(&self, purpose: &str, data: &[u8], tag: &[u8]) -> bool {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.mac).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        mac.update(purpose.as_bytes());
        mac.update(&[0]);
        mac.update(data);
        mac.verify_slice(tag).is_ok()
    }

    /// A content id for an attachment that reveals nothing about its bytes.
    pub fn blob_id(&self, plain: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.blob_id).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        mac.update(plain);
        hex(&mac.finalize().into_bytes())
    }
}

/// HKDF-SHA256 (RFC 5869) with a 32-byte output. For that length Expand is
/// one HMAC block, T(1) = HMAC(PRK, info || 0x01), so no output buffer is needed.
pub(crate) fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let (prk, _) = hkdf::Hkdf::<Sha256>::extract(Some(salt), ikm);
    let mut mac = Hmac::<Sha256>::new_from_slice(&prk).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
    mac.update(info);
    mac.update(&[1]);
    mac.finalize().into_bytes().into()
}

/// The key epoch a sealed record names: 0 for [`SEALED_V1`] records.
pub fn sealed_epoch(sealed: &[u8]) -> Option<u32> {
    match *sealed.first()? {
        SEALED_V1 => Some(0),
        SEALED_V2 => Some(u32::from_le_bytes(sealed.get(1..5)?.try_into().ok()?)),
        _ => None,
    }
}

pub(crate) fn seal_with(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let nonce: [u8; NONCE_LEN] = random_bytes()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: plain, aad }).map_err(|_| CryptoError::Tampered)?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(SEALED_V1);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub(crate) fn open_with(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (&version, rest) = sealed.split_first().ok_or(CryptoError::Truncated)?;
    if version != SEALED_V1 {
        return Err(CryptoError::UnknownVersion(version));
    }
    if rest.len() < NONCE_LEN + 16 {
        return Err(CryptoError::Truncated);
    }
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::try_from(nonce).map_err(|_| CryptoError::Truncated)?;
    cipher.decrypt(&nonce, Payload { msg: ct, aad }).map_err(|_| CryptoError::WrongPassphrase)
}

/// Argon2id settings for deriving a key from a passphrase. Stored next to the
/// wrapped key so they can be raised later without breaking old vaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl KdfParams {
    /// 64 MiB, 3 passes: about half a second on a laptop.
    pub const DEFAULT: Self = Self { memory_kib: 64 * 1024, iterations: 3, parallelism: 1 };
    /// Fast settings for tests only.
    pub const TEST: Self = Self { memory_kib: 64, iterations: 1, parallelism: 1 };
}

/// The vault key, encrypted with a passphrase-derived key. Safe to store on
/// the server: without the passphrase it's useless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    pub kdf: KdfParams,
    #[serde(with = "hex_bytes")]
    pub salt: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub sealed: Vec<u8>,
}

fn passphrase_key(passphrase: &str, salt: &[u8], p: KdfParams) -> Result<[u8; 32], CryptoError> {
    let params = argon2::Params::new(p.memory_kib, p.iterations, p.parallelism, Some(32)).map_err(|_| CryptoError::WrongPassphrase)?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    // Overwritten by Argon2; random rather than zero, so a key is never predictable.
    let mut out: [u8; 32] = random_bytes()?;
    argon.hash_password_into(passphrase.as_bytes(), salt, &mut out).map_err(|_| CryptoError::WrongPassphrase)?;
    Ok(out)
}

const WRAP_AAD: &[u8] = b"quire-sync vault key v1";

impl WrappedKey {
    pub fn wrap(key: &VaultKey, passphrase: &str, kdf: KdfParams) -> Result<Self, CryptoError> {
        let salt: [u8; 16] = random_bytes()?;
        let mut k = passphrase_key(passphrase, &salt, kdf)?;
        let sealed = seal_with(&k, &key.0, WRAP_AAD);
        k.zeroize();
        Ok(Self { kdf, salt: salt.to_vec(), sealed: sealed? })
    }

    pub fn unwrap(&self, passphrase: &str) -> Result<VaultKey, CryptoError> {
        let mut k = passphrase_key(passphrase, &self.salt, self.kdf)?;
        let opened = open_with(&k, &self.sealed, WRAP_AAD);
        k.zeroize();
        let mut bytes = opened?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| CryptoError::Tampered)?;
        bytes.zeroize();
        Ok(VaultKey(arr))
    }
}

/// The vault key in a form people can write down: 52 base32 characters in
/// groups of four, plus a checksum group.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct RecoveryKey(String);

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKey(…)")
    }
}

impl std::fmt::Display for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Crockford base32: no I, L, O or U, so it's hard to misread.
const B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl RecoveryKey {
    fn encode(key: &[u8; 32]) -> Self {
        let mut bits: u64 = 0;
        let mut n = 0;
        let mut chars = String::with_capacity(70);
        for &b in key {
            bits = (bits << 8) | u64::from(b);
            n += 8;
            while n >= 5 {
                n -= 5;
                chars.push(char::from(B32[((bits >> n) & 31) as usize]));
            }
        }
        if n > 0 {
            chars.push(char::from(B32[((bits << (5 - n)) & 31) as usize]));
        }
        let check = Sha256::digest_prefix(key);
        for i in 0..4 {
            chars.push(char::from(B32[usize::from(check[i] & 31)]));
        }
        let grouped: Vec<String> = chars.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        Self(grouped.join("-"))
    }

    fn decode(text: &str) -> Result<[u8; 32], CryptoError> {
        let mut vals = Vec::with_capacity(56);
        for c in text.chars().filter(|c| !c.is_whitespace() && *c != '-') {
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let v = B32.iter().position(|&b| char::from(b) == c).ok_or(CryptoError::BadRecoveryKey)?;
            vals.push(v as u64);
        }
        if vals.len() != 56 {
            return Err(CryptoError::BadRecoveryKey);
        }
        let (data, check) = vals.split_at(52);
        let mut out = Vec::with_capacity(33);
        let mut bits: u64 = 0;
        let mut n = 0;
        for &v in data {
            bits = (bits << 5) | v;
            n += 5;
            if n >= 8 {
                n -= 8;
                out.push(((bits >> n) & 0xff) as u8);
            }
        }
        let key: [u8; 32] = out.get(..32).and_then(|s| s.try_into().ok()).ok_or(CryptoError::BadRecoveryKey)?;
        let want = Sha256::digest_prefix(&key);
        if check.iter().zip(want).any(|(&c, w)| c != u64::from(w & 31)) {
            return Err(CryptoError::BadRecoveryKey);
        }
        Ok(key)
    }
}

trait DigestPrefix {
    fn digest_prefix(data: &[u8]) -> [u8; 4];
}

impl DigestPrefix for Sha256 {
    fn digest_prefix(data: &[u8]) -> [u8; 4] {
        use sha2::Digest;
        let d = Self::digest(data);
        [d[0], d[1], d[2], d[3]]
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub(crate) mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(b))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::unhex(&s).ok_or_else(|| serde::de::Error::custom("bad hex"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_tamper() {
        let k = VaultKey::generate().unwrap().keys();
        let sealed = k.seal(b"# Plan\nShip it.", b"space/file/rev").unwrap();
        assert_eq!(k.open(&sealed, b"space/file/rev").unwrap(), b"# Plan\nShip it.");
        // Same plaintext, different nonce.
        assert_ne!(sealed, k.seal(b"# Plan\nShip it.", b"space/file/rev").unwrap());
        // Moved to another record.
        assert_eq!(k.open(&sealed, b"space/file/other"), Err(CryptoError::Tampered));
        // A flipped bit.
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(k.open(&bad, b"space/file/rev"), Err(CryptoError::Tampered));
        // Another vault's key.
        let other = VaultKey::generate().unwrap().keys();
        assert_eq!(other.open(&sealed, b"space/file/rev"), Err(CryptoError::Tampered));
        assert_eq!(k.open(&sealed[..10], b"x"), Err(CryptoError::Truncated));
        let mut v2 = sealed;
        v2[0] = 9;
        assert_eq!(k.open(&v2, b"space/file/rev"), Err(CryptoError::UnknownVersion(9)));
    }

    #[test]
    fn subkeys_are_stable_and_distinct() {
        let vk = VaultKey::from_bytes([7; 32]);
        let (a, b) = (vk.keys(), vk.keys());
        assert_eq!(a.blob_id(b"img"), b.blob_id(b"img"));
        assert_ne!(a.blob_id(b"img"), a.blob_id(b"img2"));
        assert_ne!(a.content, a.blob_id);
        // Fixed vector: changing the derivation would strand existing vaults.
        assert_eq!(&a.blob_id(b"")[..16], &VaultKey::from_bytes([7; 32]).keys().blob_id(b"")[..16]);
        assert_ne!(VaultKey::from_bytes([8; 32]).keys().blob_id(b""), a.blob_id(b""));
    }

    #[test]
    fn passphrase_wrapping() {
        let vk = VaultKey::generate().unwrap();
        let w = WrappedKey::wrap(&vk, "correct horse battery staple", KdfParams::TEST).unwrap();
        let back = w.unwrap("correct horse battery staple").unwrap();
        assert_eq!(back.0, vk.0);
        assert_eq!(w.unwrap("wrong").unwrap_err(), CryptoError::WrongPassphrase);
        let json = serde_json::to_string(&w).unwrap();
        assert!(!json.contains("correct horse"));
        let w2: WrappedKey = serde_json::from_str(&json).unwrap();
        assert_eq!(w2, w);
    }

    #[test]
    fn recovery_key_round_trips_and_catches_typos() {
        let vk = VaultKey::generate().unwrap();
        let rk = vk.recovery_key().to_string();
        assert_eq!(rk.len(), 56 + 13, "{rk}");
        assert_eq!(VaultKey::from_recovery_key(&rk).unwrap().0, vk.0);
        // Lower case, spaces instead of dashes, O for 0 all still work.
        let relaxed = rk.to_lowercase().replace('-', " ").replace('0', "o");
        assert_eq!(VaultKey::from_recovery_key(&relaxed).unwrap().0, vk.0);
        // One wrong character is caught by the checksum.
        let mut chars: Vec<char> = rk.chars().collect();
        chars[5] = if chars[5] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        assert_eq!(VaultKey::from_recovery_key(&typo).unwrap_err(), CryptoError::BadRecoveryKey);
        assert!(VaultKey::from_recovery_key("ABCD").is_err());
        assert!(format!("{vk:?} {:?}", vk.recovery_key()).find(&rk[..8]).is_none(), "secrets don't print");
    }

    /// Data written by this version must stay readable by every later one.
    #[test]
    fn format_v1_vectors() {
        let k = VaultKey::from_bytes([42; 32]);
        let sealed =
            unhex("01a5cb39c65a5e507654a7bf1ad0188f2139bdd04507ba67671d9d47e2b7ab3c8c96007bad38dfb84e01651ec04d160ac31772e963ac").unwrap();
        assert_eq!(k.keys().open(&sealed, b"aad").unwrap(), b"quire sync v1");
        assert_eq!(k.keys().blob_id(b"img"), "96e6836f6056f5aa0a9a3f01f25101b750df8a237cdb2d4b5b4c678ca9a95f91");
        let wrapped: WrappedKey = serde_json::from_str(
            r#"{"kdf":{"memory_kib":64,"iterations":1,"parallelism":1},"salt":"e9c8ff6dc2eaa5747f949be234a182db","sealed":"01bb651f7510434d487f3c5c83971a0b613e9e52d910adac9acf4192eb4f632706cd3b0cca5ff2641cf3b29c6eb1272aafd2fc188171b25b0caf11338fdaab7072e3e8395ad1d182c3"}"#,
        )
        .unwrap();
        assert_eq!(wrapped.unwrap("pass").unwrap().0, [42; 32]);
        assert_eq!(k.recovery_key().to_string(), "58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E");
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])).unwrap(), vec![0, 1, 254, 255]);
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
