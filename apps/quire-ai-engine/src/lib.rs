// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The engine's request loop (design §D.3), shared by the real engine and
//! the test stand-in: JSON lines on stdin, replies and streamed events on
//! stdout, model work on one worker thread so `cancel` is heard while a
//! model runs, idle models unloaded, and an exit after ten minutes with
//! nothing loaded (the app starts the engine again when it needs it).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine as _;
use quire_ai::protocol::{
    EmbedParams, EmbedResult, GenUsage, GenerateParams, HelloParams, HelloResult, LoadParams, LoadResult, LoadedModel, OcrLine, OcrParams,
    OcrResult, PROTOCOL_VERSION, Reply, Request, StatusResult, code, method,
};
use serde_json::{Value, json};

pub mod template;

#[cfg(feature = "models")]
pub mod models;

#[cfg(feature = "models")]
pub mod qwen3;

#[cfg(feature = "models")]
pub mod whisper;

pub mod fake;

/// An error for a reply: a `protocol::code` and a message.
pub type Failure = (i32, String);

/// What runs the models. One instance lives on the worker thread.
pub trait Backend: Send + 'static {
    /// `cpu`, `metal`.
    fn devices(&self) -> Vec<String>;
    /// Load a model; returns the memory it takes.
    fn load(&mut self, p: &LoadParams) -> Result<u64, Failure>;
    fn unload(&mut self, model: &str);
    /// Rows of `dims` floats, one per text.
    fn embed(&mut self, p: &EmbedParams) -> Result<(u32, Vec<f32>), Failure>;
    /// Stream `{"delta"}` and `{"tool_call"}` events through `emit`.
    fn generate(&mut self, p: &GenerateParams, cancel: &AtomicBool, emit: &mut dyn FnMut(Value)) -> Result<GenUsage, Failure>;
    fn ocr(&mut self, p: &OcrParams, cancel: &AtomicBool) -> Result<Vec<OcrLine>, Failure>;
}

/// Unload a model after this long without use when `load` didn't say.
pub const DEFAULT_IDLE_UNLOAD: Duration = Duration::from_mins(5);
/// Exit after this long with nothing loaded.
pub const EXIT_WHEN_EMPTY: Duration = Duration::from_mins(10);

struct Loaded {
    kind: quire_ai::protocol::ModelKind,
    mem: u64,
    idle_after: Duration,
    last_used: Instant,
}

#[derive(Default)]
struct State {
    loaded: HashMap<String, Loaded>,
    busy: bool,
    empty_since: Option<Instant>,
}

struct Job {
    req: Request,
    cancel: Arc<AtomicBool>,
}

enum Work {
    Job(Job),
    Unload(String),
}

type Out = Arc<Mutex<std::io::Stdout>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn send(out: &Out, r: &Reply) -> bool {
    let line = serde_json::to_string(r).unwrap_or_default();
    let mut o = lock(out);
    writeln!(o, "{line}").and_then(|()| o.flush()).is_ok()
}

/// Run the loop until stdin closes, `shutdown`, or ten idle minutes with
/// nothing loaded. `idle_exit` replaces those ten minutes (tests).
pub fn serve<B: Backend>(backend: B, idle_exit: Option<Duration>) {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let state = Arc::new(Mutex::new(State { empty_since: Some(Instant::now()), ..State::default() }));
    let (work_tx, work_rx) = channel::<Work>();
    let cancels: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::default();
    {
        let (out, state, cancels) = (Arc::clone(&out), Arc::clone(&state), Arc::clone(&cancels));
        std::thread::Builder::new()
            .name("quire-ai-worker".into())
            .spawn(move || worker(backend, &work_rx, &out, &state, &cancels))
            .expect("the worker thread starts");
    }
    let (line_tx, line_rx) = channel::<Option<String>>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line_tx.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = line_tx.send(None);
    });
    let idle_exit = idle_exit.unwrap_or(EXIT_WHEN_EMPTY);
    let mut greeted = false;
    loop {
        match line_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match control(&line, &mut greeted, &state, &cancels, &work_tx) {
                    Control::Reply(r) => {
                        if !send(&out, &r) {
                            return;
                        }
                    }
                    Control::Queued => {}
                    Control::Exit(r) => {
                        send(&out, &r);
                        return;
                    }
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                let st = lock(&state);
                let now = Instant::now();
                for (m, l) in &st.loaded {
                    if !st.busy && now.duration_since(l.last_used) > l.idle_after {
                        let _ = work_tx.send(Work::Unload(m.clone()));
                    }
                }
                if !st.busy && st.loaded.is_empty() && st.empty_since.is_some_and(|t| now.duration_since(t) > idle_exit) {
                    return;
                }
            }
        }
    }
}

enum Control {
    Reply(Reply),
    Queued,
    Exit(Reply),
}

/// Requests answered on the reading thread; the rest are queued.
fn control(
    line: &str,
    greeted: &mut bool,
    state: &Mutex<State>,
    cancels: &Mutex<HashMap<u64, Arc<AtomicBool>>>,
    work: &Sender<Work>,
) -> Control {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Control::Reply(Reply::err(0, code::BAD_PARAMS, format!("not a request: {e}"))),
    };
    match req.method.as_str() {
        method::HELLO => Control::Reply(hello(&req, greeted)),
        method::SHUTDOWN => Control::Exit(Reply::ok(req.id, json!({}))),
        _ if !*greeted => Control::Reply(Reply::err(req.id, code::BAD_PARAMS, "say hello first")),
        method::CANCEL => {
            if let Some(id) = req.params["id"].as_u64()
                && let Some(flag) = lock(cancels).get(&id)
            {
                flag.store(true, Ordering::Release);
            }
            Control::Reply(Reply::ok(req.id, json!({})))
        }
        method::STATUS => {
            let st = lock(state);
            let loaded = st.loaded.iter().map(|(m, l)| LoadedModel { model: m.clone(), kind: l.kind, mem_bytes: l.mem }).collect();
            let rss = rss_bytes().unwrap_or_else(|| st.loaded.values().map(|l| l.mem).sum());
            Control::Reply(Reply::ok(req.id, serde_json::to_value(StatusResult { loaded, rss_bytes: rss }).unwrap_or_default()))
        }
        method::LOAD | method::UNLOAD | method::EMBED | method::GENERATE | method::OCR => {
            let cancel = Arc::new(AtomicBool::new(false));
            lock(cancels).insert(req.id, Arc::clone(&cancel));
            let _ = work.send(Work::Job(Job { req, cancel }));
            Control::Queued
        }
        other => Control::Reply(Reply::err(req.id, code::UNKNOWN_METHOD, format!("{other} isn't a method of this engine"))),
    }
}

fn hello(req: &Request, greeted: &mut bool) -> Reply {
    match serde_json::from_value::<HelloParams>(req.params.clone()) {
        Ok(h) if h.app_version != env!("CARGO_PKG_VERSION") || h.protocol != PROTOCOL_VERSION => Reply::err(
            req.id,
            code::VERSION_MISMATCH,
            format!("engine {} (protocol {PROTOCOL_VERSION}), app {} (protocol {})", env!("CARGO_PKG_VERSION"), h.app_version, h.protocol),
        ),
        Ok(_) => {
            *greeted = true;
            let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            let mut devices = vec!["cpu".to_string()];
            if cfg!(all(feature = "metal", target_os = "macos")) {
                devices.push("metal".into());
            }
            let h = HelloResult {
                engine_version: env!("CARGO_PKG_VERSION").into(),
                protocol: PROTOCOL_VERSION,
                devices,
                threads: u32::try_from(threads).unwrap_or(1),
            };
            Reply::ok(req.id, serde_json::to_value(h).unwrap_or_default())
        }
        Err(e) => Reply::err(req.id, code::BAD_PARAMS, e.to_string()),
    }
}

fn worker<B: Backend>(
    mut backend: B,
    rx: &Receiver<Work>,
    out: &Out,
    state: &Mutex<State>,
    cancels: &Mutex<HashMap<u64, Arc<AtomicBool>>>,
) {
    while let Ok(w) = rx.recv() {
        let job = match w {
            Work::Unload(m) => {
                backend.unload(&m);
                let mut st = lock(state);
                st.loaded.remove(&m);
                if st.loaded.is_empty() {
                    st.empty_since = Some(Instant::now());
                }
                continue;
            }
            Work::Job(j) => j,
        };
        let id = job.req.id;
        if job.cancel.load(Ordering::Acquire) {
            send(out, &Reply::err(id, code::CANCELLED, "cancelled"));
            lock(cancels).remove(&id);
            continue;
        }
        lock(state).busy = true;
        // A panic in model code ends this request, not the engine.
        let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut backend, &job, out, state)))
            .unwrap_or_else(|_| Reply::err(id, code::FAILED, "the model failed on this input"));
        {
            let mut st = lock(state);
            st.busy = false;
            if st.loaded.is_empty() && st.empty_since.is_none() {
                st.empty_since = Some(Instant::now());
            }
        }
        lock(cancels).remove(&id);
        if !send(out, &reply) {
            return;
        }
    }
}

fn touch(state: &Mutex<State>, model: &str) -> bool {
    let mut st = lock(state);
    st.loaded.get_mut(model).map(|l| l.last_used = Instant::now()).is_some()
}

fn not_loaded(id: u64, model: &str) -> Reply {
    Reply::err(id, code::MODEL_NOT_LOADED, format!("{model} isn't loaded"))
}

fn run<B: Backend>(backend: &mut B, job: &Job, out: &Out, state: &Mutex<State>) -> Reply {
    let id = job.req.id;
    let params = job.req.params.clone();
    let bad = |e: serde_json::Error| Reply::err(id, code::BAD_PARAMS, e.to_string());
    match job.req.method.as_str() {
        method::LOAD => {
            let p: LoadParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return bad(e),
            };
            match backend.load(&p) {
                Ok(mem) => {
                    let idle_after = if p.idle_unload_s == 0 { DEFAULT_IDLE_UNLOAD } else { Duration::from_secs(p.idle_unload_s) };
                    let mut st = lock(state);
                    st.loaded.insert(p.model.clone(), Loaded { kind: p.kind, mem, idle_after, last_used: Instant::now() });
                    st.empty_since = None;
                    Reply::ok(id, serde_json::to_value(LoadResult { loaded: true, mem_bytes: mem }).unwrap_or_default())
                }
                Err((c, m)) => Reply::err(id, c, m),
            }
        }
        method::UNLOAD => {
            let model = params["model"].as_str().unwrap_or_default().to_string();
            backend.unload(&model);
            let mut st = lock(state);
            st.loaded.remove(&model);
            if st.loaded.is_empty() {
                st.empty_since = Some(Instant::now());
            }
            Reply::ok(id, json!({}))
        }
        method::EMBED => {
            let p: EmbedParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return bad(e),
            };
            if p.texts.len() > 64 {
                return Reply::err(id, code::BAD_PARAMS, "at most 64 texts per request");
            }
            if !touch(state, &p.model) {
                return not_loaded(id, &p.model);
            }
            match backend.embed(&p) {
                Ok((dims, flat)) => {
                    let bytes: Vec<u8> = flat.iter().flat_map(|x| half::f16::from_f32(*x).to_le_bytes()).collect();
                    let r = EmbedResult { dims, vectors_f16_b64: base64::engine::general_purpose::STANDARD.encode(bytes) };
                    Reply::ok(id, serde_json::to_value(r).unwrap_or_default())
                }
                Err((c, m)) => Reply::err(id, c, m),
            }
        }
        method::GENERATE => {
            let p: GenerateParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return bad(e),
            };
            if !touch(state, &p.model) {
                return not_loaded(id, &p.model);
            }
            let r = backend.generate(&p, &job.cancel, &mut |ev| {
                send(out, &Reply::event(id, ev));
            });
            touch(state, &p.model);
            match r {
                Ok(usage) => Reply::ok(id, json!({ "done": true, "usage": usage })),
                Err((c, m)) => Reply::err(id, c, m),
            }
        }
        method::OCR => {
            let p: OcrParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return bad(e),
            };
            if !touch(state, &p.model_set) {
                return not_loaded(id, &p.model_set);
            }
            match backend.ocr(&p, &job.cancel) {
                Ok(lines) => Reply::ok(id, serde_json::to_value(OcrResult { lines }).unwrap_or_default()),
                Err((c, m)) => Reply::err(id, c, m),
            }
        }
        other => Reply::err(id, code::UNKNOWN_METHOD, format!("{other} isn't a method of this engine")),
    }
}

/// Resident memory of this process, where the OS tells us cheaply (Linux).
fn rss_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Arguments every engine binary understands: `--version`.
pub fn handle_args(name: &str) -> bool {
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("{name} {}", env!("CARGO_PKG_VERSION"));
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_checks_versions_and_gates_everything_else() {
        let state = Mutex::new(State::default());
        let cancels = Mutex::new(HashMap::new());
        let (tx, rx) = channel();
        let mut greeted = false;
        let call = |line: &str, greeted: &mut bool| match control(line, greeted, &state, &cancels, &tx) {
            Control::Reply(r) | Control::Exit(r) => Some(r),
            Control::Queued => None,
        };
        let r = call(r#"{"id":1,"method":"status","params":{}}"#, &mut greeted).unwrap();
        assert_eq!(r.error.unwrap().code, code::BAD_PARAMS, "hello comes first");
        let r = call(r#"{"id":2,"method":"hello","params":{"app_version":"0.0.1","protocol":1}}"#, &mut greeted).unwrap();
        assert_eq!(r.error.unwrap().code, code::VERSION_MISMATCH);
        assert!(!greeted);
        let hello = format!(r#"{{"id":3,"method":"hello","params":{{"app_version":"{}","protocol":1}}}}"#, env!("CARGO_PKG_VERSION"));
        let h: HelloResult = serde_json::from_value(call(&hello, &mut greeted).unwrap().result.unwrap()).unwrap();
        assert_eq!(h.protocol, PROTOCOL_VERSION);
        assert!(h.devices.contains(&"cpu".to_string()));
        assert!(call(r#"{"id":4,"method":"embed","params":{}}"#, &mut greeted).is_none(), "model work is queued");
        assert!(matches!(rx.try_recv(), Ok(Work::Job(_))));
        assert!(call(r#"{"id":5,"method":"cancel","params":{"id":4}}"#, &mut greeted).unwrap().error.is_none());
        assert!(cancels.lock().unwrap()[&4].load(Ordering::Acquire), "cancel reaches the queued request");
        assert_eq!(call(r#"{"id":6,"method":"nope","params":{}}"#, &mut greeted).unwrap().error.unwrap().code, code::UNKNOWN_METHOD);
        assert!(call("garbage", &mut greeted).unwrap().error.is_some());
    }
}
