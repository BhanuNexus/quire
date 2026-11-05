// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The engine process (design §D.3).
//!
//! It is found on disk, started on the first
//! request at a lower priority, restarted at most twice after a crash, and
//! stopped when the app no longer needs it. The engine unloads idle models
//! itself and exits after ten minutes with nothing loaded; the next request
//! starts it again.
//!
//! Requests go out as JSON lines; a reader thread hands each reply (and each
//! streamed event) to the caller waiting for that id, so several callers can
//! share one engine. A caller that sets its cancel flag gets
//! [`EngineError::Cancelled`] at once while the engine is told to stop; an
//! engine that goes quiet in the middle of a request is killed, so a stuck
//! model never holds up the app.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use quire_intel::chat::CancelFlag;
use serde_json::{Value, json};

use crate::protocol::{HelloParams, HelloResult, LoadParams, LoadedModel, PROTOCOL_VERSION, Reply, Request, StatusResult, code, method};

/// Name of the engine executable.
pub const ENGINE_BIN: &str = if cfg!(windows) { "quire-ai-engine.exe" } else { "quire-ai-engine" };

/// Restarts after a crash before the engine is given up on (until the
/// person restarts it in Settings).
pub const MAX_RESTARTS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// No engine binary was found (not downloaded yet).
    NotInstalled,
    /// It didn't start, answered nonsense, or not in time.
    Failed(String),
    /// It answered with an error (`code` from `protocol::code`).
    Refused { code: i32, message: String },
    /// The caller stopped the request.
    Cancelled,
    /// It stopped working too often; Settings → AI can restart it.
    GaveUp(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("the on-device AI engine isn't installed"),
            Self::Failed(why) => write!(f, "the on-device AI engine didn't work: {why}"),
            Self::Refused { message, .. } => write!(f, "the on-device AI engine said: {message}"),
            Self::Cancelled => f.write_str("stopped"),
            Self::GaveUp(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for EngineError {}

/// Where the engine binary was found ([`Engine::find_with_source`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSource {
    /// `$QUIRE_AI_ENGINE`: someone's own build (development). Not checked.
    Override,
    /// Next to the app's executable: packaged with it (the full edition).
    Bundled,
    /// Downloaded into the models folder.
    Downloaded,
}

impl EngineSource {
    /// The SHA-256 an engine from here must have before it is started.
    ///
    /// `bundled` is the hash of the engine this build was packaged with (the
    /// file next to the app); `binary` is the pinned hash of the engine this
    /// build downloads. A bundled engine is checked against `bundled`, or
    /// against `binary` when the build wasn't packaged with one (an engine
    /// put next to Quire Lite by hand). Development builds have neither pin,
    /// so nothing is checked there.
    pub fn required_sha256(self, bundled: Option<&str>, binary: Option<&str>) -> Option<String> {
        let pin = match self {
            Self::Override => None,
            Self::Bundled => bundled.filter(|s| !s.is_empty()).or(binary),
            Self::Downloaded => binary,
        };
        pin.filter(|s| !s.is_empty()).map(str::to_ascii_lowercase)
    }
}

/// How to run the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineConfig {
    pub path: Option<PathBuf>,
    /// Where `path` was found (it decides what a failed check tells the person).
    pub source: Option<EngineSource>,
    /// SHA-256 the binary must have (a downloaded or bundled engine in a
    /// release build); checked every time the engine starts.
    pub sha256: Option<String>,
    /// Inference threads (0: the engine decides). Takes effect when the
    /// engine next starts.
    pub threads: u32,
    /// Replaces how long a request may stay quiet before the engine is
    /// considered stuck and restarted (tests use a short one).
    pub quiet_limit: Option<Duration>,
    /// Extra environment for the engine process (tests and the fake engine).
    pub env: Vec<(String, String)>,
}

/// What Settings shows about the engine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EngineSnapshot {
    pub installed: bool,
    pub running: bool,
    pub loaded: Vec<LoadedModel>,
    /// Resident memory from the last `status` call.
    pub rss_bytes: u64,
    pub crashes: u32,
    pub gave_up: Option<String>,
    /// The last lines the engine wrote to stderr (why it failed).
    pub last_error: Option<String>,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    pending: Arc<Mutex<HashMap<u64, Sender<Reply>>>>,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// model id → sha256 of what is loaded.
    loaded: HashMap<String, String>,
    hello: Option<HelloResult>,
}

struct Shared {
    cfg: Mutex<EngineConfig>,
    proc: Mutex<Option<Proc>>,
    next_id: AtomicU64,
    crashes: Mutex<u32>,
    gave_up: Mutex<Option<String>>,
    snapshot: Mutex<EngineSnapshot>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A handle to the engine (not started until used). Clones share one process.
#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("path", &self.path()).finish_non_exhaustive()
    }
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                cfg: Mutex::new(cfg),
                proc: Mutex::new(None),
                next_id: AtomicU64::new(1),
                crashes: Mutex::new(0),
                gave_up: Mutex::new(None),
                snapshot: Mutex::new(EngineSnapshot::default()),
            }),
        }
    }

    /// The engine at `$QUIRE_AI_ENGINE`, or next to this executable, or in
    /// `models_dir` (where downloads go).
    pub fn find(models_dir: Option<&Path>) -> Option<PathBuf> {
        Self::find_with_source(models_dir).map(|(p, _)| p)
    }

    /// [`Engine::find`], and where the engine was found.
    pub fn find_with_source(models_dir: Option<&Path>) -> Option<(PathBuf, EngineSource)> {
        let from_env = std::env::var_os("QUIRE_AI_ENGINE").map(PathBuf::from);
        let exe = std::env::current_exe().ok();
        find_in(from_env, exe.as_deref().and_then(Path::parent), models_dir)
    }

    /// An engine found like [`Engine::find`] (not checked: tests and tools).
    pub fn locate(models_dir: Option<&Path>) -> Self {
        let found = Self::find_with_source(models_dir);
        Self::new(EngineConfig { source: found.as_ref().map(|f| f.1), path: found.map(|f| f.0), ..EngineConfig::default() })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::new(EngineConfig { path: Some(path.into()), ..EngineConfig::default() })
    }

    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.shared.cfg).path.clone()
    }

    pub fn config(&self) -> EngineConfig {
        lock(&self.shared.cfg).clone()
    }

    pub fn is_installed(&self) -> bool {
        lock(&self.shared.cfg).path.as_ref().is_some_and(|p| p.is_file())
    }

    /// Use another binary or settings from now on (the running process is
    /// stopped if the binary changed).
    pub fn configure(&self, cfg: EngineConfig) {
        let changed = {
            let old = lock(&self.shared.cfg);
            old.path != cfg.path || old.threads != cfg.threads || old.env != cfg.env
        };
        *lock(&self.shared.cfg) = cfg;
        if changed {
            self.stop();
            self.restart_allowed();
        }
    }

    /// Clear "gave up" after crashes (Settings → AI → Restart).
    pub fn restart_allowed(&self) {
        *lock(&self.shared.crashes) = 0;
        *lock(&self.shared.gave_up) = None;
        lock(&self.shared.snapshot).gave_up = None;
    }

    /// Whether the process is running now.
    pub fn is_running(&self) -> bool {
        lock(&self.shared.proc).as_ref().is_some_and(|p| p.alive.load(Ordering::Acquire))
    }

    /// What Settings shows (no request to the engine).
    pub fn snapshot(&self) -> EngineSnapshot {
        let mut s = lock(&self.shared.snapshot).clone();
        s.installed = self.is_installed();
        s.running = self.is_running();
        s.crashes = *lock(&self.shared.crashes);
        s.gave_up.clone_from(&lock(&self.shared.gave_up));
        if !s.running {
            s.loaded.clear();
            s.rss_bytes = 0;
        }
        if let Some(p) = lock(&self.shared.proc).as_ref() {
            let tail: Vec<String> = lock(&p.stderr).iter().cloned().collect();
            if !tail.is_empty() {
                s.last_error = Some(tail.join("\n"));
            }
        }
        s
    }

    /// The protocol the running engine said it speaks in `hello`.
    pub fn protocol(&self) -> Option<u32> {
        lock(&self.shared.proc).as_ref().and_then(|p| p.hello.as_ref()).map(|h| h.protocol)
    }

    /// Ask a running engine what it has loaded and how much memory it uses.
    /// Doesn't start one.
    pub fn refresh_status(&self) {
        if !self.is_running() {
            return;
        }
        if let Ok(v) = self.call(method::STATUS, json!({}), None, &mut |_| {}, Duration::from_secs(5))
            && let Ok(st) = serde_json::from_value::<StatusResult>(v)
        {
            let mut s = lock(&self.shared.snapshot);
            s.loaded = st.loaded;
            s.rss_bytes = st.rss_bytes;
        }
    }

    /// Start the engine if needed and return its `hello`.
    pub fn hello(&self, timeout: Duration) -> Result<HelloResult, EngineError> {
        let mut guard = lock(&self.shared.proc);
        self.ensure_running(&mut guard, timeout)
    }

    /// Stop the engine (it starts again on the next request).
    pub fn stop(&self) {
        let p = lock(&self.shared.proc).take();
        if let Some(mut p) = p {
            let req = Request { id: 0, method: method::SHUTDOWN.into(), params: json!({}) };
            let _ = writeln!(p.stdin, "{}", serde_json::to_string(&req).unwrap_or_default());
            let _ = p.stdin.flush();
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if matches!(p.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        let mut s = lock(&self.shared.snapshot);
        s.loaded.clear();
        s.rss_bytes = 0;
    }

    /// Make sure `spec.model` is loaded, then run `f`; if the engine had
    /// unloaded it in the meantime (idle), load it again and retry once. A
    /// request that crashed the engine isn't retried (it would likely crash
    /// it again); the next request starts a fresh engine.
    pub fn with_model<T>(&self, spec: &LoadParams, mut f: impl FnMut(&Self) -> Result<T, EngineError>) -> Result<T, EngineError> {
        for attempt in 0..2 {
            self.ensure_loaded(spec)?;
            match f(self) {
                Err(EngineError::Refused { code: code::MODEL_NOT_LOADED, .. }) if attempt == 0 => {
                    if let Some(p) = lock(&self.shared.proc).as_mut() {
                        p.loaded.remove(&spec.model);
                    }
                }
                other => return other,
            }
        }
        Err(EngineError::Failed("the model kept unloading".into()))
    }

    fn ensure_loaded(&self, spec: &LoadParams) -> Result<(), EngineError> {
        {
            let mut guard = lock(&self.shared.proc);
            self.ensure_running(&mut guard, Duration::from_secs(20))?;
            if guard.as_ref().is_some_and(|p| p.loaded.get(&spec.model) == Some(&spec.sha256)) {
                return Ok(());
            }
        }
        let params = serde_json::to_value(spec).unwrap_or_default();
        let r = self.call(method::LOAD, params, None, &mut |_| {}, Duration::from_mins(10))?;
        let mem = r["mem_bytes"].as_u64().unwrap_or(0);
        if let Some(p) = lock(&self.shared.proc).as_mut() {
            p.loaded.insert(spec.model.clone(), spec.sha256.clone());
        }
        let mut s = lock(&self.shared.snapshot);
        s.loaded.retain(|m| m.model != spec.model);
        s.loaded.push(LoadedModel { model: spec.model.clone(), kind: spec.kind, mem_bytes: mem });
        Ok(())
    }

    /// Unload one model (Delete in Settings, or switching models).
    pub fn unload(&self, model: &str) {
        if !self.is_running() {
            return;
        }
        let _ = self.call(method::UNLOAD, json!({ "model": model }), None, &mut |_| {}, Duration::from_secs(10));
        if let Some(p) = lock(&self.shared.proc).as_mut() {
            p.loaded.remove(model);
        }
        lock(&self.shared.snapshot).loaded.retain(|m| m.model != model);
    }

    /// Send one request and wait for its reply, passing streamed events to
    /// `on_event`. Starts the engine if needed. `quiet` is the longest wait
    /// for any line before the engine is considered stuck.
    pub fn call(
        &self,
        method_name: &str,
        params: Value,
        cancel: Option<&CancelFlag>,
        on_event: &mut dyn FnMut(&Value),
        quiet: Duration,
    ) -> Result<Value, EngineError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        {
            let mut guard = lock(&self.shared.proc);
            self.ensure_running(&mut guard, Duration::from_secs(20))?;
            let p = guard.as_mut().ok_or(EngineError::NotInstalled)?;
            lock(&p.pending).insert(id, tx);
            let line = serde_json::to_string(&Request { id, method: method_name.into(), params }).unwrap_or_default();
            if writeln!(p.stdin, "{line}").and_then(|()| p.stdin.flush()).is_err() {
                lock(&p.pending).remove(&id);
                p.alive.store(false, Ordering::Release);
                return Err(EngineError::Failed("the engine stopped".into()));
            }
        }
        let quiet = lock(&self.shared.cfg).quiet_limit.unwrap_or(quiet);
        self.wait(id, rx, cancel, on_event, quiet)
    }

    fn wait(
        &self,
        id: u64,
        rx: Receiver<Reply>,
        cancel: Option<&CancelFlag>,
        on_event: &mut dyn FnMut(&Value),
        quiet: Duration,
    ) -> Result<Value, EngineError> {
        let mut last = Instant::now();
        loop {
            if cancel.is_some_and(CancelFlag::is_cancelled) {
                // The caller hears at once; the engine gets a few seconds to
                // end the request before it is restarted.
                self.send_cancel(id);
                let shared = Arc::clone(&self.shared);
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(r) if r.event.is_none() => return,
                            Ok(_) | Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                            _ => break,
                        }
                    }
                    kill_proc(&shared);
                });
                return Err(EngineError::Cancelled);
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(reply) => {
                    last = Instant::now();
                    if let Some(ev) = reply.event {
                        on_event(&ev);
                        continue;
                    }
                    if let Some(e) = reply.error {
                        return Err(if e.code == code::CANCELLED {
                            EngineError::Cancelled
                        } else {
                            EngineError::Refused { code: e.code, message: e.message }
                        });
                    }
                    return Ok(reply.result.unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if last.elapsed() > quiet {
                        self.forget(id);
                        kill_proc(&self.shared);
                        self.count_crash("it stopped answering");
                        return Err(EngineError::Failed("it stopped answering and was restarted".into()));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(EngineError::Failed(self.crash_reason()));
                }
            }
        }
    }

    fn send_cancel(&self, id: u64) {
        let cid = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(p) = lock(&self.shared.proc).as_mut() {
            let line =
                serde_json::to_string(&Request { id: cid, method: method::CANCEL.into(), params: json!({ "id": id }) }).unwrap_or_default();
            let _ = writeln!(p.stdin, "{line}").and_then(|()| p.stdin.flush());
        }
    }

    fn forget(&self, id: u64) {
        if let Some(p) = lock(&self.shared.proc).as_ref() {
            lock(&p.pending).remove(&id);
        }
    }

    fn crash_reason(&self) -> String {
        let tail =
            lock(&self.shared.proc).as_ref().map(|p| lock(&p.stderr).iter().cloned().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        if tail.is_empty() { "it stopped unexpectedly".into() } else { format!("it stopped unexpectedly ({tail})") }
    }

    fn count_crash(&self, why: &str) {
        let mut c = lock(&self.shared.crashes);
        *c += 1;
        if *c > MAX_RESTARTS {
            let msg = format!(
                "The on-device AI engine stopped working {} times ({why}). It was turned off until you restart it in Settings → AI.",
                *c
            );
            *lock(&self.shared.gave_up) = Some(msg);
        }
    }

    /// Start the engine if it isn't running (counting crashes), verify its
    /// binary, and say hello.
    fn ensure_running(&self, guard: &mut MutexGuard<'_, Option<Proc>>, timeout: Duration) -> Result<HelloResult, EngineError> {
        if let Some(p) = guard.as_mut() {
            if p.alive.load(Ordering::Acquire)
                && let Some(h) = &p.hello
            {
                return Ok(h.clone());
            }
            // It ended: a clean exit (idle) isn't a crash.
            let clean = matches!(p.child.try_wait(), Ok(Some(s)) if s.success()) || matches!(p.child.wait(), Ok(s) if s.success());
            let tail = lock(&p.stderr).iter().cloned().collect::<Vec<_>>().join(" ");
            **guard = None;
            if !clean {
                self.count_crash(if tail.is_empty() { "it stopped unexpectedly" } else { &tail });
            }
        }
        let gave_up = lock(&self.shared.gave_up).clone();
        if let Some(why) = gave_up {
            return Err(EngineError::GaveUp(why));
        }
        let cfg = lock(&self.shared.cfg).clone();
        let path = cfg.path.clone().filter(|p| p.is_file()).ok_or(EngineError::NotInstalled)?;
        Self::verify(&path, cfg.sha256.as_deref(), cfg.source)?;
        let mut p = spawn(&path, cfg.threads, &cfg.env)?;
        let hello = Request {
            id: 0,
            method: method::HELLO.into(),
            params: serde_json::to_value(HelloParams { app_version: env!("CARGO_PKG_VERSION").into(), protocol: PROTOCOL_VERSION })
                .unwrap_or_default(),
        };
        let (tx, rx) = channel();
        lock(&p.pending).insert(0, tx);
        writeln!(p.stdin, "{}", serde_json::to_string(&hello).unwrap_or_default())
            .and_then(|()| p.stdin.flush())
            .map_err(|e| EngineError::Failed(e.to_string()))?;
        let reply = rx.recv_timeout(timeout);
        lock(&p.pending).remove(&0);
        let Ok(reply) = reply else {
            let tail = lock(&p.stderr).iter().cloned().collect::<Vec<_>>().join(" ");
            let _ = p.child.kill();
            let _ = p.child.wait();
            self.count_crash("it didn't start");
            return Err(EngineError::Failed(if tail.is_empty() { "no answer in time".into() } else { tail }));
        };
        if let Some(e) = reply.error {
            let _ = p.child.kill();
            let _ = p.child.wait();
            if e.code == code::VERSION_MISMATCH {
                return Err(EngineError::GaveUp(format!(
                    "The on-device AI engine doesn't match this version of Quire ({}). Download it again in Settings → AI.",
                    e.message
                )));
            }
            return Err(EngineError::Refused { code: e.code, message: e.message });
        }
        let h: HelloResult =
            serde_json::from_value(reply.result.unwrap_or_default()).map_err(|e| EngineError::Failed(format!("bad hello: {e}")))?;
        p.hello = Some(h.clone());
        **guard = Some(p);
        Ok(h)
    }

    /// A downloaded engine must still be the file that was verified, and a
    /// bundled one the file this build was packaged with. It is hashed again
    /// before every start (about 18 MB, once per process start): a file
    /// swapped with its modification time restored is caught.
    fn verify(path: &Path, sha: Option<&str>, source: Option<EngineSource>) -> Result<(), EngineError> {
        let Some(sha) = sha.filter(|s| !s.is_empty()) else { return Ok(()) };
        let got = crate::download::sha256_file(path).map_err(|e| EngineError::Failed(e.to_string()))?;
        if !got.eq_ignore_ascii_case(sha) {
            return Err(EngineError::GaveUp(if source == Some(EngineSource::Bundled) {
                format!(
                    "The on-device AI engine next to Quire ({}) isn't the one this version of Quire came with, so Quire won't run it. Reinstall Quire.",
                    path.display()
                )
            } else {
                "The on-device AI engine file changed after it was verified, so Quire won't run it. Download it again in Settings → AI."
                    .into()
            }));
        }
        Ok(())
    }
}

/// The first engine that exists: `from_env`, then next to the app in
/// `exe_dir`, then downloaded into `models_dir`.
fn find_in(from_env: Option<PathBuf>, exe_dir: Option<&Path>, models_dir: Option<&Path>) -> Option<(PathBuf, EngineSource)> {
    [
        from_env.map(|p| (p, EngineSource::Override)),
        exe_dir.map(|d| (d.join(ENGINE_BIN), EngineSource::Bundled)),
        models_dir.map(|d| (d.join(crate::catalog::ENGINE_ID).join(ENGINE_BIN), EngineSource::Downloaded)),
    ]
    .into_iter()
    .flatten()
    .find(|(p, _)| p.is_file())
}

impl Drop for Shared {
    fn drop(&mut self) {
        let proc = lock(&self.proc).take();
        if let Some(mut p) = proc {
            let _ = writeln!(p.stdin, r#"{{"id":0,"method":"shutdown","params":{{}}}}"#);
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}

/// Kill the engine process now (it starts again on the next request).
fn kill_proc(shared: &Shared) {
    let proc = lock(&shared.proc).take();
    if let Some(mut p) = proc {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
}

/// `nice -n 10 <engine>` on Unix when `nice` exists (setting the priority
/// in the child would need unsafe code); below-normal priority on Windows.
fn command(path: &Path) -> Command {
    #[cfg(unix)]
    {
        if let Some(nice) = ["/usr/bin/nice", "/bin/nice"].iter().map(Path::new).find(|p| p.is_file()) {
            let mut c = Command::new(nice);
            c.arg("-n").arg("10").arg(path);
            return c;
        }
        Command::new(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = Command::new(path);
        c.creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
        c
    }
    #[cfg(not(any(unix, windows)))]
    {
        Command::new(path)
    }
}

fn spawn(path: &Path, threads: u32, env: &[(String, String)]) -> Result<Proc, EngineError> {
    let mut cmd = command(path);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    if threads > 0 {
        // candle's thread pool reads these.
        cmd.env("RAYON_NUM_THREADS", threads.to_string()).env("CANDLE_NUM_THREADS", threads.to_string());
    }
    let mut child =
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| EngineError::Failed(e.to_string()))?;
    let stdin = child.stdin.take().ok_or_else(|| EngineError::Failed("no stdin".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| EngineError::Failed("no stdout".into()))?;
    let stderr = child.stderr.take();
    let pending: Arc<Mutex<HashMap<u64, Sender<Reply>>>> = Arc::default();
    let alive = Arc::new(AtomicBool::new(true));
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    {
        let pending = Arc::clone(&pending);
        let alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("quire-ai-engine-out".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(reply) = serde_json::from_str::<Reply>(&line) else { continue };
                    let (rid, last) = (reply.id, reply.event.is_none());
                    let tx = lock(&pending).get(&rid).cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(reply);
                        if last {
                            lock(&pending).remove(&rid);
                        }
                    }
                }
                alive.store(false, Ordering::Release);
                // Everyone still waiting learns it ended.
                lock(&pending).clear();
            })
            .map_err(|e| EngineError::Failed(e.to_string()))?;
    }
    if let Some(err) = stderr {
        let tail = Arc::clone(&tail);
        let _ = std::thread::Builder::new().name("quire-ai-engine-err".into()).spawn(move || {
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                let mut t = lock(&tail);
                t.push_back(line.chars().take(300).collect());
                while t.len() > 4 {
                    t.pop_front();
                }
            }
        });
    }
    Ok(Proc { child, stdin, pending, alive, stderr: tail, loaded: HashMap::new(), hello: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_engine_is_reported_as_such() {
        let e = Engine::new(EngineConfig::default());
        assert_eq!(e.hello(Duration::from_millis(10)), Err(EngineError::NotInstalled));
        let missing = Engine::at("/nonexistent/quire-ai-engine");
        assert_eq!(missing.hello(Duration::from_millis(100)), Err(EngineError::NotInstalled));
        assert!(!missing.snapshot().installed);
    }

    #[test]
    fn the_binary_is_hashed_at_every_start() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("quire-ai-engine");
        std::fs::write(&bin, b"the engine that was downloaded").unwrap();
        let sha = crate::download::sha256_file(&bin).unwrap();
        Engine::verify(&bin, Some(&sha), Some(EngineSource::Downloaded)).unwrap();
        // Swapped by another process, with its modification time put back.
        let mtime = std::fs::metadata(&bin).unwrap().modified().unwrap();
        std::fs::write(&bin, b"something else entirely, same size?").unwrap();
        std::fs::File::options().write(true).open(&bin).unwrap().set_modified(mtime).unwrap();
        assert!(matches!(Engine::verify(&bin, Some(&sha), Some(EngineSource::Downloaded)), Err(EngineError::GaveUp(_))));
        Engine::verify(&bin, None, None).unwrap();
    }

    #[test]
    fn the_engine_is_found_in_order_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let (env_dir, app, models) = (dir.path().join("env"), dir.path().join("app"), dir.path().join("models"));
        for d in [&env_dir, &app, &models.join(crate::catalog::ENGINE_ID)] {
            std::fs::create_dir_all(d).unwrap();
        }
        let from_env = env_dir.join(ENGINE_BIN);
        let find = || find_in(Some(from_env.clone()), Some(&app), Some(&models)).map(|f| f.1);
        assert_eq!(find(), None);
        std::fs::write(models.join(crate::catalog::ENGINE_ID).join(ENGINE_BIN), b"downloaded").unwrap();
        assert_eq!(find(), Some(EngineSource::Downloaded));
        std::fs::write(app.join(ENGINE_BIN), b"bundled").unwrap();
        assert_eq!(find(), Some(EngineSource::Bundled), "an engine next to the app wins over a downloaded one");
        std::fs::write(&from_env, b"mine").unwrap();
        assert_eq!(find(), Some(EngineSource::Override));
    }

    #[test]
    fn a_bundled_engine_is_checked_against_a_pin_in_release_builds() {
        let (bundled, binary) = ("A".repeat(64), "b".repeat(64));
        let s = EngineSource::Bundled;
        assert_eq!(s.required_sha256(Some(&bundled), Some(&binary)), Some("a".repeat(64)), "the packaged file's own hash");
        assert_eq!(s.required_sha256(None, Some(&binary)), Some(binary.clone()), "else the downloaded engine's");
        assert_eq!(s.required_sha256(Some(""), Some(&binary)), Some(binary.clone()));
        assert_eq!(s.required_sha256(None, None), None, "development builds have no pin");
        assert_eq!(EngineSource::Downloaded.required_sha256(Some(&bundled), Some(&binary)), Some(binary.clone()));
        assert_eq!(EngineSource::Override.required_sha256(Some(&bundled), Some(&binary)), None);
    }

    /// The check happens before the process is started: an engine next to
    /// the app that isn't the packaged one never runs.
    #[cfg(unix)]
    #[test]
    fn a_bundled_engine_that_fails_its_check_is_never_started() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let bin = dir.path().join(ENGINE_BIN);
        std::fs::write(&bin, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let good = crate::download::sha256_file(&bin).unwrap();
        let cfg = |sha: &str| EngineConfig {
            path: Some(bin.clone()),
            source: Some(EngineSource::Bundled),
            sha256: EngineSource::Bundled.required_sha256(Some(sha), None),
            ..EngineConfig::default()
        };
        let e = Engine::new(cfg(&"0".repeat(64)));
        let Err(EngineError::GaveUp(why)) = e.hello(Duration::from_secs(5)) else { panic!("a wrong engine must be refused") };
        assert!(why.contains("Reinstall Quire"), "{why}");
        assert!(!ran.exists(), "it was started");
        // The packaged file passes and is started (this one exits at once).
        let e = Engine::new(cfg(&good));
        assert!(matches!(e.hello(Duration::from_secs(5)), Err(EngineError::Failed(_))));
        assert!(ran.exists(), "it wasn't started");
    }
}
