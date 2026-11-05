// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The real models (design §D.3).
//!
//! Candle's quantized Qwen3 for writing and the agent, candle's BERT for embeddings (BGE, E5), and ocrs for OCR, with
//! hayro rendering PDF pages to read scans. Tokenizers use the pure-Rust
//! regex backend (fancy-regex), which handles Qwen3's look-ahead.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::quantized::gguf_file;
use candle_core::{D, DType, Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use quire_ai::protocol::{
    EmbedParams, GenUsage, GenerateParams, LoadParams, ModelKind, OcrLine, OcrParams, StopKind, TranscribeParams, TranscribeResult, code,
};
use serde_json::{Value, json};
use tokenizers::Tokenizer;

use crate::qwen3::{Options as QwenOptions, Qwen3, Repack};
use crate::template::{Piece, ReplyParser, STOP_TOKENS, render};
use crate::{Backend, Failure};

fn load_failed(e: impl std::fmt::Display) -> Failure {
    (code::LOAD_FAILED, e.to_string())
}

fn failed(e: impl std::fmt::Display) -> Failure {
    (code::FAILED, e.to_string())
}

struct Llm {
    model: Qwen3,
    tok: Tokenizer,
    device: Device,
    stop_ids: Vec<u32>,
    ctx: usize,
}

struct Embedder {
    model: BertModel,
    tok: Tokenizer,
    device: Device,
    mean_pooling: bool,
}

struct Ocr {
    engine: ocrs::OcrEngine,
}

enum Model {
    Llm(Box<Llm>),
    Embed(Box<Embedder>),
    Ocr(Box<Ocr>),
    Whisper(Box<crate::whisper::Transcriber>),
}

#[derive(Default)]
pub struct RealBackend {
    models: HashMap<String, Model>,
}

impl RealBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The device for `gpu` (`auto`, `on`, `off`): Metal on macOS builds that
/// have it, else the CPU.
fn device(gpu: &str) -> Device {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    if gpu != "off"
        && let Ok(d) = Device::new_metal(0)
    {
        return d;
    }
    let _ = gpu;
    Device::Cpu
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

fn tokenizer(dir: &Path) -> Result<Tokenizer, Failure> {
    Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| load_failed(format!("tokenizer.json: {e}")))
}

impl Backend for RealBackend {
    fn devices(&self) -> Vec<String> {
        let mut d = vec!["cpu".to_string()];
        if cfg!(all(feature = "metal", target_os = "macos")) {
            d.push("metal".into());
        }
        d
    }

    fn load(&mut self, p: &LoadParams) -> Result<u64, Failure> {
        if self.models.contains_key(&p.model) {
            return Ok(0);
        }
        let dir = Path::new(&p.path);
        let (model, mem) = match p.kind {
            ModelKind::Llm => {
                let path = dir.join("model.gguf");
                let mut f = std::fs::File::open(&path).map_err(|e| load_failed(format!("{}: {e}", path.display())))?;
                let ct = gguf_file::Content::read(&mut f).map_err(load_failed)?;
                let device = device(&p.gpu);
                let ctx = if p.ctx == 0 { 8192 } else { p.ctx as usize };
                // `repack: "all"` in the options gives every weight to candle's
                // x86 tiles (faster prompts, more memory); `"never"` none.
                let repack = match p.options["repack"].as_str() {
                    Some("all") => Repack::All,
                    Some("never") => Repack::Never,
                    _ => Repack::Q8,
                };
                let opts = QwenOptions { repack, max_ctx: ctx };
                let model = Qwen3::from_gguf(ct, &mut f, &device, opts).map_err(load_failed)?;
                let tok = tokenizer(dir)?;
                let stop_ids = STOP_TOKENS.iter().filter_map(|t| tok.token_to_id(t)).collect();
                // The quantized weights as they are in the file (the KV cache comes on top).
                let mem = file_size(&path);
                (Model::Llm(Box::new(Llm { model, tok, device, stop_ids, ctx })), mem)
            }
            ModelKind::Embed => {
                let device = device(&p.gpu);
                let config: BertConfig =
                    serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).map_err(load_failed)?).map_err(load_failed)?;
                let weights = std::fs::read(dir.join("model.safetensors")).map_err(load_failed)?;
                let mem = weights.len() as u64;
                let vb = candle_nn::VarBuilder::from_buffered_safetensors(weights, DType::F32, &device).map_err(load_failed)?;
                let model = BertModel::load(vb, &config).map_err(load_failed)?;
                let mut tok = tokenizer(dir)?;
                tok.with_padding(Some(tokenizers::PaddingParams {
                    strategy: tokenizers::PaddingStrategy::BatchLongest,
                    ..Default::default()
                }));
                tok.with_truncation(Some(tokenizers::TruncationParams { max_length: 512, ..Default::default() })).map_err(load_failed)?;
                let mean_pooling = p.options["pooling"].as_str() == Some("mean");
                (Model::Embed(Box::new(Embedder { model, tok, device, mean_pooling })), mem)
            }
            ModelKind::Ocr => {
                let det = dir.join("text-detection.onnx");
                let rec = dir.join("text-recognition.onnx");
                let mem = file_size(&det) + file_size(&rec);
                let detection_model = rten::Model::load_file(&det).map_err(load_failed)?;
                let recognition_model = rten::Model::load_file(&rec).map_err(load_failed)?;
                let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
                    detection_model: Some(detection_model),
                    recognition_model: Some(recognition_model),
                    ..Default::default()
                })
                .map_err(load_failed)?;
                (Model::Ocr(Box::new(Ocr { engine })), mem * 4)
            }
            ModelKind::Transcribe => {
                let mem = file_size(&dir.join("model.safetensors"));
                (Model::Whisper(Box::new(crate::whisper::Transcriber::load(dir, &device(&p.gpu))?)), mem)
            }
        };
        self.models.insert(p.model.clone(), model);
        Ok(mem)
    }

    fn unload(&mut self, model: &str) {
        self.models.remove(model);
    }

    fn embed(&mut self, p: &EmbedParams) -> Result<(u32, Vec<f32>), Failure> {
        let Some(Model::Embed(m)) = self.models.get(&p.model) else {
            return Err((code::MODEL_NOT_LOADED, format!("{} isn't loaded", p.model)));
        };
        if p.texts.is_empty() {
            return Ok((0, Vec::new()));
        }
        // Sub-batches of 16 bound the attention tensors (64 passages of 256
        // tokens at once took 1.8 GB in the spike; 16 take about a quarter).
        let mut dims = 0;
        let mut flat = Vec::new();
        for part in p.texts.chunks(16) {
            let texts: Vec<String> = part.iter().map(|t| format!("{}{t}", p.prefix)).collect();
            let (d, mut v) = embed_part(m, texts, p.normalize)?;
            dims = d;
            flat.append(&mut v);
        }
        Ok((u32::try_from(dims).unwrap_or(0), flat))
    }

    fn generate(&mut self, p: &GenerateParams, cancel: &AtomicBool, emit: &mut dyn FnMut(Value)) -> Result<GenUsage, Failure> {
        let Some(Model::Llm(m)) = self.models.get_mut(&p.model) else {
            return Err((code::MODEL_NOT_LOADED, format!("{} isn't loaded", p.model)));
        };
        let tools = p.tools.clone().unwrap_or_default();
        let max_new = p.max_tokens.max(1) as usize;
        // Leave room for the reply: drop the oldest turns until the prompt fits.
        let mut messages = p.messages.clone();
        let mut prompt_ids;
        loop {
            let prompt = render(&p.system, &messages, &tools, p.json);
            prompt_ids = m.tok.encode(prompt, false).map_err(failed)?.get_ids().to_vec();
            if prompt_ids.len() + max_new <= m.ctx || messages.len() <= 1 {
                break;
            }
            messages.remove(0);
        }
        if prompt_ids.len() + max_new > m.ctx {
            return Err((code::BAD_PARAMS, format!("the request is too long for this model ({} tokens)", prompt_ids.len())));
        }
        m.model.clear_kv_cache();
        let sampling = if p.temperature <= 0.0 {
            Sampling::ArgMax
        } else {
            Sampling::TopKThenTopP { k: 40, p: 0.9, temperature: f64::from(p.temperature) }
        };
        let mut sampler = LogitsProcessor::from_sampling(42, sampling);
        // Prefill in chunks so a long prompt doesn't need one huge attention matrix.
        let mut logits = None;
        let mut pos = 0;
        for chunk in prompt_ids.chunks(256) {
            if cancel.load(Ordering::Acquire) {
                return Err((code::CANCELLED, "cancelled".into()));
            }
            let input = Tensor::new(chunk, &m.device).and_then(|t| t.unsqueeze(0)).map_err(failed)?;
            logits = Some(m.model.forward(&input, pos).map_err(failed)?);
            pos += chunk.len();
        }
        let mut logits = logits.ok_or_else(|| failed("empty prompt"))?;
        let mut parser = ReplyParser::with_tools(tools.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect());
        let mut out_ids: Vec<u32> = Vec::new();
        let mut shown = 0usize;
        let mut text = String::new();
        let stop_strings = p.stop.clone().unwrap_or_default();
        let mut stop = StopKind::EndTurn;
        let emit_pieces = |pieces: Vec<Piece>, emit: &mut dyn FnMut(Value)| {
            for piece in pieces {
                match piece {
                    Piece::Text(t) => emit(json!({ "delta": t })),
                    Piece::ToolCall { id, name, input } => emit(json!({ "tool_call": { "id": id, "name": name, "input": input } })),
                    // Never run: no name, and the raw text under `_unparsed`.
                    Piece::BadCall { id, raw, error } => {
                        emit(json!({ "tool_call": { "id": id, "name": "", "input": { "_unparsed": raw, "_error": error } } }));
                    }
                }
            }
        };
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err((code::CANCELLED, "cancelled".into()));
            }
            let l = logits.squeeze(0).and_then(|x| x.to_dtype(DType::F32)).map_err(failed)?;
            let l = if out_ids.is_empty() {
                l
            } else {
                candle_transformers::utils::apply_repeat_penalty(&l, 1.1, &out_ids[out_ids.len().saturating_sub(64)..]).map_err(failed)?
            };
            let next = sampler.sample(&l).map_err(failed)?;
            if m.stop_ids.contains(&next) {
                break;
            }
            out_ids.push(next);
            // Decode everything so far and send what's new (tokens can split characters).
            let decoded = m.tok.decode(&out_ids, true).map_err(failed)?;
            if decoded.len() > shown && decoded.is_char_boundary(shown) && !decoded.ends_with('\u{fffd}') {
                let delta = decoded[shown..].to_string();
                shown = decoded.len();
                text.push_str(&delta);
                emit_pieces(parser.push(&delta), emit);
            }
            if !stop_strings.is_empty() && stop_strings.iter().any(|s| !s.is_empty() && text.ends_with(s.as_str())) {
                stop = StopKind::StopSequence;
                break;
            }
            if out_ids.len() >= max_new || pos + out_ids.len() >= m.ctx {
                stop = StopKind::MaxTokens;
                break;
            }
            let input = Tensor::new(&[next], &m.device).and_then(|t| t.unsqueeze(0)).map_err(failed)?;
            logits = m.model.forward(&input, pos + out_ids.len() - 1).map_err(failed)?;
        }
        emit_pieces(parser.finish(), emit);
        if parser.calls() > 0 && stop == StopKind::EndTurn {
            stop = StopKind::ToolUse;
        }
        Ok(GenUsage {
            input_tokens: u32::try_from(prompt_ids.len()).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(out_ids.len()).unwrap_or(u32::MAX),
            stop,
        })
    }

    fn ocr(&mut self, p: &OcrParams, cancel: &AtomicBool) -> Result<Vec<OcrLine>, Failure> {
        let Some(Model::Ocr(m)) = self.models.get(&p.model_set) else {
            return Err((code::MODEL_NOT_LOADED, format!("{} isn't loaded", p.model_set)));
        };
        let (rgb, w, h) = if let Some(img) = &p.image_path {
            let i = image::open(img).map_err(failed)?.into_rgb8();
            let (w, h) = i.dimensions();
            (i.into_raw(), w, h)
        } else if let Some(pdf) = &p.pdf {
            render_page(&pdf.path, pdf.page, pdf.dpi)?
        } else {
            return Err((code::BAD_PARAMS, "give image_path or pdf".into()));
        };
        if cancel.load(Ordering::Acquire) {
            return Err((code::CANCELLED, "cancelled".into()));
        }
        let src = ocrs::ImageSource::from_bytes(&rgb, (w, h)).map_err(failed)?;
        let input = m.engine.prepare_input(src).map_err(failed)?;
        let words = m.engine.detect_words(&input).map_err(failed)?;
        let lines = m.engine.find_text_lines(&input, &words);
        if cancel.load(Ordering::Acquire) {
            return Err((code::CANCELLED, "cancelled".into()));
        }
        let texts = m.engine.recognize_text(&input, &lines).map_err(failed)?;
        Ok(texts
            .into_iter()
            .flatten()
            .map(|line| {
                use ocrs::TextItem as _;
                let r = line.bounding_rect();
                let (l, t, rr, b) = (r.left() as f32, r.top() as f32, r.right() as f32, r.bottom() as f32);
                OcrLine { text: line.to_string(), quad: [[l, t], [rr, t], [rr, b], [l, b]], conf: 1.0 }
            })
            .collect())
    }

    fn transcribe(&mut self, p: &TranscribeParams, cancel: &AtomicBool) -> Result<TranscribeResult, Failure> {
        self.transcribe_impl(p, cancel)
    }
}

impl RealBackend {
    fn transcribe_impl(&mut self, p: &TranscribeParams, cancel: &AtomicBool) -> Result<TranscribeResult, Failure> {
        let Some(Model::Whisper(m)) = self.models.get_mut(&p.model) else {
            return Err((code::MODEL_NOT_LOADED, format!("{} isn't loaded", p.model)));
        };
        let bytes = std::fs::read(&p.audio_path).map_err(|e| failed(format!("{}: {e}", p.audio_path)))?;
        let audio = quire_ai::wav::decode(&bytes).map_err(failed)?;
        let pcm = quire_ai::wav::resample(&audio.samples, audio.rate, quire_ai::wav::VOICE_RATE);
        m.run(&pcm, &p.language, cancel)
    }
}

/// Embeddings of one sub-batch: (dims, rows).
fn embed_part(m: &Embedder, texts: Vec<String>, normalize: bool) -> Result<(usize, Vec<f32>), Failure> {
    let enc = m.tok.encode_batch(texts, true).map_err(failed)?;
    let ids: Vec<Tensor> = enc.iter().map(|e| Tensor::new(e.get_ids(), &m.device)).collect::<Result<_, _>>().map_err(failed)?;
    let mask: Vec<Tensor> = enc.iter().map(|e| Tensor::new(e.get_attention_mask(), &m.device)).collect::<Result<_, _>>().map_err(failed)?;
    let ids = Tensor::stack(&ids, 0).map_err(failed)?;
    let mask = Tensor::stack(&mask, 0).map_err(failed)?;
    let types = ids.zeros_like().map_err(failed)?;
    let hidden = m.model.forward(&ids, &types, Some(&mask)).map_err(failed)?;
    let pooled = if m.mean_pooling {
        let maskf = mask.to_dtype(DType::F32).and_then(|x| x.unsqueeze(2)).map_err(failed)?;
        let sum = hidden.broadcast_mul(&maskf).and_then(|x| x.sum(1)).map_err(failed)?;
        let count = maskf.sum(1).map_err(failed)?;
        sum.broadcast_div(&count).map_err(failed)?
    } else {
        hidden.narrow(1, 0, 1).and_then(|x| x.squeeze(1)).map_err(failed)?
    };
    let pooled = if normalize {
        let norm = pooled.sqr().and_then(|x| x.sum_keepdim(D::Minus1)).and_then(|x| x.sqrt()).map_err(failed)?;
        pooled.broadcast_div(&norm).map_err(failed)?
    } else {
        pooled
    };
    let dims = pooled.dim(1).map_err(failed)?;
    let flat: Vec<f32> = pooled.flatten_all().and_then(|x| x.to_vec1()).map_err(failed)?;
    Ok((dims, flat))
}

/// One PDF page as RGB pixels (white background) at `dpi`.
fn render_page(path: &str, page: u32, dpi: u32) -> Result<(Vec<u8>, u32, u32), Failure> {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    let data = std::fs::read(path).map_err(failed)?;
    let pdf = Pdf::new(data).map_err(|e| failed(format!("can't read the PDF: {e:?}")))?;
    let pages = pdf.pages();
    let pg = pages.get(page as usize).ok_or_else(|| (code::BAD_PARAMS, format!("the PDF has {} pages", pages.len())))?;
    let scale = (dpi.clamp(72, 300) as f32) / 72.0;
    let settings = hayro::RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        ..Default::default()
    };
    let pix = hayro::render(pg, &hayro::RenderCache::new(), &InterpreterSettings::default(), &settings);
    let (w, h) = (u32::from(pix.width()), u32::from(pix.height()));
    let rgba = pix.take_unpremultiplied();
    let rgb = rgba.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    Ok((rgb, w, h))
}

#[cfg(test)]
mod tests {
    use tokenizers::pre_tokenizers::split::{Split, SplitPattern};
    use tokenizers::{OffsetReferential, OffsetType, PreTokenizedString, PreTokenizer, SplitDelimiterBehavior};

    /// Qwen2/Qwen3's pre-tokenizer pattern (from their tokenizer.json): it
    /// needs look-ahead (`\s+(?!\S)`), which the `regex` crate lacks. The
    /// pure-Rust fancy-regex backend must split exactly as the reference
    /// (onig, as the Python tokenizers use) does.
    const QWEN: &str = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";

    fn split(text: &str) -> Vec<String> {
        let pre = Split::new(SplitPattern::Regex(QWEN.into()), SplitDelimiterBehavior::Isolated, false).unwrap();
        let mut s = PreTokenizedString::from(text);
        pre.pre_tokenize(&mut s).unwrap();
        s.get_splits(OffsetReferential::Original, OffsetType::Byte).into_iter().map(|(t, _, _)| t.to_string()).collect()
    }

    #[test]
    fn the_pure_rust_regex_backend_splits_like_qwen3_expects() {
        assert_eq!(
            split("Hello  world's 123 ünïcödé\n\n  x"),
            ["Hello", " ", " world", "'s", " ", "1", "2", "3", " ünïcödé", "\n\n", " ", " x"]
        );
        assert_eq!(split("I'LL go!!\tnow   "), ["I", "'LL", " go", "!!", "\tnow", "   "]);
        assert_eq!(split("x = f(1);\n  return"), ["x", " =", " f", "(", "1", ");\n", " ", " return"]);
        assert_eq!(split("中文 测试"), ["中文", " 测试"]);
        assert_eq!(split("<|im_start|>user"), ["<|", "im", "_start", "|>", "user"]);
    }
}
