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
