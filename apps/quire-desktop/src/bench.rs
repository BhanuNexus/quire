// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Timing runs of the real app for the CI `perf` job (design 0.9 §9,
//! docs/design/0.10-perf.md). Only in builds with the `bench` feature,
//! never in releases.
//!
//! `QUIRE_BENCH=startup quire <vault>`: from `main` to the first frame on
//! screen (the second frame starts once the first was presented).
//!
//! `QUIRE_BENCH=typing quire <vault>`: opens `QUIRE_BENCH_NOTE` (default
//! `Big.md`), waits until the vault is indexed (at most 60 s), then types a
//! character a frame and times each keystroke from its input to the end of
//! that frame's UI work (painting is the GPU's, and Xvfb's software GL says
//! nothing about a real one). Background jobs run as usual. It also reports
//! the same measure while the vault is first indexed (`indexing_p50_ms`):
//! on two cores the index threads take turns with the UI.
//!
//! `QUIRE_BENCH=canvas quire`: draws a 1,000-card canvas without a window
//! and times pan and zoom frames.
//!
//! Each run prints JSON lines `{"suite", "metric", "value"}` (milliseconds)
//! to stdout and exits.

use std::sync::OnceLock;
use std::time::Instant;

static STARTED: OnceLock<Instant> = OnceLock::new();

/// Call first thing in `main`.
pub(crate) fn mark_start() {
    let _ = STARTED.get_or_init(Instant::now);
}

/// Keystrokes timed, after the warm-up.
const KEYSTROKES: usize = 400;
/// Frames before typing starts (the note opens, fonts and caches warm up).
const WARMUP_FRAMES: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Startup,
    Typing,
}

/// A bench run inside the app (`QuireApp` holds one when `QUIRE_BENCH` asks).
#[derive(Debug)]
pub(crate) struct Run {
    kind: Kind,
    frames: u32,
    input_at: Option<Instant>,
    samples: Vec<f64>,
    /// Keystrokes timed while the vault was still being indexed.
    early: Vec<f64>,
    /// Whether the keystroke being timed came before the vault was indexed.
    early_now: bool,
    note_opened: bool,
    started: Instant,
}

/// The longest the typing run waits for the vault to be indexed.
const SETTLE_MAX: std::time::Duration = std::time::Duration::from_mins(1);

pub(crate) fn print(suite: &str, metric: &str, value: f64) {
    println!("{{\"suite\": \"{suite}\", \"metric\": \"{metric}\", \"value\": {value:.3}}}");
}

/// The `p`th percentile (0–100) of sorted `v`.
pub(crate) fn percentile(v: &[f64], p: usize) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v[(v.len() * p / 100).min(v.len() - 1)]
}

impl Run {
    pub(crate) fn from_env() -> Option<Self> {
        let kind = match std::env::var("QUIRE_BENCH").ok()?.as_str() {
            "startup" => Kind::Startup,
            "typing" => Kind::Typing,
            _ => return None,
        };
        Some(Self {
            kind,
            frames: 0,
            input_at: None,
            samples: Vec::with_capacity(KEYSTROKES),
            early: Vec::new(),
            early_now: false,
            note_opened: false,
            started: Instant::now(),
        })
    }

    /// The note to type in, once (after the first frame).
    pub(crate) fn note_to_open(&mut self) -> Option<String> {
        if self.kind != Kind::Typing || self.note_opened || self.frames < 2 {
            return None;
        }
        self.note_opened = true;
        Some(std::env::var("QUIRE_BENCH_NOTE").unwrap_or_else(|_| "Big.md".into()))
    }

    /// The start of a frame: time the first one, or type a character.
    /// `indexed`: the vault's index and meaning index are built.
    pub(crate) fn input(&mut self, raw: &mut egui::RawInput, indexed: bool) {
        self.frames += 1;
        match self.kind {
            Kind::Startup if self.frames == 2 => {
                let ms = STARTED.get().map_or(f64::NAN, |t| t.elapsed().as_secs_f64() * 1000.0);
                print("startup", "first_frame_ms", ms);
                std::process::exit(0);
            }
            Kind::Typing if self.frames > WARMUP_FRAMES => {
                let n = self.frames - WARMUP_FRAMES;
                let text = if n.is_multiple_of(9) { " " } else { "x" };
                raw.events.push(egui::Event::Text(text.into()));
                self.input_at = Some(Instant::now());
                self.early_now = !indexed && self.started.elapsed() < SETTLE_MAX;
            }
            _ => {}
        }
    }

    /// The end of a frame's UI work: keep frames coming, record, finish.
    pub(crate) fn frame_done(&mut self, ctx: &egui::Context) {
        ctx.request_repaint();
        if let Some(t) = self.input_at.take() {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if self.early_now { self.early.push(ms) } else { self.samples.push(ms) }
        }
        if self.samples.len() >= KEYSTROKES {
            let mut v = std::mem::take(&mut self.samples);
            v.sort_by(f64::total_cmp);
            let mut early = std::mem::take(&mut self.early);
            if !early.is_empty() {
                early.sort_by(f64::total_cmp);
                print("typing", "indexing_p50_ms", percentile(&early, 50));
            }
            print("typing", "p50_ms", percentile(&v, 50));
            print("typing", "p95_ms", percentile(&v, 95));
            print("typing", "p99_ms", percentile(&v, 99));
            std::process::exit(0);
        }
    }
}

/// `QUIRE_BENCH=canvas`: the headless canvas run. Returns whether it ran.
pub(crate) fn headless() -> bool {
    if std::env::var("QUIRE_BENCH").ok().as_deref() != Some("canvas") {
        return false;
    }
    let mut times = crate::canvas_ui::bench_thousand_cards();
    times.sort_by(f64::total_cmp);
    print("canvas", "frame_p50_ms", percentile(&times, 50));
    print("canvas", "frame_max_ms", times.last().copied().unwrap_or(f64::NAN));
    true
}
