// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Signing in to the web app: one passphrase, sessions, CSRF tokens and a
//! limit on wrong guesses.
//!
//! * The passphrase is set with `quire-cli serve passwd` and kept as an
//!   Argon2id hash (PHC string) in `<config>/serve/login.toml`, readable
//!   only by its owner. Changing it signs every session out.
//! * A session is a 256-bit random token in an `HttpOnly`, `SameSite=Strict`
//!   cookie (`Secure`, and named `__Host-…`, when the public URL is https).
//!   The server keeps only its SHA-256, in memory: restarting signs out.
//!   Sessions end after an idle time and after an absolute time.
//! * Every form that changes something carries the session's CSRF token,
//!   compared in constant time. The sign-in form uses a double-submit
//!   cookie instead (there is no session yet).
//! * After 5 wrong passphrases, each further try waits (1 s, doubling, at
//!   most 15 minutes) for everyone: behind a reverse proxy every visitor
//!   has the proxy's address, so the limit can't be per address. A correct
//!   passphrase clears it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The shortest passphrase `serve passwd` accepts.
pub const MIN_PASSPHRASE: usize = 10;

/// `<config>/serve/login.toml`.
#[derive(Debug, Clone)]
pub struct LoginStore {
    path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LoginFile {
    format: u32,
    /// Argon2id, PHC string format.
    hash: String,
    /// Unix ms of the last change; sessions from before it end.
    changed: i64,
    #[serde(flatten)]
    other: toml::Table,
}

impl LoginStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `<config>/serve/login.toml`.
    pub fn default_store() -> Option<Self> {
        Some(Self::at(quire_app::settings::config_dir()?.join("serve").join("login.toml")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Option<LoginFile> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let f: LoginFile = toml::from_str(&text).ok()?;
        (f.format <= 1 && !f.hash.is_empty()).then_some(f)
    }

    pub fn is_set(&self) -> bool {
        self.load().is_some()
    }

    /// When the passphrase last changed (Unix ms), if one is set.
    pub fn changed(&self) -> Option<i64> {
        self.load().map(|f| f.changed)
    }

    /// Set the passphrase (stored hashed, file readable only by its owner).
    pub fn set(&self, passphrase: &str, now_ms: i64) -> Result<(), String> {
        if passphrase.chars().count() < MIN_PASSPHRASE {
            return Err(format!("Use a passphrase of at least {MIN_PASSPHRASE} characters."));
        }
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
        let hash = argon2::Argon2::default().hash_password_with_salt(passphrase.as_bytes(), &salt).map_err(|e| e.to_string())?;
        let mut f = self.load().unwrap_or_default();
        f.format = 1;
        f.hash = hash.to_string();
        f.changed = now_ms;
        let text = toml::to_string(&f).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        quire_app::atomic::write(&self.path, text.as_bytes(), true).map_err(|e| e.to_string())
    }

    /// Whether `passphrase` is the one set (slow on purpose: Argon2id).
    pub fn verify(&self, passphrase: &str) -> bool {
        self.load().is_some_and(|f| argon2::Argon2::default().verify_password(passphrase.as_bytes(), f.hash.as_str()).is_ok())
    }
}

/// How long sessions last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Ends after this long without a request (ms).
    pub idle_ms: i64,
    /// Ends this long after signing in, whatever happens (ms).
    pub max_ms: i64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self { idle_ms: 8 * 3_600_000, max_ms: 7 * 86_400_000 }
    }
}

/// At most this many sessions; signing in again drops the oldest.
pub const MAX_SESSIONS: usize = 32;

#[derive(Debug, Clone)]
pub struct Session {
    pub created: i64,
    pub last_seen: i64,
    pub csrf: String,
}

/// A random token: 32 bytes, base64url.
pub fn random_token() -> Option<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).ok()?;
    Some(URL_SAFE_NO_PAD.encode(b))
}

pub fn hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// Equal strings, compared without stopping early.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Sessions by the hash of their token.
#[derive(Debug, Default)]
pub struct Sessions {
    by_hash: HashMap<String, Session>,
}

impl Sessions {
    /// Start a session; returns its token (for the cookie) and CSRF token.
    pub fn create(&mut self, now: i64) -> Option<(String, String)> {
        let (token, csrf) = (random_token()?, random_token()?);
        if self.by_hash.len() >= MAX_SESSIONS
            && let Some(oldest) = self.by_hash.iter().min_by_key(|(_, s)| s.created).map(|(k, _)| k.clone())
        {
            self.by_hash.remove(&oldest);
        }
        self.by_hash.insert(hash(&token), Session { created: now, last_seen: now, csrf: csrf.clone() });
        Some((token, csrf))
    }

    /// The session for a cookie's token, if it is still good; marks it used.
    pub fn check(&mut self, token: &str, now: i64, limits: SessionLimits, login_changed: Option<i64>) -> Option<Session> {
        let key = hash(token);
        let s = self.by_hash.get_mut(&key)?;
        let expired = now - s.last_seen > limits.idle_ms
            || now - s.created > limits.max_ms
            || login_changed.is_none_or(|c| c > s.created)
            || now < s.created;
        if expired {
            self.by_hash.remove(&key);
            return None;
        }
        s.last_seen = now;
        Some(s.clone())
    }

    pub fn end(&mut self, token: &str) {
        self.by_hash.remove(&hash(token));
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// Wrong passphrases before tries start to wait.
pub const FREE_TRIES: u32 = 5;
/// The longest wait between tries.
pub const MAX_WAIT_MS: i64 = 15 * 60_000;

/// Passphrase checks running at once. Each takes Argon2's 19 MiB, so a
/// burst of sign-ins can't grow memory past this (0.10.1 security review #5).
pub const MAX_VERIFYING: u32 = 2;
/// How long to ask a sign-in to wait while others are being checked.
pub const BUSY_WAIT_MS: i64 = 1000;

/// The limit on wrong passphrases (for everyone, see the module docs).
///
/// A try is checked and counted in one step under the lock ([`Self::begin`]):
/// it counts as wrong until it turns out right ([`Self::end`]), so
/// parallel guesses each use up a try instead of all passing the check
/// before the first one is counted. At most [`MAX_VERIFYING`] are checked
/// at once; the others are asked to wait.
#[derive(Debug, Default)]
pub struct Limiter {
    failures: u32,
    last_failure: i64,
    verifying: u32,
}

impl Limiter {
    /// `Err(ms)` to wait before the next try is looked at.
    pub fn check(&mut self, now: i64) -> Result<(), i64> {
        if now - self.last_failure > MAX_WAIT_MS * 2 {
            self.failures = 0;
        }
        if self.failures < FREE_TRIES {
            return Ok(());
        }
        let wait = (1000_i64 << (self.failures - FREE_TRIES).min(20)).min(MAX_WAIT_MS);
        let until = self.last_failure + wait;
        if now < until { Err(until - now) } else { Ok(()) }
    }

    /// Start a try: `Err(ms)` to wait, or the try is counted (as wrong,
    /// until [`Self::end`] says otherwise) and may be checked now. Every
    /// `Ok` must be followed by one [`Self::end`].
    pub fn begin(&mut self, now: i64) -> Result<(), i64> {
        self.check(now)?;
        if self.verifying >= MAX_VERIFYING {
            return Err(BUSY_WAIT_MS);
        }
        self.verifying += 1;
        self.failed(now);
        Ok(())
    }

    /// A try begun with [`Self::begin`] is done; `right` when the
    /// passphrase was right. A wrong one's wait counts from now.
    pub fn end(&mut self, now: i64, right: bool) {
        self.verifying = self.verifying.saturating_sub(1);
        if right {
            self.succeeded();
        } else {
            self.last_failure = self.last_failure.max(now);
        }
    }

    /// Tries being checked right now.
    pub const fn verifying(&self) -> u32 {
        self.verifying
    }

    pub fn failed(&mut self, now: i64) {
        self.failures = self.failures.saturating_add(1);
        self.last_failure = now;
    }

    pub fn succeeded(&mut self) {
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.10.1 security review #5: a parallel burst of guesses gets no more
    /// tries than one at a time, and only a few are checked at once.
    #[test]
    fn a_parallel_burst_of_guesses_is_counted_and_capped() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Barrier, Mutex};
        let limiter = Arc::new(Mutex::new(Limiter::default()));
        let checked = Arc::new(AtomicU32::new(0));
        let most = Arc::new(AtomicU32::new(0));
        let start = Arc::new(Barrier::new(40));
        let threads: Vec<_> = (0..40)
            .map(|_| {
                let (limiter, checked, most, start) = (Arc::clone(&limiter), Arc::clone(&checked), Arc::clone(&most), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    let began = {
                        let mut l = limiter.lock().unwrap();
                        let r = l.begin(1_000);
                        most.fetch_max(l.verifying(), Ordering::SeqCst);
                        r
                    };
                    if began.is_ok() {
                        checked.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(30)); // the slow verify
                        limiter.lock().unwrap().end(1_030, false);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(checked.load(Ordering::SeqCst) <= FREE_TRIES, "{} tries were checked", checked.load(Ordering::SeqCst));
        assert!(most.load(Ordering::SeqCst) <= MAX_VERIFYING);
        let mut l = limiter.lock().unwrap();
        assert_eq!(l.verifying(), 0);
        // Keep going one at a time: the free tries run out, then the wait applies.
        while l.begin(1_100).is_ok() {
            l.end(1_100, false);
        }
        assert!(l.begin(1_100).is_err());
        let wait = l.begin(1_100).unwrap_err();
        assert!(l.begin(1_100 + wait).is_ok(), "after the wait, one more try");
        l.end(1_100 + wait, true);
        assert!(l.begin(1_100 + wait).is_ok(), "a right passphrase clears the count");
        l.end(1_100 + wait, true);
    }

    #[test]
    fn passphrases_are_hashed_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let store = LoginStore::at(dir.path().join("serve/login.toml"));
        assert!(!store.is_set() && !store.verify("anything at all"));
        assert!(store.set("short", 1).is_err());
        store.set("correct horse battery", 5).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("$argon2id$") && !text.contains("correct horse"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(store.verify("correct horse battery"));
        assert!(!store.verify("correct horse batter"));
        assert_eq!(store.changed(), Some(5));
    }

    #[test]
    fn sessions_end_when_idle_too_old_or_the_passphrase_changes() {
        let limits = SessionLimits { idle_ms: 100, max_ms: 1000 };
        let mut s = Sessions::default();
        let (t, csrf) = s.create(10).unwrap();
        assert_eq!(s.check(&t, 50, limits, Some(0)).unwrap().csrf, csrf);
        assert!(s.check("not a token", 50, limits, Some(0)).is_none());
        assert!(s.check(&t, 200, limits, Some(0)).is_none(), "idle");
        assert!(s.check(&t, 60, limits, Some(0)).is_none(), "gone for good");
        let (t, _) = s.create(0).unwrap();
        let mut now = 0;
        while now < 1000 {
            now += 90;
            assert!(s.check(&t, now, limits, Some(0)).is_some() || now > 1000);
        }
        assert!(s.check(&t, 1090, limits, Some(0)).is_none(), "absolute limit");
        let (t, _) = s.create(0).unwrap();
        assert!(s.check(&t, 1, limits, Some(5)).is_none(), "passphrase changed since");
        let (t, _) = s.create(0).unwrap();
        s.end(&t);
        assert!(s.check(&t, 1, limits, Some(0)).is_none());
        for i in 0..40 {
            s.create(i).unwrap();
        }
        assert_eq!(s.len(), MAX_SESSIONS);
    }

    #[test]
    fn wrong_guesses_slow_down() {
        let mut l = Limiter::default();
        for i in 0..FREE_TRIES {
            assert!(l.check(i64::from(i)).is_ok());
            l.failed(i64::from(i));
        }
        assert_eq!(l.check(10), Err(1000 - 6));
        assert!(l.check(1004).is_ok());
        l.failed(1004);
        assert_eq!(l.check(1005), Err(1999));
        l.succeeded();
        assert!(l.check(1006).is_ok());
        assert!(same("abc", "abc") && !same("abc", "abx") && !same("abc", "ab"));
    }
}
