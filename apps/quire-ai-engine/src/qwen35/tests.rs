// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Qwen 3.5 model against an independent `f64` oracle.
//!
//! A tiny model with random weights is made in transformers' own layout
//! (`Qwen3_5ForCausalLM` parameter names and conventions: zero-centred
//! norms with `1 + w`, `A_log`, value heads grouped by key head). The oracle
//! runs transformers' forward pass on it in `f64`, written from
//! `models/qwen3_5/modeling_qwen3_5.py`. Then [`to_gguf`] applies what
//! llama.cpp's converter (`conversion/qwen.py`, `Qwen3NextModel` and
//! `_LinearAttentionVReorderBase`) does to those weights, and the engine
//! loads the result. Agreement shows that the engine undoes the
//! converter's transforms (the `+1` norms, `-exp(A_log)`, the tiled value
//! heads) and computes transformers' function.
//!
//! What this does not prove: that the port of the converter here is
//! faithful (the fixture made by the converter itself,
//! `docs/design/0.14-local-ai.md` §3.7,
//! does that), and anything about the released weights (the reference
//! check against Qwen's own weights does that).

use std::collections::HashMap;
use std::io::Cursor;

use candle_core::quantized::gguf_file::{self, Value as V};
use candle_core::quantized::{GgmlDType, QTensor};
use candle_core::{Device, Tensor};

use super::{Hparams, Qwen35};
use crate::deltanet::tests::Rng;
use crate::qwen3::{Options, Repack};

/// The tiny model of `docs/design/0.14-local-ai.md` §3.7: hidden 32, 3 DeltaNet
/// layers then 1 attention layer, 2 key and 4 value heads of 16, 4 query
/// and 2 key/value heads of 32, rotary on a quarter of each head, a
/// 512-token vocabulary.
fn tiny() -> Hparams {
    Hparams {
        layers: 4,
        hidden: 32,
        ffn: 64,
        heads: 4,
        kv_heads: 2,
        head_dim: 32,
        eps: 1e-6,
        rope_theta: 10_000_000.0,
        rope_dim: 8,
        conv_kernel: 4,
        state_size: 16,
        k_heads: 2,
        v_heads: 4,
        inner: 64,
        recurrent: vec![true, true, true, false],
    }
}

const VOCAB: usize = 512;

/// Weights by transformers' parameter name: shape and values.
struct Hf {
    w: HashMap<String, (Vec<usize>, Vec<f32>)>,
    tied: bool,
}

impl Hf {
    fn get(&self, name: &str) -> &[f32] {
        &self.w.get(name).unwrap_or_else(|| panic!("no {name}")).1
    }

    fn random(hp: &Hparams, seed: u64, tied: bool) -> Self {
        let mut rng = Rng::new(seed);
        let mut w = HashMap::new();
        let mut put = |name: String, shape: Vec<usize>, scale: f32, shift: f32, rng: &mut Rng| {
            let n = shape.iter().product();
            let v = rng.vec(n, scale).into_iter().map(|x| x + shift).collect();
            w.insert(name, (shape, v));
        };
        let (h, f) = (hp.hidden, hp.ffn);
        let lin = |k: usize| 1.5 / (k as f32).sqrt();
        put("model.embed_tokens.weight".into(), vec![VOCAB, h], 1.0, 0.0, &mut rng);
        if !tied {
            put("lm_head.weight".into(), vec![VOCAB, h], lin(h), 0.0, &mut rng);
        }
        put("model.norm.weight".into(), vec![h], 0.2, 0.0, &mut rng);
        for i in 0..hp.layers {
            let p = |s: &str| format!("model.layers.{i}.{s}");
            put(p("input_layernorm.weight"), vec![h], 0.2, 0.0, &mut rng);
            put(p("post_attention_layernorm.weight"), vec![h], 0.2, 0.0, &mut rng);
            if hp.recurrent[i] {
                let cd = hp.conv_dim();
                put(p("linear_attn.in_proj_qkv.weight"), vec![cd, h], lin(h), 0.0, &mut rng);
                put(p("linear_attn.in_proj_z.weight"), vec![hp.inner, h], lin(h), 0.0, &mut rng);
                put(p("linear_attn.in_proj_b.weight"), vec![hp.v_heads, h], lin(h), 0.0, &mut rng);
                put(p("linear_attn.in_proj_a.weight"), vec![hp.v_heads, h], lin(h), 0.0, &mut rng);
                put(p("linear_attn.conv1d.weight"), vec![cd, 1, hp.conv_kernel], 0.6, 0.0, &mut rng);
                put(p("linear_attn.dt_bias"), vec![hp.v_heads], 1.0, 0.0, &mut rng);
                // log of something in [1, 3]: decays well inside (0, 1).
                put(p("linear_attn.A_log"), vec![hp.v_heads], 0.5, 0.6, &mut rng);
                put(p("linear_attn.norm.weight"), vec![hp.state_size], 0.2, 1.0, &mut rng);
                put(p("linear_attn.out_proj.weight"), vec![h, hp.inner], lin(hp.inner), 0.0, &mut rng);
            } else {
                let (q, kv) = (hp.heads * hp.head_dim, hp.kv_heads * hp.head_dim);
                put(p("self_attn.q_proj.weight"), vec![2 * q, h], lin(h), 0.0, &mut rng);
                put(p("self_attn.k_proj.weight"), vec![kv, h], lin(h), 0.0, &mut rng);
                put(p("self_attn.v_proj.weight"), vec![kv, h], lin(h), 0.0, &mut rng);
                put(p("self_attn.o_proj.weight"), vec![h, q], lin(q), 0.0, &mut rng);
                put(p("self_attn.q_norm.weight"), vec![hp.head_dim], 0.2, 0.0, &mut rng);
                put(p("self_attn.k_norm.weight"), vec![hp.head_dim], 0.2, 0.0, &mut rng);
            }
            put(p("mlp.gate_proj.weight"), vec![f, h], lin(h), 0.0, &mut rng);
            put(p("mlp.up_proj.weight"), vec![f, h], lin(h), 0.0, &mut rng);
            put(p("mlp.down_proj.weight"), vec![h, f], lin(f), 0.0, &mut rng);
        }
        Self { w, tied }
    }
}

// ---------------------------------------------------------------------------
// The oracle: transformers' forward pass in f64.

fn matvec(w: &[f32], x: &[f64], n: usize, k: usize) -> Vec<f64> {
    (0..n).map(|r| (0..k).map(|c| f64::from(w[r * k + c]) * x[c]).sum()).collect()
}

/// `Qwen3_5RMSNorm`: `x / rms(x) · (1 + w)`.
fn rms1(x: &[f64], w: &[f32], eps: f64) -> Vec<f64> {
    let inv = 1.0 / (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64 + eps).sqrt();
    x.iter().zip(w).map(|(v, w)| v * inv * (1.0 + f64::from(*w))).collect()
}

fn sig(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Logits at every position of `ids`, read from position 0.
#[expect(
    clippy::too_many_lines,
    clippy::needless_range_loop,
    reason = "one forward pass, written out with indices so it can be read against transformers'"
)]
fn oracle(hp: &Hparams, m: &Hf, ids: &[u32]) -> Vec<Vec<f64>> {
    let (h, l) = (hp.hidden, ids.len());
    let eps = f64::from(hp.eps);
    let emb = m.get("model.embed_tokens.weight");
    let mut x: Vec<Vec<f64>> = ids.iter().map(|&t| emb[t as usize * h..][..h].iter().map(|v| f64::from(*v)).collect()).collect();
    for i in 0..hp.layers {
        let p = |s: &str| format!("model.layers.{i}.{s}");
        let xn: Vec<Vec<f64>> = x.iter().map(|r| rms1(r, m.get(&p("input_layernorm.weight")), eps)).collect();
        let mixed: Vec<Vec<f64>> = if hp.recurrent[i] {
            let (kh, vh, d, cd, kk) = (hp.k_heads, hp.v_heads, hp.state_size, hp.conv_dim(), hp.conv_kernel);
            let qkv: Vec<Vec<f64>> = xn.iter().map(|r| matvec(m.get(&p("linear_attn.in_proj_qkv.weight")), r, cd, h)).collect();
            let z: Vec<Vec<f64>> = xn.iter().map(|r| matvec(m.get(&p("linear_attn.in_proj_z.weight")), r, hp.inner, h)).collect();
            let b: Vec<Vec<f64>> = xn.iter().map(|r| matvec(m.get(&p("linear_attn.in_proj_b.weight")), r, vh, h)).collect();
            let a: Vec<Vec<f64>> = xn.iter().map(|r| matvec(m.get(&p("linear_attn.in_proj_a.weight")), r, vh, h)).collect();
            let cw = m.get(&p("linear_attn.conv1d.weight"));
            // F.conv1d with padding K − 1, first L outputs, then SiLU.
            let conv: Vec<Vec<f64>> = (0..l)
                .map(|t| {
                    (0..cd)
                        .map(|c| {
                            let s: f64 = (0..kk)
                                .map(|j| {
                                    let src = t as isize + j as isize - (kk as isize - 1);
                                    if src < 0 { 0.0 } else { f64::from(cw[c * kk + j]) * qkv[src as usize][c] }
                                })
                                .sum();
                            s * sig(s)
                        })
                        .collect()
                })
                .collect();
            let dt = m.get(&p("linear_attn.dt_bias"));
            let a_log = m.get(&p("linear_attn.A_log"));
            let nw = m.get(&p("linear_attn.norm.weight"));
            // S[h][i][j], key i, value j; value head h reads key head h / (vh / kh).
            let mut s = vec![0f64; vh * d * d];
            let mut out = Vec::new();
            for t in 0..l {
                let row = &conv[t];
                let l2 = |v: &[f64]| {
                    let n = (v.iter().map(|x| x * x).sum::<f64>() + 1e-6).sqrt();
                    v.iter().map(|x| x / n).collect::<Vec<f64>>()
                };
                let mut o = vec![0f64; vh * d];
                for hh in 0..vh {
                    let kg = hh / (vh / kh);
                    let q: Vec<f64> = l2(&row[kg * d..][..d]).iter().map(|x| x / (d as f64).sqrt()).collect();
                    let k = l2(&row[kh * d + kg * d..][..d]);
                    let v = &row[2 * kh * d + hh * d..][..d];
                    let beta = sig(b[t][hh]);
                    let sp = {
                        let y = a[t][hh] + f64::from(dt[hh]);
                        if y > 20.0 { y } else { y.exp().ln_1p() }
                    };
                    let g = -f64::from(a_log[hh]).exp() * sp;
                    let sh = &mut s[hh * d * d..][..d * d];
                    for x in sh.iter_mut() {
                        *x *= g.exp();
                    }
                    let u: Vec<f64> = (0..d).map(|j| beta * (v[j] - (0..d).map(|ii| sh[ii * d + j] * k[ii]).sum::<f64>())).collect();
                    for ii in 0..d {
                        for j in 0..d {
                            sh[ii * d + j] += k[ii] * u[j];
                        }
                    }
                    for j in 0..d {
                        o[hh * d + j] = (0..d).map(|ii| sh[ii * d + j] * q[ii]).sum();
                    }
                }
                // Qwen3_5RMSNormGated per head: rms(o) · w · silu(z).
                let mut gated = vec![0f64; vh * d];
                for hh in 0..vh {
                    let oh = &o[hh * d..][..d];
                    let inv = 1.0 / (oh.iter().map(|x| x * x).sum::<f64>() / d as f64 + eps).sqrt();
                    for j in 0..d {
                        let zz = z[t][hh * d + j];
                        gated[hh * d + j] = oh[j] * inv * f64::from(nw[j]) * zz * sig(zz);
                    }
                }
                out.push(matvec(m.get(&p("linear_attn.out_proj.weight")), &gated, h, hp.inner));
            }
            out
        } else {
            let (nh, nkv, hd, rot) = (hp.heads, hp.kv_heads, hp.head_dim, hp.rope_dim);
            let qw = m.get(&p("self_attn.q_proj.weight"));
            let mut qs = Vec::new();
            let mut gates = Vec::new();
            let mut ks = Vec::new();
            let mut vs = Vec::new();
            for (t, r) in xn.iter().enumerate() {
                let qg = matvec(qw, r, 2 * nh * hd, h);
                let kr = matvec(m.get(&p("self_attn.k_proj.weight")), r, nkv * hd, h);
                let vr = matvec(m.get(&p("self_attn.v_proj.weight")), r, nkv * hd, h);
                let rope = |v: &[f64]| {
                    let mut v = v.to_vec();
                    for i in 0..rot / 2 {
                        let a = t as f64 / hp.rope_theta.powf((2 * i) as f64 / rot as f64);
                        let (x1, x2) = (v[i], v[i + rot / 2]);
                        v[i] = x1 * a.cos() - x2 * a.sin();
                        v[i + rot / 2] = x2 * a.cos() + x1 * a.sin();
                    }
                    v
                };
                let mut q = Vec::new();
                let mut gate = Vec::new();
                for hh in 0..nh {
                    // view(…, heads, 2·head_dim).chunk(2): query first, gate second.
                    let qh = rms1(&qg[hh * 2 * hd..][..hd], m.get(&p("self_attn.q_norm.weight")), eps);
                    q.push(rope(&qh));
                    gate.extend_from_slice(&qg[hh * 2 * hd + hd..][..hd]);
                }
                let k: Vec<Vec<f64>> =
                    (0..nkv).map(|g| rope(&rms1(&kr[g * hd..][..hd], m.get(&p("self_attn.k_norm.weight")), eps))).collect();
                let v: Vec<Vec<f64>> = (0..nkv).map(|g| vr[g * hd..][..hd].to_vec()).collect();
                qs.push(q);
                gates.push(gate);
                ks.push(k);
                vs.push(v);
            }
            (0..l)
                .map(|t| {
                    let mut ctx = Vec::new();
                    for hh in 0..nh {
                        let g = hh / (nh / nkv);
                        let scores: Vec<f64> = (0..=t)
                            .map(|s| qs[t][hh].iter().zip(&ks[s][g]).map(|(a, b)| a * b).sum::<f64>() / (hd as f64).sqrt())
                            .collect();
                        let mx = scores.iter().copied().fold(f64::MIN, f64::max);
                        let e: Vec<f64> = scores.iter().map(|s| (s - mx).exp()).collect();
                        let sum: f64 = e.iter().sum();
                        for j in 0..hd {
                            ctx.push((0..=t).map(|s| e[s] / sum * vs[s][g][j]).sum::<f64>());
                        }
                    }
                    let gated: Vec<f64> = ctx.iter().zip(&gates[t]).map(|(c, g)| c * sig(*g)).collect();
                    matvec(m.get(&p("self_attn.o_proj.weight")), &gated, h, nh * hd)
                })
                .collect()
        };
        for (r, mx) in x.iter_mut().zip(&mixed) {
            r.iter_mut().zip(mx).for_each(|(a, b)| *a += b);
        }
        for r in &mut x {
            let xn = rms1(r, m.get(&p("post_attention_layernorm.weight")), eps);
            let g = matvec(m.get(&p("mlp.gate_proj.weight")), &xn, hp.ffn, h);
            let u = matvec(m.get(&p("mlp.up_proj.weight")), &xn, hp.ffn, h);
            let act: Vec<f64> = g.iter().zip(&u).map(|(g, u)| g * sig(*g) * u).collect();
            let down = matvec(m.get(&p("mlp.down_proj.weight")), &act, h, hp.ffn);
            r.iter_mut().zip(&down).for_each(|(a, b)| *a += b);
        }
    }
    let head = if m.tied { m.get("model.embed_tokens.weight") } else { m.get("lm_head.weight") };
    x.iter().map(|r| matvec(head, &rms1(r, m.get("model.norm.weight"), eps), VOCAB, h)).collect()
}

// ---------------------------------------------------------------------------
// The converter's transforms, ported, and the GGUF writer.

/// `_reorder_v_heads` along the rows: `num_k_heads × per_k × head_dim`
/// rows (grouped by key head) become `per_k × num_k_heads × head_dim`
/// (tiled), each row keeping its `cols` values.
fn reorder_rows(x: &[f32], cols: usize, k_heads: usize, per_k: usize, head_dim: usize) -> Vec<f32> {
    let mut out = vec![0f32; x.len()];
    for g in 0..k_heads {
        for j in 0..per_k {
            for d in 0..head_dim {
                let from = (g * per_k + j) * head_dim + d;
                let to = (j * k_heads + g) * head_dim + d;
                out[to * cols..][..cols].copy_from_slice(&x[from * cols..][..cols]);
            }
        }
    }
    out
}

/// The same along the columns of a `rows × (k_heads·per_k·head_dim)` matrix.
fn reorder_cols(x: &[f32], rows: usize, k_heads: usize, per_k: usize, head_dim: usize) -> Vec<f32> {
    let n = k_heads * per_k * head_dim;
    let mut out = vec![0f32; x.len()];
    for r in 0..rows {
        for g in 0..k_heads {
            for j in 0..per_k {
                for d in 0..head_dim {
                    out[r * n + (j * k_heads + g) * head_dim + d] = x[r * n + (g * per_k + j) * head_dim + d];
                }
            }
        }
    }
    out
}

/// How to lay out the file.
#[derive(Clone, Copy, Default)]
struct Form {
    /// `attn_qkv_gate` and `ssm_beta_alpha` instead of the separate weights.
    merged: bool,
    /// A multi-token-prediction block after the trunk, as the converter
    /// writes it (`block_count` + 1, `nextn_predict_layers` = 1).
    mtp: bool,
    /// The layer kinds as `full_attention_interval` instead of
    /// `attention.recurrent_layers`.
    interval_only: bool,
    /// Quantize the 2-D weights to this type.
    quant: Option<GgmlDType>,
}

fn to_gguf(hp: &Hparams, m: &Hf, form: Form) -> Vec<u8> {
    let (kh, vh, d) = (hp.k_heads, hp.v_heads, hp.state_size);
    let per_k = vh / kh;
    let mut t: Vec<(String, Vec<usize>, Vec<f32>)> = Vec::new();
    let plus1 = |v: &[f32]| v.iter().map(|x| x + 1.0).collect::<Vec<f32>>();
    t.push(("token_embd.weight".into(), vec![VOCAB, hp.hidden], m.get("model.embed_tokens.weight").to_vec()));
    if !m.tied {
        t.push(("output.weight".into(), vec![VOCAB, hp.hidden], m.get("lm_head.weight").to_vec()));
    }
    t.push(("output_norm.weight".into(), vec![hp.hidden], plus1(m.get("model.norm.weight"))));
    for i in 0..hp.layers {
        let hf = |s: &str| m.get(&format!("model.layers.{i}.{s}")).to_vec();
        let p = |s: &str| format!("blk.{i}.{s}");
        let h = hp.hidden;
        t.push((p("attn_norm.weight"), vec![h], plus1(&hf("input_layernorm.weight"))));
        t.push((p("post_attention_norm.weight"), vec![h], plus1(&hf("post_attention_layernorm.weight"))));
        if hp.recurrent[i] {
            let cd = hp.conv_dim();
            let qk = 2 * kh * d * h;
            let mut qkv = hf("linear_attn.in_proj_qkv.weight");
            let v = reorder_rows(&qkv[qk..], h, kh, per_k, d);
            qkv.truncate(qk);
            qkv.extend(v);
            let z = reorder_rows(&hf("linear_attn.in_proj_z.weight"), h, kh, per_k, d);
            let b = reorder_rows(&hf("linear_attn.in_proj_b.weight"), h, kh, per_k, 1);
            let a = reorder_rows(&hf("linear_attn.in_proj_a.weight"), h, kh, per_k, 1);
            if form.merged {
                t.push((p("attn_qkv_gate.weight"), vec![cd + hp.inner, h], [qkv, z].concat()));
                t.push((p("ssm_beta_alpha.weight"), vec![2 * vh, h], [b, a].concat()));
            } else {
                t.push((p("attn_qkv.weight"), vec![cd, h], qkv));
                t.push((p("attn_gate.weight"), vec![hp.inner, h], z));
                t.push((p("ssm_beta.weight"), vec![vh, h], b));
                t.push((p("ssm_alpha.weight"), vec![vh, h], a));
            }
            let kk = hp.conv_kernel;
            let mut conv = hf("linear_attn.conv1d.weight");
            let cv = reorder_rows(&conv[2 * kh * d * kk..], kk, kh, per_k, d);
            conv.truncate(2 * kh * d * kk);
            conv.extend(cv);
            t.push((p("ssm_conv1d.weight"), vec![cd, kk], conv));
            t.push((p("ssm_dt.bias"), vec![vh], reorder_rows(&hf("linear_attn.dt_bias"), 1, kh, per_k, 1)));
            let a_log = reorder_rows(&hf("linear_attn.A_log"), 1, kh, per_k, 1);
            t.push((p("ssm_a"), vec![vh], a_log.iter().map(|x| -x.exp()).collect()));
            t.push((p("ssm_norm.weight"), vec![d], hf("linear_attn.norm.weight")));
            t.push((p("ssm_out.weight"), vec![h, hp.inner], reorder_cols(&hf("linear_attn.out_proj.weight"), h, kh, per_k, d)));
        } else {
            let (q, kv) = (hp.heads * hp.head_dim, hp.kv_heads * hp.head_dim);
            t.push((p("attn_q.weight"), vec![2 * q, h], hf("self_attn.q_proj.weight")));
            t.push((p("attn_k.weight"), vec![kv, h], hf("self_attn.k_proj.weight")));
            t.push((p("attn_v.weight"), vec![kv, h], hf("self_attn.v_proj.weight")));
            t.push((p("attn_output.weight"), vec![h, q], hf("self_attn.o_proj.weight")));
            t.push((p("attn_q_norm.weight"), vec![hp.head_dim], plus1(&hf("self_attn.q_norm.weight"))));
            t.push((p("attn_k_norm.weight"), vec![hp.head_dim], plus1(&hf("self_attn.k_norm.weight"))));
        }
        t.push((p("ffn_gate.weight"), vec![hp.ffn, h], hf("mlp.gate_proj.weight")));
        t.push((p("ffn_up.weight"), vec![hp.ffn, h], hf("mlp.up_proj.weight")));
        t.push((p("ffn_down.weight"), vec![h, hp.ffn], hf("mlp.down_proj.weight")));
    }
    if form.mtp {
        // Garbage of the wrong shape: the loader must never read it.
        let n = hp.layers;
        for name in ["nextn.eh_proj.weight", "nextn.enorm.weight", "attn_q.weight", "ffn_up.weight"] {
            t.push((format!("blk.{n}.{name}"), vec![3, 5], vec![f32::NAN; 15]));
        }
    }
    let mut md: Vec<(String, V)> = vec![("general.architecture".into(), V::String("qwen35".into()))];
    let u = |x: usize| V::U32(u32::try_from(x).unwrap());
    let blocks = hp.layers + usize::from(form.mtp);
    for (k, v) in [
        ("block_count", u(blocks)),
        ("embedding_length", u(hp.hidden)),
        ("feed_forward_length", u(hp.ffn)),
        ("attention.head_count", u(hp.heads)),
        ("attention.head_count_kv", u(hp.kv_heads)),
        ("attention.key_length", u(hp.head_dim)),
        ("attention.value_length", u(hp.head_dim)),
        ("attention.layer_norm_rms_epsilon", V::F32(hp.eps)),
        ("rope.freq_base", V::F32(hp.rope_theta as f32)),
        ("rope.dimension_count", u(hp.rope_dim)),
        ("rope.dimension_sections", V::Array(vec![V::I32(11), V::I32(11), V::I32(10), V::I32(0)])),
        ("ssm.conv_kernel", u(hp.conv_kernel)),
        ("ssm.state_size", u(hp.state_size)),
        ("ssm.group_count", u(hp.k_heads)),
        ("ssm.time_step_rank", u(hp.v_heads)),
        ("ssm.inner_size", u(hp.inner)),
        ("full_attention_interval", u(4)),
        ("context_length", u(262_144)),
    ] {
        md.push((format!("qwen35.{k}"), v));
    }
    if form.mtp {
        md.push(("qwen35.nextn_predict_layers".into(), u(1)));
    }
    if !form.interval_only {
        let mut flags: Vec<V> = hp.recurrent.iter().map(|r| V::Bool(*r)).collect();
        flags.extend((0..usize::from(form.mtp)).map(|_| V::Bool(false)));
        md.push(("qwen35.attention.recurrent_layers".into(), V::Array(flags)));
    }
    let tensors: Vec<(String, QTensor)> = t
        .into_iter()
        .map(|(name, shape, v)| {
            let dt = match form.quant {
                Some(q) if shape.len() == 2 && shape[1] % q.block_size() == 0 && !name.contains("conv1d") => q,
                _ => GgmlDType::F32,
            };
            let x = Tensor::from_vec(v, shape, &Device::Cpu).unwrap();
            (name, QTensor::quantize(&x, dt).unwrap())
        })
        .collect();
    let mdr: Vec<(&str, &V)> = md.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let tr: Vec<(&str, &QTensor)> = tensors.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut out = Cursor::new(Vec::new());
    gguf_file::write(&mut out, &mdr, &tr).unwrap();
    out.into_inner()
}

fn load(bytes: &[u8], repack: Repack) -> candle_core::Result<Qwen35> {
    let mut r = Cursor::new(bytes);
    let ct = gguf_file::Content::read(&mut r)?;
    Qwen35::from_gguf(ct, &mut r, Options { repack, max_ctx: 1024 })
}

fn ids(seed: u64, n: usize) -> Vec<u32> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| (rng.next_u64() % VOCAB as u64) as u32).collect()
}

fn argmax(v: impl Iterator<Item = f64>) -> usize {
    v.enumerate().fold((0, f64::MIN), |best, (i, x)| if x > best.1 { (i, x) } else { best }).0
}

/// Max abs difference and whether the argmax agrees at every position.
fn compare(got: &[f32], want: &[Vec<f64>]) -> (f64, usize) {
    let mut worst = 0f64;
    let mut agree = 0;
    for (row, w) in got.chunks_exact(VOCAB).zip(want) {
        for (g, w) in row.iter().zip(w) {
            worst = worst.max((f64::from(*g) - w).abs());
        }
        agree += usize::from(argmax(row.iter().map(|x| f64::from(*x))) == argmax(w.iter().copied()));
    }
    (worst, agree)
}

#[test]
fn the_engine_computes_transformers_function_through_the_converters_layout() {
    let hp = tiny();
    for (seed, tied, form) in [
        (0, true, Form::default()),
        (1, false, Form { merged: true, ..Form::default() }),
        (2, true, Form { mtp: true, interval_only: true, ..Form::default() }),
    ] {
        let m = Hf::random(&hp, seed, tied);
        let mut model = load(&to_gguf(&hp, &m, form), Repack::default()).unwrap();
        assert_eq!(model.hparams(), &hp);
        for s in 0..3 {
            let x = ids(100 + s, 40);
            let want = oracle(&hp, &m, &x);
            model.clear();
            let got = model.logits(&x, 0, true).unwrap();
            let (worst, agree) = compare(&got, &want);
            let scale = want.iter().flatten().fold(0f64, |a, b| a.max(b.abs()));
            assert!(worst <= 1e-4 * scale.max(1.0), "seed {seed} seq {s}: max abs diff {worst} (logits up to {scale})");
            assert_eq!(agree, 40, "seed {seed} seq {s}: argmax differs");
        }
    }
}

/// The same tiny model with the value heads left grouped (as transformers
/// stores them) runs but gives different logits: the head-order test above
/// is not passing by accident.
#[test]
fn the_grouped_head_order_gives_a_different_model() {
    let hp = tiny();
    let m = Hf::random(&hp, 3, true);
    let x = ids(7, 20);
    let want = oracle(&hp, &m, &x);
    let mut good = load(&to_gguf(&hp, &m, Form::default()), Repack::default()).unwrap();
    let (ok, _) = compare(&good.logits(&x, 0, true).unwrap(), &want);
    // Undo the reorder of the converter for the value rows of in_proj_qkv: a
    // file "converted" without it.
    let mut wrong = Hf::random(&hp, 3, true);
    for i in (0..hp.layers).filter(|i| hp.recurrent[*i]) {
        let name = format!("model.layers.{i}.linear_attn.in_proj_qkv.weight");
        let (shape, mut v) = wrong.w.remove(&name).unwrap();
        let qk = 2 * hp.k_heads * hp.state_size * hp.hidden;
        // The inverse of the tiled reorder (per_k and k_heads swapped).
        let back = reorder_rows(&v[qk..], hp.hidden, hp.v_heads / hp.k_heads, hp.k_heads, hp.state_size);
        v.truncate(qk);
        v.extend(back);
        wrong.w.insert(name, (shape, v));
    }
    let mut bad = load(&to_gguf(&hp, &wrong, Form::default()), Repack::default()).unwrap();
    let (off, _) = compare(&bad.logits(&x, 0, true).unwrap(), &want);
    assert!(ok < 1e-3 && off > 100.0 * ok.max(1e-6), "right order {ok}, wrong order {off}");
}

/// A prompt fed in one chunk or in chunks of 1, 7, 64 and 256 (and at
/// random split points) gives the same logits within 1e-4.
#[test]
fn logits_do_not_depend_on_how_the_prompt_is_chunked() {
    let hp = tiny();
    let m = Hf::random(&hp, 4, true);
    let mut model = load(&to_gguf(&hp, &m, Form::default()), Repack::default()).unwrap();
    let x = ids(11, 300);
    let whole = model.logits(&x, 0, true).unwrap();
    let run = |model: &mut Qwen35, cuts: &[usize]| -> Vec<f32> {
        model.clear();
        let mut out = Vec::new();
        let mut at = 0;
        for &end in cuts.iter().chain(std::iter::once(&x.len())) {
            if end > at {
                out.extend(model.logits(&x[at..end], at, true).unwrap());
                at = end;
            }
        }
        out
    };
    let check = |got: &[f32], what: &str| {
        let worst = got.iter().zip(&whole).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(worst <= 1e-4, "{what}: max abs diff {worst}");
    };
    for size in [1usize, 7, 64, 256] {
        let cuts: Vec<usize> = (size..x.len()).step_by(size).collect();
        check(&run(&mut model, &cuts), &format!("chunks of {size}"));
    }
    // Random split points: `PROPTEST_CASES` cases (the weekly property job
    // sets it high), 8 by default.
    let cases: u64 = std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let mut rng = Rng::new(12);
    for case in 0..cases {
        let mut cuts: Vec<usize> = (0..rng.next_u64() % 6).map(|_| 1 + (rng.next_u64() % 299) as usize).collect();
        cuts.sort_unstable();
        check(&run(&mut model, &cuts), &format!("case {case}: cuts {cuts:?}"));
    }
}

/// The prompt-reuse snapshot: going back to a saved state and reading the
/// rest gives the logits of reading everything afresh (≤ 1e-5).
#[test]
fn a_restored_snapshot_continues_like_a_fresh_read() {
    let hp = tiny();
    let m = Hf::random(&hp, 5, true);
    let mut model = load(&to_gguf(&hp, &m, Form::default()), Repack::default()).unwrap();
    let prompt = ids(21, 60);
    let fresh = model.logits(&prompt, 0, false).unwrap();
    model.clear();
    model.logits(&prompt[..45], 0, false).unwrap();
    let saved = model.snapshot();
    assert_eq!(saved.len(), 45);
    // A reply is written after the saved prompt…
    for (i, t) in ids(22, 6).into_iter().enumerate() {
        model.logits(&[t], 45 + i, false).unwrap();
    }
    // …then the next prompt starts with the saved one.
    model.restore(&saved).unwrap();
    assert_eq!(model.position(), 45);
    let reused = model.logits(&prompt[45..], 45, false).unwrap();
    let worst = reused.iter().zip(&fresh).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst <= 1e-5, "max abs diff {worst}");
    // A state from before a clear is refused.
    model.clear();
    assert!(model.restore(&saved).is_err());
    // Reading out of order is refused.
    assert!(model.logits(&prompt[..3], 5, false).is_err());
}

/// The second tiny-model case: the fixture quantized to `Q8_0` (by candle, at
/// test time) agrees with the f32 oracle's argmax on at least 95% of
/// positions, through the compact path (blocks and quantized token table)
/// and through candle's own.
#[test]
fn a_q8_0_quantized_model_mostly_agrees_with_the_oracle() {
    let hp = tiny();
    let m = Hf::random(&hp, 6, true);
    let bytes = to_gguf(&hp, &m, Form { quant: Some(GgmlDType::Q8_0), ..Form::default() });
    for repack in [Repack::Never, Repack::Q8] {
        let mut model = load(&bytes, repack).unwrap();
        let (mut agree, mut total) = (0, 0);
        for s in 0..3 {
            let x = ids(300 + s, 40);
            model.clear();
            let (_, a) = compare(&model.logits(&x, 0, true).unwrap(), &oracle(&hp, &m, &x));
            agree += a;
            total += 40;
        }
        assert!(agree * 100 >= total * 95, "{repack:?}: {agree} of {total}");
    }
}

#[test]
fn the_loader_names_a_missing_or_misshapen_tensor() {
    let hp = tiny();
    let m = Hf::random(&hp, 8, true);
    let mut r = Cursor::new(to_gguf(&hp, &m, Form::default()));
    let mut ct = gguf_file::Content::read(&mut r).unwrap();
    ct.tensor_infos.remove("blk.1.ssm_a");
    let err = Qwen35::from_gguf(ct, &mut r, Options::default()).err().unwrap().to_string();
    assert!(err.contains("blk.1.ssm_a"), "{err}");
    let mut r = Cursor::new(to_gguf(&hp, &m, Form::default()));
    let mut ct = gguf_file::Content::read(&mut r).unwrap();
    let info = ct.tensor_infos.get_mut("blk.3.attn_output.weight").unwrap();
    info.shape = candle_core::Shape::from((16, 128));
    let err = Qwen35::from_gguf(ct, &mut r, Options::default()).err().unwrap().to_string();
    assert!(err.contains("blk.3.attn_output.weight"), "{err}");
}

fn metadata(hp: &Hparams, form: Form) -> HashMap<String, V> {
    let mut r = Cursor::new(to_gguf(hp, &Hf::random(hp, 9, true), form));
    gguf_file::Content::read(&mut r).unwrap().metadata
}

#[test]
fn hyperparameters_are_read_and_checked() {
    let hp = tiny();
    assert_eq!(Hparams::from_metadata(&metadata(&hp, Form::default())).unwrap(), hp);
    // The layer kinds from the interval alone; the prediction block is not a layer.
    assert_eq!(Hparams::from_metadata(&metadata(&hp, Form { mtp: true, interval_only: true, ..Form::default() })).unwrap(), hp);
    for missing in ["qwen35.ssm.inner_size", "qwen35.attention.head_count_kv", "qwen35.rope.dimension_count", "qwen35.ssm.group_count"] {
        let mut md = metadata(&hp, Form::default());
        md.remove(missing);
        let err = Hparams::from_metadata(&md).unwrap_err().to_string();
        assert!(err.contains(missing), "{missing}: {err}");
    }
    let mut md = metadata(&hp, Form { interval_only: true, ..Form::default() });
    md.remove("qwen35.full_attention_interval");
    assert!(Hparams::from_metadata(&md).unwrap_err().to_string().contains("full_attention_interval"));
    let mut md = metadata(&hp, Form::default());
    md.insert("qwen35.ssm.inner_size".into(), V::U32(60));
    assert!(Hparams::from_metadata(&md).is_err(), "inner size must be value heads × state size");
    let mut md = metadata(&hp, Form::default());
    md.insert("general.architecture".into(), V::String("qwen3".into()));
    assert!(Hparams::from_metadata(&md).unwrap_err().to_string().contains("qwen3"));
}

/// The released shapes: 32 layers, attention at 3, 7, …, 31 (0-based).
#[test]
fn the_released_models_interleave_three_deltanet_layers_with_one_attention_layer() {
    for hp in [Hparams::qwen35_4b(), Hparams::qwen35_9b()] {
        let attention: Vec<usize> = (0..hp.layers).filter(|i| !hp.recurrent[*i]).collect();
        assert_eq!(attention, [3, 7, 11, 15, 19, 23, 27, 31]);
        assert_eq!((hp.n_recurrent(), hp.n_attention()), (24, 8));
        assert_eq!(hp.conv_dim(), 8192);
        hp.check().unwrap();
    }
}

/// The memory sums of `docs/design/0.14-local-ai.md` §3.5, from the released shapes.
#[test]
fn cache_memory_is_as_the_design_note_sums_it() {
    const MIB: usize = 1 << 20;
    for hp in [Hparams::qwen35_4b(), Hparams::qwen35_9b()] {
        // 8 layers × 2 × 4 heads × 256 × 4 bytes.
        assert_eq!(hp.kv_bytes_per_token(), 64 * 1024);
        assert_eq!(4096 * hp.kv_bytes_per_token(), 256 * MIB);
        assert_eq!(8192 * hp.kv_bytes_per_token(), 512 * MIB);
        // 24 × 32 × 128 × 128 × 4 bytes, and 24 × 3 × 8,192 × 4 for the tails.
        assert_eq!(hp.recurrent_state_bytes(), 48 * MIB);
        assert_eq!(hp.conv_tail_bytes(), 2_359_296);
        // The snapshot doubles the fixed part: about 50 MB more.
        let extra = hp.cache_bytes(0, true) - hp.cache_bytes(0, false);
        assert_eq!(extra, 48 * MIB + 2_359_296);
        assert_eq!(hp.cache_bytes(2000, false), 2000 * 65_536 + 48 * MIB + 2_359_296);
    }
}

/// A Qwen 3.5 file with the token table and every weight in `Q4_K` (and
/// the projections in `Q6_K`, as `Q4_K_M` mixes them) runs through the
/// compact path, whose results equal candle's own path on the same blocks.
#[test]
fn the_compact_and_candle_paths_agree_on_k_quant_blocks() {
    // Rows of 256 for the K-quants.
    let hp = Hparams {
        layers: 4,
        hidden: 256,
        ffn: 256,
        heads: 2,
        kv_heads: 1,
        head_dim: 128,
        eps: 1e-6,
        rope_theta: 10_000_000.0,
        rope_dim: 32,
        conv_kernel: 4,
        state_size: 64,
        k_heads: 2,
        v_heads: 4,
        inner: 256,
        recurrent: vec![true, true, true, false],
    };
    let m = Hf::random(&hp, 10, true);
    let x = ids(31, 12);
    for q in [GgmlDType::Q4K, GgmlDType::Q6K] {
        let bytes = to_gguf(&hp, &m, Form { quant: Some(q), ..Form::default() });
        let a = load(&bytes, Repack::Never).unwrap().logits(&x, 0, true).unwrap();
        let b = load(&bytes, Repack::All).unwrap().logits(&x, 0, true).unwrap();
        let worst = a.iter().zip(&b).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        let scale = b.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(worst <= 1e-2 * scale.max(1.0), "{q:?}: {worst} at scale {scale}");
        // And it is close to the f32 model: the quantization works.
        let (_, agree) = compare(&a, &oracle(&hp, &m, &x));
        assert!(agree >= 6, "{q:?}: argmax agrees on {agree} of 12");
    }
}

/// A real file (the reference check itself is
/// `packaging/check-model-reference.py`, run by the `Qwen 3.5 record`
/// workflow): the released shapes load from the pinned GGUF, every tensor
/// has a type candle 0.11 reads, and a short prompt gives finite logits that
/// do not depend on the chunking.
#[test]
#[ignore = "needs a real Qwen 3.5 GGUF: QUIRE_QWEN35_GGUF=<path to model.gguf> cargo test -p quire-ai-engine --release -- --ignored real_file"]
fn real_file_loads_and_reads_a_prompt() {
    let path = std::env::var("QUIRE_QWEN35_GGUF").expect("QUIRE_QWEN35_GGUF");
    let mut f = std::fs::File::open(&path).unwrap();
    let ct = gguf_file::Content::read(&mut f).unwrap();
    let hp = Hparams::from_metadata(&ct.metadata).unwrap();
    assert!(hp == Hparams::qwen35_4b() || hp == Hparams::qwen35_9b(), "unexpected shape: {hp:?}");
    let mut model = Qwen35::from_gguf(ct, &mut f, Options::default()).unwrap();
    let x: Vec<u32> = (0..40).map(|i| 1000 + i * 37).collect();
    let whole = model.logits(&x, 0, false).unwrap();
    assert!(whole.iter().all(|v| v.is_finite()));
    model.clear();
    model.logits(&x[..17], 0, false).unwrap();
    let split = model.logits(&x[17..], 17, false).unwrap();
    let worst = whole.iter().zip(&split).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst <= 1e-3, "chunking changed the logits by {worst}");
}

/// Reads a GGUF built by [`to_gguf`] after `edit` changes its metadata or
/// tensor table, and returns the load error.
fn load_error(edit: impl FnOnce(&mut gguf_file::Content)) -> String {
    let hp = tiny();
    let bytes = to_gguf(&hp, &Hf::random(&hp, 13, true), Form::default());
    let mut r = Cursor::new(bytes);
    let mut ct = gguf_file::Content::read(&mut r).unwrap();
    edit(&mut ct);
    match Qwen35::from_gguf(ct, &mut r, Options::default()) {
        Ok(_) => panic!("loaded"),
        Err(e) => e.to_string(),
    }
}

/// Layouts no Qwen 3.5 conversion is known to write are refused, not
/// guessed: a fused attention `attn_qkv`, and Hadamard-rotated weights
/// (llama.cpp's `prism.hadamard.*` keys).
#[test]
fn unproven_layouts_are_refused_by_name() {
    let err = load_error(|ct| {
        let q = &ct.tensor_infos["blk.3.attn_q.weight"];
        let info = gguf_file::TensorInfo { ggml_dtype: q.ggml_dtype, shape: q.shape.clone(), offset: q.offset };
        ct.tensor_infos.insert("blk.3.attn_qkv.weight".into(), info);
    });
    assert!(err.contains("blk.3.attn_qkv.weight"), "{err}");
    for key in ["prism.hadamard.version", "prism.hadamard.gdn_v_grouped"] {
        let err = load_error(|ct| {
            ct.metadata.insert(key.into(), V::U32(1));
        });
        assert!(err.contains(key) && err.contains("Hadamard"), "{err}");
    }
}

/// Sizes from the file are bounded before anything is allocated: each
/// hostile value gives an error naming its key, never a panic or a huge
/// allocation.
#[test]
fn hostile_sizes_are_refused_before_allocating() {
    let hp = tiny();
    for (key, value) in [
        ("qwen35.block_count", u32::MAX),
        ("qwen35.embedding_length", u32::MAX),
        ("qwen35.feed_forward_length", 1 << 30),
        ("qwen35.attention.head_count", 100_000),
        ("qwen35.attention.key_length", 1 << 20),
        ("qwen35.rope.dimension_count", 1 << 20),
        ("qwen35.ssm.conv_kernel", 1 << 20),
        ("qwen35.ssm.state_size", 1 << 20),
        ("qwen35.ssm.time_step_rank", 1 << 20),
        ("qwen35.ssm.inner_size", u32::MAX),
        ("qwen35.nextn_predict_layers", 1000),
        ("qwen35.full_attention_interval", u32::MAX),
    ] {
        let mut md = metadata(&hp, Form { interval_only: true, ..Form::default() });
        md.insert(key.into(), V::U32(value));
        let err = Hparams::from_metadata(&md).unwrap_err().to_string();
        assert!(err.contains(key), "{key}: {err}");
    }
    // Within each bound, but a state that would need gigabytes.
    let mut md = metadata(&hp, Form { interval_only: true, ..Form::default() });
    md.insert("qwen35.block_count".into(), V::U32(256));
    md.insert("qwen35.ssm.state_size".into(), V::U32(512));
    md.insert("qwen35.ssm.time_step_rank".into(), V::U32(256));
    md.insert("qwen35.ssm.group_count".into(), V::U32(256));
    md.insert("qwen35.ssm.inner_size".into(), V::U32(256 * 512));
    let err = Hparams::from_metadata(&md).unwrap_err().to_string();
    assert!(err.contains("DeltaNet state"), "{err}");
    // More layer kinds than layers, or a negative count: errors.
    let mut md = metadata(&hp, Form::default());
    md.insert("qwen35.attention.recurrent_layers".into(), V::Array(vec![V::Bool(true)]));
    assert!(Hparams::from_metadata(&md).is_err());
    md.insert("qwen35.block_count".into(), V::I32(-1));
    assert!(Hparams::from_metadata(&md).is_err());
    // A tensor declared far larger than the file: refused before reading.
    let err = load_error(|ct| {
        let info = ct.tensor_infos.get_mut("blk.0.ffn_up.weight").unwrap();
        info.shape = candle_core::Shape::from((1 << 20, 1 << 20));
    });
    assert!(err.contains("blk.0.ffn_up.weight"), "{err}");
    // A token table of the wrong width.
    let err = load_error(|ct| {
        let info = ct.tensor_infos.get_mut("token_embd.weight").unwrap();
        info.shape = candle_core::Shape::from((VOCAB * 2, 16));
    });
    assert!(err.contains("token_embd.weight"), "{err}");
}

/// ggml's `dequantize_row_q4_K` (ggml/src/ggml-quants.c), ported line by
/// line: per 256-value block, `d` and `dmin` (f16), 12 bytes of 6-bit
/// scales and mins (`get_scale_min_k4`), then 128 bytes of 4-bit values.
fn ggml_q4k(block: &[u8]) -> Vec<f32> {
    let d = half::f16::from_le_bytes([block[0], block[1]]).to_f32();
    let min = half::f16::from_le_bytes([block[2], block[3]]).to_f32();
    let scales = &block[4..16];
    let scale_min = |j: usize| -> (u8, u8) {
        if j < 4 {
            (scales[j] & 63, scales[j + 4] & 63)
        } else {
            ((scales[j + 4] & 0xF) | ((scales[j - 4] >> 6) << 4), (scales[j + 4] >> 4) | ((scales[j] >> 6) << 4))
        }
    };
    let mut y = Vec::with_capacity(256);
    let mut q = &block[16..144];
    let mut is = 0;
    for _ in 0..4 {
        let (sc, m) = scale_min(is);
        let (d1, m1) = (d * f32::from(sc), min * f32::from(m));
        let (sc, m) = scale_min(is + 1);
        let (d2, m2) = (d * f32::from(sc), min * f32::from(m));
        y.extend(q[..32].iter().map(|b| d1 * f32::from(b & 0xF) - m1));
        y.extend(q[..32].iter().map(|b| d2 * f32::from(b >> 4) - m2));
        q = &q[32..];
        is += 2;
    }
    y
}

/// ggml's `dequantize_row_q6_K`, ported likewise: 128 bytes of low 4 bits,
/// 64 of high 2 bits, 16 signed scales, then `d` (f16).
fn ggml_q6k(block: &[u8]) -> Vec<f32> {
    let (mut ql, mut qh) = (&block[..128], &block[128..192]);
    let mut sc: Vec<i8> = block[192..208].iter().map(|b| i8::from_le_bytes([*b])).collect();
    let d = half::f16::from_le_bytes([block[208], block[209]]).to_f32();
    let mut y = vec![0f32; 256];
    for n in 0..2 {
        let out = &mut y[n * 128..][..128];
        for l in 0..32 {
            let is = l / 16;
            let q = |low: u8, shift: u8| f32::from(i8::try_from((low & 0xF) | (((qh[l] >> shift) & 3) << 4)).unwrap() - 32);
            out[l] = d * f32::from(sc[is]) * q(ql[l], 0);
            out[l + 32] = d * f32::from(sc[is + 2]) * q(ql[l + 32], 2);
            out[l + 64] = d * f32::from(sc[is + 4]) * q(ql[l] >> 4, 4);
            out[l + 96] = d * f32::from(sc[is + 6]) * q(ql[l + 32] >> 4, 6);
        }
        ql = &ql[64..];
        qh = &qh[32..];
        sc.drain(..8);
    }
    y
}

/// Hand-built `Q4_K` and `Q6_K` blocks (seeded bytes, scales chosen so the
/// values are finite): candle's dequantisation, its row lookup (the
/// quantized token table) and its block matmul (the compact weights) all
/// give ggml's numbers.
#[test]
fn k_quant_blocks_decode_as_ggml_defines_them() {
    use candle_core::quantized::GgmlDType;
    let mut rng = Rng::new(77);
    let rows = 3;
    for (dtype, size, reference) in [(GgmlDType::Q4K, 144usize, ggml_q4k as fn(&[u8]) -> Vec<f32>), (GgmlDType::Q6K, 210, ggml_q6k)] {
        let mut bytes: Vec<u8> = (0..rows * size).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        for b in bytes.chunks_mut(size) {
            let d = half::f16::from_f32(0.01 + rng.unit().abs() * 0.05).to_le_bytes();
            if dtype == GgmlDType::Q4K {
                b[..2].copy_from_slice(&d);
                b[2..4].copy_from_slice(&half::f16::from_f32(0.02).to_le_bytes());
            } else {
                b[208..210].copy_from_slice(&d);
            }
        }
        let want: Vec<f32> = bytes.chunks(size).flat_map(reference).collect();
        let w = dtype.from_data(std::borrow::Cow::Owned(bytes));
        let candle_core::CpuStorage::F32(got) = w.dequantize(rows * 256).unwrap() else { panic!("f32") };
        assert_eq!(got, want, "{dtype:?} dequantize");
        let candle_core::CpuStorage::F32(row) = w.embedding(&[2, 0], rows, 256).unwrap() else { panic!("f32") };
        assert_eq!(row[..256], want[512..], "{dtype:?} row lookup");
        // x (2 × 256) times the 3 rows, against the f64 product of ggml's values.
        let x: Vec<f32> = rng.vec(2 * 256, 1.0);
        let mut dst = vec![0f32; 2 * rows];
        w.matmul_t((2, 256, rows), &x, &mut dst).unwrap();
        for (i, got) in dst.iter().enumerate() {
            let (m, n) = (i / rows, i % rows);
            let exact: f64 = (0..256).map(|k| f64::from(x[m * 256 + k]) * f64::from(want[n * 256 + k])).sum();
            let scale: f64 = (0..256).map(|k| (f64::from(x[m * 256 + k]) * f64::from(want[n * 256 + k])).abs()).sum();
            // The block kernels quantize x to Q8_K first: about 1% of the terms' size.
            assert!((f64::from(*got) - exact).abs() <= 0.01 * scale.max(1e-6), "{dtype:?} matmul {i}: {got} vs {exact}");
        }
    }
}
