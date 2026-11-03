// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app's on-device models, all served by the engine process (design §D.3).
//!
//! [`LocalEmbedder`] for meaning search, [`LocalChat`] for writing
//! and the agent panel, [`LocalOcr`] for text in images and scans.
//!
//! Each turns engine failures into the errors its port already has, so the
//! app falls back as it does for a provider that is down: the built-in index
//! for search, a notice for chat, no OCR text.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use quire_index::semantic::Embedder;
use quire_intel::chat::{CancelFlag, ChatEvent, ChatModel, ChatRequest, Locality, StopReason, Usage};
use quire_intel::llm::LlmError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::catalog::{CatalogEntry, Tier};
use crate::engine::{Engine, EngineError};
use crate::protocol::{EmbedResult, GenUsage, LoadParams, ModelKind, OcrLine, OcrResult, PdfPage, StopKind, method};

/// Per-computer engine settings (`[ai_local]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSetup {
    pub models_dir: PathBuf,
    pub threads: u32,
    pub gpu: String,
    pub idle_unload_s: u64,
}

/// How the engine should load a catalog model.
pub fn load_params(entry: &CatalogEntry, kind: ModelKind, setup: &LocalSetup) -> LoadParams {
    LoadParams {
        model: entry.id.clone(),
        path: setup.models_dir.join(&entry.id).to_string_lossy().into_owned(),
        sha256: entry.files.first().map(|f| f.sha256.clone()).unwrap_or_default(),
        kind,
        ctx: 8192,
        idle_unload_s: setup.idle_unload_s,
        threads: setup.threads,
        gpu: setup.gpu.clone(),
        options: json!({
            "pooling": entry.pooling,
            "files": entry.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        }),
    }
}

// ------------------------------------------------------------------ embeddings

/// Vectors kept between runs: `<vault>/.quire/cache/vectors-<model>.f16`
/// (docs/formats/model-catalog.md): `QVF1`, dims (u32), the model id's
/// length (u32) and bytes, then records of a u64 key and `dims` f16 values,
/// all little-endian.
#[derive(Debug, Default)]
struct F16Cache {
    path: Option<PathBuf>,
    model: String,
    dims: usize,
    map: HashMap<u64, Vec<half::f16>>,
    used: HashSet<u64>,
    unsaved: usize,
}

const MAGIC: &[u8; 4] = b"QVF1";

impl F16Cache {
    fn open(path: Option<PathBuf>, model: &str) -> Self {
        let mut c = Self { path, model: model.to_string(), ..Self::default() };
        let Some(p) = &c.path else { return c };
        let Ok(bytes) = std::fs::read(p) else { return c };
        let Some(rest) = bytes.strip_prefix(MAGIC.as_slice()) else { return c };
        let u32_at = |b: &[u8], i: usize| b.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
        let (Some(dims), Some(mlen)) = (u32_at(rest, 0), u32_at(rest, 4)) else { return c };
        if rest.get(8..8 + mlen) != Some(model.as_bytes()) || dims == 0 {
            return c;
        }
        c.dims = dims;
        let mut i = 8 + mlen;
        let rec = 8 + dims * 2;
        while i + rec <= rest.len() {
            let key = u64::from_le_bytes(rest[i..i + 8].try_into().unwrap_or([0; 8]));
            let v = rest[i + 8..i + rec].chunks_exact(2).map(|b| half::f16::from_le_bytes([b[0], b[1]])).collect();
            c.map.insert(key, v);
            i += rec;
        }
        c
    }

    fn key(prefix: &str, text: &str) -> u64 {
        let mut h = Sha256::new();
        h.update(prefix.as_bytes());
        h.update([0]);
        h.update(text.as_bytes());
        let d = h.finalize();
        u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
    }

    fn get(&mut self, key: u64) -> Option<Vec<f32>> {
        let v = self.map.get(&key)?;
        self.used.insert(key);
        Some(v.iter().map(|x| x.to_f32()).collect())
    }

    fn put(&mut self, key: u64, v: &[f32]) {
        if self.dims == 0 {
            self.dims = v.len();
        }
        if v.len() != self.dims {
            return;
        }
        self.map.insert(key, v.iter().map(|x| half::f16::from_f32(*x)).collect());
        self.used.insert(key);
        self.unsaved += 1;
    }

    /// Write the cache. Vectors not used since it was opened are dropped when
    /// most of it was used (a full rebuild), so it doesn't grow forever.
    fn save(&mut self) {
        let Some(p) = self.path.clone() else { return };
        if self.unsaved == 0 || self.dims == 0 {
            return;
        }
        if self.used.len() * 2 >= self.map.len() {
            let used = std::mem::take(&mut self.used);
            self.map.retain(|k, _| used.contains(k));
            self.used = used;
        }
        let mut out = Vec::with_capacity(16 + self.model.len() + self.map.len() * (8 + self.dims * 2));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&u32::try_from(self.dims).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&u32::try_from(self.model.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(self.model.as_bytes());
        let mut keys: Vec<_> = self.map.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            out.extend_from_slice(&k.to_le_bytes());
            for x in &self.map[&k] {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("f16.tmp");
        if std::fs::write(&tmp, &out).and_then(|()| std::fs::rename(&tmp, &p)).is_ok() {
            self.unsaved = 0;
        }
    }
}

/// Meaning search on this computer ([`Embedder`]): batches of 64 to the
/// engine, vectors cached as f16 in the vault's cache folder.
pub struct LocalEmbedder {
    engine: Engine,
    spec: LoadParams,
    label: String,
    query_prefix: String,
    passage_prefix: String,
    cache: Mutex<F16Cache>,
}

impl LocalEmbedder {
    /// `cache_dir` is `<vault>/.quire/cache` (`None`: keep vectors in memory).
    pub fn new(engine: Engine, entry: &CatalogEntry, setup: &LocalSetup, cache_dir: Option<&Path>) -> Self {
        let path = cache_dir.map(|d| d.join(format!("vectors-{}.f16", entry.id)));
        Self {
            engine,
            spec: load_params(entry, ModelKind::Embed, setup),
            label: format!("{} (on this computer)", entry.display_name()),
            query_prefix: entry.query_prefix.clone(),
            passage_prefix: entry.passage_prefix.clone(),
            cache: Mutex::new(F16Cache::open(path, &entry.id)),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, F16Cache> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Vectors for `texts` with `prefix`, from the cache or the engine.
    pub fn embed_with(&self, texts: &[&str], prefix: &str) -> Result<Vec<Vec<f32>>, EngineError> {
        let keys: Vec<u64> = texts.iter().map(|t| F16Cache::key(prefix, t)).collect();
        let mut out: Vec<Option<Vec<f32>>> = {
            let mut c = self.cache();
            keys.iter().map(|k| c.get(*k)).collect()
        };
        let missing: Vec<usize> = (0..texts.len()).filter(|&i| out[i].is_none()).collect();
        for batch in missing.chunks(64) {
            let batch_texts: Vec<&str> = batch.iter().map(|&i| texts[i]).collect();
            let vecs = self.engine.with_model(&self.spec, |e| {
                let params = json!({ "model": self.spec.model, "texts": batch_texts, "normalize": true, "prefix": prefix });
                let r = e.call(method::EMBED, params, None, &mut |_| {}, Duration::from_mins(2))?;
                decode(&r)
            })?;
            if vecs.len() != batch.len() {
                return Err(EngineError::Failed("the engine returned the wrong number of vectors".into()));
            }
            let mut c = self.cache();
            for (&i, v) in batch.iter().zip(vecs) {
                c.put(keys[i], &v);
                out[i] = Some(v);
            }
            if c.unsaved >= 2000 {
                c.save();
            }
        }
        Ok(out.into_iter().map(Option::unwrap_or_default).collect())
    }
}

/// `{dims, vectors_f16_b64}` into rows.
fn decode(r: &Value) -> Result<Vec<Vec<f32>>, EngineError> {
    let res: EmbedResult = serde_json::from_value(r.clone()).map_err(|e| EngineError::Failed(format!("bad embed reply: {e}")))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(res.vectors_f16_b64).map_err(|e| EngineError::Failed(e.to_string()))?;
    let dims = res.dims as usize;
    if dims == 0 || !bytes.len().is_multiple_of(dims * 2) {
        return Err(EngineError::Failed("bad embed reply".into()));
    }
    Ok(bytes
        .chunks_exact(dims * 2)
        .map(|row| row.chunks_exact(2).map(|b| half::f16::from_le_bytes([b[0], b[1]]).to_f32()).collect())
        .collect())
}

impl Drop for LocalEmbedder {
    fn drop(&mut self) {
        self.cache().save();
    }
}

impl Embedder for LocalEmbedder {
    fn name(&self) -> &str {
        &self.label
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_with(&[text], &self.query_prefix).ok().and_then(|mut v| v.pop()).unwrap_or_default()
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let r = self.embed_with(texts, &self.passage_prefix).map_err(|e| e.to_string());
        self.cache().save();
        r
    }
}

// ------------------------------------------------------------------ chat

/// The on-device language model as a [`ChatModel`].
pub struct LocalChat {
    engine: Engine,
    spec: LoadParams,
    name: String,
    tier: Tier,
}

impl LocalChat {
    pub fn new(engine: Engine, entry: &CatalogEntry, setup: &LocalSetup) -> Self {
        Self {
            engine,
            spec: load_params(entry, ModelKind::Llm, setup),
            name: entry.display_name().to_string(),
            tier: entry.tier.unwrap_or_default(),
        }
    }

    pub const fn tier(&self) -> Tier {
        self.tier
    }
}

fn llm_error(e: EngineError) -> LlmError {
    match e {
        EngineError::Cancelled => LlmError::Cancelled,
        EngineError::NotInstalled => {
            LlmError::NotConfigured("the on-device AI engine isn't installed; download it in Settings → AI".into())
        }
        other => LlmError::Request(other.to_string()),
    }
}

impl ChatModel for LocalChat {
    fn name(&self) -> &str {
        &self.name
    }

    fn locality(&self) -> Locality {
        Locality::Device
    }

    fn supports_tools(&self) -> bool {
        self.tier != Tier::Small
    }

    fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError> {
        let params = json!({
            "model": self.spec.model,
            "system": req.system,
            "messages": req.messages,
            "tools": if req.tools.is_empty() || !self.supports_tools() { Value::Null } else { serde_json::to_value(&req.tools).unwrap_or_default() },
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "json": req.json,
        });
        let mut emitted = false;
        let r = self.engine.with_model(&self.spec, |e| {
            if emitted {
                // Half a reply was shown; don't start it again.
                return Err(EngineError::Failed("the engine stopped in the middle of the reply".into()));
            }
            e.call(
                method::GENERATE,
                params.clone(),
                Some(cancel),
                &mut |ev| {
                    if let Some(t) = ev["delta"].as_str() {
                        emitted = true;
                        sink(ChatEvent::Text { text: t.to_string() });
                    } else if let Some(c) = ev.get("tool_call") {
                        emitted = true;
                        sink(ChatEvent::ToolUse {
                            id: c["id"].as_str().unwrap_or_default().to_string(),
                            name: c["name"].as_str().unwrap_or_default().to_string(),
                            input: c.get("input").cloned().unwrap_or_else(|| json!({})),
                        });
                    }
                },
                Duration::from_mins(2),
            )
        });
        let done = r.map_err(llm_error)?;
        let usage: GenUsage = serde_json::from_value(done["usage"].clone()).unwrap_or_default();
        let stop = match usage.stop {
            StopKind::EndTurn => StopReason::EndTurn,
            StopKind::ToolUse => StopReason::ToolUse,
            StopKind::MaxTokens => StopReason::MaxTokens,
            StopKind::StopSequence => StopReason::StopSequence,
        };
        sink(ChatEvent::Done { stop, usage: Usage { input_tokens: usage.input_tokens, output_tokens: usage.output_tokens } });
        Ok(())
    }
}

// ------------------------------------------------------------------ OCR

/// Text in images and scanned PDF pages, read on this computer.
pub struct LocalOcr {
    engine: Engine,
    spec: LoadParams,
}

impl LocalOcr {
    pub fn new(engine: Engine, entry: &CatalogEntry, setup: &LocalSetup) -> Self {
        Self { engine, spec: load_params(entry, ModelKind::Ocr, setup) }
    }

    /// Lines of text in an image file (PNG, JPEG, WebP, GIF).
    pub fn image(&self, path: &Path, cancel: &CancelFlag) -> Result<Vec<OcrLine>, EngineError> {
        self.run(&json!({ "model_set": self.spec.model, "image_path": path.to_string_lossy() }), cancel)
    }

    /// Lines of text on one page (0-based) of a PDF, rendered at 200 dpi.
    pub fn pdf_page(&self, path: &Path, page: u32, cancel: &CancelFlag) -> Result<Vec<OcrLine>, EngineError> {
        let pdf = PdfPage { path: path.to_string_lossy().into_owned(), page, dpi: 200 };
        self.run(&json!({ "model_set": self.spec.model, "pdf": pdf }), cancel)
    }

    fn run(&self, params: &Value, cancel: &CancelFlag) -> Result<Vec<OcrLine>, EngineError> {
        self.engine.with_model(&self.spec, |e| {
            let r = e.call(method::OCR, params.clone(), Some(cancel), &mut |_| {}, Duration::from_mins(2))?;
            serde_json::from_value::<OcrResult>(r).map(|r| r.lines).map_err(|e| EngineError::Failed(format!("bad OCR reply: {e}")))
        })
    }
}

/// OCR lines as plain text, top to bottom, left to right within a line.
pub fn ocr_text(lines: &[OcrLine]) -> String {
    let mut v: Vec<&OcrLine> = lines.iter().filter(|l| !l.text.trim().is_empty()).collect();
    v.sort_by(|a, b| {
        let (ay, by) = (a.quad[0][1].min(a.quad[1][1]), b.quad[0][1].min(b.quad[1][1]));
        ay.total_cmp(&by).then(a.quad[0][0].total_cmp(&b.quad[0][0]))
    });
    v.iter().map(|l| l.text.trim()).collect::<Vec<_>>().join("\n")
}

/// The `cache/ocr/<sha256>.json` file for an attachment with this content hash.
pub fn ocr_cache_path(cache_dir: &Path, sha256: &str) -> PathBuf {
    cache_dir.join("ocr").join(format!("{sha256}.json"))
}

/// Read a cached OCR result (`{format, model, pages}`), if it was made by `model`.
pub fn read_ocr_cache(path: &Path, model: &str) -> Option<Vec<String>> {
    let mut s = String::new();
    std::fs::File::open(path).ok()?.read_to_string(&mut s).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    if v["format"].as_u64()? > 1 || v["model"].as_str()? != model {
        return None;
    }
    Some(v["pages"].as_array()?.iter().map(|p| p.as_str().unwrap_or_default().to_string()).collect())
}

pub fn write_ocr_cache(path: &Path, model: &str, pages: &[String]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(json!({ "format": 1, "model": model, "pages": pages }).to_string().as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_cache_round_trips_and_prunes_after_a_full_pass() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vectors-m.f16");
        let mut c = F16Cache::open(Some(p.clone()), "m");
        let (a, b) = (F16Cache::key("", "a"), F16Cache::key("passage: ", "a"));
        assert_ne!(a, b, "the prefix is part of the key");
        c.put(a, &[0.5, -1.0, 0.25]);
        c.put(b, &[1.0, 0.0, 0.0]);
        c.save();
        let mut c = F16Cache::open(Some(p.clone()), "m");
        assert_eq!(c.get(a), Some(vec![0.5, -1.0, 0.25]));
        c.put(F16Cache::key("", "new"), &[0.0, 1.0, 0.0]);
        c.save();
        let c = F16Cache::open(Some(p.clone()), "m");
        assert_eq!(c.map.len(), 2, "the unused vector was dropped after a pass that used most of the cache");
        assert!(F16Cache::open(Some(p), "other-model").map.is_empty(), "another model's cache isn't read");
    }

    #[test]
    fn ocr_lines_read_top_to_bottom() {
        let line = |t: &str, x: f32, y: f32| OcrLine {
            text: t.into(),
            quad: [[x, y], [x + 10.0, y], [x + 10.0, y + 5.0], [x, y + 5.0]],
            conf: 0.9,
        };
        assert_eq!(ocr_text(&[line("second", 0.0, 20.0), line("first", 0.0, 2.0), line(" ", 0.0, 9.0)]), "first\nsecond");
        let dir = tempfile::tempdir().unwrap();
        let p = ocr_cache_path(dir.path(), "ab");
        write_ocr_cache(&p, "ocrs", &["page one".into()]).unwrap();
        assert_eq!(read_ocr_cache(&p, "ocrs"), Some(vec!["page one".to_string()]));
        assert_eq!(read_ocr_cache(&p, "other"), None);
    }
}
