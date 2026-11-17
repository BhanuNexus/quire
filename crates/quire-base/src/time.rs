// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The wall clock in milliseconds, and `YYYY-MM-DD` dates.
//!
//! Code that tests must control takes a clock as an argument instead
//! (`quire_agent::Clock`, P10); [`now_ms`] is for the rest.

use chrono::NaiveDate;

/// Milliseconds since the Unix epoch; 0 for a clock set before 1970.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `s` as a `YYYY-MM-DD` date, the whole of it (`2026-11-20T10:00` is not
/// one). For a date a person typed into a date field.
pub fn parse_ymd(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// The date at the start of `s`: its first ten bytes as `YYYY-MM-DD`, so
/// `2026-11-20T10:00` and `2026-11-20 (Fri)` are 2026-11-20. For dates in
/// front matter and task fields, which may carry a time.
pub fn parse_ymd_prefix(s: &str) -> Option<NaiveDate> {
    parse_ymd(s.get(..10)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_string_or_its_first_ten_bytes() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 20);
        assert_eq!(parse_ymd("2026-11-20"), d);
        assert_eq!(parse_ymd("2026-11-20T10:00"), None);
        assert_eq!(parse_ymd_prefix("2026-11-20T10:00"), d);
        assert_eq!(parse_ymd_prefix("2026-11-20"), d);
        assert_eq!(parse_ymd_prefix("2026-11-2"), None);
        // No trimming: chrono reads the first ten bytes, " 2026-11-2", as
        // the 2nd (as every 0.13 copy did). Callers that trim do it first.
        assert_eq!(parse_ymd_prefix(" 2026-11-20"), NaiveDate::from_ymd_opt(2026, 11, 2));
        assert_eq!(parse_ymd_prefix("2026-11-2é"), None, "not a char boundary");
        assert_eq!(parse_ymd("2026-02-30"), None);
    }

    #[test]
    fn now_is_after_2026() {
        assert!(now_ms() > 1_767_225_600_000);
    }
}
