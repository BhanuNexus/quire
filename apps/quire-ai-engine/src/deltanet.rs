// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The arithmetic of Qwen 3.5's layers that candle 0.11 does not have.
//!
//! Plain loops over `f32` slices (`docs/design/0.14-local-ai.md` §3.1):
//! the Gated DeltaNet recurrence, its causal depthwise convolution, the
//! three norms and the partial rotary step of the attention layers.
//!
//! Layouts are row-major with the token outermost: a chunk of `L` tokens
//! with `H` heads of `D` values is `L × H × D`. The formulas follow the
//! transformers model (`models/qwen3_5/modeling_qwen3_5.py`) and llama.cpp's
//! CPU kernel (`ggml_compute_forward_gated_delta_net`), which agree:
//!
//! ```text
//! S ← exp(g_t) · S
//! u ← β_t · (v_t − Sᵀ k_t)
//! S ← S + k_t uᵀ
//! o_t ← Sᵀ q_t / sqrt(D)
//! ```
//!
//! One form serves prompts and decoding: the recurrence walks the tokens of
//! a chunk one by one (head outermost, so each head's state stays in cache),
//! and the heads are split across threads.

/// `1 / (1 + e^-x)`.
pub fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// `x · sigmoid(x)`.
pub fn silu(x: f32) -> f32 {
    x * sigmoid(x)
}

/// `ln(1 + e^x)`, linear above 20 as torch's `softplus` is.
pub fn softplus(x: f32) -> f32 {
    if x > 20.0 { x } else { x.exp().ln_1p() }
}

/// RMS norm of each row of `w.len()` values: `x / sqrt(mean(x²) + eps) · w`.
/// (Block norms and the attention q/k norms of a GGUF: the converter has
/// already added transformers' `1 +` to `w`.)
pub fn rms_norm_rows(x: &[f32], w: &[f32], eps: f32) -> Vec<f32> {
    let n = w.len();
    let mut out = Vec::with_capacity(x.len());
    for row in x.chunks_exact(n) {
        let inv = inv_rms(row, eps);
        out.extend(row.iter().zip(w).map(|(v, w)| v * inv * w));
    }
    out
}

fn inv_rms(row: &[f32], eps: f32) -> f32 {
    let mean = row.iter().map(|v| v * v).sum::<f32>() / row.len() as f32;
    1.0 / (mean + eps).sqrt()
}

/// L2 norm of each row of `n` values, in place: `x / sqrt(sum(x²) + eps)`.
pub fn l2_norm_rows(x: &mut [f32], n: usize, eps: f32) {
    for row in x.chunks_exact_mut(n) {
        let inv = 1.0 / (row.iter().map(|v| v * v).sum::<f32>() + eps).sqrt();
        for v in row.iter_mut() {
            *v *= inv;
        }
    }
}

/// The DeltaNet output norm: per row of `w.len()` values,
/// `rms_norm(o) · w · silu(z)` (no `1 +` on this weight).
pub fn gated_rms_norm(o: &[f32], z: &[f32], w: &[f32], eps: f32) -> Vec<f32> {
    let n = w.len();
    let mut out = Vec::with_capacity(o.len());
    for (row, gate) in o.chunks_exact(n).zip(z.chunks_exact(n)) {
        let inv = inv_rms(row, eps);
        out.extend(row.iter().zip(w).zip(gate).map(|((v, w), g)| v * inv * w * silu(*g)));
    }
    out
}

/// Causal depthwise convolution over time, then SiLU.
///
/// `x` is `L × C`; `w` is `C × K` (the GGUF's `ssm_conv1d`, the last tap
/// multiplying the newest input); `tail` holds the previous chunk's last
/// `K − 1` inputs, `(K − 1) × C` (zeros at the start), and is updated.
pub fn causal_conv_silu(x: &[f32], w: &[f32], tail: &mut [f32], channels: usize, kernel: usize) -> Vec<f32> {
    let l = x.len() / channels;
    let keep = kernel - 1;
    // The inputs as one window: the carried tail, then this chunk.
    let at = |t: usize, c: usize| if t < keep { tail[t * channels + c] } else { x[(t - keep) * channels + c] };
    let mut out = vec![0f32; l * channels];
    for t in 0..l {
        for c in 0..channels {
            let taps = &w[c * kernel..(c + 1) * kernel];
            let s: f32 = taps.iter().enumerate().map(|(j, w)| w * at(t + j, c)).sum();
            out[t * channels + c] = silu(s);
        }
    }
    // The new tail: the last K − 1 inputs of the window.
    let total = keep + l;
    let new_tail: Vec<f32> = (total - keep..total).flat_map(|t| (0..channels).map(move |c| (t, c))).map(|(t, c)| at(t, c)).collect();
    tail.copy_from_slice(&new_tail);
    out
}

/// The shape of one DeltaNet layer's recurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeltaShape {
    /// Query and key heads (16 in both released sizes).
    pub k_heads: usize,
    /// Value heads (32); a multiple of `k_heads`.
    pub v_heads: usize,
    /// Values per head, the same for keys and values (128).
    pub dim: usize,
}

impl DeltaShape {
    /// The floats of one layer's state: a `dim × dim` matrix per value head.
    pub const fn state_len(&self) -> usize {
        self.v_heads * self.dim * self.dim
    }

    /// The key head that value head `h` reads: llama.cpp's tiled order
    /// (`h mod k_heads`), which its converter makes true by permuting the
    /// value heads of every DeltaNet weight. (transformers uses
    /// `h div (v_heads / k_heads)` on the unpermuted weights.)
    pub const fn key_head(&self, h: usize) -> usize {
        h % self.k_heads
    }
}

/// The gated delta rule over one chunk.
///
/// * `state`: `v_heads × dim × dim`, stored per head with the value index
///   outer (`s[j·dim + i]` is `S[i][j]`, key `i`, value `j`), as llama.cpp
///   stores it; updated in place.
/// * `q`, `k`: `L × k_heads × dim`, already L2-normalised (not yet scaled).
/// * `v`: `L × v_heads × dim`; `g`, `beta`: `L × v_heads`.
///
/// Returns `o`, `L × v_heads × dim`, including the `1/sqrt(dim)` scale.
pub fn gated_delta_rule(
    shape: DeltaShape,
    state: &mut [f32],
    q: &[f32],
    k: &[f32],
    v: &[f32],
    g: &[f32],
    beta: &[f32],
    threads: usize,
) -> Vec<f32> {
    let DeltaShape { k_heads, v_heads, dim } = shape;
    let l = g.len() / v_heads;
    let per_thread = v_heads.div_ceil(threads.clamp(1, v_heads.max(1)));
    // Each head's outputs, `L × dim`, written by the thread that owns it.
    let mut per_head = vec![0f32; v_heads * l * dim];
    let scale = 1.0 / (dim as f32).sqrt();
    let run = |h0: usize, states: &mut [f32], outs: &mut [f32]| {
        for (i, (s, out)) in states.chunks_exact_mut(dim * dim).zip(outs.chunks_exact_mut(l * dim)).enumerate() {
            let h = h0 + i;
            let kh = shape.key_head(h);
            let mut u = vec![0f32; dim];
            for t in 0..l {
                let qt = &q[(t * k_heads + kh) * dim..][..dim];
                let kt = &k[(t * k_heads + kh) * dim..][..dim];
                let vt = &v[(t * v_heads + h) * dim..][..dim];
                let decay = g[t * v_heads + h].exp();
                let b = beta[t * v_heads + h];
                for x in s.iter_mut() {
                    *x *= decay;
                }
                for (j, row) in s.chunks_exact(dim).enumerate() {
                    let sk: f32 = row.iter().zip(kt).map(|(a, b)| a * b).sum();
                    u[j] = b * (vt[j] - sk);
                }
                for (j, row) in s.chunks_exact_mut(dim).enumerate() {
                    row.iter_mut().zip(kt).for_each(|(x, kv)| *x += kv * u[j]);
                }
                let o_t = &mut out[t * dim..][..dim];
                for (j, row) in s.chunks_exact(dim).enumerate() {
                    o_t[j] = row.iter().zip(qt).map(|(a, b)| a * b).sum::<f32>() * scale;
                }
            }
        }
    };
    if per_thread >= v_heads {
        run(0, state, &mut per_head);
    } else {
        std::thread::scope(|scope| {
            for (n, (states, outs)) in state.chunks_mut(per_thread * dim * dim).zip(per_head.chunks_mut(per_thread * l * dim)).enumerate() {
                let run = &run;
                scope.spawn(move || run(n * per_thread, states, outs));
            }
        });
    }
    // Back to token-outer order.
    let mut o = vec![0f32; l * v_heads * dim];
    for h in 0..v_heads {
        for t in 0..l {
            o[(t * v_heads + h) * dim..][..dim].copy_from_slice(&per_head[(h * l + t) * dim..][..dim]);
        }
    }
    o
}

/// Rotary position embedding on the first `rot_dim` values of each head.
///
/// "Rotate half": value `i` pairs with `i + rot_dim/2`); the rest pass
/// through. For text, Qwen 3.5's interleaved multimodal rotary has the same
/// position in all three rows and reduces to this.
#[derive(Debug, Clone)]
pub struct PartialRotary {
    rot_dim: usize,
    /// `max_pos × rot_dim/2` each.
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl PartialRotary {
    pub fn new(rot_dim: usize, theta: f64, max_pos: usize) -> Self {
        let half = rot_dim / 2;
        let inv = inv_freq(rot_dim, theta);
        let mut cos = Vec::with_capacity(max_pos * half);
        let mut sin = Vec::with_capacity(max_pos * half);
        for p in 0..max_pos {
            for f in &inv {
                // In f32 like transformers' `position * inv_freq`.
                let a = p as f32 * f;
                cos.push(a.cos());
                sin.push(a.sin());
            }
        }
        Self { rot_dim, cos, sin }
    }

    /// The most positions the tables cover.
    pub fn max_pos(&self) -> usize {
        self.cos.len() / (self.rot_dim / 2).max(1)
    }

    /// Rotates `x` (`L × heads × head_dim`) for positions `offset..offset+L`.
    pub fn apply(&self, x: &mut [f32], heads: usize, head_dim: usize, offset: usize) {
        let half = self.rot_dim / 2;
        for (t, tok) in x.chunks_exact_mut(heads * head_dim).enumerate() {
            let cos = &self.cos[(offset + t) * half..][..half];
            let sin = &self.sin[(offset + t) * half..][..half];
            for head in tok.chunks_exact_mut(head_dim) {
                rotate(head, cos, sin);
            }
        }
    }
}

/// `1 / theta^(2i / rot_dim)` for `i < rot_dim/2`.
fn inv_freq(rot_dim: usize, theta: f64) -> Vec<f32> {
    (0..rot_dim / 2).map(|i| (1.0 / theta.powf((2 * i) as f64 / rot_dim as f64)) as f32).collect()
}

fn rotate(head: &mut [f32], cos: &[f32], sin: &[f32]) {
    let half = cos.len();
    for i in 0..half {
        let (a, b) = (head[i], head[i + half]);
        head[i] = a * cos[i] - b * sin[i];
        head[i + half] = b * cos[i] + a * sin[i];
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Each kernel against a naive `f64` oracle written from the equations
    //! (`docs/design/0.14-local-ai.md` §3.7), on seeded random inputs.
    use super::*;

    /// splitmix64: seeded, so every run sees the same numbers.
    pub(crate) struct Rng(u64);

    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed)
        }

        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        /// Uniform in `[-1, 1)`.
        pub(crate) fn unit(&mut self) -> f32 {
            ((self.next_u64() >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }

        pub(crate) fn vec(&mut self, n: usize, scale: f32) -> Vec<f32> {
            (0..n).map(|_| self.unit() * scale).collect()
        }
    }

    fn close(got: &[f32], want: &[f64], rel: f64) {
        assert_eq!(got.len(), want.len());
        let norm = want.iter().map(|w| w.abs()).fold(0.0, f64::max).max(1e-12);
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert!((f64::from(*g) - w).abs() <= rel * norm, "at {i}: {g} vs {w} (scale {norm})");
        }
    }

    /// The recurrence in f64, straight from the equations, with value head
    /// `h` reading key head `h mod k_heads`.
    fn oracle_delta(shape: DeltaShape, s0: &[f32], q: &[f32], k: &[f32], v: &[f32], g: &[f32], beta: &[f32]) -> (Vec<f64>, Vec<f64>) {
        let DeltaShape { k_heads, v_heads, dim } = shape;
        let l = g.len() / v_heads;
        // S[h][i][j], key i, value j.
        let mut s = vec![0f64; v_heads * dim * dim];
        for h in 0..v_heads {
            for i in 0..dim {
                for j in 0..dim {
                    s[(h * dim + i) * dim + j] = f64::from(s0[(h * dim + j) * dim + i]);
                }
            }
        }
        let mut o = vec![0f64; l * v_heads * dim];
        for t in 0..l {
            for h in 0..v_heads {
                let kh = h % k_heads;
                let qv = |i: usize| f64::from(q[(t * k_heads + kh) * dim + i]) / (dim as f64).sqrt();
                let kv = |i: usize| f64::from(k[(t * k_heads + kh) * dim + i]);
                let vv = |j: usize| f64::from(v[(t * v_heads + h) * dim + j]);
                let d = f64::from(g[t * v_heads + h]).exp();
                let b = f64::from(beta[t * v_heads + h]);
                let sh = &mut s[h * dim * dim..][..dim * dim];
                for x in sh.iter_mut() {
                    *x *= d;
                }
                let u: Vec<f64> = (0..dim).map(|j| b * (vv(j) - (0..dim).map(|i| sh[i * dim + j] * kv(i)).sum::<f64>())).collect();
                for i in 0..dim {
                    for j in 0..dim {
                        sh[i * dim + j] += kv(i) * u[j];
                    }
                }
                for j in 0..dim {
                    o[(t * v_heads + h) * dim + j] = (0..dim).map(|i| sh[i * dim + j] * qv(i)).sum();
                }
            }
        }
        // Back to the kernel's layout for comparison.
        let mut st = vec![0f64; s.len()];
        for h in 0..v_heads {
            for i in 0..dim {
                for j in 0..dim {
                    st[(h * dim + j) * dim + i] = s[(h * dim + i) * dim + j];
                }
            }
        }
        (o, st)
    }

    /// A state and the inputs of one chunk: `(s0, q, k, v, g, beta)`.
    type Case = (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>);

    fn random_case(rng: &mut Rng, shape: DeltaShape, l: usize) -> Case {
        let DeltaShape { k_heads, v_heads, dim } = shape;
        let mut q = rng.vec(l * k_heads * dim, 1.0);
        let mut k = rng.vec(l * k_heads * dim, 1.0);
        l2_norm_rows(&mut q, dim, 1e-6);
        l2_norm_rows(&mut k, dim, 1e-6);
        let v = rng.vec(l * v_heads * dim, 1.0);
        // g ≤ 0 as in the model (decay in (0, 1]); beta in (0, 1).
        let g: Vec<f32> = (0..l * v_heads).map(|_| -rng.unit().abs() * 2.0).collect();
        let beta: Vec<f32> = (0..l * v_heads).map(|_| sigmoid(rng.unit() * 3.0)).collect();
        let s0 = rng.vec(shape.state_len(), 0.5);
        (s0, q, k, v, g, beta)
    }

    #[test]
    fn the_recurrence_matches_the_f64_oracle() {
        let mut rng = Rng::new(1);
        for (shape, l, threads) in [
            (DeltaShape { k_heads: 2, v_heads: 4, dim: 16 }, 9, 1),
            (DeltaShape { k_heads: 2, v_heads: 4, dim: 16 }, 9, 3),
            (DeltaShape { k_heads: 1, v_heads: 1, dim: 8 }, 1, 1),
            (DeltaShape { k_heads: 4, v_heads: 8, dim: 32 }, 17, 4),
        ] {
            let (s0, q, k, v, g, beta) = random_case(&mut rng, shape, l);
            let mut s = s0.clone();
            let o = gated_delta_rule(shape, &mut s, &q, &k, &v, &g, &beta, threads);
            let (want_o, want_s) = oracle_delta(shape, &s0, &q, &k, &v, &g, &beta);
            close(&o, &want_o, 1e-5);
            close(&s, &want_s, 1e-5);
        }
    }

    /// One head, `dim` 2, worked by hand: `S₀ = 0`, `k = (1, 0)`,
    /// `v = (2, 4)`, `β = 0.5`, `g = 0`: `u = 0.5·(v − 0) = (1, 2)`,
    /// `S = k uᵀ = [[1, 2], [0, 0]]`; with `q = (1, 0)`,
    /// `o = Sᵀ q / √2 = (1, 2)/√2`. A second token with `g = ln 0.5`,
    /// `k = (0, 1)`, `v = (6, 8)`, `β = 1`: `S = [[0.5, 1], [0, 0]]`,
    /// `Sᵀ k = (0, 0)`, `u = (6, 8)`, `S = [[0.5, 1], [6, 8]]`; with
    /// `q = (0, 1)`, `o = (6, 8)/√2`.
    #[test]
    fn the_recurrence_matches_a_case_worked_by_hand() {
        let shape = DeltaShape { k_heads: 1, v_heads: 1, dim: 2 };
        let mut s = vec![0f32; 4];
        let q = [1.0, 0.0, 0.0, 1.0];
        let k = [1.0, 0.0, 0.0, 1.0];
        let v = [2.0, 4.0, 6.0, 8.0];
        let g = [0.0, 0.5f32.ln()];
        let beta = [0.5, 1.0];
        let o = gated_delta_rule(shape, &mut s, &q, &k, &v, &g, &beta, 1);
        let r = std::f32::consts::FRAC_1_SQRT_2;
        let want = [r, 2.0 * r, 6.0 * r, 8.0 * r];
        for (a, b) in o.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{o:?} vs {want:?}");
        }
        // Stored value-outer: s[j·2 + i] = S[i][j]; S = [[0.5, 1], [6, 8]].
        let want_s = [0.5, 6.0, 1.0, 8.0];
        for (a, b) in s.iter().zip(want_s) {
            assert!((a - b).abs() < 1e-6, "{s:?} vs {want_s:?}");
        }
    }

    /// The tiled head order matters: with 2 key heads and 4 value heads,
    /// value head 1 reads key head 1 (tiled), not key head 0 (grouped).
    #[test]
    fn value_heads_read_key_heads_in_tiled_order() {
        let shape = DeltaShape { k_heads: 2, v_heads: 4, dim: 2 };
        assert_eq!((0..4).map(|h| shape.key_head(h)).collect::<Vec<_>>(), [0, 1, 0, 1]);
        let mut s = vec![0f32; shape.state_len()];
        // Key head 0 points along x, key head 1 along y; queries the same.
        let qk = [1.0, 0.0, 0.0, 1.0];
        let v = [1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0];
        let o = gated_delta_rule(shape, &mut s, &qk, &qk, &v, &[0.0; 4], &[1.0; 4], 2);
        // Every head stores v along its key and reads it back with the same query.
        let r = std::f32::consts::FRAC_1_SQRT_2;
        for (a, b) in o.iter().zip(v) {
            assert!((a - b * r).abs() < 1e-6, "{o:?}");
        }
        // Head 1's state lives on key row 1 (y), which the grouped order would not give.
        assert_eq!(&s[4..8], &[0.0, 2.0, 0.0, 2.0]);
    }

    #[test]
    fn the_recurrence_carries_state_across_any_split_of_the_chunk() {
        let shape = DeltaShape { k_heads: 2, v_heads: 4, dim: 8 };
        let l = 23;
        let mut rng = Rng::new(7);
        let (s0, q, k, v, g, beta) = random_case(&mut rng, shape, l);
        let mut whole = s0.clone();
        let o = gated_delta_rule(shape, &mut whole, &q, &k, &v, &g, &beta, 2);
        for split in [1usize, 5, 7, 22] {
            let mut s = s0.clone();
            let kq = shape.k_heads * shape.dim;
            let vd = shape.v_heads * shape.dim;
            let vh = shape.v_heads;
            let mut a = gated_delta_rule(
                shape,
                &mut s,
                &q[..split * kq],
                &k[..split * kq],
                &v[..split * vd],
                &g[..split * vh],
                &beta[..split * vh],
                1,
            );
            a.extend(gated_delta_rule(
                shape,
                &mut s,
                &q[split * kq..],
                &k[split * kq..],
                &v[split * vd..],
                &g[split * vh..],
                &beta[split * vh..],
                3,
            ));
            for (x, y) in a.iter().zip(&o) {
                assert!((x - y).abs() <= 1e-6 * y.abs().max(1.0), "split {split}");
            }
            assert_eq!(s.len(), whole.len());
        }
    }

    #[test]
    fn the_convolution_matches_the_oracle_and_carries_its_tail() {
        let mut rng = Rng::new(3);
        let (c, kk, l) = (5usize, 4usize, 11usize);
        let x = rng.vec(l * c, 1.0);
        let w = rng.vec(c * kk, 1.0);
        // Oracle: zero-padded on the left, then silu, in f64.
        let at = |t: isize, ch: usize| if t < 0 { 0.0 } else { f64::from(x[t as usize * c + ch]) };
        let want: Vec<f64> = (0..l)
            .flat_map(|t| (0..c).map(move |ch| (t, ch)))
            .map(|(t, ch)| {
                let s: f64 = (0..kk).map(|j| f64::from(w[ch * kk + j]) * at(t as isize - (kk as isize - 1) + j as isize, ch)).sum();
                s / (1.0 + (-s).exp())
            })
            .collect();
        let mut tail = vec![0f32; (kk - 1) * c];
        close(&causal_conv_silu(&x, &w, &mut tail, c, kk), &want, 1e-6);
        // In pieces of 1, 3 and 7 tokens, carrying the tail: the same.
        let mut tail = vec![0f32; (kk - 1) * c];
        let mut got = Vec::new();
        let mut t = 0;
        for n in [1usize, 3, 7] {
            got.extend(causal_conv_silu(&x[t * c..(t + n) * c], &w, &mut tail, c, kk));
            t += n;
        }
        close(&got, &want, 1e-6);
        // The tail is the last K − 1 inputs.
        assert_eq!(tail, x[(l - (kk - 1)) * c..]);
    }

    #[test]
    fn the_norms_match_their_formulas() {
        let mut rng = Rng::new(5);
        let n = 16;
        let x = rng.vec(3 * n, 2.0);
        let w = rng.vec(n, 1.0);
        let z = rng.vec(3 * n, 2.0);
        let eps = 1e-6f64;
        let rms = |row: &[f32]| (row.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / n as f64 + eps).sqrt();
        let want: Vec<f64> = x.chunks(n).flat_map(|r| r.iter().zip(&w).map(move |(v, w)| f64::from(*v) / rms(r) * f64::from(*w))).collect();
        close(&rms_norm_rows(&x, &w, 1e-6), &want, 1e-6);
        let want_g: Vec<f64> = x
            .chunks(n)
            .zip(z.chunks(n))
            .flat_map(|(r, zr)| {
                r.iter().zip(&w).zip(zr).map(move |((v, w), g)| {
                    let g = f64::from(*g);
                    f64::from(*v) / rms(r) * f64::from(*w) * g / (1.0 + (-g).exp())
                })
            })
            .collect();
        close(&gated_rms_norm(&x, &z, &w, 1e-6), &want_g, 1e-6);
        let mut y = x.clone();
        l2_norm_rows(&mut y, n, 1e-6);
        let want_l2: Vec<f64> = x
            .chunks(n)
            .flat_map(|r| {
                let s = (r.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() + eps).sqrt();
                r.iter().map(move |v| f64::from(*v) / s)
            })
            .collect();
        close(&y, &want_l2, 1e-6);
        // llama.cpp writes the L2 norm as an RMS norm with eps/n, scaled by
        // 1/sqrt(n) (`build_gdn_l2_norm`): the same numbers.
        let ones = vec![1f32; n];
        let via_rms: Vec<f32> = rms_norm_rows(&x, &ones, 1e-6 / n as f32).iter().map(|v| v / (n as f32).sqrt()).collect();
        close(&via_rms, &want_l2, 1e-5);
        assert!((softplus(0.0) - 2f32.ln()).abs() < 1e-7 && (softplus(30.0) - 30.0).abs() < 1e-6 && softplus(-30.0) > 0.0);
    }

    #[test]
    fn partial_rotary_matches_the_oracle_and_passes_the_rest_through() {
        let mut rng = Rng::new(9);
        let (heads, hd, rot, l, off) = (3usize, 32usize, 8usize, 5usize, 100usize);
        let theta = 10_000_000.0;
        let x = rng.vec(l * heads * hd, 1.0);
        let rope = PartialRotary::new(rot, theta, 200);
        let mut y = x.clone();
        rope.apply(&mut y, heads, hd, off);
        for t in 0..l {
            for h in 0..heads {
                let base = (t * heads + h) * hd;
                for i in 0..rot / 2 {
                    let a = (off + t) as f64 / theta.powf((2 * i) as f64 / rot as f64);
                    let (x1, x2) = (f64::from(x[base + i]), f64::from(x[base + i + rot / 2]));
                    let want1 = x1 * a.cos() - x2 * a.sin();
                    let want2 = x2 * a.cos() + x1 * a.sin();
                    assert!((f64::from(y[base + i]) - want1).abs() < 1e-4, "t {t} h {h} i {i}");
                    assert!((f64::from(y[base + i + rot / 2]) - want2).abs() < 1e-4);
                }
                assert_eq!(&y[base + rot..base + hd], &x[base + rot..base + hd], "dims past the rotary pass through");
            }
        }
    }

    /// transformers' interleaved multimodal rotary
    /// (`Qwen3_5TextRotaryEmbedding.recomposition_frequencies` with
    /// `mrope_section` [11, 11, 10]): frequency `i` takes its position from
    /// row 1 (height) when `i mod 3 == 1` and `i < 33`, from row 2 (width)
    /// when `i mod 3 == 2` and `i < 30`, else from row 0 (time). With the same
    /// position in all three rows (text), it is the 1-D rotary.
    #[test]
    fn the_sectioned_rotary_with_equal_positions_is_the_one_dimensional_one() {
        let (rot, theta) = (64usize, 10_000_000.0f64);
        let section = [11usize, 11, 10];
        let inv = inv_freq(rot, theta);
        let sectioned = |pos: [usize; 3]| -> Vec<f32> {
            (0..rot / 2)
                .map(|i| {
                    let row = if i % 3 == 1 && i < section[1] * 3 {
                        1
                    } else if i % 3 == 2 && i < section[2] * 3 {
                        2
                    } else {
                        0
                    };
                    pos[row] as f32 * inv[i]
                })
                .collect()
        };
        let rope = PartialRotary::new(rot, theta, 5000);
        for p in [0usize, 1, 17, 4095] {
            let angles = sectioned([p, p, p]);
            let mut a = vec![0f32; rot];
            let mut b = a.clone();
            for (i, v) in a.iter_mut().enumerate() {
                *v = i as f32 + 1.0;
            }
            b.copy_from_slice(&a);
            rope.apply(&mut a, 1, rot, p);
            let cos: Vec<f32> = angles.iter().map(|x| x.cos()).collect();
            let sin: Vec<f32> = angles.iter().map(|x| x.sin()).collect();
            rotate(&mut b, &cos, &sin);
            assert_eq!(a, b, "position {p}");
        }
        // And it is not the identity: different rows give different angles.
        assert_ne!(sectioned([5, 6, 7]), sectioned([5, 5, 5]));
    }
}
