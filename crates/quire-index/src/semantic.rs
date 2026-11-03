// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Meaning-based retrieval.
//!
//! [`Embedder`] is the port: anything that turns passages and queries into
//! comparable vectors. The built-in [`Lsa`] embedder learns a latent semantic
//! space from the vault itself (TF-IDF reduced by randomized truncated SVD),
//! so terms that co-occur across notes land near each other and a query can
//! match a passage that shares none of its words. It needs no model download
//! and rebuilds in the background in well under a second for thousands of
//! passages. A neural embedder can implement the same trait.

use std::collections::HashMap;

use quire_vault::NotePath;

use crate::chunk::Chunk;
use crate::tokenize::content_tokens;

/// Turns text into vectors comparable by cosine similarity.
pub trait Embedder: Send + Sync {
    fn name(&self) -> &str;
    /// One text. May return an empty vector if the embedder is unavailable.
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Many texts at once; remote embedders override this to batch requests.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }
}

/// Passage vectors persisted between runs, keyed by (model, text hash), so a
/// rebuild with a slow embedder only embeds passages that changed.
#[derive(Debug, Default)]
pub struct VectorCache {
    path: Option<std::path::PathBuf>,
    model: String,
    map: HashMap<u64, Vec<f32>>,
    dirty: bool,
}

fn text_hash(model: &str, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

impl VectorCache {
    /// Load from `path` (missing or unreadable files start empty).
    pub fn open(path: impl Into<std::path::PathBuf>, model: &str) -> Self {
        let path = path.into();
        let mut map = HashMap::new();
        if let Ok(bytes) = std::fs::read(&path) {
            // Format: [u64 key][u32 dims][dims × f32] repeated, little endian.
            let mut i = 0;
            while i + 12 <= bytes.len() {
                let key = u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
                let n = u32::from_le_bytes(bytes[i + 8..i + 12].try_into().expect("4 bytes")) as usize;
                i += 12;
                if i + n * 4 > bytes.len() {
                    break;
                }
                let v = bytes[i..i + n * 4].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
                map.insert(key, v);
                i += n * 4;
            }
        }
        Self { path: Some(path), model: model.to_string(), map, dirty: false }
    }

    pub fn in_memory(model: &str) -> Self {
        Self { model: model.to_string(), ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Keep only `live` keys, then write to disk if anything changed.
    pub fn save(&mut self, live: &std::collections::HashSet<u64>) -> std::io::Result<()> {
        let before = self.map.len();
        self.map.retain(|k, _| live.contains(k));
        if !self.dirty && before == self.map.len() {
            return Ok(());
        }
        let Some(path) = &self.path else { return Ok(()) };
        let mut out = Vec::with_capacity(self.map.len() * 400);
        for (k, v) in &self.map {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, path)?;
        self.dirty = false;
        Ok(())
    }
}

/// A built index: passages, their unit vectors, and the embedder for queries.
pub struct SemanticIndex {
    pub chunks: Vec<Chunk>,
    vectors: Vec<Vec<f32>>,
    embedder: Box<dyn Embedder>,
}

impl std::fmt::Debug for SemanticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticIndex").field("chunks", &self.chunks.len()).field("embedder", &self.embedder.name()).finish_non_exhaustive()
    }
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-9 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl SemanticIndex {
    pub fn new(chunks: Vec<Chunk>, embedder: Box<dyn Embedder>) -> Self {
        let vectors = chunks
            .iter()
            .map(|c| {
                let mut v = embedder.embed(&c.text);
                normalize(&mut v);
                v
            })
            .collect();
        Self { chunks, vectors, embedder }
    }

    /// Build with any embedder, reusing cached vectors and batching the rest.
    pub fn try_with_cache(chunks: Vec<Chunk>, embedder: Box<dyn Embedder>, cache: &mut VectorCache) -> Result<Self, String> {
        let keys: Vec<u64> = chunks.iter().map(|c| text_hash(&cache.model, &c.text)).collect();
        let missing: Vec<usize> = (0..chunks.len()).filter(|&i| !cache.map.contains_key(&keys[i])).collect();
        for batch in missing.chunks(64) {
            let texts: Vec<&str> = batch.iter().map(|&i| chunks[i].text.as_str()).collect();
            let vecs = embedder.embed_batch(&texts)?;
            if vecs.len() != texts.len() {
                return Err("embedder returned the wrong number of vectors".into());
            }
            for (&i, v) in batch.iter().zip(vecs) {
                cache.map.insert(keys[i], v);
                cache.dirty = true;
            }
        }
        let vectors = keys
            .iter()
            .map(|k| {
                let mut v = cache.map.get(k).cloned().unwrap_or_default();
                normalize(&mut v);
                v
            })
            .collect();
        let live = keys.into_iter().collect();
        if let Err(e) = cache.save(&live) {
            // A cache we can't write only costs speed next time.
            eprintln!("quire: couldn't save embedding cache: {e}");
        }
        Ok(Self { chunks, vectors, embedder })
    }

    pub fn embedder_name(&self) -> &str {
        self.embedder.name()
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn embed_query(&self, q: &str) -> Vec<f32> {
        let mut v = self.embedder.embed(q);
        normalize(&mut v);
        v
    }

    /// Passages most similar to `query`, best first: (chunk index, cosine).
    pub fn search(&self, query: &str, k: usize) -> Vec<(usize, f32)> {
        let q = self.embed_query(query);
        if q.iter().all(|x| *x == 0.0) {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> =
            self.vectors.iter().enumerate().map(|(i, v)| (i, dot(&q, v))).filter(|(_, s)| *s > 0.05).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored
    }

    /// The stored unit vector of the passage of `path` starting at byte `start`.
    pub fn vector_of(&self, path: &NotePath, start: usize) -> Option<&[f32]> {
        let i = self.chunks.iter().position(|c| &c.path == path && c.range.start == start)?;
        self.vectors.get(i).map(Vec::as_slice)
    }

    /// Cosine similarity between a query vector and a passage.
    pub fn similarity(&self, query_vec: &[f32], chunk: usize) -> f32 {
        self.vectors.get(chunk).map_or(0.0, |v| dot(query_vec, v))
    }

    /// Mean passage vector per note.
    fn note_vector(&self, path: &NotePath) -> Option<Vec<f32>> {
        let mut acc: Option<Vec<f32>> = None;
        for (c, v) in self.chunks.iter().zip(&self.vectors) {
            if &c.path == path {
                let a = acc.get_or_insert_with(|| vec![0.0; v.len()]);
                a.iter_mut().zip(v).for_each(|(x, y)| *x += y);
            }
        }
        acc.map(|mut v| {
            normalize(&mut v);
            v
        })
    }

    /// Notes nearest to `path` in meaning: (path, cosine), best first.
    pub fn related(&self, path: &NotePath, k: usize) -> Vec<(NotePath, f32)> {
        let Some(me) = self.note_vector(path) else { return Vec::new() };
        let mut best: HashMap<&NotePath, f32> = HashMap::new();
        for (c, v) in self.chunks.iter().zip(&self.vectors) {
            if &c.path != path {
                let s = dot(&me, v);
                let e = best.entry(&c.path).or_insert(f32::MIN);
                *e = e.max(s);
            }
        }
        let mut out: Vec<(NotePath, f32)> = best.into_iter().map(|(p, s)| (p.clone(), s)).filter(|(_, s)| *s > 0.1).collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(k);
        out
    }
}

// ------------------------------------------------------------------ LSA

/// Width of the hashed lexical component appended to latent vectors.
const HASH_DIMS: usize = 512;
/// Relative weight of exact-word overlap versus latent similarity.
const LEXICAL_WEIGHT: f32 = 0.7;

/// Latent semantic analysis embedder fitted on a corpus.
pub struct Lsa {
    vocab: HashMap<String, usize>,
    idf: Vec<f32>,
    /// Term → k-dimensional concept loading (rows of `U_k`).
    term_vecs: Vec<Vec<f32>>,
    dims: usize,
}

/// Deterministic xorshift for reproducible random projections.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// Orthonormalize the columns of `m` (rows × cols, row-major) in place.
fn orthonormalize(m: &mut [Vec<f32>], cols: usize) {
    for j in 0..cols {
        for p in 0..j {
            let d: f32 = m.iter().map(|r| r[j] * r[p]).sum();
            for r in m.iter_mut() {
                r[j] -= d * r[p];
            }
        }
        let n = m.iter().map(|r| r[j] * r[j]).sum::<f32>().sqrt();
        if n > 1e-8 {
            for r in m.iter_mut() {
                r[j] /= n;
            }
        } else {
            for r in m.iter_mut() {
                r[j] = 0.0;
            }
        }
    }
}

/// Eigen-decomposition of a small symmetric matrix by cyclic Jacobi rotations.
/// Returns (eigenvalues, eigenvectors as columns).
#[allow(clippy::needless_range_loop, reason = "index-based numeric code reads clearer here")]
fn jacobi_eigen(mut a: Vec<Vec<f64>>) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = a.len();
    let mut v = vec![vec![0.0; n]; n];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _sweep in 0..60 {
        let off: f64 = (0..n).flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j))).map(|(i, j)| a[i][j] * a[i][j]).sum();
        if off < 1e-18 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                if a[p][q].abs() < 1e-14 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for row in &mut v {
                    let (vkp, vkq) = (row[p], row[q]);
                    row[p] = c * vkp - s * vkq;
                    row[q] = s * vkp + c * vkq;
                }
            }
        }
    }
    ((0..n).map(|i| a[i][i]).collect(), v)
}

impl Lsa {
    /// Fit on passages. `max_dims` caps the latent dimensionality.
    #[allow(clippy::needless_range_loop, reason = "matrix indices read clearer than zipped iterators")]
    pub fn fit(texts: &[&str], max_dims: usize) -> Self {
        let docs: Vec<HashMap<String, u32>> = texts
            .iter()
            .map(|t| {
                let mut tf = HashMap::new();
                for tok in content_tokens(t) {
                    *tf.entry(tok).or_insert(0u32) += 1;
                }
                tf
            })
            .collect();
        let n = docs.len().max(1);
        let mut df: HashMap<&str, u32> = HashMap::new();
        for d in &docs {
            for t in d.keys() {
                *df.entry(t.as_str()).or_default() += 1;
            }
        }
        let min_df = if n >= 40 { 2 } else { 1 };
        let mut terms: Vec<(&str, u32)> = df.into_iter().filter(|(_, d)| *d >= min_df && (*d as usize) < n.max(2)).collect();
        terms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        terms.truncate(8000);
        let vocab: HashMap<String, usize> = terms.iter().enumerate().map(|(i, (t, _))| (t.to_string(), i)).collect();
        let idf: Vec<f32> = terms.iter().map(|(_, d)| ((n as f32 + 1.0) / (*d as f32 + 0.5)).ln()).collect();
        let v = vocab.len();

        // Sparse columns (term, weight), unit length.
        let cols: Vec<Vec<(usize, f32)>> = docs
            .iter()
            .map(|d| {
                let mut c: Vec<(usize, f32)> =
                    d.iter().filter_map(|(t, &f)| vocab.get(t).map(|&i| (i, (1.0 + (f as f32).ln()) * idf[i]))).collect();
                let norm = c.iter().map(|(_, w)| w * w).sum::<f32>().sqrt().max(1e-9);
                for (_, w) in &mut c {
                    *w /= norm;
                }
                c
            })
            .collect();

        // Generalisation needs far fewer dimensions than passages.
        let k = max_dims.min((1.2 * (n as f32).sqrt()).ceil() as usize).min(v.saturating_sub(1)).min(n.saturating_sub(1)).max(1);
        if v == 0 {
            return Self { vocab, idf, term_vecs: Vec::new(), dims: 0 };
        }
        let l = (k + 10).min(v).min(n.max(1));
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        // Y = A Ω   (v × l)
        let omega: Vec<Vec<f32>> = (0..n).map(|_| (0..l).map(|_| rng.next_f32()).collect()).collect();
        let mut y = vec![vec![0.0f32; l]; v];
        let a_times = |y: &mut Vec<Vec<f32>>, right: &[Vec<f32>]| {
            for r in y.iter_mut() {
                r.fill(0.0);
            }
            for (j, col) in cols.iter().enumerate() {
                for &(t, w) in col {
                    let (row, rj) = (&mut y[t], &right[j]);
                    row.iter_mut().zip(rj).for_each(|(a, b)| *a += w * b);
                }
            }
        };
        let at_times = |q: &[Vec<f32>]| -> Vec<Vec<f32>> {
            cols.iter()
                .map(|col| {
                    let mut out = vec![0.0f32; l];
                    for &(t, w) in col {
                        out.iter_mut().zip(&q[t]).for_each(|(a, b)| *a += w * b);
                    }
                    out
                })
                .collect()
        };
        a_times(&mut y, &omega);
        orthonormalize(&mut y, l);
        // One power iteration sharpens the spectrum: Y = A Aᵀ Y.
        let z = at_times(&y);
        a_times(&mut y, &z);
        orthonormalize(&mut y, l);

        // B = Qᵀ A (l × n); eigen of B Bᵀ gives U_b and Σ².
        let bt = at_times(&y); // n × l, i.e. Bᵀ
        let mut bbt = vec![vec![0.0f64; l]; l];
        for row in &bt {
            for i in 0..l {
                if row[i] == 0.0 {
                    continue;
                }
                for j in i..l {
                    bbt[i][j] += f64::from(row[i]) * f64::from(row[j]);
                }
            }
        }
        for i in 0..l {
            for j in 0..i {
                bbt[i][j] = bbt[j][i];
            }
        }
        let (vals, vecs) = jacobi_eigen(bbt);
        let mut order: Vec<usize> = (0..l).collect();
        order.sort_by(|&a, &b| vals[b].total_cmp(&vals[a]));
        let order: Vec<usize> = order.into_iter().take(k).filter(|&i| vals[i] > 1e-10).collect();
        let dims = order.len();

        // U_k = Q U_b, rows are term loadings.
        let term_vecs: Vec<Vec<f32>> =
            y.iter().map(|qrow| order.iter().map(|&e| (0..l).map(|i| qrow[i] * vecs[i][e] as f32).sum()).collect()).collect();
        Self { vocab, idf, term_vecs, dims }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }
}

impl Embedder for Lsa {
    fn name(&self) -> &'static str {
        "latent semantic index (on-device)"
    }

    /// `[Uₖᵀ·a , λ·hash(a)]` where `a` is the unit TF-IDF vector of `text`.
    ///
    /// The latent part generalises across co-occurring words; the hashed
    /// lexical part keeps passages the latent space barely represents (small
    /// topics in a small vault) from collapsing onto noise.
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut out = vec![0.0f32; self.dims + HASH_DIMS];
        let mut tf: HashMap<usize, u32> = HashMap::new();
        for tok in content_tokens(text) {
            if let Some(&i) = self.vocab.get(&tok) {
                *tf.entry(i).or_default() += 1;
            }
        }
        let weights: Vec<(usize, f32)> = tf.into_iter().map(|(i, f)| (i, (1.0 + (f as f32).ln()) * self.idf[i])).collect();
        let norm = weights.iter().map(|(_, w)| w * w).sum::<f32>().sqrt().max(1e-9);
        for (i, w) in weights {
            let w = w / norm;
            out[..self.dims].iter_mut().zip(&self.term_vecs[i]).for_each(|(a, b)| *a += w * b);
            let bucket = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
            out[self.dims + (bucket as usize % HASH_DIMS)] += LEXICAL_WEIGHT * w;
        }
        out
    }
}

/// Build a semantic index from passages using the built-in embedder.
pub fn build_lsa_index(chunks: Vec<Chunk>) -> SemanticIndex {
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let lsa = Lsa::fit(&texts, 96);
    SemanticIndex::new(chunks, Box::new(lsa))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::chunk_note;

    struct CountingEmbedder(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Embedder for CountingEmbedder {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            vec![text.len() as f32, 1.0]
        }
    }

    #[test]
    fn cache_skips_unchanged_passages_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emb.bin");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chunks = corpus();
        let n = chunks.len();
        let mut cache = VectorCache::open(&path, "m1");
        SemanticIndex::try_with_cache(chunks.clone(), Box::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut cache).unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), n);
        let mut cache = VectorCache::open(&path, "m1");
        assert_eq!(cache.len(), n);
        let ix =
            SemanticIndex::try_with_cache(chunks.clone(), Box::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut cache).unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), n, "second build is fully cached");
        assert_eq!(ix.len(), n);
        let mut other = VectorCache::open(&path, "m2");
        SemanticIndex::try_with_cache(chunks, Box::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut other).unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 2 * n, "a different model re-embeds");
    }

    #[test]
    fn jacobi_recovers_eigenvalues() {
        let (vals, vecs) = jacobi_eigen(vec![vec![2.0, 1.0], vec![1.0, 2.0]]);
        let mut v = vals;
        v.sort_by(f64::total_cmp);
        assert!((v[0] - 1.0).abs() < 1e-9 && (v[1] - 3.0).abs() < 1e-9);
        let dotp: f64 = (0..2).map(|i| vecs[i][0] * vecs[i][1]).sum();
        assert!(dotp.abs() < 1e-9);
    }

    fn corpus() -> Vec<Chunk> {
        let notes = [
            ("Cars.md", "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road."),
            ("Autos.md", "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long."),
            ("Bread.md", "# Bread\nSourdough bread needs flour, water and a slow rise in a warm oven."),
            ("Baking.md", "# Baking\nBaking cakes needs flour, sugar and an oven at the right heat."),
            ("Editors.md", "# Editors\nA text editor stores the buffer in a rope; the editor renders lines."),
            ("Ropes.md", "# Ropes\nRope data structures make text buffer edits fast in an editor."),
        ];
        notes.iter().flat_map(|(p, s)| chunk_note(&NotePath::new(*p), s)).collect()
    }

    #[test]
    fn finds_meaning_without_shared_words() {
        let ix = build_lsa_index(corpus());
        // "automobile" only appears in Autos, but co-occurs with vehicle/engine/road,
        // so a query about an automobile should also surface Cars before bread.
        let hits = ix.search("automobile", 6);
        let paths: Vec<&str> = hits.iter().map(|(i, _)| ix.chunks[*i].path.as_str()).collect();
        assert_eq!(paths[0], "Autos.md");
        let cars = paths.iter().position(|p| *p == "Cars.md").expect("cars found by meaning");
        assert!(paths.iter().position(|p| *p == "Bread.md").is_none_or(|b| cars < b));

        let rel = ix.related(&NotePath::new("Bread.md"), 3);
        assert_eq!(rel[0].0.as_str(), "Baking.md");
        assert!(ix.search("zzzz unknown", 3).is_empty());
    }
}
