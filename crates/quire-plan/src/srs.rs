// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Spaced repetition (design §E.6): review logs, card state and the FSRS-6
//! scheduler.
//!
//! Each device appends its reviews to `.quire/shared/srs/<device-id>.jsonl`,
//! one [`ReviewEvent`] per line (docs/formats/srs.md). Card state is never
//! stored: [`replay`] works it out from every device's events in
//! `(at, device)` order, so every device gets the same answer.
//!
//! The scheduler is FSRS-6 with its default parameters and a target
//! retention of 0.9, written to match the `fsrs` crate (checked against it
//! on 10,000 seeded review histories in the tests). Elapsed time counts
//! whole days between reviews (`floor(Δms / 1 day)`), which needs no time
//! zone, so two devices in different zones replay the same way.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const SRS_DIR: &str = ".quire/shared/srs";
pub const REVIEW_LOG_VERSION: u32 = 1;

/// Again, Hard, Good, Easy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Rating {
    Again,
    Hard,
    Good,
    Easy,
}

impl Rating {
    pub const ALL: [Self; 4] = [Self::Again, Self::Hard, Self::Good, Self::Easy];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Again => "Again",
            Self::Hard => "Hard",
            Self::Good => "Good",
            Self::Easy => "Easy",
        }
    }

    const fn grade(self) -> f64 {
        match self {
            Self::Again => 1.0,
            Self::Hard => 2.0,
            Self::Good => 3.0,
            Self::Easy => 4.0,
        }
    }
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

/// One line of a device's log: a review, or a card's id changing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewEvent {
    pub v: u32,
    /// `k_` + hash of the question, or the card's `^id`.
    pub card: String,
    /// The note, vault-relative.
    pub note: String,
    /// Unix ms.
    pub at: i64,
    /// The answer; absent on a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<Rating>,
    /// Whole days since the card's previous review, as the device saw it.
    #[serde(default)]
    pub elapsed_days: f64,
    pub device: String,
    /// A rename: the card's previous id; its history continues as `card`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl ReviewEvent {
    pub fn review(card: &str, note: &str, at: i64, rating: Rating, elapsed_days: f64, device: &str) -> Self {
        Self {
            v: REVIEW_LOG_VERSION,
            card: card.into(),
            note: note.into(),
            at,
            rating: Some(rating),
            elapsed_days,
            device: device.into(),
            from: None,
            other: serde_json::Map::new(),
        }
    }

    pub fn rename(from: &str, to: &str, note: &str, at: i64, device: &str) -> Self {
        Self {
            v: REVIEW_LOG_VERSION,
            card: to.into(),
            note: note.into(),
            at,
            rating: None,
            elapsed_days: 0.0,
            device: device.into(),
            from: Some(from.into()),
            other: serde_json::Map::new(),
        }
    }

    /// The event as one log line (no newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Parse a log, skipping lines that are unreadable or from a newer version.
pub fn parse_log(text: &str) -> Vec<ReviewEvent> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<ReviewEvent>(l).ok())
        .filter(|e| e.v <= REVIEW_LOG_VERSION && (e.rating.is_some() || e.from.is_some()))
        .collect()
}

/// Every device's events in replay order: by time, then device.
pub fn merge_logs(logs: Vec<Vec<ReviewEvent>>) -> Vec<ReviewEvent> {
    let mut all: Vec<ReviewEvent> = logs.into_iter().flatten().collect();
    all.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.device.cmp(&b.device)));
    all
}

// ------------------------------------------------------------------ FSRS-6

/// FSRS-6 default parameters (the `fsrs` crate's `DEFAULT_PARAMETERS`).
pub const DEFAULT_W: [f64; 21] = [
    0.212, 1.2931, 2.3065, 8.2956, 6.4133, 0.8334, 3.0194, 0.001, 1.8722, 0.1666, 0.796, 1.4835, 0.0614, 0.2629, 1.6483, 0.6014, 1.8729,
    0.5425, 0.0912, 0.0658, 0.1542,
];
/// The share of cards you should still remember when they come due.
pub const RETENTION: f64 = 0.9;
const S_MIN: f64 = 0.001;
const S_MAX: f64 = 36500.0;
const D_MIN: f64 = 1.0;
const D_MAX: f64 = 10.0;
const DAY_MS: i64 = 86_400_000;
/// An "Again" card comes back in the same session after this long.
pub const RELEARN_MS: i64 = 10 * 60 * 1000;

/// What FSRS knows about a card's memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Memory {
    /// Days until recall probability falls to 90 %.
    pub stability: f64,
    /// 1 (easy) … 10 (hard).
    pub difficulty: f64,
}

/// Probability of recall after `t` days at stability `s`.
pub fn retrievability(w: &[f64; 21], t: f64, s: f64) -> f64 {
    let decay = -w[20];
    let factor = 0.9f64.powf(1.0 / decay) - 1.0;
    (t / s * factor + 1.0).powf(decay)
}

/// Days until recall probability falls to `retention`.
pub fn interval(w: &[f64; 21], s: f64, retention: f64) -> f64 {
    let decay = -w[20];
    let factor = 0.9f64.powf(1.0 / decay) - 1.0;
    s / factor * (retention.powf(1.0 / decay) - 1.0)
}

fn init_stability(w: &[f64; 21], g: f64) -> f64 {
    w[(g as usize).saturating_sub(1).min(3)]
}

fn init_difficulty(w: &[f64; 21], g: f64) -> f64 {
    w[4] - (w[5] * (g - 1.0)).exp() + 1.0
}

/// The memory after answering `rating` `delta_t` whole days after the
/// previous review (`None`: the first review).
pub fn step(w: &[f64; 21], prev: Option<Memory>, delta_t: f64, rating: Rating) -> Memory {
    let g = rating.grade();
    let Some(prev) = prev else {
        return Memory { stability: init_stability(w, g).clamp(S_MIN, S_MAX), difficulty: init_difficulty(w, g).clamp(D_MIN, D_MAX) };
    };
    let s = prev.stability.clamp(S_MIN, S_MAX);
    let d = prev.difficulty.clamp(D_MIN, D_MAX);
    let r = retrievability(w, delta_t, s);
    let new_s = if delta_t == 0.0 {
        let sinc = (w[17] * (g - 3.0 + w[18])).exp() * s.powf(-w[19]);
        s * if g >= 2.0 { sinc.max(1.0) } else { sinc }
    } else if rating == Rating::Again {
        let new_s = w[11] * d.powf(-w[12]) * ((s + 1.0).powf(w[13]) - 1.0) * ((1.0 - r) * w[14]).exp();
        new_s.min(s / (w[17] * w[18]).exp())
    } else {
        let hard = if rating == Rating::Hard { w[15] } else { 1.0 };
        let easy = if rating == Rating::Easy { w[16] } else { 1.0 };
        s * (w[8].exp() * (11.0 - d) * s.powf(-w[9]) * ((1.0 - r) * w[10]).exp_m1() * hard * easy + 1.0)
    };
    let damped = d + (10.0 - d) * (-w[6] * (g - 3.0)) / 9.0;
    let new_d = (w[7] * (init_difficulty(w, 4.0) - damped) + damped).clamp(D_MIN, D_MAX);
    Memory { stability: new_s.clamp(S_MIN, S_MAX), difficulty: new_d }
}

/// A card's state, derived from its reviews.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardState {
    pub memory: Memory,
    /// Unix ms of the last review.
    pub last_review: i64,
    /// Unix ms when it is next due.
    pub due: i64,
    pub reviews: u32,
    pub lapses: u32,
    pub last_rating: Rating,
}

/// Whole days from `a` to `b` (ms).
pub const fn whole_days(a: i64, b: i64) -> i64 {
    (b - a).div_euclid(DAY_MS)
}

/// A deterministic number in `[0, 1)` from the card id and review count.
fn seeded_unit(card: &str, n: u32) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in card.bytes().chain(n.to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // splitmix64 finish, then 53 bits.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Whole days for an interval, with Anki's fuzz (±5–15 %) seeded by the
/// card and its review count, so it's the same on every device.
pub fn fuzzed_days(ivl: f64, card: &str, reviews: u32) -> i64 {
    let ivl = ivl.clamp(1.0, S_MAX);
    if ivl < 2.5 {
        return ivl.round().max(1.0) as i64;
    }
    let mut delta = 1.0;
    for (start, end, factor) in [(2.5, 7.0, 0.15), (7.0, 20.0, 0.1), (20.0, f64::INFINITY, 0.05)] {
        delta += factor * (ivl.min(end) - start).max(0.0);
    }
    let lo = (ivl - delta).round().max(2.0);
    let hi = (ivl + delta).round().max(lo);
    (seeded_unit(card, reviews) * (hi - lo + 1.0) + lo).floor().clamp(1.0, S_MAX) as i64
}

/// The state after reviewing `card` at `at` (ms) with `rating`.
pub fn review(card: &str, prev: Option<&CardState>, at: i64, rating: Rating) -> CardState {
    let w = &DEFAULT_W;
    let delta = prev.map_or(0, |p| whole_days(p.last_review, at).max(0)) as f64;
    let memory = step(w, prev.map(|p| p.memory), delta, rating);
    let reviews = prev.map_or(0, |p| p.reviews) + 1;
    let lapses = prev.map_or(0, |p| p.lapses) + u32::from(rating == Rating::Again && prev.is_some());
    let due = if rating == Rating::Again {
        at + RELEARN_MS
    } else {
        at + fuzzed_days(interval(w, memory.stability, RETENTION), card, reviews) * DAY_MS
    };
    CardState { memory, last_review: at, due, reviews, lapses, last_rating: rating }
}

/// When the card would next be due for each answer, from `now` (ms).
pub fn preview(card: &str, prev: Option<&CardState>, now: i64) -> [i64; 4] {
    Rating::ALL.map(|r| review(card, prev, now, r).due - now)
}

/// A short label for a time span: `10m`, `1d`, `3w`, `4mo`, `1.2y`.
pub fn span_label(ms: i64) -> String {
    let mins = ms / 60_000;
    let days = ms as f64 / DAY_MS as f64;
    if mins < 60 {
        format!("{}m", mins.max(1))
    } else if days < 1.0 {
        format!("{}h", (ms / 3_600_000).max(1))
    } else if days < 14.0 {
        format!("{}d", days.round() as i64)
    } else if days < 60.0 {
        format!("{}w", (days / 7.0).round() as i64)
    } else if days < 365.0 {
        format!("{}mo", (days / 30.4).round() as i64)
    } else {
        format!("{:.1}y", days / 365.25)
    }
}

/// Every card's state from merged events ([`merge_logs`] order).
pub fn replay(events: &[ReviewEvent]) -> HashMap<String, CardState> {
    let mut out: HashMap<String, CardState> = HashMap::new();
    for e in events {
        match (&e.from, e.rating) {
            (Some(from), _) => {
                if let Some(s) = out.remove(from) {
                    out.insert(e.card.clone(), s);
                }
            }
            (None, Some(r)) => {
                let next = review(&e.card, out.get(&e.card), e.at, r);
                out.insert(e.card.clone(), next);
            }
            (None, None) => {}
        }
    }
    out
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
        assert_eq!(b[0].rating, Some(Rating::Good));
        let merged = merge_logs(vec![b.clone(), a.clone()]);
        assert_eq!(merged.iter().map(|e| e.device.as_str()).collect::<Vec<_>>(), ["d_a", "d_b"]);
        assert_eq!(merge_logs(vec![a.clone(), b]), merged);
        let line = serde_json::to_string(&a[0]).unwrap();
        assert!(line.contains(r#""rating":1"#) && line.contains(r#""extra":true"#));
        let rename = ReviewEvent::rename("k_1", "k_2", "Bio.md", 40, "d_a");
        assert_eq!(parse_log(&rename.to_line()), vec![rename.clone()]);
        assert!(!rename.to_line().contains("rating"));
    }

    #[test]
    fn first_reviews_match_fsrs_docs() {
        // The `fsrs` crate documents these for a new card.
        let m = step(&DEFAULT_W, None, 0.0, Rating::Hard);
        assert!((m.stability - 1.2931).abs() < 1e-6 && (m.difficulty - 5.112_170_7).abs() < 1e-5, "{m:?}");
        assert!((step(&DEFAULT_W, None, 0.0, Rating::Easy).difficulty - 1.0).abs() < 1e-9);
        assert!((interval(&DEFAULT_W, 2.3065, RETENTION) - 2.3065).abs() < 1e-9, "90 % retention: interval = stability");
    }

    #[test]
    fn reviews_space_out_and_lapses_shrink() {
        let day = DAY_MS;
        let mut s = review("k_a", None, 0, Rating::Good);
        let first = s.due;
        assert_eq!(first, 2 * day);
        s = review("k_a", Some(&s), s.due, Rating::Good);
        assert!(s.due - s.last_review > 4 * day, "{s:?}");
        let before = s.memory.stability;
        let lapse = review("k_a", Some(&s), s.due, Rating::Again);
        assert!(lapse.memory.stability < before && lapse.lapses == 1);
        assert_eq!(lapse.due - lapse.last_review, RELEARN_MS);
        let p = preview("k_a", Some(&s), s.due);
        assert!(p[0] < p[1] && p[1] <= p[2] && p[2] <= p[3], "{p:?}");
        assert_eq!(span_label(RELEARN_MS), "10m");
        assert_eq!(span_label(3 * day), "3d");
        assert_eq!(span_label(400 * day), "1.1y");
    }

    #[test]
    fn fuzz_is_deterministic_and_bounded() {
        for ivl in [1.0, 2.0, 3.0, 10.0, 50.0, 400.0] {
            let a = fuzzed_days(ivl, "k_x", 3);
            assert_eq!(a, fuzzed_days(ivl, "k_x", 3));
            assert!((a as f64 - ivl).abs() <= 1.0 + ivl * 0.15 + 0.5, "{ivl} → {a}");
        }
        let spread: std::collections::HashSet<i64> = (0..50).map(|n| fuzzed_days(30.0, "k_y", n)).collect();
        assert!(spread.len() > 1, "fuzz varies with the review count");
    }

    /// Two devices' logs give the same state whichever order they are read in.
    #[test]
    fn two_devices_replay_identically() {
        let day = DAY_MS;
        let a = vec![
            ReviewEvent::review("k_1", "Bio.md", 0, Rating::Good, 0.0, "d_a"),
            ReviewEvent::review("k_1", "Bio.md", 3 * day, Rating::Hard, 3.0, "d_a"),
            ReviewEvent::rename("k_1", "k_9", "Bio.md", 4 * day, "d_a"),
            ReviewEvent::review("k_2", "Bio.md", 5 * day, Rating::Again, 0.0, "d_a"),
        ];
        let b = vec![
            ReviewEvent::review("k_1", "Bio.md", 2 * day, Rating::Easy, 2.0, "d_b"),
            ReviewEvent::review("k_9", "Bio.md", 9 * day, Rating::Good, 5.0, "d_b"),
            ReviewEvent::review("k_2", "Bio.md", 5 * day, Rating::Good, 0.0, "d_b"),
        ];
        let one = replay(&merge_logs(vec![a.clone(), b.clone()]));
        let two = replay(&merge_logs(vec![b, a]));
        assert_eq!(one, two);
        assert!(!one.contains_key("k_1"), "renamed");
        assert_eq!(one["k_9"].reviews, 4);
        // Same time, two devices: device order decides (d_a first).
        assert_eq!(one["k_2"].last_rating, Rating::Good);
    }

    /// The oracle: the `fsrs` crate (dev-dependency) agrees on 10,000 histories.
    #[test]
    fn matches_fsrs_crate() {
        let fsrs = fsrs::FSRS::new(&[]).expect("default parameters");
        let mut seed = 0x853c_49e6_748f_ea9b_u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut worst: f64 = 0.0;
        for _ in 0..10_000 {
            let len = 1 + rnd(12);
            let (mut ours, mut theirs): (Option<Memory>, Option<fsrs::MemoryState>) = (None, None);
            for i in 0..len {
                let rating = Rating::ALL[rnd(4) as usize];
                let delta = if i == 0 { 0 } else { [0, 0, 1, 1, 2, 3, 5, 8, 13, 30, 90, 365][rnd(12) as usize] };
                ours = Some(step(&DEFAULT_W, ours, f64::from(delta), rating));
                let next = fsrs.next_states(theirs, RETENTION as f32, delta).expect("valid state");
                let st = match rating {
                    Rating::Again => next.again,
                    Rating::Hard => next.hard,
                    Rating::Good => next.good,
                    Rating::Easy => next.easy,
                };
                theirs = Some(st.memory);
                let (o, t) = (ours.expect("set"), st.memory);
                let rel = |a: f64, b: f32| (a - f64::from(b)).abs() / f64::from(b).abs().max(1e-3);
                let err = rel(o.stability, t.stability).max(rel(o.difficulty, t.difficulty));
                worst = worst.max(err);
                assert!(err < 2e-3, "step {i}: ours {o:?} theirs {t:?}");
                let ivl = interval(&DEFAULT_W, o.stability, RETENTION);
                assert!((ivl - f64::from(st.interval)).abs() <= 1e-3 * ivl.max(1.0), "interval {ivl} vs {}", st.interval);
            }
        }
        assert!(worst < 2e-3, "{worst}");
    }
}
