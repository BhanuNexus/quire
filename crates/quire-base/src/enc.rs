// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Text encodings: percent-encoding (RFC 3986), HTML escaping, hex and
//! SHA-256 hex.
//!
//! **Percent-decoding has three behaviours, chosen per caller:**
//! - [`percent_decode_strict`]: `None` on a broken escape (`100%`, `%zz`,
//!   `%2`) or bytes that aren't UTF-8. Where a path or a name results
//!   (the web app, WebDAV listings, links copied as HTML): a mangled name
//!   must not be taken for another.
//! - [`percent_decode_lossy`]: a broken escape stays as written, bytes that
//!   aren't UTF-8 become U+FFFD. For text shown or matched (link targets
//!   in notes, importers).
//! - [`percent_decode_bytes`]: the raw bytes, a broken escape as written.
//!   For a caller that decodes the bytes itself.
//!
//! An escape is `%` and exactly two hex digits. Before 0.14 every copy
//! also took `%+1` as an escape (`u8::from_str_radix` accepts a sign);
//! now it is a broken escape.

use std::fmt::Write as _;

/// The value of one hex digit.
const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The byte of the escape `%hh` starting at `i`, if it is one.
fn escape_at(b: &[u8], i: usize) -> Option<u8> {
    if b.get(i) != Some(&b'%') {
        return None;
    }
    let (hi, lo) = (hex_value(*b.get(i + 1)?)?, hex_value(*b.get(i + 2)?)?);
    Some(hi << 4 | lo)
}

/// The bytes `s` stands for; a broken escape stays as written.
pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if let Some(v) = escape_at(b, i) {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// The text `s` stands for: a broken escape stays as written, bytes that
/// aren't UTF-8 become U+FFFD.
pub fn percent_decode_lossy(s: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(s)).into_owned()
}

/// The text `s` stands for, or `None` on a broken escape or bytes that
/// aren't UTF-8.
pub fn percent_decode_strict(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(escape_at(b, i)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode every byte but RFC 3986's unreserved ones
/// (`A-Z a-z 0-9 - . _ ~`) and those in `keep`, as `%HH` (upper case).
///
/// `keep` is `b""` for a query value or a SigV4 name, `b"/"` for a URL
/// path, `b"/:"` for a `file://` URL.
pub fn percent_encode(s: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || keep.contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Escape `& < > " '` for HTML text and attribute values in either quote.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Lower-case hex, two digits a byte.
pub fn hex(b: &[u8]) -> String {
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// The bytes of hex text (either case), or `None` if it isn't hex: an odd
/// length, or anything but hex digits (no sign, no spaces, nothing outside
/// ASCII).
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    b.chunks_exact(2).map(|p| Some(hex_value(p[0])? << 4 | hex_value(p[1])?)).collect()
}

/// Lower-case hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest as _;
    hex(&sha2::Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inputs of the differential test of the eight 0.13.2 copies
    /// (health research, `diff/pct.rs`), with what each behaviour gives.
    #[test]
    fn percent_decoding_on_the_differential_inputs() {
        let cases: [(&str, Option<&str>, &str); 8] = [
            ("a%20b", Some("a b"), "a b"),
            ("caf%C3%A9", Some("café"), "café"),
            ("100%", None, "100%"),
            ("%zz", None, "%zz"),
            ("%2", None, "%2"),
            ("%FF", None, "\u{FFFD}"),
            // Every copy decoded this to U+0001: a sign is not a hex digit.
            ("%+1", None, "%+1"),
            ("%e9t%C3%A9", None, "\u{FFFD}té"),
        ];
        for (input, strict, lossy) in cases {
            assert_eq!(percent_decode_strict(input).as_deref(), strict, "strict {input}");
            assert_eq!(percent_decode_lossy(input), lossy, "lossy {input}");
        }
        assert_eq!(percent_decode_bytes("%FF%2"), [0xFF, b'%', b'2']);
        assert_eq!(percent_decode_bytes("%-1"), b"%-1");
    }

    #[test]
    fn percent_encoding_keeps_unreserved_and_what_is_asked() {
        assert_eq!(percent_encode("a b/c~d.e_f-g", b""), "a%20b%2Fc~d.e_f-g");
        assert_eq!(percent_encode("a b/c", b"/"), "a%20b/c");
        assert_eq!(percent_encode("/C:/x y", b"/:"), "/C:/x%20y");
        assert_eq!(percent_encode("é", b""), "%C3%A9");
        assert_eq!(percent_decode_strict(&percent_encode("Ünïcødé/ü?&=", b"")).as_deref(), Some("Ünïcødé/ü?&="));
    }

    #[test]
    fn html_escaping_covers_both_quotes() {
        assert_eq!(escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#), "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;");
    }

    #[test]
    fn hex_round_trips_and_refuses_what_isnt_hex() {
        assert_eq!(hex(&[0, 0xab, 0xFF]), "00abff");
        assert_eq!(unhex("00abFF"), Some(vec![0, 0xab, 0xff]));
        assert_eq!(unhex("abc"), None);
        // `u8::from_str_radix` took a sign; the 0.13 copies read "+a" as 10.
        assert_eq!(unhex("+a"), None);
        assert_eq!(unhex("é1"), None);
        assert_eq!(unhex(" a"), None);
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
