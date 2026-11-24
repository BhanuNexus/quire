// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The day-1 spike: how fast and how big candle's
//! quantized Qwen3 and BERT, and the engine's Qwen 3.5, are on this machine. The weights are random,
//! with the real models' shapes (`Qwen3-1.7B` at `Q4_K`, `bge-small-en-v1.5`),
//! which is enough for speed and memory: the arithmetic doesn't depend on
//! the values. Run in release:
//!
//!     cargo run --release -p quire-ai-engine --example spike -- <scratch dir> qwen3 compact
//!     cargo run --release -p quire-ai-engine --example spike -- <scratch dir> qwen35 4b
#![expect(clippy::print_stdout, clippy::print_stderr, reason = "a measurement run by hand: it prints its results")]
#![expect(clippy::disallowed_methods, reason = "a developer spike writes its output file directly")]

use std::time::Instant;

use candle_core::quantized::gguf_file::Value as V;
use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
use candle_core::{DType, Device, Tensor};

/// Peak resident memory so far (`VmHWM`), in MB.
fn rss_mb() -> u64 {
    status_mb("VmHWM:")
}

/// Resident memory now (`VmRSS`), in MB.
fn now_mb() -> u64 {
    status_mb("VmRSS:")
}

fn status_mb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map_or(0, |kb| kb / 1024)
}

fn q(shape: &[usize], dt: GgmlDType, dev: &Device) -> QTensor {
    let t = Tensor::randn(0f32, 0.02, shape, dev).expect("random weights");
    QTensor::quantize(&t, dt).expect("quantize")
}

fn f(shape: &[usize], dev: &Device) -> QTensor {
    let t = Tensor::ones(shape, DType::F32, dev).expect("norm weights");
    QTensor::quantize(&t, GgmlDType::F32).expect("f32")
}

/// Which Qwen3 implementation to measure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Impl {
    /// candle-transformers' `quantized_qwen3` (f32 token table, x86 repacking).
    Candle,
    /// The engine's adapted model with candle's repacking kept.
    Repack,
    /// The engine's adapted model with nothing repacked.
    Compact,
    /// The engine's adapted model as shipped (only `Q8_0` repacked).
    Auto,
}

enum Loaded {
    Candle(candle_transformers::models::quantized_qwen3::ModelWeights),
    Engine(quire_ai_engine::qwen3::Qwen3),
}

impl Loaded {
    fn forward(&mut self, x: &Tensor, pos: usize) -> candle_core::Result<Tensor> {
        match self {
            Self::Candle(m) => m.forward(x, pos),
            Self::Engine(m) => m.forward(x, pos),
        }
    }

    fn clear_kv_cache(&mut self) {
        match self {
            Self::Candle(m) => m.clear_kv_cache(),
            Self::Engine(m) => m.clear_kv_cache(),
        }
    }
}

/// Qwen3 1.7B at `Q4_K_M` (`small` = false) or 0.6B at `Q8_0`.
fn qwen3(dir: &std::path::Path, small: bool, which: Impl) {
    let dev = Device::Cpu;
    let (hidden, inter, layers, heads, kv, head_dim, vocab) = if small {
        (1024usize, 3072usize, 28usize, 16usize, 8usize, 128usize, 151_936usize)
    } else {
        (2048, 6144, 28, 16, 8, 128, 151_936)
    };
    let (q4, q6) = if small { (GgmlDType::Q8_0, GgmlDType::Q8_0) } else { (GgmlDType::Q4K, GgmlDType::Q6K) };
    let name = if small { "qwen3-0.6b q8_0" } else { "qwen3-1.7b q4_k_m" };
    let path = dir.join(if small { "qwen3-0.6b-random-q8.gguf" } else { "qwen3-1.7b-random-q4k.gguf" });
    if !path.exists() {
        let t = Instant::now();
        let mut tensors: Vec<(String, QTensor)> =
            vec![("token_embd.weight".into(), q(&[vocab, hidden], q4, &dev)), ("output_norm.weight".into(), f(&[hidden], &dev))];
        for i in 0..layers {
            let p = format!("blk.{i}");
            tensors.push((format!("{p}.attn_q.weight"), q(&[heads * head_dim, hidden], q4, &dev)));
            tensors.push((format!("{p}.attn_k.weight"), q(&[kv * head_dim, hidden], q4, &dev)));
            // Q4_K_M keeps attn_v and ffn_down at Q6_K.
            tensors.push((format!("{p}.attn_v.weight"), q(&[kv * head_dim, hidden], q6, &dev)));
            tensors.push((format!("{p}.attn_output.weight"), q(&[hidden, heads * head_dim], q4, &dev)));
            tensors.push((format!("{p}.attn_q_norm.weight"), f(&[head_dim], &dev)));
            tensors.push((format!("{p}.attn_k_norm.weight"), f(&[head_dim], &dev)));
            tensors.push((format!("{p}.attn_norm.weight"), f(&[hidden], &dev)));
            tensors.push((format!("{p}.ffn_norm.weight"), f(&[hidden], &dev)));
            tensors.push((format!("{p}.ffn_gate.weight"), q(&[inter, hidden], q4, &dev)));
            tensors.push((format!("{p}.ffn_up.weight"), q(&[inter, hidden], q4, &dev)));
            tensors.push((format!("{p}.ffn_down.weight"), q(&[hidden, inter], q6, &dev)));
        }
        let md: Vec<(&str, V)> = vec![
            ("general.architecture", V::String("qwen3".into())),
            ("qwen3.attention.head_count", V::U32(heads as u32)),
            ("qwen3.attention.head_count_kv", V::U32(kv as u32)),
            ("qwen3.attention.key_length", V::U32(head_dim as u32)),
            ("qwen3.block_count", V::U32(layers as u32)),
            ("qwen3.embedding_length", V::U32(hidden as u32)),
            ("qwen3.context_length", V::U32(8192)),
            ("qwen3.attention.layer_norm_rms_epsilon", V::F32(1e-6)),
            ("qwen3.rope.freq_base", V::F32(1_000_000.0)),
        ];
        let mdr: Vec<(&str, &V)> = md.iter().map(|(k, v)| (*k, v)).collect();
        let tr: Vec<(&str, &QTensor)> = tensors.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let mut out = std::fs::File::create(&path).expect("create gguf");
        gguf_file::write(&mut out, &mdr, &tr).expect("write gguf");
        eprintln!(
            "wrote {} ({} MB) in {:.0}s",
            path.display(),
            std::fs::metadata(&path).map_or(0, |m| m.len() / 1_000_000),
            t.elapsed().as_secs_f64()
        );
    }
    let t = Instant::now();
    let mut file = std::fs::File::open(&path).expect("open gguf");
    let ct = gguf_file::Content::read(&mut file).expect("read gguf");
    let mut model = match which {
        Impl::Candle => {
            Loaded::Candle(candle_transformers::models::quantized_qwen3::ModelWeights::from_gguf(ct, &mut file, &dev).expect("load"))
        }
        Impl::Repack | Impl::Compact | Impl::Auto => {
            use quire_ai_engine::qwen3::Repack;
            let repack = match which {
                Impl::Repack => Repack::All,
                Impl::Compact => Repack::Never,
                _ => Repack::Q8,
            };
            let opts = quire_ai_engine::qwen3::Options { repack, max_ctx: 8192 };
            Loaded::Engine(quire_ai_engine::qwen3::Qwen3::from_gguf(ct, &mut file, &dev, opts).expect("load"))
        }
    };
    let label = match which {
        Impl::Candle => "candle",
        Impl::Repack => "engine, repacked",
        Impl::Compact => "engine, compact",
        Impl::Auto => "engine, as shipped",
    };
    println!("{name} ({label}): load {:.1}s, RSS after load {} MB (peak {} MB)", t.elapsed().as_secs_f64(), now_mb(), rss_mb());
    let threads = candle_core::utils::get_num_threads();
    for prompt_len in [16usize, 256, 1024] {
        model.clear_kv_cache();
        let prompt: Vec<u32> = (0..prompt_len).map(|i| (i * 7919 % 150_000) as u32).collect();
        let t = Instant::now();
        let mut pos = 0;
        let mut logits = None;
        for chunk in prompt.chunks(256) {
            let x = Tensor::new(chunk, &dev).and_then(|x| x.unsqueeze(0)).expect("input");
            logits = Some(model.forward(&x, pos).expect("prefill"));
            pos += chunk.len();
        }
        let prefill = t.elapsed().as_secs_f64();
        println!("  after prefill of {prompt_len}: RSS {} MB", now_mb());
        let mut next =
            logits.and_then(|l| l.squeeze(0).ok()).and_then(|l| l.argmax(0).ok()).and_then(|t| t.to_scalar::<u32>().ok()).unwrap_or(0);
        let n = 24;
        let t = Instant::now();
        for i in 0..n {
            let x = Tensor::new(&[next], &dev).and_then(|x| x.unsqueeze(0)).expect("input");
            let l = model.forward(&x, pos + i).expect("decode");
            next = l.squeeze(0).and_then(|l| l.argmax(0)).and_then(|t| t.to_scalar::<u32>()).unwrap_or(0);
        }
        let decode = t.elapsed().as_secs_f64();
        println!(
            "  prompt {prompt_len}: prefill {:.1} tok/s ({prefill:.1}s), generate {:.2} tok/s ({n} tokens in {decode:.1}s), {threads} threads, RSS {} MB (peak {} MB)",
            prompt_len as f64 / prefill,
            f64::from(n as u32) / decode,
            now_mb(),
            rss_mb()
        );
    }
}

fn bert() {
    use candle_transformers::models::bert::{BertModel, Config};
    let dev = Device::Cpu;
    let cfg: Config = serde_json::from_value(serde_json::json!({
        "vocab_size": 30522, "hidden_size": 384, "num_hidden_layers": 12, "num_attention_heads": 12,
        "intermediate_size": 1536, "hidden_act": "gelu", "hidden_dropout_prob": 0.1, "max_position_embeddings": 512,
        "type_vocab_size": 2, "initializer_range": 0.02, "layer_norm_eps": 1e-12, "pad_token_id": 0, "model_type": "bert"
    }))
    .expect("config");
    let vm = candle_nn::VarMap::new();
    let vb = candle_nn::VarBuilder::from_varmap(&vm, DType::F32, &dev);
    let model = BertModel::load(vb, &cfg).expect("bert");
    let before = rss_mb();
    for (batch, len) in [(64usize, 128usize), (64, 256)] {
        let ids = Tensor::zeros((batch, len), DType::U32, &dev).expect("ids");
        let types = ids.zeros_like().expect("types");
        let mask = Tensor::ones((batch, len), DType::U32, &dev).expect("mask");
        let t = Instant::now();
        let _ = model.forward(&ids, &types, Some(&mask)).expect("forward");
        let s = t.elapsed().as_secs_f64();
        println!(
            "bge-small: {batch} passages × {len} tokens in {s:.2}s = {:.0} passages/s, peak RSS {} MB (model {} MB)",
            batch as f64 / s,
            rss_mb(),
            before
        );
    }
}

/// Whisper at the shapes of `openai/whisper-{tiny,base,small}` with random
/// weights: memory, and how long 30 seconds of audio take (the decoder runs
/// its full 224 tokens, as random weights never end a reply: the worst case).
fn whisper(size: &str) {
    use candle_transformers::models::whisper::{Config, model::Whisper};
    let (d, layers, heads) = match size {
        "tiny" => (384, 4, 6),
        "small" => (768, 12, 12),
        _ => (512, 6, 8),
    };
    let cfg = Config {
        num_mel_bins: 80,
        max_source_positions: 1500,
        d_model: d,
        encoder_attention_heads: heads,
        encoder_layers: layers,
        vocab_size: 51_865,
        max_target_positions: 448,
        decoder_attention_heads: heads,
        decoder_layers: layers,
        suppress_tokens: vec![],
    };
    let dev = Device::Cpu;
    let t = Instant::now();
    let vm = candle_nn::VarMap::new();
    let vb = candle_nn::VarBuilder::from_varmap(&vm, DType::F32, &dev);
    let model = Whisper::load(&vb, cfg).expect("whisper");
    // A word-level tokenizer with Whisper's special tokens at their ids.
    let specials = [
        ("<|endoftext|>", 50_257u32),
        ("<|startoftranscript|>", 50_258),
        ("<|en|>", 50_259),
        ("<|transcribe|>", 50_359),
        ("<|notimestamps|>", 50_363),
    ];
    let mut vocab: std::collections::HashMap<String, u32> = specials.iter().map(|(t, i)| ((*t).to_string(), *i)).collect();
    vocab.insert("<unk>".into(), 0);
    let wl = tokenizers::models::wordlevel::WordLevel::builder()
        .vocab(vocab.into_iter().collect())
        .unk_token("<unk>".into())
        .build()
        .expect("vocab");
    let tok = tokenizers::Tokenizer::new(wl);
    let mut tr = quire_ai_engine::whisper::Transcriber::new(model, tok, dev).expect("transcriber");
    println!("whisper-{size}: load {:.1}s, RSS {} MB", t.elapsed().as_secs_f64(), now_mb());
    let pcm: Vec<f32> = (0..16_000 * 30).map(|i| ((i as f32) * 0.05).sin() * 0.1).collect();
    let t = Instant::now();
    let r = tr.run(&pcm, "en", &std::sync::atomic::AtomicBool::new(false)).expect("run");
    let s = t.elapsed().as_secs_f64();
    println!("  30 s of audio in {s:.1}s (worst case: 224 tokens), {} chars, peak RSS {} MB", r.text.len(), rss_mb());
}

/// `spike <dir> [bert|qwen3|qwen3-small|whisper] [candle|repack|compact|auto|tiny|base|small]`,
/// `spike <dir> qwen35 [4b|9b]` or
/// `spike <dir> qwen35-logits <ids.json> <out.jsonl> [<want.jsonl>]`:
/// one model per run, so peak memory is that model's.
fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let which = match std::env::args().nth(3).as_deref() {
        Some("candle") => Impl::Candle,
        Some("repack") => Impl::Repack,
        Some("compact") => Impl::Compact,
        _ => Impl::Auto,
    };
    match std::env::args().nth(2).as_deref() {
        Some("bert") => bert(),
        Some("qwen3") => qwen3(&dir, false, which),
        Some("qwen3-small") => qwen3(&dir, true, which),
        Some("qwen35") => qwen35::speed(&dir, std::env::args().nth(3).as_deref() == Some("9b")),
        Some("qwen35-logits") => {
            let arg = |n: usize| std::env::args().nth(n).map(std::path::PathBuf::from);
            let (Some(ids), Some(out)) = (arg(3), arg(4)) else {
                eprintln!("usage: spike <model dir> qwen35-logits <ids.json> <out.jsonl> [<want.jsonl>]");
                std::process::exit(2);
            };
            qwen35::logits(&dir, &ids, &out, arg(5).as_deref());
        }
        Some("whisper") => whisper(std::env::args().nth(3).as_deref().unwrap_or("base")),
        _ => {
            bert();
            qwen3(&dir, false, which);
        }
    }
}

/// Qwen 3.5: the speed and memory record with random weights of the real
/// shapes (`docs/design/0.14-local-ai.md` §3.6, written to
/// `docs/formats/model-catalog.md`), and the per-position log-probabilities
/// the reference check compares with transformers
/// (`docs/design/0.14-local-ai.md` §3.3).
mod qwen35 {
    use std::io::Write as _;
    use std::time::Instant;

    use candle_core::quantized::gguf_file::Value as V;
    use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
    use candle_core::{DType, Device, Tensor};
    use quire_ai_engine::qwen3::Options;
    use quire_ai_engine::qwen35::{Hparams, Qwen35};

    use super::{now_mb, q, rss_mb};

    fn filled(shape: &[usize], value: f32) -> QTensor {
        let t = (Tensor::ones(shape, DType::F32, &Device::Cpu).expect("ones") * f64::from(value)).expect("fill");
        QTensor::quantize(&t, GgmlDType::F32).expect("f32")
    }

    /// A GGUF with random weights in the released shapes, quantized the way
    /// a `Q4_K_M` file mixes `Q4_K` and `Q6_K` (the exact mix of a real
    /// file can differ; the reading and writing speed depend on it a little).
    fn make(path: &std::path::Path, hp: &Hparams, tied: bool) {
        let dev = Device::Cpu;
        let (h, vocab) = (hp.hidden, 248_320usize);
        let (q4, q6) = (GgmlDType::Q4K, GgmlDType::Q6K);
        let t = Instant::now();
        let mut ts: Vec<(String, QTensor)> =
            vec![("token_embd.weight".into(), q(&[vocab, h], q4, &dev)), ("output_norm.weight".into(), filled(&[h], 1.0))];
        if !tied {
            ts.push(("output.weight".into(), q(&[vocab, h], q6, &dev)));
        }
        for i in 0..hp.layers {
            let p = |s: &str| format!("blk.{i}.{s}");
            ts.push((p("attn_norm.weight"), filled(&[h], 1.0)));
            ts.push((p("post_attention_norm.weight"), filled(&[h], 1.0)));
            if hp.recurrent[i] {
                let cd = hp.conv_dim();
                ts.push((p("attn_qkv.weight"), q(&[cd, h], q4, &dev)));
                ts.push((p("attn_gate.weight"), q(&[hp.inner, h], q4, &dev)));
                ts.push((p("ssm_beta.weight"), q(&[hp.v_heads, h], q4, &dev)));
                ts.push((p("ssm_alpha.weight"), q(&[hp.v_heads, h], q4, &dev)));
                ts.push((p("ssm_conv1d.weight"), filled(&[cd, hp.conv_kernel], 0.25)));
                ts.push((p("ssm_dt.bias"), filled(&[hp.v_heads], 0.0)));
                ts.push((p("ssm_a"), filled(&[hp.v_heads], -1.0)));
                ts.push((p("ssm_norm.weight"), filled(&[hp.state_size], 1.0)));
                ts.push((p("ssm_out.weight"), q(&[h, hp.inner], q6, &dev)));
            } else {
                let (qd, kv) = (hp.heads * hp.head_dim, hp.kv_heads * hp.head_dim);
                ts.push((p("attn_q.weight"), q(&[2 * qd, h], q4, &dev)));
                ts.push((p("attn_k.weight"), q(&[kv, h], q4, &dev)));
                ts.push((p("attn_v.weight"), q(&[kv, h], q6, &dev)));
                ts.push((p("attn_output.weight"), q(&[h, qd], q4, &dev)));
                ts.push((p("attn_q_norm.weight"), filled(&[hp.head_dim], 1.0)));
                ts.push((p("attn_k_norm.weight"), filled(&[hp.head_dim], 1.0)));
            }
            ts.push((p("ffn_gate.weight"), q(&[hp.ffn, h], q4, &dev)));
            ts.push((p("ffn_up.weight"), q(&[hp.ffn, h], q4, &dev)));
            ts.push((p("ffn_down.weight"), q(&[h, hp.ffn], q6, &dev)));
        }
        let u = |x: usize| V::U32(u32::try_from(x).expect("small"));
        let mut md: Vec<(String, V)> = vec![("general.architecture".into(), V::String("qwen35".into()))];
        for (k, v) in [
            ("block_count", u(hp.layers)),
            ("embedding_length", u(h)),
            ("feed_forward_length", u(hp.ffn)),
            ("attention.head_count", u(hp.heads)),
            ("attention.head_count_kv", u(hp.kv_heads)),
            ("attention.key_length", u(hp.head_dim)),
            ("attention.layer_norm_rms_epsilon", V::F32(hp.eps)),
            ("rope.freq_base", V::F32(hp.rope_theta as f32)),
            ("rope.dimension_count", u(hp.rope_dim)),
            ("ssm.conv_kernel", u(hp.conv_kernel)),
            ("ssm.state_size", u(hp.state_size)),
            ("ssm.group_count", u(hp.k_heads)),
            ("ssm.time_step_rank", u(hp.v_heads)),
            ("ssm.inner_size", u(hp.inner)),
            ("full_attention_interval", u(4)),
        ] {
            md.push((format!("qwen35.{k}"), v));
        }
        let mdr: Vec<(&str, &V)> = md.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let tr: Vec<(&str, &QTensor)> = ts.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let mut out = std::fs::File::create(path).expect("create gguf");
        gguf_file::write(&mut out, &mdr, &tr).expect("write gguf");
        eprintln!(
            "wrote {} ({} MB) in {:.0}s",
            path.display(),
            std::fs::metadata(path).map_or(0, |m| m.len() / 1_000_000),
            t.elapsed().as_secs_f64()
        );
    }

    /// The machine the numbers were taken on.
    fn machine() -> String {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu = info.lines().find_map(|l| l.strip_prefix("model name")).map_or("unknown CPU", |l| l.trim_start_matches([' ', '\t', ':']));
        let flags = info.lines().find_map(|l| l.strip_prefix("flags")).unwrap_or_default();
        let simd: Vec<&str> =
            ["avx2", "avx512f", "avx512_vnni", "avx_vnni"].into_iter().filter(|f| flags.split_whitespace().any(|x| x == *f)).collect();
        let mem = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines().find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()))
            })
            .map_or(0, |kb| kb / 1024);
        let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
        format!("{cpu} ({}), {cores} threads available, {mem} MB memory", simd.join(" "))
    }

    /// `spike <dir> qwen35 [4b|9b]`: load time and memory, then for prompts
    /// of 256, 1,024 and 2,000 tokens (read in chunks of 256, as the engine
    /// does) the reading speed, then the writing speed over 16 tokens, and
    /// the peak memory (`docs/design/0.14-local-ai.md` §2).
    pub(crate) fn speed(dir: &std::path::Path, nine: bool) {
        let (hp, name) = if nine { (Hparams::qwen35_9b(), "qwen3.5-9b") } else { (Hparams::qwen35_4b(), "qwen3.5-4b") };
        let path = dir.join(format!("{name}-random-q4km.gguf"));
        if !path.exists() {
            make(&path, &hp, !nine);
            // Start the peak-memory count again: making the file is not the engine.
            let _ = std::fs::write("/proc/self/clear_refs", "5");
        }
        println!("machine: {}", machine());
        let t = Instant::now();
        let mut file = std::fs::File::open(&path).expect("open gguf");
        let ct = gguf_file::Content::read(&mut file).expect("read gguf");
        let mut model = Qwen35::from_gguf(ct, &mut file, Options::default()).expect("load");
        let threads = candle_core::utils::get_num_threads();
        println!(
            "{name} (random weights, Q4_K/Q6_K): load {:.1}s, RSS after load {} MB (peak {} MB), {threads} threads",
            t.elapsed().as_secs_f64(),
            now_mb(),
            rss_mb()
        );
        for prompt_len in [256usize, 1024, 2000] {
            model.clear();
            model.reserve(prompt_len + 16);
            let prompt: Vec<u32> = (0..prompt_len).map(|i| (i * 7919 % 248_000) as u32).collect();
            let t = Instant::now();
            let mut pos = 0;
            let mut last = Vec::new();
            for chunk in prompt.chunks(256) {
                last = model.logits(chunk, pos, false).expect("prefill");
                pos += chunk.len();
            }
            let prefill = t.elapsed().as_secs_f64();
            let n = 16u32;
            let t = Instant::now();
            for i in 0..n as usize {
                let next = last.iter().enumerate().fold((0, f32::MIN), |b, (j, x)| if *x > b.1 { (j, *x) } else { b }).0;
                last = model.logits(&[u32::try_from(next).unwrap_or(0)], pos + i, false).expect("decode");
            }
            let decode = t.elapsed().as_secs_f64();
            println!(
                "| {name} | {prompt_len} | {:.2} | {:.2} | {} | {} |",
                prompt_len as f64 / prefill,
                f64::from(n) / decode,
                now_mb(),
                rss_mb()
            );
        }
        println!("columns: model | prompt tokens | reading tokens/s | writing tokens/s | RSS MB | peak RSS MB");
    }

    fn log_softmax(row: &[f32]) -> Vec<f64> {
        let mx = row.iter().fold(f32::MIN, |a, b| a.max(*b));
        let lse = f64::from(mx) + row.iter().map(|x| f64::from(x - mx).exp()).sum::<f64>().ln();
        row.iter().map(|x| f64::from(*x) - lse).collect()
    }

    /// `spike <model dir> qwen35-logits <ids.json> <out.jsonl> [<want.jsonl>]`:
    /// reads the token ids (a JSON array) through `<model dir>/model.gguf`
    /// in chunks of 256 and writes, per position, the 40 most likely next
    /// tokens with their log-probabilities and, when `want.jsonl` gives a
    /// JSON array of ids for that position, their log-probabilities too.
    pub(crate) fn logits(dir: &std::path::Path, ids: &std::path::Path, out: &std::path::Path, want: Option<&std::path::Path>) {
        let ids: Vec<u32> = serde_json::from_str(&std::fs::read_to_string(ids).expect("read ids")).expect("ids: a JSON array");
        let want: Vec<Vec<usize>> = want.map_or_else(Vec::new, |w| {
            std::fs::read_to_string(w)
                .expect("read want")
                .lines()
                .map(|l| serde_json::from_str(l).expect("want: a JSON array per line"))
                .collect()
        });
        let path = dir.join("model.gguf");
        let mut file = std::fs::File::open(&path).expect("open model.gguf");
        let ct = gguf_file::Content::read(&mut file).expect("read gguf");
        let max_ctx = ids.len().max(1);
        let mut model = Qwen35::from_gguf(ct, &mut file, Options { max_ctx, ..Options::default() }).expect("load");
        let mut w = std::io::BufWriter::new(std::fs::File::create(out).expect("create output"));
        let mut pos = 0;
        let t = Instant::now();
        for chunk in ids.chunks(256) {
            let all = model.logits(chunk, pos, true).expect("forward");
            let vocab = all.len() / chunk.len();
            for row in all.chunks_exact(vocab) {
                let lp = log_softmax(row);
                let mut order: Vec<usize> = (0..vocab).collect();
                order.select_nth_unstable_by(39.min(vocab - 1), |a, b| lp[*b].total_cmp(&lp[*a]));
                order.truncate(40);
                order.sort_by(|a, b| lp[*b].total_cmp(&lp[*a]));
                let top: Vec<(usize, f64)> = order.iter().map(|i| (*i, lp[*i])).collect();
                let wanted: Vec<f64> = want
                    .get(pos)
                    .map_or_else(Vec::new, |ids| ids.iter().map(|i| lp.get(*i).copied().unwrap_or(f64::NEG_INFINITY)).collect());
                writeln!(w, "{}", serde_json::json!({ "pos": pos, "top": top, "want": wanted })).expect("write");
                pos += 1;
            }
            eprintln!("{pos} of {} positions, {:.1} tokens/s", ids.len(), pos as f64 / t.elapsed().as_secs_f64());
        }
        w.flush().expect("flush");
    }
}
