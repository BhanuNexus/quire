// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-FileCopyrightText: 2024 The HuggingFace Inc. team (candle)
// SPDX-License-Identifier: AGPL-3.0-or-later AND (MIT OR Apache-2.0)
//! Qwen3 from GGUF, adapted from candle-transformers 0.11.1
//! (`models/quantized_qwen3.rs`, MIT or Apache-2.0) to use less memory.
//!
//! Two changes from candle's model (measured in docs/formats/model-catalog.md):
//!
//! * **The token-embedding table stays quantized.** Candle dequantizes it to
//!   f32 (1.2 GB for Qwen3's 152k × 2048 table); here a lookup dequantizes
//!   only the rows it needs. Qwen3 ties the output projection to the same
//!   table, so the one quantized copy serves both.
//! * **Weights stay in their GGUF blocks on x86** ([`MatMul::Compact`]).
//!   Candle repacks `Q4_K` and `Q8_0` weights into wider tiles the first time
//!   each is used (on CPUs with AVX2 or AVX-512), a second copy larger than
//!   the first. The compact path uses candle's own block kernels (the
//!   ones it already uses for `Q6_K` when decoding). [`Options::repack`]
//!   chooses which weights candle may still repack ([`Repack::Q8`] by
//!   default: the Small model's `Q8_0`, where tiles cost no extra size
//!   beyond the copy and are much faster).
//!
//! The KV cache is kept once (candle keeps a second copy for prefill), and
//! the rotary tables only cover the context the engine allows.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use candle_core::quantized::{GgmlDType, QMatMul, QTensor, QuantizedType, gguf_file};
use candle_core::{CpuStorage, DType, Device, Module, Result, Tensor, bail};
use candle_nn::Activation;
use candle_nn::attention::cpu_flash::causal::causal_decode_f32_interleaved;
use candle_nn::attention::{AttnMask, flash_attn};
use candle_nn::kv_cache::{ConcatKvCache, RawInterleavedKvCache};
use candle_transformers::quantized_nn::RmsNorm;
use candle_transformers::utils::repeat_kv;

/// Which weights candle may repack into its x86 tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repack {
    /// None: the least memory.
    Never,
    /// `Q8_0` only (the Small model): its tiles are the size of the blocks,
    /// and on x86 they make prompts 2–4× and replies about 1.8× faster
    /// (measured, docs/formats/model-catalog.md). K-quant tiles are larger
    /// than their blocks and were no faster at writing replies.
    #[default]
    Q8,
    /// Every weight candle can repack (faster prompts, about twice the memory
    /// for `Q4_K`).
    All,
}

/// How to hold the weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub repack: Repack,
    /// The longest context the caller will use (bounds the rotary tables).
    pub max_ctx: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { repack: Repack::default(), max_ctx: 8192 }
    }
}

/// A quantized weight `(n, k)`: candle's `QMatMul` (with its repacking), or
/// the GGUF blocks multiplied by candle's block kernels.
#[derive(Clone)]
enum MatMul {
    Candle(QMatMul),
    Compact { w: Arc<dyn QuantizedType>, n: usize, k: usize },
}

impl MatMul {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        match self {
            Self::Candle(m) => m.forward(x),
            Self::Compact { w, n, k } => {
                let mut dims = x.dims().to_vec();
                if dims.last() != Some(k) {
                    bail!("matmul: input {dims:?} doesn't end in {k}");
                }
                let m = x.elem_count() / k;
                let lhs = x.to_dtype(DType::F32)?.flatten_all()?.to_vec1::<f32>()?;
                let mut dst = vec![0f32; m * n];
                w.matmul_t((m, *k, *n), &lhs, &mut dst)?;
                if let Some(last) = dims.last_mut() {
                    *last = *n;
                }
                Tensor::from_vec(dst, dims, x.device())?.to_dtype(x.dtype())
            }
        }
    }
}

/// The token table: quantized blocks, looked up a row at a time.
#[derive(Clone)]
enum Embed {
    Candle(Arc<QTensor>),
    Compact { w: Arc<dyn QuantizedType>, rows: usize, hidden: usize },
}

impl Embed {
    fn forward(&self, ids: &Tensor) -> Result<Tensor> {
        match self {
            Self::Candle(t) => t.embedding(ids),
            Self::Compact { w, rows, hidden } => {
                let mut shape = ids.dims().to_vec();
                shape.push(*hidden);
                let ids = ids.flatten_all()?.to_dtype(DType::U32)?.to_vec1::<u32>()?;
                let CpuStorage::F32(rows_f32) = w.embedding(&ids, *rows, *hidden)? else {
                    bail!("embedding: expected f32 rows");
                };
                Tensor::from_vec(rows_f32, shape, &Device::Cpu)
            }
        }
    }
}

/// Reads tensors from the GGUF file.
struct Gguf<'a, R: Read + Seek> {
    ct: gguf_file::Content,
    reader: &'a mut R,
    device: Device,
    opts: Options,
}

impl<R: Read + Seek> Gguf<'_, R> {
    fn md_u32(&self, key: &str) -> Result<usize> {
        match self.ct.metadata.get(key) {
            Some(v) => Ok(v.to_u32()? as usize),
            None => bail!("cannot find {key} in metadata"),
        }
    }

    fn md_f32(&self, key: &str) -> Result<f64> {
        match self.ct.metadata.get(key) {
            Some(v) => Ok(f64::from(v.to_f32()?)),
            None => bail!("cannot find {key} in metadata"),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.ct.tensor_infos.contains_key(name)
    }

    fn qtensor(&mut self, name: &str) -> Result<QTensor> {
        self.ct.tensor(self.reader, name, &self.device)
    }

    /// The tensor's GGUF blocks, read straight from the file (no `QTensor`
    /// in between, so there is one copy).
    fn blocks(&mut self, name: &str) -> Result<(Arc<dyn QuantizedType>, usize, usize)> {
        let Some(info) = self.ct.tensor_infos.get(name) else { bail!("cannot find tensor {name}") };
        let (n, k) = info.shape.dims2()?;
        let dtype = info.ggml_dtype;
        let bytes = n * k / dtype.block_size() * dtype.type_size();
        self.reader.seek(SeekFrom::Start(self.ct.tensor_data_offset + info.offset))?;
        let mut raw = vec![0u8; bytes];
        self.reader.read_exact(&mut raw)?;
        let w: Box<dyn QuantizedType> = dtype.from_data(std::borrow::Cow::Owned(raw));
        Ok((Arc::from(w), n, k))
    }

    /// Whether a weight stays in compact blocks here (the CPU, a quantized
    /// type, and not one [`Options::repack`] gives to candle).
    fn compact(&self, name: &str) -> bool {
        let Some(dtype) = self.ct.tensor_infos.get(name).map(|i| i.ggml_dtype) else { return false };
        let repacked = match self.opts.repack {
            Repack::Never => false,
            Repack::Q8 => dtype == GgmlDType::Q8_0,
            Repack::All => true,
        };
        self.device.is_cpu() && !repacked && !matches!(dtype, GgmlDType::F32 | GgmlDType::F16 | GgmlDType::BF16)
    }

    fn matmul(&mut self, name: &str) -> Result<MatMul> {
        if self.compact(name) {
            let (w, n, k) = self.blocks(name)?;
            return Ok(MatMul::Compact { w, n, k });
        }
        Ok(MatMul::Candle(QMatMul::from_arc(Arc::new(self.qtensor(name)?))?))
    }

    fn rms_norm(&mut self, name: &str, eps: f64) -> Result<RmsNorm> {
        RmsNorm::from_qtensor(self.qtensor(name)?, eps)
    }

    /// The token table and, when the model ties them, the output projection
    /// sharing its blocks.
    fn embed_and_head(&mut self) -> Result<(Embed, MatMul)> {
        let tied = !self.has("output.weight");
        if self.compact("token_embd.weight") {
            let (w, rows, hidden) = self.blocks("token_embd.weight")?;
            let head = if tied { MatMul::Compact { w: Arc::clone(&w), n: rows, k: hidden } } else { self.matmul("output.weight")? };
            return Ok((Embed::Compact { w, rows, hidden }, head));
        }
        let t = Arc::new(self.qtensor("token_embd.weight")?);
        let head = if tied { MatMul::Candle(QMatMul::from_arc(Arc::clone(&t))?) } else { self.matmul("output.weight")? };
        Ok((Embed::Candle(t), head))
    }
}

#[derive(Clone)]
struct Mlp {
    gate: MatMul,
    up: MatMul,
    down: MatMul,
}

impl Mlp {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let gate = self.gate.forward(x)?.apply(&Activation::Silu)?;
        let up = self.up.forward(x)?;
        self.down.forward(&(gate * up)?)
    }
}

struct Rotary {
    sin: Tensor,
    cos: Tensor,
}

impl Rotary {
    fn new(head_dim: usize, max_pos: usize, theta: f64, dev: &Device) -> Result<Self> {
        let inv: Vec<f32> = (0..head_dim).step_by(2).map(|i| 1f32 / theta.powf(i as f64 / head_dim as f64) as f32).collect();
        let n = inv.len();
        let inv = Tensor::from_vec(inv, (1, n), dev)?;
        let t = Tensor::arange(0u32, u32::try_from(max_pos).unwrap_or(u32::MAX), dev)?.to_dtype(DType::F32)?.reshape((max_pos, 1))?;
        let freqs = t.matmul(&inv)?;
        Ok(Self { sin: freqs.sin()?, cos: freqs.cos()? })
    }

    fn apply(&self, q: &Tensor, k: &Tensor, offset: usize) -> Result<(Tensor, Tensor)> {
        let (_, _, len, _) = q.dims4()?;
        let cos = self.cos.narrow(0, offset, len)?.to_dtype(q.dtype())?;
        let sin = self.sin.narrow(0, offset, len)?.to_dtype(q.dtype())?;
        let q = candle_nn::rotary_emb::rope(&q.contiguous()?, &cos, &sin)?;
        let k = candle_nn::rotary_emb::rope(&k.contiguous()?, &cos, &sin)?;
        Ok((q, k))
    }
}

/// The KV cache: one interleaved buffer on the CPU, candle's concatenating
/// cache elsewhere.
enum Kv {
    Cpu(RawInterleavedKvCache),
    Other(ConcatKvCache),
}

struct Attention {
    q: MatMul,
    k: MatMul,
    v: MatMul,
    o: MatMul,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
    rotary: Arc<Rotary>,
    kv: Kv,
}

impl Attention {
    fn forward(&mut self, x: &Tensor, mask: Option<&Tensor>, offset: usize) -> Result<Tensor> {
        let (b, l, _) = x.dims3()?;
        let hidden = self.heads * self.head_dim;
        let q = self.q.forward(x)?.reshape((b, l, self.heads, self.head_dim))?.transpose(1, 2)?;
        let k = self.k.forward(x)?.reshape((b, l, self.kv_heads, self.head_dim))?.transpose(1, 2)?;
        let v = self.v.forward(x)?.reshape((b, l, self.kv_heads, self.head_dim))?.transpose(1, 2)?;
        let q = self.q_norm.forward(&q.flatten(0, 2)?)?.reshape((b, self.heads, l, self.head_dim))?;
        let k = self.k_norm.forward(&k.flatten(0, 2)?)?.reshape((b, self.kv_heads, l, self.head_dim))?;
        let (q, k) = self.rotary.apply(&q, &k, offset)?;
        match &mut self.kv {
            Kv::Cpu(cache) if b == 1 && q.dtype() == DType::F32 => {
                let scale = 1.0 / (self.head_dim as f32).sqrt();
                // (1, H, L, D) → rows of (L, H·D).
                let rows =
                    |t: &Tensor| -> Result<Vec<f32>> { t.squeeze(0)?.transpose(0, 1)?.contiguous()?.flatten_all()?.to_vec1::<f32>() };
                cache.write_kv_batch(&rows(&k)?, &rows(&v)?, l);
                let len = cache.len();
                let ctx = if l == 1 {
                    let q = q.flatten_all()?.to_vec1::<f32>()?;
                    causal_decode_f32_interleaved(&q, cache.data(), self.heads, self.kv_heads, self.head_dim, len, scale)?
                } else {
                    // Prefill reads the same buffer: (S, H_kv, 2·D), K then V per head.
                    let kv = Tensor::from_slice(cache.data(), (len, self.kv_heads, 2 * self.head_dim), x.device())?;
                    let k = kv.narrow(2, 0, self.head_dim)?.unsqueeze(0)?.contiguous()?;
                    let v = kv.narrow(2, self.head_dim, self.head_dim)?.unsqueeze(0)?.contiguous()?;
                    let q = q.transpose(1, 2)?.contiguous()?;
                    flash_attn::<f32>(&q, &k, &v, scale, AttnMask::causal_with_offset(offset), None, None)?.transpose(1, 2)?
                };
                self.o.forward(&ctx.reshape((b, l, hidden))?)
            }
            Kv::Cpu(_) => bail!("the CPU path takes one sequence of f32"),
            Kv::Other(cache) => {
                let (k, v) = cache.append(&k, &v)?;
                let groups = self.heads / self.kv_heads;
                let k = repeat_kv(k, groups)?.contiguous()?;
                let v = repeat_kv(v, groups)?.contiguous()?;
                let mut scores = (q.matmul(&k.transpose(2, 3)?)? * (1.0 / (self.head_dim as f64).sqrt()))?;
                if let Some(m) = mask {
                    scores = scores.broadcast_add(&m.to_dtype(scores.dtype())?)?;
                }
                let probs = candle_nn::ops::softmax_last_dim(&scores)?;
                let ctx = probs.matmul(&v)?.transpose(1, 2)?.reshape((b, l, hidden))?;
                self.o.forward(&ctx)
            }
        }
    }

    fn clear(&mut self) {
        match &mut self.kv {
            Kv::Cpu(c) => c.reset(),
            Kv::Other(c) => c.reset(),
        }
    }
}

struct Layer {
    attn: Attention,
    mlp: Mlp,
    ln1: RmsNorm,
    ln2: RmsNorm,
}

/// Qwen3 weights and caches.
pub struct Qwen3 {
    embed: Embed,
    layers: Vec<Layer>,
    norm: RmsNorm,
    head: MatMul,
    device: Device,
}

impl Qwen3 {
    pub fn from_gguf<R: Read + Seek>(ct: gguf_file::Content, reader: &mut R, device: &Device, opts: Options) -> Result<Self> {
        let mut gg = Gguf { ct, reader, device: device.clone(), opts };
        let heads = gg.md_u32("qwen3.attention.head_count")?;
        let kv_heads = gg.md_u32("qwen3.attention.head_count_kv")?;
        let head_dim = gg.md_u32("qwen3.attention.key_length")?;
        let n_layers = gg.md_u32("qwen3.block_count")?;
        let max_pos = gg.md_u32("qwen3.context_length")?.min(opts.max_ctx.max(1));
        let eps = gg.md_f32("qwen3.attention.layer_norm_rms_epsilon")?;
        let theta = gg.md_f32("qwen3.rope.freq_base")?;
        let (embed, head) = gg.embed_and_head()?;
        let rotary = Arc::new(Rotary::new(head_dim, max_pos, theta, device)?);
        let mut layers = Vec::with_capacity(n_layers);
        for i in 0..n_layers {
            let p = format!("blk.{i}");
            let kv = if device.is_cpu() {
                // Room for 512 positions to start with; it doubles as needed.
                Kv::Cpu(RawInterleavedKvCache::new(kv_heads, head_dim, 512))
            } else {
                Kv::Other(ConcatKvCache::new(2))
            };
            layers.push(Layer {
                ln1: gg.rms_norm(&format!("{p}.attn_norm.weight"), eps)?,
                ln2: gg.rms_norm(&format!("{p}.ffn_norm.weight"), eps)?,
                attn: Attention {
                    q: gg.matmul(&format!("{p}.attn_q.weight"))?,
                    k: gg.matmul(&format!("{p}.attn_k.weight"))?,
                    v: gg.matmul(&format!("{p}.attn_v.weight"))?,
                    o: gg.matmul(&format!("{p}.attn_output.weight"))?,
                    q_norm: gg.rms_norm(&format!("{p}.attn_q_norm.weight"), eps)?,
                    k_norm: gg.rms_norm(&format!("{p}.attn_k_norm.weight"), eps)?,
                    heads,
                    kv_heads,
                    head_dim,
                    rotary: Arc::clone(&rotary),
                    kv,
                },
                mlp: Mlp {
                    gate: gg.matmul(&format!("{p}.ffn_gate.weight"))?,
                    up: gg.matmul(&format!("{p}.ffn_up.weight"))?,
                    down: gg.matmul(&format!("{p}.ffn_down.weight"))?,
                },
            });
        }
        let norm = gg.rms_norm("output_norm.weight", eps)?;
        Ok(Self { embed, layers, norm, head, device: device.clone() })
    }

    /// Logits for the last position of `input` (`(1, L)` token ids) placed at `offset`.
    pub fn forward(&mut self, input: &Tensor, offset: usize) -> Result<Tensor> {
        let (_, l) = input.dims2()?;
        let mut h = self.embed.forward(input)?.to_device(&self.device)?;
        let mask = if l == 1 || self.device.is_cpu() {
            None
        } else {
            Some(candle_transformers::utils::build_additive_causal_mask(l, offset, None, &self.device, DType::F32)?)
        };
        for layer in &mut self.layers {
            let x = layer.ln1.forward(&h)?;
            let x = layer.attn.forward(&x, mask.as_ref(), offset)?;
            let h1 = (h + x)?;
            let x = layer.ln2.forward(&h1)?;
            h = (&h1 + layer.mlp.forward(&x)?)?;
        }
        let h = self.norm.forward(&h)?;
        self.head.forward(&h.narrow(1, l - 1, 1)?)?.squeeze(1)
    }

    pub fn clear_kv_cache(&mut self) {
        for l in &mut self.layers {
            l.attn.clear();
        }
    }
}
