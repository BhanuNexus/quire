// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every client path against the stand-in engine (`quire-ai-engine-fake`,
//! design §6.2 WS-D acceptance): the embedder (with its f16 cache and the
//! fallback when the engine is missing), chat with streaming and tool calls,
//! OCR, Stop, crashes and restarts, a hung engine, and the idle exit.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use quire_ai::catalog::Catalog;
use quire_ai::engine::{Engine, EngineConfig, EngineError};
use quire_ai::local::{LocalChat, LocalEmbedder, LocalOcr, LocalSetup, LocalTranscriber, ocr_text};
use quire_ai::protocol::{LoadParams, ModelKind};
use quire_index::semantic::Embedder;
use quire_intel::chat::{CancelFlag, ChatEvent, ChatMessage, ChatModel, ChatRequest, StopReason, ToolDef};
use serde_json::json;

fn fake() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_quire-ai-engine-fake"))
}

fn engine_with(env: &[(&str, &str)], quiet: Option<Duration>) -> Engine {
    Engine::new(EngineConfig {
        path: Some(fake()),
        env: env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        quiet_limit: quiet,
        ..EngineConfig::default()
    })
}

fn catalog() -> Catalog {
    Catalog::builtin()
}

fn setup(dir: &std::path::Path) -> LocalSetup {
    LocalSetup { models_dir: dir.to_path_buf(), threads: 0, gpu: "auto".into(), idle_unload_s: 300, fast_prompts: false }
}

#[test]
fn the_embedder_batches_caches_as_f16_and_survives_a_missing_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let c = catalog();
    let entry = c.get("multilingual-e5-small").unwrap();
    let engine = engine_with(&[], None);
    let cache = tmp.path().join("cache");
    let e = LocalEmbedder::new(engine.clone(), entry, &setup(tmp.path()), Some(&cache));
    assert!(e.name().contains("on this computer"));
    let texts: Vec<String> = (0..70).map(|i| format!("passage {i} about ropes and editors")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let v = e.embed_batch(&refs).unwrap();
    assert_eq!(v.len(), 70, "two engine batches of at most 64");
    assert_eq!(v[0].len(), 16);
    let q = e.embed("ropes and editors");
    let dot: f32 = q.iter().zip(&v[0]).map(|(a, b)| a * b).sum();
    assert!(dot > 0.5, "related texts are close: {dot}");
    drop(e);
    let file = cache.join("vectors-multilingual-e5-small.f16");
    assert!(file.is_file());
    assert_eq!(&std::fs::read(&file).unwrap()[..4], b"QVF1");
    // The engine goes away: cached vectors still come back, new ones fail cleanly.
    engine.configure(EngineConfig { path: Some(tmp.path().join("missing-engine")), ..EngineConfig::default() });
    let e = LocalEmbedder::new(engine, entry, &setup(tmp.path()), Some(&cache));
    let again = e.embed_batch(&refs[..3]).unwrap();
    assert!((again[1][0] - v[1][0]).abs() < 1e-3, "from the f16 cache");
    assert!(e.embed_batch(&["something new"]).is_err());
    assert!(e.embed("query").is_empty(), "a failed query embeds to nothing (search falls back)");
}

#[test]
fn search_falls_back_to_the_built_in_index_when_the_engine_is_missing() {
    use quire_index::semantic::{SemanticIndex, VectorCache};
    let tmp = tempfile::tempdir().unwrap();
    let c = catalog();
    let entry = c.get("bge-small-en-v1.5").unwrap().clone();
    let s = setup(tmp.path());
    let engine = Engine::new(EngineConfig { path: Some(tmp.path().join("nope")), ..EngineConfig::default() });
    let svc = quire_index::IndexService::spawn();
    let builder: quire_index::SemanticBuilder = std::sync::Arc::new(move |chunks| {
        let emb = LocalEmbedder::new(engine.clone(), &entry, &s, None);
        SemanticIndex::try_with_cache(chunks, Box::new(emb), &mut VectorCache::in_memory("bge"))
    });
    svc.set_semantic_builder(Some(builder));
    svc.load(quire_vault::Vault::in_memory(&[("A.md", "# A\nRopes make undo cheap because subtrees are shared.")]));
    svc.flush();
    svc.flush_semantic();
    assert!(svc.semantic_error().is_some_and(|e| e.contains("isn't installed")), "{:?}", svc.semantic_error());
    assert!(svc.semantic().unwrap().embedder_name().contains("latent"), "LSA took over");
}

#[test]
fn chat_streams_text_and_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let c = catalog();
    let chat = LocalChat::new(engine_with(&[], None), c.get("qwen3-1.7b-q4km").unwrap(), &setup(tmp.path()));
    assert!(chat.supports_tools());
    let small = LocalChat::new(engine_with(&[], None), c.get("qwen3-0.6b-q8").unwrap(), &setup(tmp.path()));
    assert!(!small.supports_tools(), "the small model only chats");
    let req = ChatRequest {
        system: "sys".into(),
        messages: vec![ChatMessage::user("Please search for ropes.")],
        tools: vec![ToolDef { name: "search".into(), description: "Search".into(), input_schema: json!({"type": "object"}) }],
        max_tokens: 200,
        temperature: 0.0,
        json: false,
    };
    let mut events = Vec::new();
    chat.chat(&req, &CancelFlag::new(), &mut |e| events.push(e)).unwrap();
    assert_eq!(events[0], ChatEvent::Text { text: "Searching your notes.".into() });
    assert_eq!(events[1], ChatEvent::ToolUse { id: "call_1".into(), name: "search".into(), input: json!({"query": "ropes"}) });
    assert!(matches!(events[2], ChatEvent::Done { stop: StopReason::ToolUse, .. }));
    let (text, _) = quire_intel::chat::chat_text(&chat, &ChatRequest::simple("", "hello there", 50), &CancelFlag::new()).unwrap();
    assert_eq!(text, "You said: hello there");
}

#[test]
fn ocr_reads_images() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("scan.png");
    std::fs::write(&img, b"not really a png").unwrap();
    std::fs::write(tmp.path().join("scan.png.txt"), "Second line\nFirst line").unwrap();
    let c = catalog();
    let ocr = LocalOcr::new(engine_with(&[], None), c.get("ocrs").unwrap(), &setup(tmp.path()));
    let lines = ocr.image(&img, &CancelFlag::new()).unwrap();
    assert_eq!(ocr_text(&lines), "Second line\nFirst line");
    let page = ocr.pdf_page(&tmp.path().join("doc.pdf"), 2, &CancelFlag::new()).unwrap();
    assert_eq!(ocr_text(&page), "Page 3 of doc.pdf");
}

#[test]
fn voice_notes_are_transcribed_through_protocol_two_and_stop_works() {
    let tmp = tempfile::tempdir().unwrap();
    let wav = tmp.path().join("Voice note.wav");
    std::fs::write(&wav, quire_ai::wav::encode_pcm16(&vec![0.1; 48_000], 16_000)).unwrap();
    std::fs::write(tmp.path().join("Voice note.wav.txt"), "Buy milk, then call Sam.").unwrap();
    let c = catalog();
    let engine = engine_with(&[], None);
    let t = LocalTranscriber::new(engine.clone(), c.get("whisper-base").unwrap(), &setup(tmp.path()));
    let r = t.transcribe(&wav, "", &CancelFlag::new()).unwrap();
    assert_eq!((r.text.as_str(), r.language.as_str()), ("Buy milk, then call Sam.", "en"));
    assert!((r.seconds - 3.0).abs() < 0.01);
    assert_eq!(engine.protocol(), Some(quire_ai::protocol::PROTOCOL_VERSION));
    // Not a WAV file: an error, and the engine keeps working.
    let bad = tmp.path().join("bad.wav");
    std::fs::write(&bad, b"OggS").unwrap();
    assert!(t.transcribe(&bad, "", &CancelFlag::new()).is_err());
    let slow = tmp.path().join("__slow__.wav");
    std::fs::write(&slow, quire_ai::wav::encode_pcm16(&[0.0; 1600], 16_000)).unwrap();
    let cancel = CancelFlag::new();
    let stopper = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        stopper.cancel();
    });
    let started = Instant::now();
    assert!(matches!(t.transcribe(&slow, "de", &cancel), Err(EngineError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(t.transcribe(&wav, "de", &CancelFlag::new()).unwrap().language, "de");
}

#[test]
fn stop_answers_at_once_and_the_engine_keeps_working() {
    let tmp = tempfile::tempdir().unwrap();
    let c = catalog();
    let chat = LocalChat::new(engine_with(&[], None), c.get("qwen3-1.7b-q4km").unwrap(), &setup(tmp.path()));
    let cancel = CancelFlag::new();
    let stopper = cancel.clone();
    let mut got = 0;
    let started = Instant::now();
    let r = chat.chat(&ChatRequest::simple("", "__slow__ one two three four five six seven eight", 100), &cancel, &mut |e| {
        if matches!(e, ChatEvent::Text { .. }) {
            got += 1;
            stopper.cancel();
        }
    });
    assert!(matches!(r, Err(quire_intel::llm::LlmError::Cancelled)), "{r:?}");
    assert!(got <= 2, "nothing much arrives after Stop");
    assert!(started.elapsed() < Duration::from_secs(3));
    let (text, _) = quire_intel::chat::chat_text(&chat, &ChatRequest::simple("", "still there?", 50), &CancelFlag::new()).unwrap();
    assert_eq!(text, "You said: still there?");
}

fn load(engine: &Engine, model: &str, path: &str) -> Result<(), EngineError> {
    let spec = LoadParams {
        model: model.into(),
        path: path.into(),
        sha256: "x".into(),
        kind: ModelKind::Llm,
        ctx: 0,
        idle_unload_s: 0,
        threads: 0,
        gpu: String::new(),
        options: json!({}),
    };
    engine.with_model(&spec, |_| Ok(()))
}

#[test]
fn crashes_restart_twice_then_the_engine_gives_up() {
    let engine = engine_with(&[], None);
    load(&engine, "m", "/models/m").unwrap();
    for i in 1..=3 {
        let r = load(&engine, &format!("crash{i}"), "/models/__crash__");
        assert!(matches!(r, Err(EngineError::Failed(_))), "crash {i}: {r:?}");
        if i < 3 {
            load(&engine, "m", "/models/m").unwrap_or_else(|e| panic!("restart {i}: {e}"));
        }
    }
    let r = load(&engine, "m", "/models/m");
    assert!(matches!(r, Err(EngineError::GaveUp(ref m)) if m.contains("Settings → AI")), "{r:?}");
    assert!(engine.snapshot().gave_up.is_some());
    engine.restart_allowed();
    load(&engine, "m", "/models/m").unwrap();
    assert!(engine.snapshot().running);
}

#[test]
fn a_hung_engine_is_restarted_instead_of_waited_for() {
    let engine = engine_with(&[], Some(Duration::from_millis(800)));
    let started = Instant::now();
    let r = load(&engine, "h", "/models/__hang__");
    assert!(matches!(r, Err(EngineError::Failed(ref m)) if m.contains("stopped answering")), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    load(&engine, "m", "/models/m").unwrap();
}

#[test]
fn an_idle_engine_exits_and_comes_back_without_counting_as_a_crash() {
    let engine = engine_with(&[("QUIRE_FAKE_IDLE_EXIT_MS", "300")], None);
    load(&engine, "m", "/models/m").unwrap();
    engine.unload("m");
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!engine.is_running(), "it exited with nothing loaded");
    load(&engine, "m", "/models/m").unwrap();
    assert_eq!(engine.snapshot().crashes, 0);
    engine.refresh_status();
    let s = engine.snapshot();
    assert_eq!(s.loaded.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(), ["m"]);
    engine.stop();
    assert!(!engine.is_running());
}

#[test]
fn a_missing_model_file_is_an_error_not_a_crash() {
    let engine = engine_with(&[], None);
    let r = load(&engine, "m", "/models/missing");
    assert!(matches!(r, Err(EngineError::Refused { code: quire_ai::protocol::code::LOAD_FAILED, .. })), "{r:?}");
    assert_eq!(engine.snapshot().crashes, 0);
}
