// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The wall clock in milliseconds. Dates are in [`crate::dates`].
//!
//! Code that tests must control takes a clock as an argument instead
//! (`quire_agent::Clock`, P10); [`now_ms`] is for the rest.

/// Milliseconds since the Unix epoch; 0 for a clock set before 1970.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_after_2026() {
        assert!(now_ms() > 1_767_225_600_000);
    }
}
