// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The providers against loopback servers that replay SSE fixtures in each
//! API's wire format (design §D.4): streaming text, tool calls assembled
//! from pieces, usage, stop reasons, errors, servers that don't stream, and
//! stopping mid-reply.
#![cfg(feature = "remote-llm")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use quire_intel::chat::{CancelFlag, ChatEvent, ChatMessage, ChatModel, ChatRequest, Part, Role, StopReason, ToolDef, Usage, chat_text};
use quire_intel::llm::{AnthropicModel, Assist, LanguageModel, LlmError, OpenAiCompatible, ProviderKind, build_request, probe};
use serde_json::{Value, json};

/// What the mock server answers.
#[derive(Clone)]
enum Reply {
    /// `text/event-stream` with this body.
    Sse(&'static str),
    /// `application/json`.
    Json(u16, String),
    /// Send the first `n` events of an SSE body, then stall.
    Stall(&'static str, usize),
}

/// A loopback HTTP server answering each request with the next reply. It
/// sends (request line, headers, body) for each request it gets.
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn server(replies: Vec<Reply>) -> (String, mpsc::Receiver<(String, String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (stream, reply) in listener.incoming().zip(replies) {
            let mut s = stream.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let (mut len, mut headers) = (0, String::new());
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                headers.push_str(&h.to_ascii_lowercase());
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            tx.send((line.trim().to_string(), headers, body)).unwrap();
            match reply {
                Reply::Sse(text) => {
                    let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
                    // In pieces, as a server streams it.
                    for chunk in text.split_inclusive("\n\n") {
                        let _ = s.write_all(chunk.as_bytes());
                        let _ = s.flush();
                    }
                }
                Reply::Json(status, text) => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                }
                Reply::Stall(text, n) => {
                    let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
                    for chunk in text.split_inclusive("\n\n").take(n) {
                        let _ = s.write_all(chunk.as_bytes());
                        let _ = s.flush();
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    });
    (addr, rx)
}

fn tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "search".into(),
        description: "Search notes".into(),
        input_schema: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
    }]
}

fn conversation() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("What do my notes say about ropes?"),
        ChatMessage {
            role: Role::Assistant,
            parts: vec![Part::ToolUse { id: "toolu_00".into(), name: "search".into(), input: json!({"query": "ropes"}) }],
        },
        ChatMessage {
            role: Role::Tool,
            parts: vec![Part::ToolResult { id: "toolu_00".into(), content: "Rope Notes.md".into(), is_error: false }],
        },
    ]
}

fn run(m: &dyn ChatModel, req: &ChatRequest) -> (Vec<ChatEvent>, Result<(), LlmError>) {
    let mut events = Vec::new();
    let r = m.chat(req, &CancelFlag::new(), &mut |e| events.push(e));
    (events, r)
}

#[test]
fn anthropic_streams_text_and_assembles_tool_calls() {
    let (addr, seen) = server(vec![Reply::Sse(include_str!("fixtures/anthropic-tools.sse"))]);
    let m = AnthropicModel::new("sk-ant-test", "claude-sonnet-5-5").with_base_url(&addr);
    let req = ChatRequest {
        system: "You are Quire's agent.".into(),
        messages: conversation(),
        tools: tools(),
        max_tokens: 800,
        temperature: 0.2,
        json: false,
    };
    let (events, r) = run(&m, &req);
    r.unwrap();
    assert_eq!(
        events,
        vec![
            ChatEvent::Text { text: "I'll search ".into() },
            ChatEvent::Text { text: "your notes.".into() },
            ChatEvent::ToolUse { id: "toolu_01A".into(), name: "search".into(), input: json!({"query": "ropes", "limit": 5}) },
            ChatEvent::Done { stop: StopReason::ToolUse, usage: Usage { input_tokens: 412, output_tokens: 61 } },
        ]
    );
    let (line, headers, body) = seen.recv().unwrap();
    assert_eq!(line, "POST /v1/messages HTTP/1.1");
    assert!(headers.contains("x-api-key: sk-ant-test") && headers.contains("anthropic-version: 2023-06-01"), "{headers}");
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"], "You are Quire's agent.");
    assert_eq!(body["tools"][0]["input_schema"]["required"][0], "query");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_00");
}

#[test]
fn anthropic_errors_in_the_stream_are_errors() {
    let (addr, _seen) = server(vec![
        Reply::Sse(include_str!("fixtures/anthropic-error.sse")),
        Reply::Json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into()),
    ]);
    let m = AnthropicModel::new("bad", "claude-sonnet-5-5").with_base_url(&addr);
    let (_, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("Overloaded")), "{r:?}");
    let (_, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("401") && e.contains("invalid x-api-key")), "{r:?}");
}

#[test]
fn anthropic_json_mode_asks_in_the_system_prompt_and_complete_works() {
    let non_streamed = json!({"content": [{"type": "text", "text": "{\"ok\": true}"}], "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 4}});
    let (addr, seen) = server(vec![Reply::Json(200, non_streamed.to_string()), Reply::Sse(include_str!("fixtures/anthropic-tools.sse"))]);
    let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
    let mut req = ChatRequest::simple("Extract claims.", "text", 100);
    req.json = true;
    let (text, stop) = chat_text(&m, &req, &CancelFlag::new()).unwrap();
    assert_eq!((text.as_str(), stop), ("{\"ok\": true}", StopReason::EndTurn), "a server that doesn't stream still works");
    assert!(seen.recv().unwrap().2["system"].as_str().unwrap().contains("JSON"));
    // The one-shot port for the assist features rides on the same call.
    let out = LanguageModel::complete(&m, &Assist::Summarize, &build_request(&Assist::Summarize, "x")).unwrap();
    assert_eq!(out, "I'll search your notes.");
}

#[test]
fn openai_streams_text_and_parallel_tool_calls() {
    let (addr, seen) = server(vec![Reply::Sse(include_str!("fixtures/openai-tools.sse"))]);
    let m = OpenAiCompatible::new(format!("{addr}/v1/"), "gpt-5", Some("sk-test".into()));
    let req = ChatRequest { system: "sys".into(), messages: conversation(), tools: tools(), max_tokens: 500, temperature: 0.0, json: true };
    let (events, r) = run(&m, &req);
    r.unwrap();
    assert_eq!(
        events,
        vec![
            ChatEvent::Text { text: "Let me look".into() },
            ChatEvent::Text { text: " that up.".into() },
            ChatEvent::ToolUse { id: "call_abc".into(), name: "view".into(), input: json!({"path": "Rope Notes.md"}) },
            ChatEvent::ToolUse { id: "call_def".into(), name: "outline".into(), input: json!({"path": "Ropes.md"}) },
            ChatEvent::Done { stop: StopReason::ToolUse, usage: Usage { input_tokens: 388, output_tokens: 42 } },
        ]
    );
    let (line, headers, body) = seen.recv().unwrap();
    assert_eq!(line, "POST /v1/chat/completions HTTP/1.1");
    assert!(headers.contains("authorization: bearer sk-test"), "{headers}");
    assert_eq!(body["messages"][0], json!({"role": "system", "content": "sys"}));
    assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "search");
    assert_eq!(body["messages"][3], json!({"role": "tool", "tool_call_id": "toolu_00", "content": "Rope Notes.md"}));
    assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(body["response_format"]["type"], "json_object");
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn openai_compatible_complete_embeddings_and_probe() {
    let (addr, seen) = server(vec![
        Reply::Sse(include_str!("fixtures/openai-text.sse")),
        Reply::Json(200, r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#.into()),
        Reply::Json(200, r#"{"data":[{"id":"llama3.2"},{"id":"nomic-embed-text"}]}"#.into()),
        Reply::Json(200, r#"{"choices":[{"message":{"role":"assistant","content":"plain json"},"finish_reason":"stop"}]}"#.into()),
    ]);
    let m = OpenAiCompatible::new(&addr, "llama3.2", None);
    let out = LanguageModel::complete(&m, &Assist::Summarize, &build_request(&Assist::Summarize, "text")).unwrap();
    assert_eq!(out, "- local answer");
    let (_, headers, _) = seen.recv().unwrap();
    assert!(!headers.contains("authorization"), "no key, no header");

    let v = m.embed("nomic-embed-text", &["a", "b"]).unwrap();
    assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]], "re-ordered by index");
    let _ = seen.recv().unwrap();

    let p = probe(ProviderKind::OpenAiCompatible, &addr, None).unwrap();
    assert!(p.contains("2 models") && p.contains("llama3.2"), "{p}");
    let _ = seen.recv().unwrap();

    let (text, _) = chat_text(&m, &ChatRequest::simple("", "hi", 10), &CancelFlag::new()).unwrap();
    assert_eq!(text, "plain json", "servers that ignore stream: true still work");
}

#[test]
fn stop_ends_a_stalled_stream_and_unreachable_servers_fail_fast() {
    let (addr, _seen) = server(vec![Reply::Stall(include_str!("fixtures/openai-tools.sse"), 2)]);
    let m = OpenAiCompatible::new(&addr, "gpt-5", None);
    let cancel = CancelFlag::new();
    let stopper = cancel.clone();
    let mut texts = 0;
    let started = std::time::Instant::now();
    let r = m.chat(&ChatRequest::simple("", "hi", 10), &cancel, &mut |e| {
        if matches!(e, ChatEvent::Text { .. }) {
            texts += 1;
            stopper.cancel();
        }
    });
    assert!(matches!(r, Err(LlmError::Cancelled)), "{r:?}");
    assert_eq!(texts, 1, "nothing more arrives after Stop");
    assert!(started.elapsed() < Duration::from_secs(4), "Stop doesn't wait for the stalled server");

    let m = OpenAiCompatible::new("http://127.0.0.1:9", "x", None);
    assert!(m.complete(&Assist::Summarize, &build_request(&Assist::Summarize, "t")).is_err());
}

/// One request to a loopback server: (headers, raw body); answers `status` with `json`.
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn raw_server(status: u16, json: &'static str) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut s = stream;
        let mut r = BufReader::new(s.try_clone().unwrap());
        let (mut len, mut head) = (0, String::new());
        loop {
            let mut h = String::new();
            r.read_line(&mut h).unwrap();
            if h == "\r\n" || h.is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            head.push_str(&h);
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        tx.send((head, body)).unwrap();
        let _ = write!(
            s,
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
            json.len()
        );
    });
    (addr, rx)
}

#[test]
fn openai_transcription_sends_the_recording_as_multipart() {
    let (addr, rx) = raw_server(200, r#"{"text": " Buy milk and call Sam. "}"#);
    let m = OpenAiCompatible::new(addr, "chat-model", Some("sk-test".into()));
    let wav = b"RIFF\x24\0\0\0WAVEfmt fake audio bytes";
    let text = m.transcribe("whisper-1", wav, "Voice 2026-11-05.wav", "en").unwrap();
    assert_eq!(text, "Buy milk and call Sam.");
    let (head, body) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(head.starts_with("POST /v1/audio/transcriptions "), "{head}");
    assert!(head.to_ascii_lowercase().contains("authorization: bearer sk-test"));
    let boundary = head.lines().find_map(|l| l.to_ascii_lowercase().split("boundary=").nth(1).map(str::to_string)).unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"), "{body}");
    assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"));
    assert!(body.contains("filename=\"Voice 2026-11-05.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF"));
    assert!(body.trim_end().ends_with(&format!("--{boundary}--")));

    let (addr, _rx) = raw_server(401, r#"{"error": {"message": "Incorrect API key"}}"#);
    let err = OpenAiCompatible::new(addr, "m", None).transcribe("whisper-1", wav, "a.wav", "").unwrap_err();
    assert!(err.to_string().contains("HTTP 401: Incorrect API key"), "{err}");
}

// ---------------------------------------------------------------- 0.13.1: documented streaming and error cases
//
// Checked against the providers' published wire formats (Anthropic's Messages
// API types in its SDK, Ollama's `docs/api` and OpenAI-compatibility pages).
// No live provider was reachable: these replay the documented shapes.

const ANTHROPIC_PING_AND_EXTRAS: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":25,\"output_tokens\":1,\"cache_read_input_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQBCgJkAhIM\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"citations_delta\",\"citation\":{\"type\":\"char_location\",\"cited_text\":\"x\"}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: some_future_event\ndata: {\"type\":\"some_future_event\",\"x\":1}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":25,\"output_tokens\":15,\"cache_read_input_tokens\":0}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

fn anthropic_stop_sse(reason: &str) -> &'static str {
    let text = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":1}}}}}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":7}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    Box::leak(text.into_boxed_str())
}

#[test]
fn anthropic_ignores_pings_thinking_citations_and_unknown_events() {
    let (addr, _seen) = server(vec![Reply::Sse(ANTHROPIC_PING_AND_EXTRAS)]);
    let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
    let (events, r) = run(&m, &ChatRequest::simple("", "hi", 50));
    r.unwrap();
    assert_eq!(
        events,
        vec![
            ChatEvent::Text { text: "Hello".into() },
            ChatEvent::Text { text: " there".into() },
            ChatEvent::Done { stop: StopReason::EndTurn, usage: Usage { input_tokens: 25, output_tokens: 15 } },
        ]
    );
}

#[test]
fn anthropic_stop_reasons_beyond_the_first_four() {
    for (reason, want) in [
        ("max_tokens", StopReason::MaxTokens),
        ("model_context_window_exceeded", StopReason::MaxTokens),
        ("pause_turn", StopReason::EndTurn),
    ] {
        let (addr, _seen) = server(vec![Reply::Sse(anthropic_stop_sse(reason))]);
        let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
        let (events, r) = run(&m, &ChatRequest::simple("", "hi", 50));
        r.unwrap();
        assert!(matches!(events.last(), Some(ChatEvent::Done { stop, .. }) if *stop == want), "{reason}: {events:?}");
    }
    // A refusal is said, not shown as an ordinary end of the reply.
    let (addr, _seen) = server(vec![Reply::Sse(anthropic_stop_sse("refusal"))]);
    let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
    let (events, r) = run(&m, &ChatRequest::simple("", "hi", 50));
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("declined") && e.contains("refusal")), "{r:?}");
    assert!(!events.iter().any(|e| matches!(e, ChatEvent::Done { .. })));
}

#[test]
fn anthropic_error_bodies_name_their_type_and_status() {
    for (status, kind, msg) in [
        (400, "invalid_request_error", "messages: first message must use the \"user\" role"),
        (403, "permission_error", "Your API key does not have permission"),
        (404, "not_found_error", "model: claude-nope"),
        (413, "request_too_large", "Request exceeds the maximum allowed number of bytes"),
        (429, "rate_limit_error", "This request would exceed your rate limit"),
        (500, "api_error", "Internal server error"),
        (529, "overloaded_error", "Overloaded"),
    ] {
        let body = json!({"type": "error", "error": {"type": kind, "message": msg}, "request_id": "req_011"}).to_string();
        let (addr, _seen) = server(vec![Reply::Json(status, body)]);
        let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
        let (events, r) = run(&m, &ChatRequest::simple("", "hi", 10));
        assert!(events.is_empty());
        assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains(&format!("HTTP {status}")) && e.contains(msg)), "{status}: {r:?}");
    }
    // The error event inside a stream names its type.
    let (addr, _seen) = server(vec![Reply::Sse(include_str!("fixtures/anthropic-error.sse"))]);
    let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
    let (_, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("overloaded_error") && e.contains("Overloaded")), "{r:?}");
}

#[test]
fn a_provider_that_refuses_a_parameter_is_asked_again_without_it() {
    // Newer Anthropic models refuse `temperature`.
    let refuse =
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "`temperature` is not supported for this model"}})
            .to_string();
    let (addr, seen) = server(vec![Reply::Json(400, refuse), Reply::Sse(anthropic_stop_sse("end_turn"))]);
    let m = AnthropicModel::new("k", "claude-sonnet-5-5").with_base_url(&addr);
    let mut req = ChatRequest::simple("", "hi", 50);
    req.temperature = 0.3;
    let (events, r) = run(&m, &req);
    r.unwrap();
    assert!(matches!(events.last(), Some(ChatEvent::Done { .. })));
    let first = seen.recv().unwrap().2;
    let second = seen.recv().unwrap().2;
    assert!(first.get("temperature").is_some() && second.get("temperature").is_none(), "{first} / {second}");
    assert_eq!(second["messages"], first["messages"], "nothing else changes");

    // OpenAI's newer models want `max_completion_tokens`, and some refuse `temperature` as well.
    let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (addr, seen) = server(vec![
        Reply::Json(400, json!({"error": {"message": "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.", "type": "invalid_request_error", "param": "max_tokens", "code": "unsupported_parameter"}}).to_string()),
        Reply::Json(400, json!({"error": {"message": "Unsupported value: 'temperature' does not support 0.3 with this model. Only the default (1) value is supported."}}).to_string()),
        Reply::Sse(ok),
    ]);
    let m = OpenAiCompatible::new(&addr, "gpt-x", Some("sk-test".into()));
    let (events, r) = run(&m, &req);
    r.unwrap();
    assert_eq!(events[0], ChatEvent::Text { text: "ok".into() });
    let bodies: Vec<Value> = (0..3).map(|_| seen.recv().unwrap().2).collect();
    assert!(bodies[0].get("max_tokens").is_some());
    assert!(bodies[1].get("max_tokens").is_none() && bodies[1]["max_completion_tokens"] == 50 && bodies[1].get("temperature").is_some());
    assert!(bodies[2].get("temperature").is_none() && bodies[2]["max_completion_tokens"] == 50);

    // A 400 about something else is not retried, and says what it said.
    let (addr, seen) = server(vec![Reply::Json(400, json!({"error": {"message": "messages: too long"}}).to_string())]);
    let m = OpenAiCompatible::new(&addr, "gpt-x", None);
    let (_, r) = run(&m, &req);
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("HTTP 400") && e.contains("too long")), "{r:?}");
    assert!(seen.recv().is_ok() && seen.try_recv().is_err(), "one request only");
}

#[test]
fn ollama_errors_are_plain_strings_and_mid_stream_errors_stop_the_reply() {
    // Ollama: `{"error": "…"}` with 404 for a model that isn't pulled, 400, 429 and 502.
    for (status, msg) in [
        (404, "model \"llama9\" not found, try pulling it first"),
        (400, "invalid JSON"),
        (429, "too many requests"),
        (502, "cloud model unreachable"),
    ] {
        let (addr, _seen) = server(vec![Reply::Json(status, json!({ "error": msg }).to_string())]);
        let m = OpenAiCompatible::new(&addr, "llama9", None);
        let (_, r) = run(&m, &ChatRequest::simple("", "hi", 10));
        assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains(&format!("HTTP {status}")) && e.contains(msg)), "{status}: {r:?}");
    }
    // An error object in the middle of the SSE stream (the status is already 200).
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Yes\"},\"finish_reason\":null}]}\n\ndata: {\"error\":{\"message\":\"an error was encountered while running the model\"}}\n\n";
    let (addr, _seen) = server(vec![Reply::Sse(sse)]);
    let m = OpenAiCompatible::new(&addr, "llama3", None);
    let (events, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    assert_eq!(events, vec![ChatEvent::Text { text: "Yes".into() }], "what arrived before is kept");
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("cut off") && e.contains("while running the model")), "{r:?}");
    // …and the error given as a bare string, as Ollama's own API sends it.
    let sse = "data: {\"error\":\"model runner has unexpectedly stopped\"}\n\n";
    let (addr, _seen) = server(vec![Reply::Sse(sse)]);
    let m = OpenAiCompatible::new(&addr, "llama3", None);
    let (_, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    assert!(matches!(r, Err(LlmError::Request(ref e)) if e.contains("unexpectedly stopped")), "{r:?}");
    // Usage arrives in a last chunk with no choices (`stream_options.include_usage`); a stream with the finish and usage chunks and no `[DONE]` still ends.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"length\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n";
    let (addr, _seen) = server(vec![Reply::Sse(sse)]);
    let m = OpenAiCompatible::new(&addr, "llama3", None);
    let (events, r) = run(&m, &ChatRequest::simple("", "hi", 10));
    r.unwrap();
    assert_eq!(events.last(), Some(&ChatEvent::Done { stop: StopReason::MaxTokens, usage: Usage { input_tokens: 9, output_tokens: 4 } }));
}
