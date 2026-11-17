// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hostile JSON nested 100,000 deep (lists, maps, a mix) must be refused by
//! every JSON reader of the canvas crate, not overflow the stack. Each case
//! runs on an ordinary spawned thread (2 MiB), so an overflow aborts the
//! test run and fails it.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use quire_canvas::{drawing, excalidraw, model, ojson};

fn deep_json_inputs() -> Vec<String> {
    let n = 100_000;
    vec![
        "[".repeat(n),
        format!("{}1{}", "[".repeat(n), "]".repeat(n)),
        format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n)),
        format!("{}1{}", "[{\"a\":".repeat(n / 2), "}]".repeat(n / 2)),
    ]
}

fn on_a_normal_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().spawn(f).expect("a thread").join().expect("no panic and no abort");
}

#[test]
fn canvas_files() {
    on_a_normal_stack(|| {
        for text in deep_json_inputs() {
            assert!(model::parse(&text).is_err());
            let _ = model::parse(&format!("{{\"nodes\":[],\"edges\":[],\"x\":{text}}}"));
        }
    });
}

#[test]
fn ordered_json_reader() {
    on_a_normal_stack(|| {
        for text in deep_json_inputs() {
            assert!(ojson::parse(&text).is_err());
            assert!(ojson::parse(&format!("{{\"nodes\":[],\"x\":{text}}}")).is_err());
        }
    });
}

#[test]
fn excalidraw_files() {
    on_a_normal_stack(|| {
        for text in deep_json_inputs() {
            assert!(excalidraw::parse_json(&text).is_err());
            assert!(excalidraw::parse_json(&format!("{{\"elements\":[],\"appState\":{text}}}")).is_err());
            assert!(excalidraw::parse(&text).is_err());
            let _ = drawing::Drawing::parse(&text);
        }
    });
}
