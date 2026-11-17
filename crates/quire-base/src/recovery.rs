// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The recovery-key codec: a 32-byte key in a form people can write down.
//!
//! 52 Crockford base32 characters (no I, L, O or U, so it's hard to
//! misread) and a 4-character checksum (the first bytes of the key's
//! SHA-256), in groups of four joined by `-`. Sync's vault key
//! (`quire_sync::RecoveryKey`) and locked folders (`quire_vault::lock`)
//! write the same format. It is persisted on paper: never change it.
//!
//! The codec only: the callers keep the key in their own wiped types.

use sha2::{Digest as _, Sha256};

/// Crockford base32.
const B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn checksum(key: &[u8; 32]) -> [u8; 4] {
    let d = Sha256::digest(key);
    [d[0], d[1], d[2], d[3]]
}

/// `key` as a recovery key (`XXXX-XXXX-…`, 69 characters).
pub fn encode(key: &[u8; 32]) -> String {
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
    let check = checksum(key);
    for c in check {
        chars.push(char::from(B32[usize::from(c & 31)]));
    }
    let grouped: Vec<String> = chars.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    grouped.join("-")
}

/// The key `text` stands for, or `None` if it isn't one or its checksum
/// doesn't match. Lower case, spaces or no separators, `O` for `0` and
/// `I`/`L` for `1` are all accepted.
pub fn decode(text: &str) -> Option<[u8; 32]> {
    let mut vals = Vec::with_capacity(56);
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '-') {
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let v = B32.iter().position(|&b| char::from(b) == c)?;
        vals.push(v as u64);
    }
    if vals.len() != 56 {
        return None;
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
    let key: [u8; 32] = out.get(..32).and_then(|s| s.try_into().ok())?;
    let want = checksum(&key);
    if check.iter().zip(want).any(|(&c, w)| c != u64::from(w & 31)) {
        return None;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 0.12 vector of `quire-sync`'s `format_v1_vectors`: the format
    /// is the same after the move.
    #[test]
    fn the_format_is_pinned() {
        assert_eq!(encode(&[42; 32]), "58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E");
        assert_eq!(decode("58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N2-MAHA-58N0-ME2E"), Some([42; 32]));
    }

    #[test]
    fn round_trips_and_catches_typos() {
        let key: [u8; 32] = std::array::from_fn(|i| u8::try_from(i * 7 % 256).unwrap());
        let rk = encode(&key);
        assert_eq!(rk.len(), 56 + 13, "{rk}");
        assert_eq!(decode(&rk), Some(key));
        let relaxed = rk.to_lowercase().replace('-', " ").replace('0', "o").replace('1', "l");
        assert_eq!(decode(&relaxed), Some(key));
        let mut chars: Vec<char> = rk.chars().collect();
        chars[5] = if chars[5] == 'A' { 'B' } else { 'A' };
        assert_eq!(decode(&chars.into_iter().collect::<String>()), None);
        assert_eq!(decode("ABCD"), None);
        assert_eq!(decode(&format!("{rk}U")), None, "U is not in the alphabet");
    }
}
