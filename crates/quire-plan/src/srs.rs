// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Spaced repetition (design §E.6): review logs and card state.
//!
//! Each device appends its reviews to `.quire/shared/srs/<device-id>.jsonl`,
//! one [`ReviewEvent`] per line (docs/formats/srs.md). Card state is derived
//! by replaying every device's log in `(at, device)` order. The FSRS-6
//! scheduler arrives with WS-E.

use serde::{Deserialize, Serialize};

pub const SRS_DIR: &str = ".quire/shared/srs";
pub const REVIEW_LOG_VERSION: u32 = 1;

/// Again, Hard, Good, Easy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Rating {
    Again,
    Hard,
    Good,
    Easy,
}

impl TryFrom<u8> for Rating {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(Self::Again),
            2 => Ok(Self::Hard),
            3 => Ok(Self::Good),
            4 => Ok(Self::Easy),
            n => Err(format!("rating {n} is not 1–4")),
        }
    }
}

impl From<Rating> for u8 {
    fn from(r: Rating) -> Self {
        match r {
            Rating::Again => 1,
            Rating::Hard => 2,
            Rating::Good => 3,
            Rating::Easy => 4,
        }
    }
}

/// One review, one line of a device's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewEvent {
    pub v: u32,
    /// `k_` + hash of the question, or the card's `^id`.
    pub card: String,
    /// The note, vault-relative.
    pub note: String,
    /// Unix ms.
    pub at: i64,
    pub rating: Rating,
    pub elapsed_days: f64,
    pub device: String,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// Parse a log, skipping lines that are unreadable or from a newer version.
pub fn parse_log(text: &str) -> Vec<ReviewEvent> {
    text.lines().filter_map(|l| serde_json::from_str::<ReviewEvent>(l).ok()).filter(|e| e.v <= REVIEW_LOG_VERSION).collect()
}

/// Every device's events in replay order: by time, then device.
pub fn merge_logs(logs: Vec<Vec<ReviewEvent>>) -> Vec<ReviewEvent> {
    let mut all: Vec<ReviewEvent> = logs.into_iter().flatten().collect();
    all.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.device.cmp(&b.device)));
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_round_trip_and_merge_deterministically() {
        let laptop = r#"{"v":1,"card":"k_1","note":"Bio.md","at":20,"rating":3,"elapsed_days":1.5,"device":"d_b"}
{"v":2,"card":"k_1","note":"Bio.md","at":30,"rating":9,"device":"d_b"}
not json"#;
        let phone = r#"{"v":1,"card":"k_1","note":"Bio.md","at":20,"rating":1,"elapsed_days":0,"device":"d_a","extra":true}"#;
        let a = parse_log(phone);
        let b = parse_log(laptop);
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_eq!(b[0].rating, Rating::Good);
        let merged = merge_logs(vec![b.clone(), a.clone()]);
        assert_eq!(merged.iter().map(|e| e.device.as_str()).collect::<Vec<_>>(), ["d_a", "d_b"]);
        assert_eq!(merge_logs(vec![a.clone(), b]), merged);
        let line = serde_json::to_string(&a[0]).unwrap();
        assert!(line.contains(r#""rating":1"#) && line.contains(r#""extra":true"#));
    }
}
