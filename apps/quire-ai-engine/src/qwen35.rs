// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Qwen 3.5 (4B and 9B, text only) from GGUF.
//!
//! See `docs/design/0.14-local-ai.md` §3.1. candle 0.11 has no Qwen 3.5, so
//! the model is written here: three
//! of every four layers are Gated DeltaNet ([`crate::deltanet`]), the
//! fourth is gated attention with a partial rotary step.
//!
//! The weights are held as [`crate::qwen3`] holds Qwen3's: quantized blocks
//! multiplied by candle's block kernels, and the token table quantized with
//! only the rows looked up dequantized (the 248,320-row table in f32 would
//! cost 2.5 GB for 4B). Everything runs on the CPU, on every platform.
//!
//! Tensor and key names are the ones llama.cpp's converter writes
//! (`gguf-py/gguf/constants.py`, `src/models/qwen35.cpp`). Both forms of
//! the DeltaNet input projections load: separate `attn_qkv` + `attn_gate`
//! and `ssm_beta` + `ssm_alpha`, or the merged `attn_qkv_gate` and
//! `ssm_beta_alpha`, whose output is the separate outputs side by side.
//! The multi-token-prediction block (`blk.<n_layer>.nextn.*`) is never read.

use std::collections::HashMap;
use std::io::{Read, Seek};

use candle_core::quantized::gguf_file::{self, Value};
use candle_core::{Device, Result, Tensor, bail};
use candle_nn::attention::cpu_flash::causal::causal_decode_f32_interleaved;
use candle_nn::attention::{AttnMask, flash_attn};

use crate::deltanet::{
    DeltaShape, PartialRotary, causal_conv_silu, gated_delta_rule, gated_rms_norm, l2_norm_rows, rms_norm_rows, sigmoid, silu, softplus,
};
use crate::qwen3::{Embed, Gguf, MatMul, Options};

/// `general.architecture` of a Qwen 3.5 GGUF.
pub const ARCH: &str = "qwen35";

/// The shape of a Qwen 3.5 model, read from the GGUF's metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Hparams {
    /// Layers run in the main pass (`block_count` less any
    /// `nextn_predict_layers`).
    pub layers: usize,
    pub hidden: usize,
    pub ffn: usize,
    /// Attention layers: query heads, key/value heads, values per head.
    pub heads: usize,
    pub kv_heads: usize,
    pub head_dim: usize,
    pub eps: f32,
    pub rope_theta: f64,
    /// How many of each head's values the rotary step turns.
    pub rope_dim: usize,
    /// DeltaNet layers: conv kernel, values per head, key heads, value heads,
    /// all value heads together.
    pub conv_kernel: usize,
    pub state_size: usize,
    pub k_heads: usize,
    pub v_heads: usize,
    pub inner: usize,
    /// Which of the `layers` are DeltaNet (the rest are attention).
    pub recurrent: Vec<bool>,
}

fn key(arch: &str, k: &str) -> String {
    format!("{arch}.{k}")
}

fn get<'a>(md: &'a HashMap<String, Value>, k: &str) -> Result<&'a Value> {
    match md.get(k) {
        Some(v) => Ok(v),
        None => bail!("the model file has no {k}"),
    }
}

fn count(md: &HashMap<String, Value>, k: &str) -> Result<usize> {
    usize::try_from(get(md, k)?.to_u64()?).or_else(|_| bail!("{k} is too large"))
}

fn float(md: &HashMap<String, Value>, k: &str) -> Result<f64> {
    match get(md, k)? {
        Value::F32(v) => Ok(f64::from(*v)),
        Value::F64(v) => Ok(*v),
        v => bail!("{k} is not a number: {v:?}"),
    }
}

impl Hparams {
    /// Reads and checks the shape; a missing key or an inconsistent shape
    /// is an error naming the key.
    pub fn from_metadata(md: &HashMap<String, Value>) -> Result<Self> {
        let arch = get(md, "general.architecture")?.to_string()?;
        if arch != ARCH {
            bail!("not a Qwen 3.5 model: general.architecture is {arch}");
        }
        let k = |s: &str| key(ARCH, s);
        let all = count(md, &k("block_count"))?;
        let nextn = if md.contains_key(&k("nextn_predict_layers")) { count(md, &k("nextn_predict_layers"))? } else { 0 };
        let Some(layers) = all.checked_sub(nextn).filter(|n| *n > 0) else {
            bail!("block_count {all} leaves no layers after {nextn} prediction layers");
        };
        let recurrent: Vec<bool> = if let Some(v) = md.get(&k("attention.recurrent_layers")) {
            let flags = v.to_vec()?.iter().map(|f| f.to_u64().map(|x| x != 0)).collect::<Result<Vec<_>>>()?;
            if flags.len() < layers {
                bail!("attention.recurrent_layers has {} entries for {layers} layers", flags.len());
            }
            flags[..layers].to_vec()
        } else {
            let every = count(md, &k("full_attention_interval"))?;
            if every == 0 {
                bail!("full_attention_interval is 0");
            }
            (0..layers).map(|i| (i + 1) % every != 0).collect()
        };
        let hp = Self {
            layers,
            hidden: count(md, &k("embedding_length"))?,
            ffn: count(md, &k("feed_forward_length"))?,
            heads: count(md, &k("attention.head_count"))?,
            kv_heads: count(md, &k("attention.head_count_kv"))?,
            head_dim: count(md, &k("attention.key_length"))?,
            eps: float(md, &k("attention.layer_norm_rms_epsilon"))? as f32,
            rope_theta: float(md, &k("rope.freq_base"))?,
            rope_dim: count(md, &k("rope.dimension_count"))?,
            conv_kernel: count(md, &k("ssm.conv_kernel"))?,
            state_size: count(md, &k("ssm.state_size"))?,
            k_heads: count(md, &k("ssm.group_count"))?,
            v_heads: count(md, &k("ssm.time_step_rank"))?,
            inner: count(md, &k("ssm.inner_size"))?,
            recurrent,
        };
        if md.contains_key(&k("attention.value_length")) && count(md, &k("attention.value_length"))? != hp.head_dim {
            bail!("attention.value_length differs from attention.key_length");
        }
        hp.check()?;
        Ok(hp)
    }

    fn check(&self) -> Result<()> {
        let nonzero = [
            self.hidden,
            self.ffn,
            self.heads,
            self.kv_heads,
            self.head_dim,
            self.conv_kernel,
            self.state_size,
            self.k_heads,
            self.v_heads,
        ];
        if nonzero.contains(&0) {
            bail!("a size in the model file is 0: {self:?}");
        }
        if !self.heads.is_multiple_of(self.kv_heads) {
            bail!("{} attention heads don't share {} key/value heads evenly", self.heads, self.kv_heads);
        }
        if !self.v_heads.is_multiple_of(self.k_heads) {
            bail!("{} value heads don't share {} key heads evenly", self.v_heads, self.k_heads);
        }
        if self.inner != self.v_heads * self.state_size {
            bail!("ssm.inner_size {} isn't {} value heads × {}", self.inner, self.v_heads, self.state_size);
        }
        if !self.rope_dim.is_multiple_of(2) || self.rope_dim > self.head_dim {
            bail!("rope.dimension_count {} doesn't fit a head of {}", self.rope_dim, self.head_dim);
        }
        Ok(())
    }

    /// Qwen 3.5 4B as its `config.json` describes it (the shapes for random
    /// weights and the memory sums; a real file is always read instead).
    pub fn qwen35_4b() -> Self {
        Self::released(2560, 9216)
    }

    /// Qwen 3.5 9B, likewise.
    pub fn qwen35_9b() -> Self {
        Self::released(4096, 12_288)
    }

    fn released(hidden: usize, ffn: usize) -> Self {
        Self {
            layers: 32,
            hidden,
            ffn,
            heads: 16,
            kv_heads: 4,
            head_dim: 256,
            eps: 1e-6,
            rope_theta: 10_000_000.0,
            rope_dim: 64,
            conv_kernel: 4,
            state_size: 128,
            k_heads: 16,
            v_heads: 32,
            inner: 4096,
            recurrent: (0..32).map(|i| (i + 1) % 4 != 0).collect(),
        }
    }

    pub fn n_recurrent(&self) -> usize {
        self.recurrent.iter().filter(|r| **r).count()
    }

    pub fn n_attention(&self) -> usize {
        self.layers - self.n_recurrent()
    }

    /// The channels of the DeltaNet convolution: queries, keys and values.
    pub const fn conv_dim(&self) -> usize {
        2 * self.k_heads * self.state_size + self.inner
    }

    pub const fn delta_shape(&self) -> DeltaShape {
        DeltaShape { k_heads: self.k_heads, v_heads: self.v_heads, dim: self.state_size }
    }

    /// Bytes of attention cache per token of context (f32 keys and values).
    pub fn kv_bytes_per_token(&self) -> usize {
        self.n_attention() * 2 * self.kv_heads * self.head_dim * 4
    }

    /// Bytes of DeltaNet state, whatever the context length.
    pub fn recurrent_state_bytes(&self) -> usize {
        self.n_recurrent() * self.delta_shape().state_len() * 4
    }

    /// Bytes of the convolutions' carried inputs.
    pub fn conv_tail_bytes(&self) -> usize {
        self.n_recurrent() * (self.conv_kernel - 1) * self.conv_dim() * 4
    }

    /// Bytes of the caches at `tokens` of context: attention cache, DeltaNet
    /// state and conv tails, plus one more state and tails for the
    /// prompt-reuse snapshot when `snapshot` is set.
    pub fn cache_bytes(&self, tokens: usize, snapshot: bool) -> usize {
        let fixed = self.recurrent_state_bytes() + self.conv_tail_bytes();
        tokens * self.kv_bytes_per_token() + fixed * if snapshot { 2 } else { 1 }
    }
}

/// A projection whose output is one or more parts side by side: separate
/// weights, or one merged weight whose output rows are the parts in order.
enum Proj {
    Split(Vec<MatMul>),
    Merged(MatMul, Vec<usize>),
}

impl Proj {
    fn forward(&self, x: &[f32], rows: usize, k: usize) -> Result<Vec<Vec<f32>>> {
        match self {
            Self::Split(ms) => ms.iter().map(|m| mm(m, x, rows, k)).collect(),
            Self::Merged(m, widths) => {
                let all = mm(m, x, rows, k)?;
                let n: usize = widths.iter().sum();
                let mut parts: Vec<Vec<f32>> = widths.iter().map(|w| Vec::with_capacity(rows * w)).collect();
                for row in all.chunks_exact(n) {
                    let mut at = 0;
                    for (p, w) in parts.iter_mut().zip(widths) {
                        p.extend_from_slice(&row[at..at + w]);
                        at += w;
                    }
                }
                Ok(parts)
            }
        }
    }
}

/// `x` (`rows × k`) times the weight, as rows of its output width.
fn mm(m: &MatMul, x: &[f32], rows: usize, k: usize) -> Result<Vec<f32>> {
    let t = Tensor::from_slice(x, (1, rows, k), &Device::Cpu)?;
    m.forward(&t)?.flatten_all()?.to_vec1::<f32>()
}

struct Mlp {
    gate: MatMul,
    up: MatMul,
    down: MatMul,
}

struct DeltaLayer {
    /// `[q k v]` (conv channels) and `z` (the output gate).
    qkvz: Proj,
    /// `b` (for β) and `a` (for the decay), one per value head.
    beta_alpha: Proj,
    conv: Vec<f32>,
    dt_bias: Vec<f32>,
    /// `-exp(A_log)`, as the converter stores it.
    a: Vec<f32>,
    norm: Vec<f32>,
    out: MatMul,
    state: Vec<f32>,
    tail: Vec<f32>,
}

/// The attention cache: per position, per key/value head, the key then the
/// value (the layout candle's `causal_decode_f32_interleaved` reads).
struct KvBuf {
    data: Vec<f32>,
    len: usize,
    stride: usize,
}

impl KvBuf {
    fn push(&mut self, k: &[f32], v: &[f32], heads: usize, d: usize) {
        let l = k.len() / (heads * d);
        self.data.truncate(self.len * self.stride);
        for t in 0..l {
            for h in 0..heads {
                self.data.extend_from_slice(&k[(t * heads + h) * d..][..d]);
                self.data.extend_from_slice(&v[(t * heads + h) * d..][..d]);
            }
        }
        self.len += l;
    }

    fn view(&self) -> &[f32] {
        &self.data[..self.len * self.stride]
    }
}

struct AttnLayer {
    /// The query with its gate (per head: `head_dim` query values, then
    /// `head_dim` gate values), the keys and the values.
    qkv: Proj,
    q_norm: Vec<f32>,
    k_norm: Vec<f32>,
    out: MatMul,
    kv: KvBuf,
}

enum Mixer {
    Delta(Box<DeltaLayer>),
    Attn(Box<AttnLayer>),
}

struct Layer {
    attn_norm: Vec<f32>,
    post_norm: Vec<f32>,
    mixer: Mixer,
    mlp: Mlp,
}

/// The DeltaNet states and the attention cache length after some tokens:
/// what the prompt-reuse snapshot keeps (`docs/design/0.14-local-ai.md` §3.2).
#[derive(Clone)]
pub struct State {
    /// Per DeltaNet layer: its state and its conv tail.
    recurrent: Vec<(Vec<f32>, Vec<f32>)>,
    pos: usize,
    /// Which run of the caches this belongs to (a clear starts a new one).
    epoch: u64,
}

impl State {
    /// How many tokens the state has read.
    pub const fn len(&self) -> usize {
        self.pos
    }

    pub const fn is_empty(&self) -> bool {
        self.pos == 0
    }
}

/// Qwen 3.5 weights and caches.
pub struct Qwen35 {
    hp: Hparams,
    embed: Embed,
    head: MatMul,
    out_norm: Vec<f32>,
    layers: Vec<Layer>,
    rope: PartialRotary,
    threads: usize,
    /// Tokens read since the caches were cleared.
    pos: usize,
    epoch: u64,
}

/// The rows of a 2-D weight, `(n, k)`, as the file declares it.
fn dims(gg: &Gguf<'_, impl Read + Seek>, name: &str) -> Result<(usize, usize)> {
    match gg.ct.tensor_infos.get(name) {
        Some(info) => info.shape.dims2(),
        None => bail!("the model file has no tensor {name}"),
    }
}

/// A weight that must be `(n, k)`.
fn weight(gg: &mut Gguf<'_, impl Read + Seek>, name: &str, n: usize, k: usize) -> Result<MatMul> {
    let got = dims(gg, name)?;
    if got != (n, k) {
        bail!("{name} is {got:?}, expected ({n}, {k})");
    }
    gg.matmul(name)
}

/// A small tensor (norms, conv, decay) as f32, which must hold `n` values.
fn floats(gg: &mut Gguf<'_, impl Read + Seek>, name: &str, n: usize) -> Result<Vec<f32>> {
    let v = gg.qtensor(name)?.dequantize(&Device::Cpu)?.flatten_all()?.to_vec1::<f32>()?;
    if v.len() != n {
        bail!("{name} has {} values, expected {n}", v.len());
    }
    Ok(v)
}

/// Separate weights `names` of the given widths, or the merged one.
fn proj(gg: &mut Gguf<'_, impl Read + Seek>, merged: &str, names: &[&str], widths: &[usize], k: usize) -> Result<Proj> {
    if gg.has(merged) {
        let n = widths.iter().sum();
        return Ok(Proj::Merged(weight(gg, merged, n, k)?, widths.to_vec()));
    }
    names.iter().zip(widths).map(|(name, n)| weight(gg, name, *n, k)).collect::<Result<Vec<_>>>().map(Proj::Split)
}

impl Qwen35 {
    /// Loads the weights. `opts.max_ctx` bounds the rotary tables;
    /// `opts.repack` is honoured as for Qwen3.
    pub fn from_gguf<R: Read + Seek>(ct: gguf_file::Content, reader: &mut R, opts: Options) -> Result<Self> {
        let hp = Hparams::from_metadata(&ct.metadata)?;
        let mut gg = Gguf { ct, reader, device: Device::Cpu, opts };
        let (embed, head) = gg.embed_and_head()?;
        let (h, vd) = (hp.hidden, hp.state_size);
        let mut layers = Vec::with_capacity(hp.layers);
        for i in 0..hp.layers {
            let p = |s: &str| format!("blk.{i}.{s}");
            let mixer = if hp.recurrent[i] {
                let (cd, kk) = (hp.conv_dim(), hp.conv_kernel);
                Mixer::Delta(Box::new(DeltaLayer {
                    qkvz: proj(&mut gg, &p("attn_qkv_gate.weight"), &[&p("attn_qkv.weight"), &p("attn_gate.weight")], &[cd, hp.inner], h)?,
                    beta_alpha: proj(
                        &mut gg,
                        &p("ssm_beta_alpha.weight"),
                        &[&p("ssm_beta.weight"), &p("ssm_alpha.weight")],
                        &[hp.v_heads, hp.v_heads],
                        h,
                    )?,
                    conv: floats(&mut gg, &p("ssm_conv1d.weight"), cd * kk)?,
                    dt_bias: floats(&mut gg, &p("ssm_dt.bias"), hp.v_heads)?,
                    a: floats(&mut gg, &p("ssm_a"), hp.v_heads)?,
                    norm: floats(&mut gg, &p("ssm_norm.weight"), vd)?,
                    out: weight(&mut gg, &p("ssm_out.weight"), h, hp.inner)?,
                    state: vec![0.0; hp.delta_shape().state_len()],
                    tail: vec![0.0; (kk - 1) * cd],
                }))
            } else {
                let (q, kv) = (hp.heads * hp.head_dim, hp.kv_heads * hp.head_dim);
                // llama.cpp also accepts one fused `attn_qkv` here (`create_tensor_qkv`).
                let qkv = proj(
                    &mut gg,
                    &p("attn_qkv.weight"),
                    &[&p("attn_q.weight"), &p("attn_k.weight"), &p("attn_v.weight")],
                    &[2 * q, kv, kv],
                    h,
                )?;
                Mixer::Attn(Box::new(AttnLayer {
                    qkv,
                    q_norm: floats(&mut gg, &p("attn_q_norm.weight"), hp.head_dim)?,
                    k_norm: floats(&mut gg, &p("attn_k_norm.weight"), hp.head_dim)?,
                    out: weight(&mut gg, &p("attn_output.weight"), h, q)?,
                    kv: KvBuf { data: Vec::new(), len: 0, stride: 2 * kv },
                }))
            };
            layers.push(Layer {
                attn_norm: floats(&mut gg, &p("attn_norm.weight"), h)?,
                post_norm: floats(&mut gg, &p("post_attention_norm.weight"), h)?,
                mixer,
                mlp: Mlp {
                    gate: weight(&mut gg, &p("ffn_gate.weight"), hp.ffn, h)?,
                    up: weight(&mut gg, &p("ffn_up.weight"), hp.ffn, h)?,
                    down: weight(&mut gg, &p("ffn_down.weight"), h, hp.ffn)?,
                },
            });
        }
        let out_norm = floats(&mut gg, "output_norm.weight", h)?;
        let rope = PartialRotary::new(hp.rope_dim, hp.rope_theta, opts.max_ctx.max(1));
        let threads = candle_core::utils::get_num_threads().max(1);
        Ok(Self { hp, embed, head, out_norm, layers, rope, threads, pos: 0, epoch: 0 })
    }

    pub const fn hparams(&self) -> &Hparams {
        &self.hp
    }

    /// Tokens read since the caches were cleared.
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Room in the attention cache for `tokens` positions in all, so a long
    /// prompt never holds two copies while the buffer grows.
    pub fn reserve(&mut self, tokens: usize) {
        for l in &mut self.layers {
            if let Mixer::Attn(a) = &mut l.mixer {
                let want = tokens * a.kv.stride;
                a.kv.data.reserve_exact(want.saturating_sub(a.kv.data.len()));
            }
        }
    }

    /// Logits for the last token of `input` (`(1, L)` ids) read at
    /// `offset`, which must be the number of tokens read so far (the
    /// DeltaNet state cannot skip or go back): `(1, vocab)`.
    pub fn forward(&mut self, input: &Tensor, offset: usize) -> Result<Tensor> {
        let ids = input.flatten_all()?.to_dtype(candle_core::DType::U32)?.to_vec1::<u32>()?;
        let logits = self.logits(&ids, offset, false)?;
        let n = logits.len();
        Tensor::from_vec(logits, (1, n), &Device::Cpu)
    }

    /// Logits for `ids` read at `offset`: for the last token, or for every
    /// token (`all`, `L × vocab`, for the reference check).
    pub fn logits(&mut self, ids: &[u32], offset: usize, all: bool) -> Result<Vec<f32>> {
        if offset != self.pos {
            bail!("Qwen 3.5 reads tokens in order: at {offset}, but {} were read", self.pos);
        }
        let l = ids.len();
        if l == 0 {
            bail!("no tokens");
        }
        if offset + l > self.rope.max_pos() {
            bail!("{} tokens is past this model's context of {}", offset + l, self.rope.max_pos());
        }
        let hp = &self.hp;
        let ids_t = Tensor::from_slice(ids, (1, l), &Device::Cpu)?;
        let mut h = self.embed.forward(&ids_t)?.flatten_all()?.to_vec1::<f32>()?;
        for layer in &mut self.layers {
            let x = rms_norm_rows(&h, &layer.attn_norm, hp.eps);
            let mixed = match &mut layer.mixer {
                Mixer::Delta(d) => delta_forward(d, hp, &x, l, self.threads)?,
                Mixer::Attn(a) => attn_forward(a, hp, &self.rope, &x, l, offset)?,
            };
            h.iter_mut().zip(&mixed).for_each(|(a, b)| *a += b);
            let x = rms_norm_rows(&h, &layer.post_norm, hp.eps);
            let gate = mm(&layer.mlp.gate, &x, l, hp.hidden)?;
            let up = mm(&layer.mlp.up, &x, l, hp.hidden)?;
            let act: Vec<f32> = gate.iter().zip(&up).map(|(g, u)| silu(*g) * u).collect();
            let down = mm(&layer.mlp.down, &act, l, hp.ffn)?;
            h.iter_mut().zip(&down).for_each(|(a, b)| *a += b);
        }
        self.pos += l;
        let (rows, from) = if all { (l, 0) } else { (1, l - 1) };
        let x = rms_norm_rows(&h[from * hp.hidden..], &self.out_norm, hp.eps);
        mm(&self.head, &x, rows, hp.hidden)
    }

    /// Forgets everything read.
    pub fn clear(&mut self) {
        for l in &mut self.layers {
            match &mut l.mixer {
                Mixer::Delta(d) => {
                    d.state.fill(0.0);
                    d.tail.fill(0.0);
                }
                Mixer::Attn(a) => {
                    a.kv.len = 0;
                    a.kv.data.clear();
                }
            }
        }
        self.pos = 0;
        self.epoch += 1;
    }

    /// The state after the tokens read so far (about 50 MB for the
    /// released sizes: the DeltaNet states and conv tails; the attention
    /// cache is kept in place and only its length is recorded).
    pub fn snapshot(&self) -> State {
        let recurrent = self
            .layers
            .iter()
            .filter_map(|l| match &l.mixer {
                Mixer::Delta(d) => Some((d.state.clone(), d.tail.clone())),
                Mixer::Attn(_) => None,
            })
            .collect();
        State { recurrent, pos: self.pos, epoch: self.epoch }
    }

    /// Goes back to `s`: the DeltaNet states are copied back and the
    /// attention cache is cut to `s`'s length. Refused (with the caches
    /// unchanged) for a state from before a [`Qwen35::clear`] or one that is
    /// ahead of what was read.
    pub fn restore(&mut self, s: &State) -> Result<()> {
        if s.epoch != self.epoch || s.pos > self.pos || s.recurrent.len() != self.hp.n_recurrent() {
            bail!("this saved state doesn't belong to the current caches");
        }
        let mut saved = s.recurrent.iter();
        for l in &mut self.layers {
            match &mut l.mixer {
                Mixer::Delta(d) => {
                    if let Some((state, tail)) = saved.next() {
                        d.state.copy_from_slice(state);
                        d.tail.copy_from_slice(tail);
                    }
                }
                Mixer::Attn(a) => a.kv.len = s.pos,
            }
        }
        self.pos = s.pos;
        Ok(())
    }
}

fn delta_forward(d: &mut DeltaLayer, hp: &Hparams, x: &[f32], l: usize, threads: usize) -> Result<Vec<f32>> {
    let shape = hp.delta_shape();
    let (kd, cd) = (hp.k_heads * hp.state_size, hp.conv_dim());
    let mut parts = d.qkvz.forward(x, l, hp.hidden)?.into_iter();
    let (Some(qkv), Some(z)) = (parts.next(), parts.next()) else { bail!("DeltaNet projection parts") };
    let mut parts = d.beta_alpha.forward(x, l, hp.hidden)?.into_iter();
    let (Some(b), Some(a)) = (parts.next(), parts.next()) else { bail!("DeltaNet beta/alpha parts") };
    let mixed = causal_conv_silu(&qkv, &d.conv, &mut d.tail, cd, hp.conv_kernel);
    let mut q = Vec::with_capacity(l * kd);
    let mut k = Vec::with_capacity(l * kd);
    let mut v = Vec::with_capacity(l * hp.inner);
    for row in mixed.chunks_exact(cd) {
        q.extend_from_slice(&row[..kd]);
        k.extend_from_slice(&row[kd..2 * kd]);
        v.extend_from_slice(&row[2 * kd..]);
    }
    l2_norm_rows(&mut q, hp.state_size, hp.eps);
    l2_norm_rows(&mut k, hp.state_size, hp.eps);
    let beta: Vec<f32> = b.iter().map(|x| sigmoid(*x)).collect();
    let g: Vec<f32> = a.iter().enumerate().map(|(i, x)| d.a[i % hp.v_heads] * softplus(x + d.dt_bias[i % hp.v_heads])).collect();
    let o = gated_delta_rule(shape, &mut d.state, &q, &k, &v, &g, &beta, threads);
    let normed = gated_rms_norm(&o, &z, &d.norm, hp.eps);
    mm(&d.out, &normed, l, hp.inner)
}

fn attn_forward(at: &mut AttnLayer, hp: &Hparams, rope: &PartialRotary, x: &[f32], l: usize, offset: usize) -> Result<Vec<f32>> {
    let (hd, heads, kvh) = (hp.head_dim, hp.heads, hp.kv_heads);
    let mut parts = at.qkv.forward(x, l, hp.hidden)?.into_iter();
    let (Some(qg), Some(k), Some(v)) = (parts.next(), parts.next(), parts.next()) else { bail!("attention projection parts") };
    let mut q = Vec::with_capacity(l * heads * hd);
    let mut gate = Vec::with_capacity(l * heads * hd);
    for head in qg.chunks_exact(2 * hd) {
        q.extend_from_slice(&head[..hd]);
        gate.extend_from_slice(&head[hd..]);
    }
    let mut q = rms_norm_rows(&q, &at.q_norm, hp.eps);
    let mut k = rms_norm_rows(&k, &at.k_norm, hp.eps);
    rope.apply(&mut q, heads, hd, offset);
    rope.apply(&mut k, kvh, hd, offset);
    at.kv.push(&k, &v, kvh, hd);
    let len = at.kv.len;
    let scale = 1.0 / (hd as f32).sqrt();
    let ctx = if l == 1 {
        causal_decode_f32_interleaved(&q, at.kv.view(), heads, kvh, hd, len, scale)?
    } else {
        let kv = Tensor::from_slice(at.kv.view(), (len, kvh, 2 * hd), &Device::Cpu)?;
        let kt = kv.narrow(2, 0, hd)?.unsqueeze(0)?.contiguous()?;
        let vt = kv.narrow(2, hd, hd)?.unsqueeze(0)?.contiguous()?;
        let qt = Tensor::from_slice(&q, (1, l, heads, hd), &Device::Cpu)?;
        flash_attn::<f32>(&qt, &kt, &vt, scale, AttnMask::causal_with_offset(offset), None, None)?.transpose(1, 2)?.contiguous()?
    };
    let ctx = ctx.flatten_all()?.to_vec1::<f32>()?;
    let gated: Vec<f32> = ctx.iter().zip(&gate).map(|(c, g)| c * sigmoid(*g)).collect();
    mm(&at.out, &gated, l, heads * hd)
}

#[cfg(test)]
mod tests;
