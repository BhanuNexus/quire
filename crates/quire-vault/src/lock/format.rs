// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bytes of locked folders (docs/formats/locked-folders.md): `.qenc`
//! files and `quire-lock.json`, the folder's wrapped key.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// First bytes of every `.qenc` file.
pub const MAGIC: &[u8; 4] = b"QENC";
pub const VERSION: u8 = 1;
const ID_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// Magic, version, kind, folder id: the associated data.
const AAD_LEN: usize = 4 + 1 + 1 + ID_LEN;
/// The whole header, nonce included.
pub const HEADER_LEN: usize = AAD_LEN + NONCE_LEN;
/// Associated data of the wrapped key, followed by the folder id.
const WRAP_AAD: &[u8] = b"quire-lock key v1";
/// Associated data of the previous key (after a key change), followed by the folder id.
const PREVIOUS_AAD: &[u8] = b"quire-lock previous key v1";
/// HKDF salt of the folder key's subkeys.
const HKDF_SALT: &[u8] = b"quire-lock v1";

/// What a `.qenc` file holds; part of the associated data, so one can't
/// pass for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A note or attachment in the folder: the file's bytes.
    File = 1,
    /// A history snapshot of a note in the folder: the gzip of its text.
    Snapshot = 2,
}

impl Kind {
    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::File),
            2 => Some(Self::Snapshot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("that recovery key isn't valid for this folder")]
    BadRecoveryKey,
    #[error("the passphrase must be at least 8 characters")]
    WeakPassphrase,
    #[error("not an encrypted Quire file")]
    NotEncrypted,
    #[error("made by a newer Quire (format {0})")]
    Newer(u8),
    #[error("the file is cut short")]
    Truncated,
    #[error("the file belongs to another locked folder")]
    OtherFolder,
    #[error("the file isn't what it should be (a history version in place of a note, or the reverse)")]
    WrongKind,
    #[error("the file is damaged or was changed")]
    Damaged,
    #[error("the folder's key file can't be read: {0}")]
    KeyFile(String),
    #[error("the system couldn't provide random numbers")]
    NoRandomness,
}

pub(crate) fn random<const N: usize>() -> Result<[u8; N], LockError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| LockError::NoRandomness)?;
    Ok(b)
}

/// A locked folder's random id, in its key file and every file's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FolderId(pub [u8; ID_LEN]);

impl FolderId {
    pub fn generate() -> Result<Self, LockError> {
        Ok(Self(random()?))
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        unhex(s)?.try_into().ok().map(Self)
    }
}

/// The 256-bit key a folder's files are encrypted with. Wiped when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct FolderKey([u8; 32]);

impl std::fmt::Debug for FolderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderKey(…)")
    }
}

impl FolderKey {
    pub fn generate() -> Result<Self, LockError> {
        Ok(Self(random()?))
    }

    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    /// The key in the recovery key's alphabet (52 Crockford base32
    /// characters and a 4-character check, in groups of four), as Quire
    /// Sync writes its recovery key.
    pub fn recovery_key(&self) -> String {
        quire_sync::VaultKey::from_bytes(self.0).recovery_key().to_string()
    }

    /// The key a recovery key stands for (its own checksum checked; whether
    /// it opens a folder is [`KeyFile::check_key`]).
    pub fn from_recovery_key(text: &str) -> Result<Self, LockError> {
        let k = quire_sync::VaultKey::from_recovery_key(text).map_err(|_| LockError::BadRecoveryKey)?;
        Ok(Self(*k.as_bytes()))
    }

    /// For the OS keychain ("unlock automatically on this computer").
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let mut v = unhex(s.trim())?;
        let arr: Option<[u8; 32]> = v.as_slice().try_into().ok();
        v.zeroize();
        arr.map(Self)
    }

    /// HKDF-SHA256 subkey (RFC 5869; one block, so `T(1)` only).
    fn subkey(&self, info: &[u8]) -> [u8; 32] {
        let (prk, _) = hkdf::Hkdf::<Sha256>::extract(Some(HKDF_SALT), &self.0);
        let mut prk: [u8; 32] = prk.into();
        let mut mac = Hmac::<Sha256>::new_from_slice(&prk).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        prk.zeroize();
        mac.update(info);
        mac.update(&[1]);
        mac.finalize().into_bytes().into()
    }

    /// The value in the key file that tells whether a key is this folder's.
    fn check(&self) -> String {
        let mut c = self.subkey(b"check");
        let out = hex(&c[..16]);
        c.zeroize();
        out
    }

    /// Whether this key gives `check` (a key file's check value, hex).
    pub fn matches_check(&self, check: &str) -> bool {
        constant_eq(self.check().as_bytes(), check.as_bytes())
    }

    /// A keyed fingerprint of `bytes` (hex, 16 bytes of HMAC-SHA256 under a
    /// subkey): tells whether a file is the one a conversion recorded
    /// without keeping its text, or a hash anyone could test guesses
    /// against.
    pub fn tag(&self, bytes: &[u8]) -> String {
        let mut k = self.subkey(b"journal");
        let mut mac = Hmac::<Sha256>::new_from_slice(&k).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        k.zeroize();
        mac.update(bytes);
        hex(&mac.finalize().into_bytes()[..16])
    }

    /// Encrypt `plain` as a `.qenc` file of `folder`.
    pub fn seal(&self, kind: Kind, folder: FolderId, plain: &[u8]) -> Result<Vec<u8>, LockError> {
        let nonce: [u8; NONCE_LEN] = random()?;
        let mut out = Vec::with_capacity(HEADER_LEN + plain.len() + TAG_LEN);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(kind as u8);
        out.extend_from_slice(&folder.0);
        let mut key = self.subkey(b"file");
        let cipher = XChaCha20Poly1305::new((&key).into());
        key.zeroize();
        let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: plain, aad: &out }).map_err(|_| LockError::Damaged)?;
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt a `.qenc` file, which must be of `kind` and `folder`.
    pub fn open(&self, kind: Kind, folder: FolderId, data: &[u8]) -> Result<Vec<u8>, LockError> {
        let h = Header::parse(data)?;
        if h.folder != folder {
            return Err(LockError::OtherFolder);
        }
        if h.kind != kind {
            return Err(LockError::WrongKind);
        }
        let (aad, rest) = data.split_at(AAD_LEN);
        let (nonce, ct) = rest.split_at(NONCE_LEN);
        let mut key = self.subkey(b"file");
        let cipher = XChaCha20Poly1305::new((&key).into());
        key.zeroize();
        let nonce = XNonce::try_from(nonce).map_err(|_| LockError::Truncated)?;
        cipher.decrypt(&nonce, Payload { msg: ct, aad }).map_err(|_| LockError::Damaged)
    }
}

/// The readable part of a `.qenc` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub folder: FolderId,
}

impl Header {
    pub fn parse(data: &[u8]) -> Result<Self, LockError> {
        if data.len() < 5 || &data[..4] != MAGIC {
            return Err(if data.len() < 5 && MAGIC.starts_with(data) { LockError::Truncated } else { LockError::NotEncrypted });
        }
        if data[4] != VERSION {
            return Err(LockError::Newer(data[4]));
        }
        if data.len() < HEADER_LEN + TAG_LEN {
            return Err(LockError::Truncated);
        }
        let kind = Kind::from_byte(data[5]).ok_or(LockError::Damaged)?;
        let mut id = [0u8; ID_LEN];
        id.copy_from_slice(&data[6..AAD_LEN]);
        Ok(Self { kind, folder: FolderId(id) })
    }
}

/// Argon2id settings, stored in the key file so they can be raised later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    /// Always `argon2id`.
    pub name: Argon2id,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

/// `"argon2id"`, the only KDF of format 1 (a type so a key file naming
/// another one is refused rather than misread).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Argon2id;

impl Serialize for Argon2id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("argon2id")
    }
}

impl<'de> Deserialize<'de> for Argon2id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "argon2id" { Ok(Self) } else { Err(serde::de::Error::custom(format!("unknown kdf {s}"))) }
    }
}

impl Kdf {
    /// 64 MiB, 3 passes: Quire Sync's setting, about half a second.
    pub const DEFAULT: Self = Self { name: Argon2id, memory_kib: 64 * 1024, iterations: 3, parallelism: 1 };
    /// Fast settings for tests only: refused in a key file outside tests
    /// ([`Self::check_bounds`]).
    #[doc(hidden)]
    pub const TEST: Self = Self { name: Argon2id, memory_kib: 64, iterations: 1, parallelism: 1 };
    /// The most a key file may ask for: 4 GiB, 64 passes, 16 lanes.
    pub const MAX: Self = Self { name: Argon2id, memory_kib: 4 * 1024 * 1024, iterations: 64, parallelism: 16 };

    /// The weakest settings a key file may hold: [`Self::DEFAULT`] (anyone
    /// who can write the key file could otherwise lower them, so that a new
    /// passphrase is cheap to guess), or [`Self::TEST`] in Quire's own
    /// tests (the `weak-kdf` feature, which only tests turn on).
    const fn floor() -> Self {
        if cfg!(any(test, feature = "weak-kdf")) { Self::TEST } else { Self::DEFAULT }
    }

    /// Refuse settings below [`Self::floor`] or above [`Self::MAX`]: weak
    /// ones make a passphrase cheap to guess, huge ones make every unlock
    /// hang or run out of memory.
    pub fn check_bounds(&self) -> Result<(), LockError> {
        self.check_bounds_from(Self::floor())
    }

    fn check_bounds_from(&self, lo: Self) -> Result<(), LockError> {
        let hi = Self::MAX;
        let ok = |v: u32, lo: u32, hi: u32| (lo..=hi).contains(&v);
        if ok(self.memory_kib, lo.memory_kib, hi.memory_kib)
            && ok(self.iterations, lo.iterations, hi.iterations)
            && ok(self.parallelism, 1, hi.parallelism)
            && self.memory_kib >= 8 * self.parallelism
        {
            Ok(())
        } else {
            Err(LockError::KeyFile(format!(
                "its Argon2id settings ({} KiB, {} passes, {} lanes) are outside what Quire accepts ({}–{} KiB, {}–{} passes); \
                 someone may have changed the file. Restore it from a backup, or use the recovery key",
                self.memory_kib, self.iterations, self.parallelism, lo.memory_kib, hi.memory_kib, lo.iterations, hi.iterations
            )))
        }
    }

    /// The stronger of these settings and `lo`, per field: what a new
    /// passphrase is wrapped with, so it never keeps settings someone
    /// lowered in the key file.
    fn at_least(self, lo: Self) -> Self {
        Self {
            name: Argon2id,
            memory_kib: self.memory_kib.max(lo.memory_kib),
            iterations: self.iterations.max(lo.iterations),
            parallelism: self.parallelism.max(lo.parallelism),
        }
    }

    fn derive(&self, passphrase: &str, salt: &[u8]) -> Result<[u8; 32], LockError> {
        self.check_bounds()?;
        let params = argon2::Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32))
            .map_err(|e| LockError::KeyFile(format!("argon2 settings: {e}")))?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out: [u8; 32] = random()?;
        argon.hash_password_into(passphrase.as_bytes(), salt, &mut out).map_err(|e| LockError::KeyFile(format!("argon2: {e}")))?;
        Ok(out)
    }
}

/// `quire-lock.json`: the folder's key, wrapped by the passphrase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    pub format: u32,
    /// Always `quire-locked-folder`, so a stray JSON file isn't taken for one.
    #[serde(rename = "type")]
    pub kind: String,
    pub folder_id: String,
    /// Unix ms.
    pub created: i64,
    pub kdf: Kdf,
    /// Hex, 16 bytes.
    pub salt: String,
    /// Hex: nonce (24 bytes), then the encrypted folder key with its tag.
    pub wrapped_key: String,
    /// Hex: the first 16 bytes of HKDF(folder key, "check").
    pub check: String,
    /// After "Change the folder's key": hex of nonce (24 bytes) ‖ the
    /// previous folder key, sealed with a subkey of this one. Opens the
    /// files still encrypted with the previous key (only for someone who
    /// has the current key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_key: Option<String>,
    /// A key change is under way: some files may still be encrypted with
    /// the previous key. Running it again finishes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rotating: bool,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

pub const KEY_FILE_FORMAT: u32 = 1;
const KEY_FILE_TYPE: &str = "quire-locked-folder";

/// The shortest passphrase accepted (as for Quire Sync).
pub const MIN_PASSPHRASE: usize = 8;

impl KeyFile {
    /// A new key file for `key`, wrapped by `passphrase`.
    pub fn new(id: FolderId, key: &FolderKey, passphrase: &str, kdf: Kdf, now_ms: i64) -> Result<Self, LockError> {
        if passphrase.chars().count() < MIN_PASSPHRASE {
            return Err(LockError::WeakPassphrase);
        }
        let mut kf = Self {
            format: KEY_FILE_FORMAT,
            kind: KEY_FILE_TYPE.into(),
            folder_id: id.to_hex(),
            created: now_ms,
            kdf,
            salt: String::new(),
            wrapped_key: String::new(),
            check: key.check(),
            previous_key: None,
            rotating: false,
            other: serde_json::Map::new(),
        };
        kf.rewrap(key, passphrase)?;
        Ok(kf)
    }

    /// Wrap `key` with a new passphrase (and a new salt). The files stay as
    /// they are.
    pub fn rewrap(&mut self, key: &FolderKey, passphrase: &str) -> Result<(), LockError> {
        if passphrase.chars().count() < MIN_PASSPHRASE {
            return Err(LockError::WeakPassphrase);
        }
        if self.check_key(key).is_err() {
            return Err(LockError::BadRecoveryKey);
        }
        let id = self.id()?;
        // A new passphrase never keeps weaker settings than today's default
        // (the file's might have been lowered by someone who can write it).
        self.kdf = self.kdf.at_least(Kdf::floor());
        let salt: [u8; 16] = random()?;
        let mut wrap = self.kdf.derive(passphrase, &salt)?;
        let nonce: [u8; NONCE_LEN] = random()?;
        let cipher = XChaCha20Poly1305::new((&wrap).into());
        wrap.zeroize();
        let aad = [WRAP_AAD, &id.0].concat();
        let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: &key.0, aad: &aad }).map_err(|_| LockError::Damaged)?;
        self.salt = hex(&salt);
        self.wrapped_key = hex(&[nonce.as_slice(), &ct].concat());
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self, LockError> {
        let kf: Self = serde_json::from_str(text).map_err(|e| LockError::KeyFile(e.to_string()))?;
        if kf.kind != KEY_FILE_TYPE {
            return Err(LockError::KeyFile(format!("type is {:?}, not {KEY_FILE_TYPE}", kf.kind)));
        }
        if kf.format > KEY_FILE_FORMAT {
            return Err(LockError::Newer(u8::try_from(kf.format).unwrap_or(u8::MAX)));
        }
        kf.id()?;
        kf.kdf.check_bounds()?;
        Ok(kf)
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_else(|_| unreachable!("plain data serializes"));
        s.push('\n');
        s
    }

    pub fn id(&self) -> Result<FolderId, LockError> {
        FolderId::from_hex(&self.folder_id).ok_or_else(|| LockError::KeyFile("bad folder_id".into()))
    }

    /// The folder key, from the passphrase.
    pub fn unwrap_key(&self, passphrase: &str) -> Result<FolderKey, LockError> {
        let id = self.id()?;
        let salt = unhex(&self.salt).ok_or_else(|| LockError::KeyFile("bad salt".into()))?;
        let wrapped = unhex(&self.wrapped_key).ok_or_else(|| LockError::KeyFile("bad wrapped_key".into()))?;
        if wrapped.len() != NONCE_LEN + 32 + TAG_LEN {
            return Err(LockError::KeyFile("wrapped_key has the wrong length".into()));
        }
        let (nonce, ct) = wrapped.split_at(NONCE_LEN);
        let mut wrap = self.kdf.derive(passphrase, &salt)?;
        let cipher = XChaCha20Poly1305::new((&wrap).into());
        wrap.zeroize();
        let aad = [WRAP_AAD, &id.0].concat();
        let nonce = XNonce::try_from(nonce).map_err(|_| LockError::KeyFile("bad nonce".into()))?;
        let mut plain = cipher.decrypt(&nonce, Payload { msg: ct, aad: &aad }).map_err(|_| LockError::WrongPassphrase)?;
        let arr: Option<[u8; 32]> = plain.as_slice().try_into().ok();
        plain.zeroize();
        let key = FolderKey(arr.ok_or(LockError::WrongPassphrase)?);
        // A key file whose check doesn't match its own key was tampered with.
        self.check_key(&key).map_err(|_| LockError::KeyFile("its check value doesn't match its key".into()))?;
        Ok(key)
    }

    /// The previous folder key ([`Self::previous_key`]), opened with the
    /// current one.
    pub fn previous(&self, key: &FolderKey) -> Result<Option<FolderKey>, LockError> {
        let Some(h) = &self.previous_key else { return Ok(None) };
        let id = self.id()?;
        let data = unhex(h).ok_or_else(|| LockError::KeyFile("bad previous_key".into()))?;
        if data.len() != NONCE_LEN + 32 + TAG_LEN {
            return Err(LockError::KeyFile("previous_key has the wrong length".into()));
        }
        let (nonce, ct) = data.split_at(NONCE_LEN);
        let mut k = key.subkey(b"previous");
        let cipher = XChaCha20Poly1305::new((&k).into());
        k.zeroize();
        let aad = [PREVIOUS_AAD, &id.0].concat();
        let nonce = XNonce::try_from(nonce).map_err(|_| LockError::KeyFile("bad nonce".into()))?;
        let mut plain = cipher.decrypt(&nonce, Payload { msg: ct, aad: &aad }).map_err(|_| LockError::Damaged)?;
        let arr: Option<[u8; 32]> = plain.as_slice().try_into().ok();
        plain.zeroize();
        Ok(Some(FolderKey(arr.ok_or(LockError::Damaged)?)))
    }

    /// Keep `previous` in this key file, sealed with `key` (the current one).
    pub fn set_previous(&mut self, key: &FolderKey, previous: &FolderKey) -> Result<(), LockError> {
        let id = self.id()?;
        let nonce: [u8; NONCE_LEN] = random()?;
        let mut k = key.subkey(b"previous");
        let cipher = XChaCha20Poly1305::new((&k).into());
        k.zeroize();
        let aad = [PREVIOUS_AAD, &id.0].concat();
        let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: &previous.0, aad: &aad }).map_err(|_| LockError::Damaged)?;
        self.previous_key = Some(hex(&[nonce.as_slice(), &ct].concat()));
        Ok(())
    }

    /// Whether `key` (from a recovery key or the keychain) is this folder's.
    pub fn check_key(&self, key: &FolderKey) -> Result<(), LockError> {
        if constant_eq(key.check().as_bytes(), self.check.as_bytes()) { Ok(()) } else { Err(LockError::BadRecoveryKey) }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> FolderId {
        FolderId([b; 16])
    }

    #[test]
    fn seal_open_and_every_way_to_fail() {
        let k = FolderKey::generate().unwrap();
        let sealed = k.seal(Kind::File, id(1), b"# Dear diary\nsecret").unwrap();
        assert_eq!(&sealed[..4], b"QENC");
        assert_eq!(k.open(Kind::File, id(1), &sealed).unwrap(), b"# Dear diary\nsecret");
        assert!(!sealed.windows(6).any(|w| w == b"secret"), "no plain text in the file");
        assert_ne!(sealed, k.seal(Kind::File, id(1), b"# Dear diary\nsecret").unwrap(), "a new nonce each time");
        // Another folder's file, a snapshot passed off as a note, another key.
        assert_eq!(k.open(Kind::File, id(2), &sealed), Err(LockError::OtherFolder));
        assert_eq!(k.open(Kind::Snapshot, id(1), &sealed), Err(LockError::WrongKind));
        assert_eq!(FolderKey::generate().unwrap().open(Kind::File, id(1), &sealed), Err(LockError::Damaged));
        // Header fields are authenticated: rewriting the folder id or kind in place fails too.
        let mut moved = sealed.clone();
        moved[6] = 2;
        assert_eq!(k.open(Kind::File, FolderId([2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]), &moved), Err(LockError::Damaged));
        let mut kind = sealed.clone();
        kind[5] = 2;
        assert_eq!(k.open(Kind::Snapshot, id(1), &kind), Err(LockError::Damaged));
        // Every single flipped bit is caught.
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 0x10;
            assert!(k.open(Kind::File, id(1), &bad).is_err(), "byte {i}");
        }
        for n in 0..sealed.len() {
            assert!(k.open(Kind::File, id(1), &sealed[..n]).is_err(), "cut at {n}");
        }
        assert_eq!(k.open(Kind::File, id(1), b"# plain note"), Err(LockError::NotEncrypted));
        let mut newer = sealed;
        newer[4] = 2;
        assert_eq!(k.open(Kind::File, id(1), &newer), Err(LockError::Newer(2)));
    }

    #[test]
    fn key_file_wraps_unwraps_and_rewraps() {
        let key = FolderKey::generate().unwrap();
        let fid = FolderId::generate().unwrap();
        assert_eq!(KeyFile::new(fid, &key, "short", Kdf::TEST, 0).unwrap_err(), LockError::WeakPassphrase);
        let mut kf = KeyFile::new(fid, &key, "correct horse", Kdf::TEST, 7).unwrap();
        let json = kf.to_json();
        assert!(!json.contains("correct horse"));
        let back = KeyFile::parse(&json).unwrap();
        assert_eq!(back, kf);
        assert_eq!(back.unwrap_key("correct horse").unwrap().0, key.0);
        assert_eq!(back.unwrap_key("wrong horse").unwrap_err(), LockError::WrongPassphrase);
        // Recovery key round trip, and it's checked against the folder.
        let rk = key.recovery_key();
        let from_rk = FolderKey::from_recovery_key(&rk).unwrap();
        assert!(kf.check_key(&from_rk).is_ok());
        assert_eq!(kf.check_key(&FolderKey::generate().unwrap()), Err(LockError::BadRecoveryKey));
        // A new passphrase; the old one stops working, the key is the same.
        kf.rewrap(&from_rk, "battery staple").unwrap();
        assert_eq!(kf.unwrap_key("battery staple").unwrap().0, key.0);
        assert_eq!(kf.unwrap_key("correct horse").unwrap_err(), LockError::WrongPassphrase);
        // Moved to another folder id: refused.
        let mut other = kf.clone();
        other.folder_id = FolderId::generate().unwrap().to_hex();
        assert_eq!(other.unwrap_key("battery staple").unwrap_err(), LockError::WrongPassphrase);
        // Unknown fields survive; newer formats and other types are refused.
        let extra = json.replacen('{', "{\n  \"future\": 1,", 1);
        assert!(KeyFile::parse(&extra).unwrap().to_json().contains("\"future\": 1"));
        assert!(matches!(KeyFile::parse(&json.replace("\"format\": 1", "\"format\": 2")), Err(LockError::Newer(2))));
        assert!(KeyFile::parse(&json.replace("quire-locked-folder", "other")).is_err());
        assert!(KeyFile::parse(&json.replace("argon2id", "scrypt")).is_err());
        assert!(format!("{key:?}").find(&hex(&key.0)[..8]).is_none(), "keys don't print");
    }

    #[test]
    fn argon2_settings_out_of_bounds_are_refused_and_never_kept() {
        let key = FolderKey::generate().unwrap();
        let kf = KeyFile::new(FolderId([1; 16]), &key, "correct horse", Kdf::TEST, 0).unwrap();
        // Absurd settings (an unlock would hang or run out of memory): refused on read, before any work.
        for (m, t, p) in [(u32::MAX, 3, 1), (65536, u32::MAX, 1), (65536, 3, 0), (65536, 3, 1000), (8, 1, 1), (64, 0, 1)] {
            let json = kf
                .to_json()
                .replace("\"memory_kib\": 64", &format!("\"memory_kib\": {m}"))
                .replace("\"iterations\": 1,", &format!("\"iterations\": {t},"));
            let json = json.replace("\"parallelism\": 1", &format!("\"parallelism\": {p}"));
            let e = KeyFile::parse(&json).unwrap_err();
            assert!(matches!(&e, LockError::KeyFile(m) if m.contains("Argon2id settings")), "{m} {t} {p}: {e}");
        }
        // Outside tests the floor is the default: test settings are refused there.
        assert!(Kdf::TEST.check_bounds_from(Kdf::DEFAULT).is_err());
        assert!(Kdf::DEFAULT.check_bounds_from(Kdf::DEFAULT).is_ok());
        // A new passphrase never keeps settings below the floor.
        let weak = Kdf { memory_kib: 8, iterations: 1, ..Kdf::DEFAULT };
        assert_eq!(weak.at_least(Kdf::DEFAULT), Kdf::DEFAULT);
        let strong = Kdf { memory_kib: 1 << 20, ..Kdf::DEFAULT };
        assert_eq!(strong.at_least(Kdf::DEFAULT), strong);
    }

    /// Files written by 0.11 must stay readable by every later version.
    #[test]
    fn format_1_vectors() {
        let key = FolderKey::from_bytes([42; 32]);
        let fid = FolderId([7; 16]);
        let kf = KeyFile::parse(KEY_FILE_V1).unwrap();
        let k = kf.unwrap_key("pass phrase").unwrap();
        assert_eq!(k.0, [42; 32]);
        assert_eq!(kf.id().unwrap(), fid);
        let sealed = unhex(SEALED_V1).unwrap();
        assert_eq!(k.open(Kind::File, fid, &sealed).unwrap(), b"quire lock v1");
        assert_eq!(key.recovery_key(), "58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E");
    }

    const KEY_FILE_V1: &str = include_str!("testdata/quire-lock.json");
    const SEALED_V1: &str = include_str!("testdata/sealed.hex");

    /// Prints fresh vectors (run with `--ignored --nocapture` after a
    /// deliberate format change, never to paper over a failure).
    #[test]
    #[ignore = "writes test vectors"]
    fn print_vectors() {
        let key = FolderKey::from_bytes([42; 32]);
        let fid = FolderId([7; 16]);
        let kf = KeyFile::new(fid, &key, "pass phrase", Kdf::TEST, 1_793_000_000_000).unwrap();
        println!("{}", kf.to_json());
        println!("{}", hex(&key.seal(Kind::File, fid, b"quire lock v1").unwrap()));
    }
}
