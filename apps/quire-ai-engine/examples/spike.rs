// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-D's day-1 spike (design §6.2): how fast and how big candle's
//! quantized Qwen3 and BERT are on this machine. The weights are random,
//! with the real models' shapes (`Qwen3-1.7B` at `Q4_K`, `bge-small-en-v1.5`),
//! which is enough for speed and memory: the arithmetic doesn't depend on
//! the values. Run in release:
//!
//!     cargo run --release -p quire-ai-engine --example spike -- <scratch dir>

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

/// Qwen3 1.7B at `Q4_K_M` (`small` = false) or 0.6B at `Q8_0`.
fn qwen3(dir: &std::path::Path, small: bool) {
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
    let mut model = candle_transformers::models::quantized_qwen3::ModelWeights::from_gguf(ct, &mut file, &dev).expect("load");
    println!("{name}: load {:.1}s, RSS after load {} MB (peak {} MB)", t.elapsed().as_secs_f64(), now_mb(), rss_mb());
    let threads = candle_core::utils::get_num_threads();
    for prompt_len in [16usize, 256] {
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

/// `spike <dir> [bert|qwen3]`: one model per run, so peak memory is that model's.
fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    match std::env::args().nth(2).as_deref() {
        Some("bert") => bert(),
        Some("qwen3") => qwen3(&dir, false),
        Some("qwen3-small") => qwen3(&dir, true),
        _ => {
            bert();
            qwen3(&dir, false);
        }
    }
}
