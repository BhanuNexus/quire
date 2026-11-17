// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Random bytes from the operating system, or an error: never zeros.
//!
//! `let _ = getrandom::fill(&mut b)` leaves `b` all zeros when the system's
//! random source fails, and a zero key, nonce, code or id looks like any
//! other. This is the one place that calls `getrandom`; its result must be
//! handled (`#[must_use]`), and `clippy.toml` refuses `getrandom::fill`
//! everywhere else.

/// The system's random source failed (rare: a sandbox without
/// `getrandom(2)`, or entropy not yet available early at boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRandomness;

impl std::fmt::Display for NoRandomness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the system couldn't provide random numbers")
    }
}

impl std::error::Error for NoRandomness {}

impl From<NoRandomness> for std::io::Error {
    fn from(e: NoRandomness) -> Self {
        Self::other(e)
    }
}

/// Fill `buf` with random bytes. On an error `buf` must not be used.
///
/// The same arguments as `getrandom::fill`, so `cargo clippy --fix` can
/// switch a call over; the caller then handles [`NoRandomness`].
#[must_use = "on an error the buffer is not random (often all zeros): handle it"]
pub fn fill(buf: &mut [u8]) -> Result<(), NoRandomness> {
    if FAILING.get() {
        return Err(NoRandomness);
    }
    getrandom::fill(buf).map_err(|_| NoRandomness)
}

thread_local! {
    static FAILING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` as if the system's random source had failed, on this thread
/// only: for tests that show a caller handles [`NoRandomness`] (other
/// crates' tests can't make the system fail). Nothing else sets it.
pub fn failing<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            FAILING.set(self.0);
        }
    }
    let _reset = Reset(FAILING.replace(true));
    f()
}

/// `N` random bytes.
#[must_use = "handle the error: there are no random bytes without it"]
pub fn bytes<const N: usize>() -> Result<[u8; N], NoRandomness> {
    let mut b = [0u8; N];
    fill(&mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_different_bytes_each_time() {
        let (a, b) = (bytes::<32>().unwrap(), bytes::<32>().unwrap());
        assert_ne!(a, b);
        assert_ne!(a, [0; 32]);
        let mut v = vec![0u8; 64];
        fill(&mut v).unwrap();
        assert!(v.iter().any(|&x| x != 0));
    }

    #[test]
    fn failing_fails_on_this_thread_only_and_ends_with_the_closure() {
        assert_eq!(failing(bytes::<4>), Err(NoRandomness));
        assert!(failing(|| std::thread::scope(|s| s.spawn(bytes::<4>).join().unwrap().is_ok())), "another thread is not affected");
        assert!(bytes::<4>().is_ok());
    }

    #[test]
    fn the_error_reads_as_a_sentence_and_converts_to_io() {
        let e: std::io::Error = NoRandomness.into();
        assert_eq!(e.to_string(), "the system couldn't provide random numbers");
    }
}
