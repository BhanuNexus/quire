// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `synthetic-edits` suite at full size runs well within CI's minute
//! (design §B.6) and reports every metric.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::time::{Duration, Instant};

#[test]
fn synthetic_edits_runs_in_under_a_minute() {
    let t = Instant::now();
    let suite = quire_mcp::eval::synthetic(42, 300, 200);
    let r = quire_mcp::eval::run(&suite, 6);
    let took = t.elapsed();
    assert!(took < Duration::from_mins(1), "took {took:?}");
    for k in ["recall_at_k", "note_recall_at_k", "citation_span_accuracy", "abstain_recall", "view_tokens_per_answer"] {
        assert!(r.json[k].is_number(), "{k}: {}", r.json);
    }
    assert!(r.json["note_recall_at_k"].as_f64().unwrap() > 0.5, "Quire finds the right note for most questions: {}", r.text);
    eprintln!("{}", r.text);
}
