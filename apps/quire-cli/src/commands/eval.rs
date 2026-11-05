// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli eval <suite> …` (design §B.6), built only with the `eval`
//! feature and never in releases. The suites and metrics are in
//! `quire_mcp::eval`; this writes `target/eval/<suite>-<date>.json`.

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "eval",
    rank: 90,
    usage: "  quire-cli eval      synthetic-edits [--seed N] | sample-vault [dir] | longmemeval-s <dataset.json> [--limit N]
                      | contradictions [--judge endpoint --url URL --model NAME]
                      [--k 6] [--json] [--embedder mock | --embedder endpoint --url URL --model NAME]
                      measure retrieval: recall@k, citation spans, abstention and a τ_sem sweep, tokens, latency
                      (writes target/eval/). `mock` serves deterministic vectors from a loopback /v1/embeddings",
    run,
}];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn num(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    flag(args, name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} takes a number, not “{v}”")))
}

fn run(args: &[String]) -> Result<(), String> {
    let k = usize::try_from(num(args, "--k", 6)?).unwrap_or(6).clamp(1, 50);
    if arg(args, 1)? == "contradictions" {
        let judge: Option<std::sync::Arc<dyn quire_app::intel::chat::ChatModel>> = match flag(args, "--judge") {
            None | Some("scripted") => None,
            Some("endpoint") => {
                let url = flag(args, "--url").ok_or("--judge endpoint needs --url")?;
                Some(std::sync::Arc::new(quire_app::intel::llm::OpenAiCompatible::new(
                    url,
                    flag(args, "--model").unwrap_or("llama3.2"),
                    None,
                )))
            }
            Some(other) => return Err(format!("unknown judge “{other}” (scripted, endpoint)")),
        };
        return write_report("contradictions", &quire_mcp::eval::contradict::run(judge.as_ref()), args);
    }
    let suite = match arg(args, 1)? {
        "synthetic-edits" => quire_mcp::eval::synthetic(num(args, "--seed", 42)?, 300, 200),
        "sample-vault" => {
            let dir = args.get(2).filter(|a| !a.starts_with("--")).map_or("sample-vault", String::as_str);
            quire_mcp::eval::sample_vault(std::path::Path::new(dir))?
        }
        "longmemeval-s" => {
            let path = arg(args, 2)?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            quire_mcp::eval::longmemeval(&text, usize::try_from(num(args, "--limit", 500)?).unwrap_or(500))?
        }
        other => return Err(format!("unknown suite “{other}” (synthetic-edits, sample-vault, longmemeval-s, contradictions)")),
    };
    let _mock;
    let builder = match flag(args, "--embedder") {
        None | Some("lsa") => None,
        Some("mock") => {
            let m = mock::MockEmbeddings::start()?;
            let b = endpoint_builder(&m.url, mock::MODEL);
            _mock = m;
            Some(b)
        }
        Some("endpoint") => {
            let url = flag(args, "--url").ok_or("--embedder endpoint needs --url (e.g. http://localhost:11434)")?;
            Some(endpoint_builder(url, flag(args, "--model").unwrap_or("nomic-embed-text")))
        }
        Some(other) => return Err(format!("unknown embedder “{other}” (lsa, mock, endpoint)")),
    };
    let report = quire_mcp::eval::run_with(&suite, k, builder);
    write_report(&suite.name, &report, args)
}

fn write_report(name: &str, report: &quire_mcp::eval::Report, args: &[String]) -> Result<(), String> {
    let dir = std::path::Path::new("target").join("eval");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{name}-{}.json", chrono_date()));
    let pretty = serde_json_pretty(&report.json);
    std::fs::write(&file, &pretty).map_err(|e| format!("{}: {e}", file.display()))?;
    if args.iter().any(|a| a == "--json") {
        println!("{pretty}");
    } else {
        print!("{}", report.text);
        println!("  written to {}", file.display());
    }
    Ok(())
}

/// An index builder that embeds through an OpenAI-compatible `/v1/embeddings`.
fn endpoint_builder(url: &str, model: &str) -> quire_app::index::SemanticBuilder {
    use quire_app::index::semantic::{SemanticIndex, VectorCache};
    let (url, model) = (url.to_string(), model.to_string());
    std::sync::Arc::new(move |chunks| {
        let emb = quire_app::intel::embed::EndpointEmbedder::new(&url, &model, None);
        SemanticIndex::try_with_cache(chunks, Box::new(emb), &mut VectorCache::in_memory(&model))
    })
}

/// A loopback `/v1/embeddings` server with deterministic vectors, so the
/// endpoint path (requests, batching, the neural `τ_sem`) runs offline.
///
/// The vectors are hashed features, not a trained model: signed hashes of
/// words, word pairs and character 4-grams in 384 dimensions, normalized.
/// Their similarities show how the abstain rule behaves with a non-LSA
/// embedder; a real model's scale differs, so re-run with
/// `--embedder endpoint` against one before trusting a threshold.
mod mock {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    pub(super) const MODEL: &str = "quire-mock-hash-384";
    const DIMS: usize = 384;

    pub(super) struct MockEmbeddings {
        pub(super) url: String,
    }

    fn fnv(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    /// The vector for `text`.
    pub(super) fn embed(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).collect();
        let mut v = vec![0f32; DIMS];
        let mut add = |key: &str, w: f32| {
            let h = fnv(key);
            let sign = if h >> 63 == 0 { 1.0 } else { -1.0 };
            v[(h % DIMS as u64) as usize] += sign * w;
        };
        for w in &words {
            add(w, 1.0);
            let chars: Vec<char> = format!("^{w}$").chars().collect();
            for g in chars.windows(4) {
                add(&g.iter().collect::<String>(), 0.25);
            }
        }
        for pair in words.windows(2) {
            add(&format!("{} {}", pair[0], pair[1]), 0.5);
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in &mut v {
                *x /= n;
            }
        }
        v
    }

    impl MockEmbeddings {
        pub(super) fn start() -> Result<Self, String> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            let url = format!("http://{}", listener.local_addr().map_err(|e| e.to_string())?);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = serve(stream);
                }
            });
            Ok(Self { url })
        }
    }

    fn serve(mut s: std::net::TcpStream) -> std::io::Result<()> {
        let mut r = BufReader::new(s.try_clone()?);
        let mut line = String::new();
        r.read_line(&mut line)?;
        let mut len = 0;
        loop {
            let mut h = String::new();
            r.read_line(&mut h)?;
            if h == "\r\n" || h.is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let inputs: Vec<String> = match &req["input"] {
            serde_json::Value::String(t) => vec![t.clone()],
            serde_json::Value::Array(a) => a.iter().map(|t| t.as_str().unwrap_or_default().to_string()).collect(),
            _ => Vec::new(),
        };
        let data: Vec<serde_json::Value> =
            inputs.iter().enumerate().map(|(i, t)| serde_json::json!({"index": i, "embedding": embed(t)})).collect();
        let out = serde_json::json!({"data": data, "model": MODEL}).to_string();
        write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len())?;
        s.flush()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn mock_vectors_are_deterministic_and_lexical() {
            let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
            let (a, b, c) =
                (super::embed("Why is undo cheap with ropes?"), super::embed("ropes make undo cheap"), super::embed("sourdough bread"));
            assert_eq!(a, super::embed("Why is undo cheap with ropes?"));
            assert!(dot(&a, &b) > dot(&a, &c) + 0.2);
            assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
        }
    }
}

fn chrono_date() -> String {
    quire_app::chrono_fmt(quire_app::history_ops::now_ms()).split_whitespace().next().unwrap_or("today").to_string()
}

fn serde_json_pretty(v: &quire_mcp::eval::Json) -> String {
    quire_mcp::eval::pretty(v)
}
