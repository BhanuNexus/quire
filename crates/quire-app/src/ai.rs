// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wiring settings + secrets to the assistant, the semantic embedder and the
//! on-device models (design §D.1–§D.3).
//!
//! Nothing on this computer runs or downloads until the person turns a
//! capability on *and* its model is installed (they pressed Download, or
//! installed it from a file). The engine process starts on the first
//! request from one of those models.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, unbounded};
use quire_ai::catalog::{BINARY_SHA256, Catalog, CatalogEntry};
use quire_ai::download::{Control, DownloadError, Downloader, Manifest, Progress};
pub use quire_ai::engine::EngineSource;
use quire_ai::engine::{Engine, EngineConfig, EngineSnapshot};
use quire_ai::local::{LocalChat, LocalEmbedder, LocalSetup};
use quire_intel::chat::{ChatLanguageModel, ChatModel, Locality};
use quire_intel::llm::{LanguageModel, LocalModel};

use crate::secrets;
use crate::settings::Settings;
use crate::workspace::{NoticeLevel, Workspace};

pub mod ocr;

pub use quire_ai::catalog::{ENGINE_ID, Purpose, Tier, human_size};

pub const PROVIDERS: &[(&str, &str)] =
    &[("local", "Offline (built-in)"), ("anthropic", "Anthropic (Claude)"), ("openai", "OpenAI-compatible (OpenAI, Ollama, LM Studio…)")];

pub fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "http://localhost:11434",
        _ => "",
    }
}

pub fn default_model(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-sonnet-5-5",
        "openai" => "llama3.2",
        _ => "",
    }
}

fn or_default(v: &str, d: &str) -> String {
    if v.trim().is_empty() { d.to_string() } else { v.trim().to_string() }
}

/// The base URL and model actually used for the configured provider.
pub fn resolved(settings: &Settings) -> (String, String) {
    let p = settings.ai_provider.as_str();
    (or_default(&settings.ai_base_url, default_base_url(p)), or_default(&settings.ai_model, default_model(p)))
}

pub fn embed_base_url(settings: &Settings) -> String {
    if !settings.embed_base_url.trim().is_empty() {
        return settings.embed_base_url.trim().to_string();
    }
    if settings.ai_provider == "openai" { resolved(settings).0 } else { default_base_url("openai").to_string() }
}

// ------------------------------------------------------------------ models

/// What Quire says while on-device AI is off (the experimental switch,
/// design §D.1): onboarding, Settings → AI and refused downloads.
pub const ON_DEVICE_LATER: &str =
    "On-device AI is coming in an update. Use a provider such as Ollama or LM Studio to keep everything on this computer today.";

/// Whether on-device AI is offered (the experimental switch or
/// `QUIRE_EXPERIMENTAL_ON_DEVICE=1`).
pub fn on_device(settings: &Settings) -> bool {
    settings.ai_local.on_device()
}

/// `<data_dir>/models` (device-local, never in the vault). `QUIRE_MODELS_DIR` overrides it.
pub fn models_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("QUIRE_MODELS_DIR") {
        return Some(PathBuf::from(d));
    }
    directories::ProjectDirs::from("dev", "Quire", "Quire").map(|d| d.data_dir().join("models"))
}

/// The catalog built into this version.
pub fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(Catalog::builtin)
}

fn setup(settings: &Settings) -> Option<LocalSetup> {
    Some(LocalSetup {
        models_dir: models_dir()?,
        threads: settings.ai_local.threads,
        gpu: settings.ai_local.gpu.clone(),
        idle_unload_s: settings.ai_local.idle_unload_s,
        fast_prompts: settings.ai_local.fast_prompts,
    })
}

/// The engine settings local models are made with (`None` without a data folder).
pub fn local_setup(settings: &Settings) -> Option<LocalSetup> {
    setup(settings)
}

/// The one engine this process talks to (shared by search, chat and OCR).
pub fn engine(settings: &Settings) -> Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    let e = E.get_or_init(|| Engine::new(EngineConfig::default()));
    let found = Engine::find_with_source(models_dir().as_deref());
    // A downloaded engine must still be the file that was verified, and one
    // next to the app the file this build was packaged with (design §D.3).
    let binary = catalog().get(ENGINE_ID).and_then(|en| en.other.get(BINARY_SHA256)).and_then(toml::Value::as_str);
    let sha256 = found.as_ref().and_then(|(_, src)| src.required_sha256(quire_ai::catalog::bundled_engine_sha256(), binary));
    let (path, source) = found.map_or((None, None), |(p, s)| (Some(p), Some(s)));
    let cfg = EngineConfig { path, source, sha256, threads: settings.ai_local.threads, ..EngineConfig::default() };
    if e.config() != cfg {
        e.configure(cfg);
    }
    e.clone()
}

/// The engine is installed (downloaded, or found next to the app or at
/// `QUIRE_AI_ENGINE` in development).
pub fn engine_installed() -> bool {
    Engine::find(models_dir().as_deref()).is_some()
}

/// Where the engine in use comes from: packaged with Quire (the full
/// edition), downloaded, or `QUIRE_AI_ENGINE`. `None` while there is none.
pub fn engine_source() -> Option<EngineSource> {
    Engine::find_with_source(models_dir().as_deref()).map(|(_, s)| s)
}

/// Whether a model's files are on this computer and verified.
pub fn installed(entry: &CatalogEntry) -> bool {
    if entry.purpose == Purpose::Engine {
        return engine_installed();
    }
    models_dir().is_some_and(|d| Manifest::load(&d).is_installed(&d, entry))
}

/// The on-device language model, when it is on and installed.
pub fn device_chat(settings: &Settings) -> Option<(Arc<LocalChat>, Tier)> {
    let a = &settings.ai_local;
    if !a.llm_on() {
        return None;
    }
    let entry = catalog().get(&a.llm_model)?;
    if !installed(entry) || !engine_installed() {
        return None;
    }
    let chat = LocalChat::new(engine(settings), entry, &setup(settings)?);
    let tier = chat.tier();
    Some((Arc::new(chat), tier))
}

/// The configured cloud or server provider as a chat model (with tools and
/// streaming), if there is one.
pub fn provider_chat(settings: &Settings) -> Option<Arc<dyn ChatModel>> {
    #[cfg(feature = "remote-llm")]
    {
        let (base, model) = resolved(settings);
        match settings.ai_provider.as_str() {
            "anthropic" => {
                let key = secrets::api_key(secrets::store().as_ref(), "anthropic")?;
                return Some(Arc::new(quire_intel::llm::AnthropicModel::new(key, model).with_base_url(base)));
            }
            "openai" => {
                let key = secrets::api_key(secrets::store().as_ref(), "openai");
                return Some(Arc::new(quire_intel::llm::OpenAiCompatible::new(base, model, key)));
            }
            _ => {}
        }
    }
    let _ = settings;
    None
}

/// The chat model features use: the provider, else the on-device model.
/// `None` means no AI (features fall back to what works offline). The CLI
/// and the MCP server's `ask` use this too.
pub fn chat_model(settings: &Settings) -> Option<Arc<dyn ChatModel>> {
    provider_chat(settings).or_else(|| device_chat(settings).map(|(c, _)| c as Arc<dyn ChatModel>))
}

/// The model the agent panel uses, its provider key (`anthropic`,
/// `openai`, `device`) and, on this computer, its tier.
pub fn panel_chat(settings: &Settings) -> Option<(Arc<dyn ChatModel>, String, Option<Tier>)> {
    let provider = || provider_chat(settings).map(|m| (m, settings.ai_provider.clone(), None));
    let device = || device_chat(settings).map(|(m, t)| (m as Arc<dyn ChatModel>, "device".to_string(), Some(t)));
    match settings.agent_panel.model.as_str() {
        "device" => device().or_else(provider),
        _ => provider().or_else(device),
    }
}

/// "this computer", "cloud", "your server".
pub const fn locality_words(l: Locality) -> &'static str {
    match l {
        Locality::Cloud => "cloud",
        Locality::Device => "this computer",
        Locality::LocalServer => "your server",
    }
}

pub(crate) fn make_assistant(settings: &Settings) -> Arc<dyn LanguageModel> {
    #[cfg(feature = "remote-llm")]
    {
        // The secret store (OS keychain) is only touched when a provider is configured.
        let (base, model) = resolved(settings);
        match settings.ai_provider.as_str() {
            "anthropic" => {
                if let Some(key) = secrets::api_key(secrets::store().as_ref(), "anthropic") {
                    return Arc::new(quire_intel::llm::AnthropicModel::new(key, model).with_base_url(base));
                }
            }
            "openai" => {
                let key = secrets::api_key(secrets::store().as_ref(), "openai");
                return Arc::new(quire_intel::llm::OpenAiCompatible::new(base, model, key));
            }
            _ => {}
        }
    }
    if let Some((chat, _)) = device_chat(settings) {
        return Arc::new(ChatLanguageModel::new(chat));
    }
    Arc::new(LocalModel)
}

/// Where API keys are stored on this machine.
pub fn secret_store_kind() -> String {
    secrets::store().kind()
}

pub fn has_api_key(provider: &str) -> bool {
    secrets::api_key(secrets::store().as_ref(), provider).is_some()
}

// ------------------------------------------------------------------ on-device state

/// Where a model stands, for Settings → AI.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelState {
    /// This build has no verified hash for it.
    Unavailable,
    NotInstalled,
    Queued,
    Downloading {
        done: u64,
        total: u64,
        bytes_per_s: f64,
    },
    Verifying,
    Paused {
        done: u64,
        total: u64,
    },
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Dl {
    Queued,
    Running { done: u64, total: u64, rate: f64 },
    Verifying,
    Paused,
    Failed(String),
}

enum DlMsg {
    Progress(String, Progress),
    Finished(String, Result<(), DownloadError>),
}

/// On-device AI state (WS-D): downloads in progress and the engine's
/// status. Kept in `ws.features.ai`; the settings are `ws.settings.ai_local`.
#[derive(Default)]
pub struct LocalAiState {
    downloads: BTreeMap<String, Dl>,
    queue: VecDeque<String>,
    active: Option<(String, Control)>,
    rx: Option<Receiver<DlMsg>>,
    /// Installed models, re-read after downloads and deletions.
    installed: Option<BTreeMap<String, bool>>,
    snapshot: Option<(Instant, EngineSnapshot)>,
    status_rx: Option<Receiver<EngineSnapshot>>,
    pub(crate) ocr: ocr::OcrState,
    /// Voice notes (0.10).
    pub voice: crate::voice::VoiceState,
}

impl std::fmt::Debug for LocalAiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAiState").field("downloads", &self.downloads).finish_non_exhaustive()
    }
}

impl LocalAiState {
    fn installed_map(&mut self) -> &BTreeMap<String, bool> {
        self.installed.get_or_insert_with(|| catalog().models.iter().map(|m| (m.id.clone(), installed(m))).collect())
    }

    /// Something is downloading or queued (the frontend repaints for progress).
    pub fn busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }
}

impl Workspace {
    /// Re-read AI settings and keys: swap the assistant, the semantic
    /// embedder and the chat model background jobs use.
    pub fn apply_ai_settings(&mut self) {
        // 0.11: the vault's "AI off" wins over the settings.
        if crate::secure_state::ai_off(self) {
            self.features.secure.reapply_ai_switch();
            self.apply_ai_switch();
            return;
        }
        self.assistant = make_assistant(&self.settings);
        self.chat = chat_model(&self.settings);
        self.chat_generation += 1;
        self.apply_embedder();
        self.refresh_job_chat();
    }

    /// Use `chat` as the chat model features use (embedders and tests; the
    /// app builds it from the AI settings).
    pub fn set_chat_model(&mut self, chat: Option<Arc<dyn ChatModel>>) {
        self.chat = chat;
        self.chat_generation += 1;
        self.refresh_job_chat();
    }

    /// Give background jobs the current chat model.
    pub(crate) fn refresh_job_chat(&self) {
        if let (Some(vault), Some(agent)) = (&self.vault, &self.agent) {
            self.jobs.set_context(Some(crate::jobs::JobCx {
                vault: vault.without_keys(),
                index: self.index.handle(),
                agent: agent.clone(),
                chat: self.chat.clone(),
                user_active: self.jobs.user_active_flag(),
                cancel: quire_intel::chat::CancelFlag::new(),
            }));
        }
    }

    pub(crate) fn apply_embedder(&self) {
        let cache_dir = self.vault_root().map(|r| r.join(".quire").join("cache"));
        if self.settings.embeddings == "local"
            && self.settings.ai_local.embeddings_on()
            && let Some(entry) = catalog().get(&self.settings.ai_local.embed_model)
            && installed(entry)
            && engine_installed()
            && let Some(setup) = setup(&self.settings)
        {
            use quire_index::semantic::{SemanticIndex, VectorCache};
            let engine = engine(&self.settings);
            let entry = entry.clone();
            let model = entry.id.clone();
            let builder: quire_index::SemanticBuilder = Arc::new(move |chunks| {
                // Vectors are cached as f16 by the embedder itself.
                let emb = LocalEmbedder::new(engine.clone(), &entry, &setup, cache_dir.as_deref());
                SemanticIndex::try_with_cache(chunks, Box::new(emb), &mut VectorCache::in_memory(&model))
            });
            self.index.set_semantic_builder(Some(builder));
            return;
        }
        #[cfg(feature = "remote-llm")]
        if self.settings.embeddings == "endpoint" {
            use quire_index::semantic::{SemanticIndex, VectorCache};
            let base = embed_base_url(&self.settings);
            let model = self.settings.embed_model.clone();
            let key = if self.settings.ai_provider == "openai" { secrets::api_key(secrets::store().as_ref(), "openai") } else { None };
            let cache_path = cache_dir.map(|c| c.join("embeddings.bin"));
            let builder: quire_index::SemanticBuilder = Arc::new(move |chunks| {
                let mut cache = match &cache_path {
                    Some(p) => VectorCache::open(p, &model),
                    None => VectorCache::in_memory(&model),
                };
                let emb = quire_intel::embed::EndpointEmbedder::new(&base, &model, key.clone());
                SemanticIndex::try_with_cache(chunks, Box::new(emb), &mut cache)
            });
            self.index.set_semantic_builder(Some(builder));
            return;
        }
        self.index.set_semantic_builder(None);
    }

    pub fn save_api_key(&mut self, provider: &str, key: &str) -> Result<(), String> {
        secrets::set_api_key(secrets::store().as_ref(), provider, key.trim())?;
        self.apply_ai_settings();
        Ok(())
    }

    /// Check the configured provider in the background.
    pub fn test_ai_connection(&self) -> Receiver<Result<String, String>> {
        let (tx, rx) = bounded(1);
        let provider = self.settings.ai_provider.clone();
        let (base, _) = resolved(&self.settings);
        let key = secrets::api_key(secrets::store().as_ref(), &provider);
        std::thread::spawn(move || {
            #[cfg(feature = "remote-llm")]
            let r = {
                use quire_intel::llm::{ProviderKind, probe};
                match provider.as_str() {
                    "anthropic" if key.is_none() => Err("Add an API key first".to_string()),
                    "anthropic" => probe(ProviderKind::Anthropic, &base, key.as_deref()).map_err(|e| e.to_string()),
                    "openai" => probe(ProviderKind::OpenAiCompatible, &base, key.as_deref()).map_err(|e| e.to_string()),
                    _ => Ok("Offline assistant — nothing to connect".to_string()),
                }
            };
            #[cfg(not(feature = "remote-llm"))]
            let r = {
                let _ = (provider, base, key);
                Err("This build has no network providers".to_string())
            };
            let _ = tx.send(r);
        });
        rx
    }

    // -------------------------------------------------------------- downloads

    /// Where a model stands.
    pub fn model_state(&mut self, id: &str) -> ModelState {
        let Some(entry) = catalog().get(id) else { return ModelState::Unavailable };
        match self.features.ai.downloads.get(id) {
            Some(Dl::Queued) => return ModelState::Queued,
            Some(Dl::Running { done, total, rate }) => return ModelState::Downloading { done: *done, total: *total, bytes_per_s: *rate },
            Some(Dl::Verifying) => return ModelState::Verifying,
            Some(Dl::Failed(e)) => return ModelState::Failed(e.clone()),
            Some(Dl::Paused) => {
                let done = models_dir().map_or(0, |d| quire_ai::download::partial_bytes(&d, entry));
                return ModelState::Paused { done, total: entry.size() };
            }
            None => {}
        }
        if self.features.ai.installed_map().get(id).copied().unwrap_or(false) {
            return ModelState::Ready;
        }
        if !entry.is_pinned() {
            return ModelState::Unavailable;
        }
        let partial = models_dir().map_or(0, |d| quire_ai::download::partial_bytes(&d, entry));
        if partial > 0 { ModelState::Paused { done: partial, total: entry.size() } } else { ModelState::NotInstalled }
    }

    /// Start (or resume, or queue) downloading a model, and the engine first
    /// when it isn't installed. Only ever called when the person asks.
    pub fn ai_download(&mut self, id: &str) {
        let Some(entry) = catalog().get(id) else { return };
        if !self.settings.ai_local.on_device() {
            self.notify(NoticeLevel::Info, ON_DEVICE_LATER);
            return;
        }
        if !entry.is_pinned() {
            self.notify(NoticeLevel::Info, format!("{} isn't available in this build of Quire", entry.display_name()));
            return;
        }
        if id != ENGINE_ID
            && !engine_installed()
            && !self.features.ai.queue.iter().any(|q| q == ENGINE_ID)
            && self.features.ai.active.as_ref().is_none_or(|(a, _)| a != ENGINE_ID)
        {
            self.ai_download(ENGINE_ID);
        }
        let st = &mut self.features.ai;
        if st.active.as_ref().is_some_and(|(a, _)| a == id) || st.queue.iter().any(|q| q == id) {
            return;
        }
        st.downloads.insert(id.to_string(), Dl::Queued);
        st.queue.push_back(id.to_string());
        self.start_next_download();
    }

    fn start_next_download(&mut self) {
        let st = &mut self.features.ai;
        if st.active.is_some() {
            return;
        }
        let Some(id) = st.queue.pop_front() else { return };
        let (Some(entry), Some(dir)) = (catalog().get(&id).cloned(), models_dir()) else {
            st.downloads.insert(id, Dl::Failed("no folder for models on this system".into()));
            return;
        };
        let ctl = Control::new();
        let (tx, rx) = unbounded();
        let mirror = self.settings.ai_local.mirror_url.clone();
        {
            let ctl = ctl.clone();
            let id = id.clone();
            std::thread::Builder::new()
                .name("quire-model-download".into())
                .spawn(move || {
                    let d = Downloader::new(&mirror);
                    let r = d.fetch(&entry, &dir, &ctl, &mut |p| {
                        let _ = tx.send(DlMsg::Progress(id.clone(), p));
                    });
                    let _ = tx.send(DlMsg::Finished(id, r));
                })
                .ok();
        }
        st.downloads.insert(id.clone(), Dl::Running { done: 0, total: catalog().size_of(&id), rate: 0.0 });
        st.active = Some((id, ctl));
        st.rx = Some(rx);
    }

    /// Stop a download, keeping what arrived (Download resumes it).
    pub fn ai_pause(&mut self, id: &str) {
        let st = &mut self.features.ai;
        if let Some((a, ctl)) = &st.active
            && a == id
        {
            ctl.pause();
        } else if st.queue.iter().any(|q| q == id) {
            st.queue.retain(|q| q != id);
            st.downloads.insert(id.to_string(), Dl::Paused);
        }
    }

    /// Stop a download and delete what arrived.
    pub fn ai_cancel(&mut self, id: &str) {
        let st = &mut self.features.ai;
        if let Some((a, ctl)) = &st.active
            && a == id
        {
            ctl.cancel();
            return;
        }
        st.queue.retain(|q| q != id);
        st.downloads.remove(id);
        if let (Some(d), Some(entry)) = (models_dir(), catalog().get(id))
            && !installed(entry)
        {
            let _ = quire_ai::download::delete(&d, id);
        }
        st.installed = None;
    }

    /// Delete a model's files (the engine stops using it first).
    pub fn ai_delete(&mut self, id: &str) {
        let e = engine(&self.settings);
        if id == ENGINE_ID {
            e.stop();
        } else {
            e.unload(id);
        }
        if let Some(d) = models_dir()
            && let Err(err) = quire_ai::download::delete(&d, id)
        {
            self.notify(NoticeLevel::Error, format!("Couldn't delete the files: {err}"));
        }
        self.features.ai.downloads.remove(id);
        self.features.ai.installed = None;
        self.apply_ai_settings();
    }

    /// "Install from file…": files downloaded elsewhere, matched by hash.
    pub fn ai_install_files(&mut self, paths: &[PathBuf]) {
        let Some(dir) = models_dir() else { return };
        let results = quire_ai::download::install_from_files(catalog(), paths, &dir);
        let ok = results.iter().filter(|r| matches!(r, quire_ai::download::Installed1::Ok { .. })).count();
        let unknown = results.iter().filter(|r| matches!(r, quire_ai::download::Installed1::Unknown { .. })).count();
        for r in &results {
            if let quire_ai::download::Installed1::Failed { path, error } = r {
                self.notify(NoticeLevel::Error, format!("{}: {error}", path.display()));
            }
        }
        if unknown > 0 {
            self.notify(
                NoticeLevel::Warn,
                format!("{unknown} file(s) didn't match any model this version of Quire knows (checked by SHA-256)"),
            );
        }
        if ok > 0 {
            self.notify(NoticeLevel::Info, format!("Installed {ok} file(s)"));
        }
        self.features.ai.installed = None;
        self.apply_ai_settings();
    }

    /// The engine's status for Settings (refreshed in the background every few seconds).
    pub fn ai_engine_status(&mut self) -> EngineSnapshot {
        let st = &mut self.features.ai;
        if let Some(rx) = &st.status_rx
            && let Ok(s) = rx.try_recv()
        {
            st.snapshot = Some((Instant::now(), s));
            st.status_rx = None;
        }
        let stale = st.snapshot.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(3));
        if stale && st.status_rx.is_none() {
            let e = engine(&self.settings);
            let (tx, rx) = bounded(1);
            std::thread::spawn(move || {
                e.refresh_status();
                let _ = tx.send(e.snapshot());
            });
            st.status_rx = Some(rx);
        }
        st.snapshot.as_ref().map_or_else(|| engine(&self.settings).snapshot(), |(_, s)| s.clone())
    }

    /// Stop the engine (Settings → AI); it starts again when needed.
    pub fn ai_stop_engine(&mut self) {
        let e = engine(&self.settings);
        e.stop();
        e.restart_allowed();
        self.features.ai.snapshot = None;
    }

    /// Drain download progress (from `assistant_state::tick`). True to repaint.
    pub(crate) fn ai_tick(&mut self) -> bool {
        let Some(rx) = self.features.ai.rx.clone() else { return self.features.ai.status_rx.is_some() };
        let mut changed = false;
        let mut finished = Vec::new();
        while let Ok(m) = rx.try_recv() {
            changed = true;
            match m {
                DlMsg::Progress(id, Progress::Bytes { done, total, bytes_per_s }) => {
                    self.features.ai.downloads.insert(id, Dl::Running { done, total, rate: bytes_per_s });
                }
                DlMsg::Progress(id, Progress::Verifying) => {
                    self.features.ai.downloads.insert(id, Dl::Verifying);
                }
                DlMsg::Finished(id, r) => finished.push((id, r)),
            }
        }
        for (id, r) in finished {
            let st = &mut self.features.ai;
            st.active = None;
            st.rx = None;
            st.installed = None;
            let name = catalog().get(&id).map_or_else(|| id.clone(), |e| e.display_name().to_string());
            match r {
                Ok(()) => {
                    st.downloads.remove(&id);
                    if st.queue.is_empty() {
                        self.notify(NoticeLevel::Info, format!("{name} is ready. It works offline from now on."));
                    }
                    self.apply_ai_settings();
                }
                Err(DownloadError::Paused) => {
                    st.downloads.insert(id, Dl::Paused);
                }
                Err(DownloadError::Cancelled) => {
                    st.downloads.remove(&id);
                }
                Err(e) => {
                    let msg = e.to_string();
                    st.downloads.insert(id, Dl::Failed(msg.clone()));
                    self.notify(NoticeLevel::Error, format!("{name}: {msg}"));
                }
            }
            self.start_next_download();
        }
        changed || self.features.ai.active.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "remote-llm")]
    #[test]
    fn unreachable_embedding_endpoint_falls_back_to_lsa() {
        let mut w =
            Workspace::new(Settings { embeddings: "endpoint".into(), embed_base_url: "http://127.0.0.1:9".into(), ..Default::default() });
        w.attach_vault(quire_vault::Vault::in_memory(&[("A.md", "# A\nRopes make undo cheap because subtrees are shared.")]));
        w.apply_ai_settings();
        w.index().flush_semantic();
        assert!(w.index().semantic_error().is_some());
        let sem = w.index().semantic().expect("LSA fallback built");
        assert!(sem.embedder_name().contains("latent"));
    }

    #[test]
    fn defaults_resolve_per_provider() {
        let mut s = Settings::default();
        assert_eq!(resolved(&s), (String::new(), String::new()));
        s.ai_provider = "openai".into();
        assert_eq!(resolved(&s).0, "http://localhost:11434");
        assert_eq!(embed_base_url(&s), "http://localhost:11434");
        s.ai_base_url = "http://box:1234/v1".into();
        s.ai_model = "qwen".into();
        assert_eq!(resolved(&s), ("http://box:1234/v1".into(), "qwen".into()));
        assert_eq!(embed_base_url(&s), "http://box:1234/v1");
        s.ai_provider = "anthropic".into();
        assert_eq!(embed_base_url(&s), "http://localhost:11434");
    }

    /// On-device AI is experimental in 0.9.0: with every capability turned
    /// on in settings.toml but the switch off, nothing on-device is used,
    /// downloaded or spawned, and the keys keep their values.
    #[test]
    fn on_device_ai_is_off_until_the_experimental_switch() {
        if std::env::var_os(crate::settings::EXPERIMENTAL_ENV).is_some() {
            return; // the switch is on for this process
        }
        let mut s = Settings::default();
        assert!(!s.ai_local.experimental && !on_device(&s), "off by default");
        s.ai_local.engine = true;
        s.ai_local.embeddings = true;
        s.ai_local.llm = true;
        s.ai_local.ocr = true;
        s.embeddings = "local".into();
        assert!(!s.ai_local.embeddings_on() && !s.ai_local.llm_on() && !s.ai_local.ocr_on());
        assert!(device_chat(&s).is_none() && chat_model(&s).is_none() && panel_chat(&s).is_none());
        let mut w = Workspace::new(s.clone());
        w.attach_vault(quire_vault::Vault::in_memory(&[("A.md", "# A\nRopes make undo cheap.")]));
        w.apply_ai_settings();
        for id in ["engine", "bge-small-en-v1.5", "ocrs"] {
            w.ai_download(id);
        }
        assert!(!w.features.ai.busy(), "nothing downloads");
        for _ in 0..5 {
            w.tick(Instant::now());
        }
        assert!(w.jobs.statuses().iter().all(|j| j.id != "ocr"), "no OCR job");
        assert!(!engine(&w.settings).is_running(), "no engine process");
        // Meaning search falls back to the built-in index.
        w.index().flush_semantic();
        assert!(w.index().semantic().is_some_and(|sem| sem.embedder_name().contains("latent")));
        assert_eq!(w.settings.ai_local, s.ai_local, "the choices are kept");

        // The switch brings back today's behaviour: the language model is
        // asked for again (still absent without its files).
        w.settings.ai_local.experimental = true;
        assert!(w.settings.ai_local.on_device() && w.settings.ai_local.llm_on() && w.settings.ai_local.ocr_on());
    }

    #[test]
    fn nothing_on_device_without_opt_in() {
        let s = Settings::default();
        assert!(device_chat(&s).is_none(), "the language model is off by default");
        assert!(chat_model(&s).is_none(), "no provider and nothing on this computer: no AI");
        assert!(panel_chat(&s).is_none());
        let mut w = Workspace::new(s);
        assert!(!w.features.ai.busy(), "nothing downloads on its own");
        w.ai_download("no-such-model");
        assert!(!w.features.ai.busy());
    }
}
