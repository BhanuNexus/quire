// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The contradiction suite (design §C.5).
//!
//! Claim pairs labelled contradiction, tension, compatible or unrelated, run through the same
//! steps as the app (vectors from the vault's embedder, candidates by
//! cosine and a shared subject word, a judge, the Inbox cut-offs), and
//! scored as the Inbox would show them: precision and recall of
//! "contradiction or tension" items, at the shipped cut-offs and across a
//! sweep.
//!
//! The judge is scripted (each pair's reply is in the fixture) unless a
//! chat model is given; candidate pairs that aren't fixture pairs are
//! answered "unrelated" by the script.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use quire_app::intel::chat::{CancelFlag, ChatModel, chat_text};
use quire_app::intel::contradict::{
    CANDIDATE_MIN_LSA, CANDIDATE_MIN_NEURAL, CONTRADICTION_MIN, ClaimIndex, ClaimRef, JudgeSide, Judgement, Relation, TENSION_MIN,
    judge_request, parse_judgement,
};
use quire_app::vault::{NotePath, Vault};
use serde_json::{Value, json};

use super::{Report, ratio};
use crate::server::{Server, ServerConfig};

const FIXTURES: &str = include_str!("contradictions.jsonl");

struct Pair {
    id: String,
    gold: Relation,
    a: Value,
    b: Value,
    subject_a: String,
    subject_b: String,
    judge: String,
}

fn relation(s: &str) -> Option<Relation> {
    Some(match s {
        "contradiction" => Relation::Contradiction,
        "tension" => Relation::Tension,
        "compatible" => Relation::Compatible,
        "unrelated" => Relation::Unrelated,
        _ => return None,
    })
}

fn pairs() -> Vec<Pair> {
    FIXTURES
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("_comment").is_none())
        .filter_map(|v| {
            Some(Pair {
                id: v["id"].as_str()?.to_string(),
                gold: relation(v["gold"].as_str()?)?,
                a: v["a"].clone(),
                b: v["b"].clone(),
                subject_a: v["subject_a"].as_str().unwrap_or_default().to_string(),
                subject_b: v["subject_b"].as_str().unwrap_or_default().to_string(),
                judge: v["judge"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

fn path(id: &str, side: &str) -> NotePath {
    NotePath::new(format!("Claims/{id}-{side}.md"))
}

fn note(side: &Value) -> String {
    format!("# {}\n\n{}\n", side["title"].as_str().unwrap_or_default(), side["context"].as_str().unwrap_or_default())
}

/// Whether the Inbox shows a judgement at these cut-offs.
fn shown(j: &Judgement, c_min: f32, t_min: f32) -> bool {
    match j.relation {
        Relation::Contradiction => j.confidence >= c_min,
        Relation::Tension => j.confidence >= t_min,
        Relation::Compatible | Relation::Unrelated => false,
    }
}

const fn worth_showing(r: Relation) -> bool {
    matches!(r, Relation::Contradiction | Relation::Tension)
}

/// Run the suite. `judge` replaces the scripted replies with a model.
pub fn run(judge: Option<&Arc<dyn ChatModel>>) -> Report {
    let pairs = pairs();
    let mut files: Vec<(String, String)> = Vec::new();
    for p in &pairs {
        files.push((path(&p.id, "a").as_str().to_string(), note(&p.a)));
        files.push((path(&p.id, "b").as_str().to_string(), note(&p.b)));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let srv = Server::with_config(Vault::in_memory(&refs), None, ServerConfig::default());
    let sem = srv.index().semantic();
    let neural = sem.as_deref().is_some_and(|s| !s.embedder_name().contains("latent semantic"));
    let min_cos = if neural { CANDIDATE_MIN_NEURAL } else { CANDIDATE_MIN_LSA };
    let embed = |t: &str| sem.as_deref().map(|s| s.embed_query(t)).unwrap_or_default();
    // Claims as the app holds them: a vector each, in a ClaimIndex.
    let mut claims = ClaimIndex::default();
    let mut refs_by_pair: HashMap<String, (ClaimRef, ClaimRef)> = HashMap::new();
    for p in &pairs {
        let mk = |side: &Value, subject: &str, s: &str| ClaimRef {
            id: format!("c_{}{s}", p.id),
            text: side["text"].as_str().unwrap_or_default().to_string(),
            subject: subject.to_string(),
            path: path(&p.id, s),
            passage: 0,
            span: (0, 0),
        };
        let (a, b) = (mk(&p.a, &p.subject_a, "a"), mk(&p.b, &p.subject_b, "b"));
        claims.add(a.clone(), embed(&a.text));
        claims.add(b.clone(), embed(&b.text));
        refs_by_pair.insert(p.id.clone(), (a, b));
    }
    // Step 2: candidates of every claim (both directions, as new claims
    // arrive in either order).
    let mut candidate: HashSet<(String, String)> = HashSet::new();
    for (a, b) in refs_by_pair.values() {
        for c in [a, b] {
            for (other, _) in claims.candidates(c, &embed(&c.text), min_cos, &HashSet::new()) {
                let (x, y) = if c.id < other.id { (c.id.clone(), other.id.clone()) } else { (other.id.clone(), c.id.clone()) };
                candidate.insert((x, y));
            }
        }
    }
    let key = |a: &ClaimRef, b: &ClaimRef| if a.id < b.id { (a.id.clone(), b.id.clone()) } else { (b.id.clone(), a.id.clone()) };
    // Step 3: the judge, for each fixture pair that became a candidate.
    let mut judged: Vec<(Relation, Option<Judgement>)> = Vec::new();
    let mut candidate_recall = 0usize;
    let mut must_find = 0usize;
    let mut misses = Vec::new();
    for p in &pairs {
        let (a, b) = &refs_by_pair[&p.id];
        let is_candidate = candidate.contains(&key(a, b));
        if worth_showing(p.gold) {
            must_find += 1;
            if is_candidate {
                candidate_recall += 1;
            } else {
                misses.push(json!({"id": p.id, "stage": "candidates"}));
            }
        }
        let j = is_candidate.then(|| match judge {
            None => parse_judgement(&p.judge),
            Some(m) => {
                let side = |s: &Value| {
                    (
                        s["text"].as_str().unwrap_or_default().to_string(),
                        s["context"].as_str().unwrap_or_default().to_string(),
                        s["title"].as_str().unwrap_or_default().to_string(),
                        s["modified"].as_str().unwrap_or_default().to_string(),
                    )
                };
                let (at, ac, att, am) = side(&p.a);
                let (bt, bc, btt, bm) = side(&p.b);
                let req = judge_request(
                    &JudgeSide { text: &at, context: &ac, title: &att, modified: &am },
                    &JudgeSide { text: &bt, context: &bc, title: &btt, modified: &bm },
                );
                chat_text(m.as_ref(), &req, &CancelFlag::new()).ok().and_then(|(t, _)| parse_judgement(&t))
            }
        });
        judged.push((p.gold, j.flatten()));
    }
    let fixture_keys: HashSet<(String, String)> = refs_by_pair.values().map(|(a, b)| key(a, b)).collect();
    let extra = candidate.iter().filter(|k| !fixture_keys.contains(*k)).count();
    let score = |c_min: f32, t_min: f32| {
        let shown_items: Vec<&(Relation, Option<Judgement>)> =
            judged.iter().filter(|(_, j)| j.as_ref().is_some_and(|j| shown(j, c_min, t_min))).collect();
        let right = shown_items.iter().filter(|(g, _)| worth_showing(*g)).count();
        let same_kind = shown_items.iter().filter(|(g, j)| j.as_ref().is_some_and(|j| j.relation == *g)).count();
        (shown_items.len(), right, same_kind)
    };
    let (n_shown, right, same_kind) = score(CONTRADICTION_MIN, TENSION_MIN);
    let mut sweep = Vec::new();
    for c in [0.6f32, 0.65, 0.7, 0.75, 0.8, 0.85, 0.9] {
        for t in [0.7f32, 0.75, 0.8, 0.85, 0.9] {
            let (n, r, _) = score(c, t);
            let two = |x: f32| (f64::from(x) * 100.0).round() / 100.0;
            sweep
                .push(json!({"contradiction_min": two(c), "tension_min": two(t), "precision": ratio(r, n), "recall": ratio(r, must_find)}));
        }
    }
    for (p, (_, j)) in pairs.iter().zip(&judged) {
        if worth_showing(p.gold) && j.as_ref().is_some_and(|j| !shown(j, CONTRADICTION_MIN, TENSION_MIN)) {
            misses.push(json!({"id": p.id, "stage": "judge", "judgement": j.as_ref().map(|j| json!({"relation": format!("{:?}", j.relation).to_lowercase(), "confidence": j.confidence}))}));
        }
        if !worth_showing(p.gold) && j.as_ref().is_some_and(|j| shown(j, CONTRADICTION_MIN, TENSION_MIN)) {
            misses.push(json!({"id": p.id, "stage": "false alarm"}));
        }
    }
    let by_gold = |r: Relation| pairs.iter().filter(|p| p.gold == r).count();
    let json = json!({
        "suite": "contradictions",
        "pairs": pairs.len(),
        "gold": {"contradiction": by_gold(Relation::Contradiction), "tension": by_gold(Relation::Tension), "compatible": by_gold(Relation::Compatible), "unrelated": by_gold(Relation::Unrelated)},
        "embedder": sem.as_deref().map_or("none", |s| s.embedder_name()),
        "candidate_min": (f64::from(min_cos) * 100.0).round() / 100.0,
        "judge": if judge.is_some() { "model" } else { "scripted" },
        "cut_offs": {"contradiction": CONTRADICTION_MIN, "tension": TENSION_MIN},
        "candidate_recall": ratio(candidate_recall, must_find),
        "extra_candidate_pairs": extra,
        "inbox_items": n_shown,
        "inbox_precision": ratio(right, n_shown),
        "inbox_recall": ratio(right, must_find),
        "kind_accuracy": ratio(same_kind, n_shown),
        "sweep": sweep,
        "misses": misses,
    });
    let mut text = format!("contradictions ({} pairs, judge {})\n", pairs.len(), json["judge"].as_str().unwrap_or_default());
    for k in ["candidate_recall", "extra_candidate_pairs", "inbox_items", "inbox_precision", "inbox_recall", "kind_accuracy"] {
        let _ = writeln!(text, "  {k:<24} {}", json[k]);
    }
    let _ = writeln!(text, "  {:<24} contradiction ≥ {CONTRADICTION_MIN}, tension ≥ {TENSION_MIN}, candidates ≥ {min_cos}", "cut-offs");
    let _ = writeln!(text, "  sweep (contradiction, tension: precision / recall):");
    for row in &sweep {
        let _ = writeln!(text, "    {} {}: {} / {}", row["contradiction_min"], row["tension_min"], row["precision"], row["recall"]);
    }
    Report { json, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_parse_and_the_suite_runs() {
        let p = pairs();
        assert_eq!(p.len(), 32);
        assert!(p.iter().all(|x| parse_judgement(&x.judge).is_some()), "every scripted reply parses");
        let r = run(None);
        assert_eq!(r.json["pairs"], 32);
        assert!(r.json["inbox_precision"].is_number(), "{}", r.text);
    }
}
