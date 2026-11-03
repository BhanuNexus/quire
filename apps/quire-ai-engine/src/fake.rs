// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A stand-in backend for tests and UI checks (design §6.2: "a fake engine
//! drives every client path"). It speaks the real protocol through the same
//! loop, with tiny deterministic "models":
//!
//! * embeddings: 16 hashed word buckets, normalized (texts sharing words are close);
//! * generation: a script from `QUIRE_FAKE_SCRIPT` (a JSON array of turns,
//!   each an array of `{"delta"}` / `{"tool_call"}` events), else simple
//!   rules: a message saying "search for X" calls `search`, a tool result
//!   is summarized, anything else is echoed;
//! * OCR: the text of `<image>.txt` next to the image, else a fixed line.
//!
//! Inputs containing `__crash__` end the process, `__hang__` stops
//! answering, and `__slow__` streams slowly, to test restarts and Stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use quire_ai::protocol::{EmbedParams, GenUsage, GenerateParams, LoadParams, OcrLine, OcrParams, StopKind, code};
use serde_json::{Value, json};

use crate::{Backend, Failure};

pub const DIMS: usize = 16;

#[derive(Debug, Default)]
pub struct FakeBackend {
    script: Vec<Vec<Value>>,
    turn: usize,
    loaded: Vec<String>,
}

impl FakeBackend {
    /// With the script named by `QUIRE_FAKE_SCRIPT`, if any.
    pub fn from_env() -> Self {
        let script = std::env::var_os("QUIRE_FAKE_SCRIPT")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Vec<Vec<Value>>>(&t).ok())
            .unwrap_or_default();
        Self { script, ..Self::default() }
    }
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The fake embedding of `text`.
pub fn embed_one(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    for w in text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
        v[(fnv(&w.to_lowercase()) % DIMS as u64) as usize] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    } else {
        v[0] = 1.0;
    }
    v
}

fn text_of(m: &Value) -> String {
    m["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str().or_else(|| p["content"].as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn trap(text: &str) {
    if text.contains("__crash__") {
        std::process::exit(101);
    }
    if text.contains("__hang__") {
        loop {
            std::thread::sleep(Duration::from_mins(1));
        }
    }
}

impl Backend for FakeBackend {
    fn devices(&self) -> Vec<String> {
        vec!["cpu".into()]
    }

    fn load(&mut self, p: &LoadParams) -> Result<u64, Failure> {
        if p.path.contains("missing") {
            return Err((code::LOAD_FAILED, format!("no model at {}", p.path)));
        }
        trap(&p.path);
        if !self.loaded.contains(&p.model) {
            self.loaded.push(p.model.clone());
        }
        Ok(1_000_000)
    }

    fn unload(&mut self, model: &str) {
        self.loaded.retain(|m| m != model);
    }

    fn embed(&mut self, p: &EmbedParams) -> Result<(u32, Vec<f32>), Failure> {
        let mut flat = Vec::with_capacity(p.texts.len() * DIMS);
        for t in &p.texts {
            trap(t);
            flat.extend(embed_one(&format!("{}{t}", p.prefix)));
        }
        Ok((DIMS as u32, flat))
    }

    fn generate(&mut self, p: &GenerateParams, cancel: &AtomicBool, emit: &mut dyn FnMut(Value)) -> Result<GenUsage, Failure> {
        let last = p.messages.last().cloned().unwrap_or(Value::Null);
        let last_text = text_of(&last);
        trap(&last_text);
        let slow = last_text.contains("__slow__");
        let events: Vec<Value> = if let Some(turn) = self.script.get(self.turn) {
            self.turn += 1;
            turn.clone()
        } else if last["role"] == "tool" {
            let summary: String = last_text.chars().take(160).collect();
            vec![json!({ "delta": format!("Here is what I found: {summary}") })]
        } else if let (Some(i), true, lower) =
            (last_text.to_lowercase().find("search for "), p.tools.as_ref().is_some_and(|t| !t.is_empty()), last_text.to_lowercase())
        {
            let q = lower[i + "search for ".len()..].trim().trim_end_matches(['.', '?']).to_string();
            vec![
                json!({ "delta": "Searching your notes." }),
                json!({ "tool_call": { "id": "call_1", "name": "search", "input": { "query": q } } }),
            ]
        } else {
            let reply = if p.json { json!({ "echo": last_text }).to_string() } else { format!("You said: {last_text}") };
            reply.split_inclusive(' ').map(|w| json!({ "delta": w })).collect()
        };
        let mut out_tokens = 0;
        let mut called = false;
        for ev in events {
            if cancel.load(Ordering::Acquire) {
                return Err((code::CANCELLED, "cancelled".into()));
            }
            if slow {
                std::thread::sleep(Duration::from_millis(200));
            }
            called |= ev.get("tool_call").is_some();
            out_tokens += 1;
            emit(ev);
            if out_tokens >= p.max_tokens {
                return Ok(GenUsage { input_tokens: 10, output_tokens: out_tokens, stop: StopKind::MaxTokens });
            }
        }
        Ok(GenUsage { input_tokens: 10, output_tokens: out_tokens, stop: if called { StopKind::ToolUse } else { StopKind::EndTurn } })
    }

    fn ocr(&mut self, p: &OcrParams, _cancel: &AtomicBool) -> Result<Vec<OcrLine>, Failure> {
        let text = if let Some(img) = &p.image_path {
            trap(img);
            std::fs::read_to_string(format!("{img}.txt")).unwrap_or_else(|_| {
                format!(
                    "Text read from {}",
                    std::path::Path::new(img).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                )
            })
        } else if let Some(pdf) = &p.pdf {
            format!(
                "Page {} of {}",
                pdf.page + 1,
                std::path::Path::new(&pdf.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            )
        } else {
            return Err((code::BAD_PARAMS, "give image_path or pdf".into()));
        };
        Ok(text
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let y = i as f32 * 20.0;
                OcrLine { text: l.to_string(), quad: [[0.0, y], [100.0, y], [100.0, y + 16.0], [0.0, y + 16.0]], conf: 0.99 }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embeddings_put_shared_words_close() {
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let (a, b, c) = (embed_one("ropes make undo cheap"), embed_one("Cheap undo: ROPES make!"), embed_one("bread recipes"));
        assert!(dot(&a, &b) > 0.99);
        assert!(dot(&a, &c) < dot(&a, &b));
        assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fake_generation_follows_simple_rules() {
        let mut f = FakeBackend::default();
        let mut events = Vec::new();
        let p = GenerateParams {
            model: "m".into(),
            system: String::new(),
            messages: vec![json!({"role": "user", "parts": [{"type": "text", "text": "Please search for ropes."}]})],
            tools: Some(vec![json!({"name": "search"})]),
            max_tokens: 100,
            temperature: 0.0,
            stop: None,
            json: false,
        };
        let u = f.generate(&p, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
        assert_eq!(u.stop, StopKind::ToolUse);
        assert_eq!(events[1]["tool_call"]["input"]["query"], "ropes");
        assert!(f.generate(&p, &AtomicBool::new(true), &mut |_| {}).is_err(), "cancelled before the first token");
    }
}

/// The engine built without models: it answers `hello` and refuses to load.
#[derive(Debug, Default)]
pub struct NoModels;

impl Backend for NoModels {
    fn devices(&self) -> Vec<String> {
        vec!["cpu".into()]
    }

    fn load(&mut self, _p: &LoadParams) -> Result<u64, Failure> {
        Err((code::LOAD_FAILED, "this engine was built without models".into()))
    }

    fn unload(&mut self, _model: &str) {}

    fn embed(&mut self, _p: &EmbedParams) -> Result<(u32, Vec<f32>), Failure> {
        Err((code::MODEL_NOT_LOADED, "no models in this build".into()))
    }

    fn generate(&mut self, _p: &GenerateParams, _c: &AtomicBool, _e: &mut dyn FnMut(Value)) -> Result<GenUsage, Failure> {
        Err((code::MODEL_NOT_LOADED, "no models in this build".into()))
    }

    fn ocr(&mut self, _p: &OcrParams, _c: &AtomicBool) -> Result<Vec<OcrLine>, Failure> {
        Err((code::MODEL_NOT_LOADED, "no models in this build".into()))
    }
}
