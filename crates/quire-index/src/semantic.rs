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

use crate::Keep;
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
        #[expect(clippy::iter_over_hash_type, reason = "a cache file read back into a map: the order of its records doesn't matter")]
        for (k, v) in &self.map {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        quire_base::fs::write_atomic(path, &out, quire_base::fs::Options::STATE)?;
        self.dirty = false;
        Ok(())
    }
}

// ------------------------------------------------------------------ f16

/// `x` as IEEE half precision (round to nearest, ties to even), its bits.
/// Safe code, no crate (`docs/design/0.14-index-startup.md` §3.3).
pub(crate) fn f16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man == 0 { 0 } else { 0x200 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = u32::try_from(14 - e).unwrap_or(31);
        let half = 1u32 << (shift - 1);
        let rounded = m + (half - 1) + ((m >> shift) & 1);
        return sign | u16::try_from(rounded >> shift).unwrap_or(0x3ff);
    }
    let rounded = man + 0xfff + ((man >> 13) & 1);
    let (e, rounded) = if rounded & 0x80_0000 == 0 { (e, rounded) } else { (e + 1, 0) };
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    sign | (u16::try_from(e).unwrap_or(0x1f) << 10) | u16::try_from(rounded >> 13).unwrap_or(0)
}

/// The value of half-precision bits.
pub(crate) fn f16_value(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    let bits = match exp {
        0 if man == 0 => sign,
        0 => {
            // A subnormal: shift it into a normal f32.
            let (mut e, mut m) = (127 - 15 + 1, man);
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (man << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

// ------------------------------------------------------------------ rows

/// A passage's vector, unit length, held small (`docs/design/0.14-index-startup.md` §3.3).
#[derive(Debug, Clone, PartialEq)]
enum Vector {
    /// LSA's `[latent | lexical]`: the latent part as f16, the hashed lexical
    /// part (mostly zeros) as sorted `(bucket, f16 weight)` pairs.
    Compact { latent: Box<[u16]>, lexical: Box<[(u16, u16)]> },
    /// Any other embedder's vector, as f16.
    Dense(Box<[u16]>),
}

impl Vector {
    /// `v` (unit length already), for a model with `dims` latent dimensions
    /// (`None`: not LSA).
    fn of(v: &[f32], dims: Option<usize>) -> Self {
        match dims {
            Some(d) => Self::Compact {
                latent: v.iter().take(d).map(|x| f16_bits(*x)).collect(),
                lexical: v
                    .iter()
                    .enumerate()
                    .skip(d)
                    .filter(|(_, x)| **x != 0.0)
                    .map(|(i, x)| (u16::try_from(i - d).unwrap_or(u16::MAX), f16_bits(*x)))
                    .collect(),
            },
            None => Self::Dense(v.iter().map(|x| f16_bits(*x)).collect()),
        }
    }

    /// Cosine with a unit query vector `q`. Every sum in a fixed order.
    fn dot(&self, q: &[f32]) -> f32 {
        match self {
            Self::Compact { latent, lexical } => {
                let d = latent.len();
                let mut acc = [0.0f32; 8];
                let lat = q.get(..d).unwrap_or(q);
                let (ca, cq) = (latent.chunks_exact(8), lat.chunks_exact(8));
                let (ra, rq) = (ca.remainder(), cq.remainder());
                for (x, y) in ca.zip(cq) {
                    for i in 0..8 {
                        acc[i] += f16_value(x[i]) * y[i];
                    }
                }
                let tail: f32 = ra.iter().zip(rq).map(|(x, y)| f16_value(*x) * y).sum();
                let lex: f32 = lexical.iter().map(|(b, w)| f16_value(*w) * q.get(d + usize::from(*b)).copied().unwrap_or(0.0)).sum();
                acc.iter().sum::<f32>() + tail + lex
            }
            Self::Dense(v) => {
                let mut acc = [0.0f32; 8];
                let (ca, cq) = (v.chunks_exact(8), q.chunks_exact(8));
                let (ra, rq) = (ca.remainder(), cq.remainder());
                for (x, y) in ca.zip(cq) {
                    for i in 0..8 {
                        acc[i] += f16_value(x[i]) * y[i];
                    }
                }
                acc.iter().sum::<f32>() + ra.iter().zip(rq).map(|(x, y)| f16_value(*x) * y).sum::<f32>()
            }
        }
    }

    /// As a dense f32 vector `width` long.
    fn to_f32(&self, width: usize) -> Vec<f32> {
        match self {
            Self::Compact { latent, lexical } => {
                let mut out: Vec<f32> = latent.iter().map(|x| f16_value(*x)).collect();
                out.resize(width.max(latent.len()), 0.0);
                for (b, w) in lexical {
                    if let Some(x) = out.get_mut(latent.len() + usize::from(*b)) {
                        *x = f16_value(*w);
                    }
                }
                out
            }
            Self::Dense(v) => v.iter().map(|x| f16_value(*x)).collect(),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Compact { latent, lexical } => latent.len() * 2 + lexical.len() * 4,
            Self::Dense(v) => v.len() * 2,
        }
    }
}

/// Where a passage is: no text (it is cut again from the note's source
/// when shown, [`SemanticIndex::passage_text`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    pub path: NotePath,
    /// Nearest heading above the passage.
    pub heading: Option<std::sync::Arc<str>>,
    /// Byte range in the note's source.
    pub range: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
struct Row {
    passage: Passage,
    /// The hash of the passage's text (an embedder's cache key).
    #[expect(dead_code, reason = "the passage cache's key (`docs/design/0.14-index-startup.md` step I2b)")]
    text_key: u64,
    vector: Vector,
}

/// Rows per block: an update copies only the blocks it touches.
const BLOCK: usize = 4096;

#[derive(Debug, Clone, Default)]
struct Block {
    /// `None`: a row whose note changed or went (compacted when many).
    rows: Vec<Option<Row>>,
}

/// The model a [`SemanticIndex`] embeds with.
#[derive(Clone)]
enum Model {
    /// The built-in LSA, fitted on the vault.
    Lsa(std::sync::Arc<Lsa>),
    /// Another embedder (an endpoint, a local model): nothing fitted on the vault.
    Other(std::sync::Arc<dyn Embedder>),
}

impl Model {
    fn embedder(&self) -> &dyn Embedder {
        match self {
            Self::Lsa(l) => l.as_ref(),
            Self::Other(e) => e.as_ref(),
        }
    }

    fn lsa_dims(&self) -> Option<usize> {
        match self {
            Self::Lsa(l) => Some(l.dims),
            Self::Other(_) => None,
        }
    }
}

/// A built index: passage rows (place and vector, no text) in blocks, and
/// the model for queries (`docs/design/0.14-index-startup.md` §3.3).
pub struct SemanticIndex {
    blocks: Vec<std::sync::Arc<Block>>,
    live: usize,
    dead: usize,
    model: Model,
    /// The notes the model was fitted on, sorted: what a refit read. Carried
    /// unchanged by per-note updates (whose rows are folded in, not fitted),
    /// and what an agent view checks (`FitSet`; Fable's review, F1).
    fitted: std::sync::Arc<[NotePath]>,
    /// Fitted while a folder that is now locked was open: serves the
    /// person's own search until a refit, never a view, never written.
    tainted: bool,
    /// Why the vector cache couldn't be saved, if it couldn't.
    cache_problem: Option<String>,
}

impl std::fmt::Debug for SemanticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticIndex").field("passages", &self.live).field("embedder", &self.embedder_name()).finish_non_exhaustive()
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

/// The fitted notes of `chunks`, sorted.
fn fitted_of(chunks: &[Chunk]) -> std::sync::Arc<[NotePath]> {
    let mut p: Vec<NotePath> = chunks.iter().map(|c| c.path.clone()).collect();
    p.sort_unstable();
    p.dedup();
    p.into()
}

impl SemanticIndex {
    /// Rows for `chunks` with `vectors` (unit length, one each).
    fn from_rows(chunks: Vec<Chunk>, vectors: &[Vec<f32>], keys: &[u64], model: Model, fitted: std::sync::Arc<[NotePath]>) -> Self {
        let dims = model.lsa_dims();
        let mut ix = Self { blocks: Vec::new(), live: 0, dead: 0, model, fitted, tainted: false, cache_problem: None };
        let rows: Vec<Row> = chunks
            .into_iter()
            .zip(vectors)
            .zip(keys)
            .map(|((c, v), k)| Row {
                passage: Passage { path: c.path, heading: c.heading.map(Into::into), range: c.range },
                text_key: *k,
                vector: Vector::of(v, dims),
            })
            .collect();
        ix.append(rows);
        ix
    }

    fn append(&mut self, rows: Vec<Row>) -> Vec<usize> {
        let mut ids = Vec::with_capacity(rows.len());
        for r in rows {
            if self.blocks.last().is_none_or(|b| b.rows.len() >= BLOCK) {
                self.blocks.push(std::sync::Arc::new(Block::default()));
            }
            let bi = self.blocks.len() - 1;
            let block = std::sync::Arc::make_mut(&mut self.blocks[bi]);
            ids.push(bi * BLOCK + block.rows.len());
            block.rows.push(Some(r));
            self.live += 1;
        }
        ids
    }

    /// Fit the built-in LSA on `chunks` and embed them with it.
    pub fn fit_lsa(chunks: Vec<Chunk>) -> Self {
        // In (path, offset) order whatever order they came in: two devices with the same notes
        // fit the same model (the fit sorts its texts too).
        let mut chunks = chunks;
        chunks.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
        let lsa = {
            let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
            Lsa::fit(&texts, 96)
        };
        let fitted = fitted_of(&chunks);
        let dims = Some(lsa.dims);
        let mut ix = Self {
            blocks: Vec::new(),
            live: 0,
            dead: 0,
            model: Model::Lsa(std::sync::Arc::new(lsa)),
            fitted,
            tainted: false,
            cache_problem: None,
        };
        // Each row made compact as it is embedded: no full-width copy of every vector at once.
        let Model::Lsa(lsa) = ix.model.clone() else { unreachable!("made above") };
        let rows: Vec<Row> = chunks
            .into_iter()
            .map(|c| Row {
                text_key: text_hash("lsa", &c.text),
                vector: Vector::of(&lsa.project(&c.text), dims),
                passage: Passage { path: c.path, heading: c.heading.map(Into::into), range: c.range },
            })
            .collect();
        ix.append(rows);
        ix
    }

    /// Build with any embedder, reusing cached vectors and batching the rest.
    pub fn try_with_cache(chunks: Vec<Chunk>, embedder: std::sync::Arc<dyn Embedder>, cache: &mut VectorCache) -> Result<Self, String> {
        let mut chunks = chunks;
        chunks.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
        let (vectors, keys) = embed_cached(&chunks, embedder.as_ref(), cache)?;
        let live: std::collections::HashSet<u64> = keys.iter().copied().collect();
        // A cache that can't be written only costs speed next time: the index
        // is built anyway, and says why (Settings → AI shows it).
        let cache_problem = cache.save(&live).err().map(|e| format!("couldn't save the embedding cache: {e}"));
        let fitted = fitted_of(&chunks);
        let mut ix = Self::from_rows(chunks, &vectors, &keys, Model::Other(embedder), fitted);
        ix.cache_problem = cache_problem;
        Ok(ix)
    }

    /// This index with the rows `gone` dropped and `added` embedded with the
    /// current model and appended (folded in: an LSA model stays as fitted),
    /// and the ids of the new rows. Copies only the blocks it touches.
    pub(crate) fn updated(&self, gone: &[usize], added: Vec<Chunk>, cache: Option<&mut VectorCache>) -> Result<(Self, Vec<usize>), String> {
        let mut next = Self {
            blocks: self.blocks.clone(),
            live: self.live,
            dead: self.dead,
            model: self.model.clone(),
            fitted: std::sync::Arc::clone(&self.fitted),
            tainted: self.tainted,
            cache_problem: None,
        };
        for &id in gone {
            let (b, s) = (id / BLOCK, id % BLOCK);
            if let Some(block) = next.blocks.get_mut(b)
                && block.rows.get(s).is_some_and(Option::is_some)
            {
                std::sync::Arc::make_mut(block).rows[s] = None;
                next.live -= 1;
                next.dead += 1;
            }
        }
        let (vectors, keys) = match &next.model {
            Model::Lsa(l) => {
                (added.iter().map(|c| l.project(&c.text)).collect(), added.iter().map(|c| text_hash("lsa", &c.text)).collect())
            }
            Model::Other(e) => {
                let mut scratch = VectorCache::in_memory(e.name());
                embed_cached(&added, e.as_ref(), cache.unwrap_or(&mut scratch))?
            }
        };
        let dims = next.model.lsa_dims();
        let rows: Vec<Row> = added
            .into_iter()
            .zip(vectors.iter())
            .zip(keys)
            .map(|((c, v), k)| Row {
                passage: Passage { path: c.path, heading: c.heading.map(Into::into), range: c.range },
                text_key: k,
                vector: Vector::of(v, dims),
            })
            .collect();
        let ids = next.append(rows);
        Ok((next, ids))
    }

    /// The same rows with no dead ones, renumbered (when more than a quarter
    /// are dead), and the new id of each old live one.
    pub(crate) fn compacted(&self) -> Option<(Self, HashMap<usize, usize>)> {
        if self.dead * 4 <= self.live + self.dead {
            return None;
        }
        let mut next = Self {
            blocks: Vec::new(),
            live: 0,
            dead: 0,
            model: self.model.clone(),
            fitted: std::sync::Arc::clone(&self.fitted),
            tainted: self.tainted,
            cache_problem: None,
        };
        let (olds, rows): (Vec<usize>, Vec<Row>) = self.rows().map(|(i, r)| (i, r.clone())).unzip();
        let ids = next.append(rows);
        Some((next, olds.into_iter().zip(ids).collect()))
    }

    /// Marked as fitted while a folder now locked was open.
    pub(crate) fn tainted(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            live: self.live,
            dead: self.dead,
            model: self.model.clone(),
            fitted: std::sync::Arc::clone(&self.fitted),
            tainted: true,
            cache_problem: None,
        }
    }

    /// Whether the model was fitted while a folder now locked was open (it
    /// serves the person's own search until a refit; views never use it).
    pub const fn is_tainted(&self) -> bool {
        self.tainted
    }

    /// The notes the model was fitted on, sorted (F1).
    pub fn fitted(&self) -> &[NotePath] {
        &self.fitted
    }

    pub(crate) fn fitted_arc(&self) -> std::sync::Arc<[NotePath]> {
        std::sync::Arc::clone(&self.fitted)
    }

    /// Whether this is the built-in LSA (refitted on the vault).
    pub const fn is_lsa(&self) -> bool {
        matches!(self.model, Model::Lsa(_))
    }

    fn rows(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.blocks
            .iter()
            .enumerate()
            .flat_map(|(b, block)| block.rows.iter().enumerate().filter_map(move |(s, r)| Some((b * BLOCK + s, r.as_ref()?))))
    }

    fn row(&self, id: usize) -> Option<&Row> {
        self.blocks.get(id / BLOCK)?.rows.get(id % BLOCK)?.as_ref()
    }

    /// Every live passage and its id.
    pub fn passages(&self) -> impl Iterator<Item = (usize, &Passage)> {
        self.rows().map(|(i, r)| (i, &r.passage))
    }

    /// The passage with id `id`.
    pub fn passage(&self, id: usize) -> Option<&Passage> {
        self.row(id).map(|r| &r.passage)
    }

    /// The text of passage `id`, cut again from its note's source in `ix`
    /// (as [`crate::chunk::chunk_note`] cut it); `None` if the note or the
    /// range is gone (the index can be a moment ahead of these rows).
    pub fn passage_text(&self, id: usize, ix: &crate::Index) -> Option<String> {
        let p = self.passage(id)?;
        crate::chunk::passage_text(&p.path, p.heading.as_deref(), ix.source(&p.path)?, p.range.clone())
    }

    /// Bytes the rows hold on the heap (vectors, places), roughly.
    pub fn heap_bytes(&self) -> usize {
        self.rows().map(|(_, r)| r.vector.heap_bytes() + r.passage.path.as_str().len() + 64).sum()
    }

    pub fn embedder_name(&self) -> &str {
        self.model.embedder().name()
    }

    /// Why the vector cache couldn't be saved when this index was built, if
    /// it couldn't (the next build embeds those passages again).
    pub fn cache_problem(&self) -> Option<&str> {
        self.cache_problem.as_deref()
    }

    /// Live passages.
    pub const fn len(&self) -> usize {
        self.live
    }

    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn embed_query(&self, q: &str) -> Vec<f32> {
        let mut v = self.model.embedder().embed(q);
        normalize(&mut v);
        v
    }

    /// Passages most similar to `query`, best first: (passage id, cosine).
    pub fn search(&self, query: &str, k: usize) -> Vec<(usize, f32)> {
        self.search_where(query, k, &|_| true)
    }

    /// [`Self::search`] over the passages of the notes `keep` accepts: the
    /// `k` best of those, so other notes take no places. Ties by path and
    /// offset, whatever order the rows are in.
    pub fn search_where(&self, query: &str, k: usize, keep: Keep<'_>) -> Vec<(usize, f32)> {
        let q = self.embed_query(query);
        if q.iter().all(|x| *x == 0.0) {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32, &Passage)> =
            self.rows().map(|(i, r)| (i, r.vector.dot(&q), &r.passage)).filter(|(_, s, _)| *s > 0.05).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.path.cmp(&b.2.path)).then(a.2.range.start.cmp(&b.2.range.start)));
        // `keep` after sorting, on the best passages only: it can be costly
        // (an agent's policy), and it is asked once per note.
        let mut seen: HashMap<&NotePath, bool> = HashMap::new();
        scored
            .into_iter()
            .filter(|(_, _, p)| *seen.entry(&p.path).or_insert_with(|| keep(&p.path)))
            .take(k)
            .map(|(i, s, _)| (i, s))
            .collect()
    }

    /// The stored unit vector of the passage of `path` starting at byte `start`.
    pub fn vector_of(&self, path: &NotePath, start: usize) -> Option<Vec<f32>> {
        let width = self.model.lsa_dims().map_or(0, |d| d + HASH_DIMS);
        self.rows().find(|(_, r)| &r.passage.path == path && r.passage.range.start == start).map(|(_, r)| r.vector.to_f32(width))
    }

    /// Cosine similarity between a query vector and a passage.
    pub fn similarity(&self, query_vec: &[f32], id: usize) -> f32 {
        self.row(id).map_or(0.0, |r| r.vector.dot(query_vec))
    }

    /// Mean passage vector per note.
    fn note_vector(&self, path: &NotePath) -> Option<Vec<f32>> {
        let width = self.model.lsa_dims().map_or(0, |d| d + HASH_DIMS);
        let mut acc: Option<Vec<f32>> = None;
        for (_, r) in self.rows().filter(|(_, r)| &r.passage.path == path) {
            let v = r.vector.to_f32(width);
            let a = acc.get_or_insert_with(|| vec![0.0; v.len()]);
            a.iter_mut().zip(&v).for_each(|(x, y)| *x += y);
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
        for (_, r) in self.rows() {
            if &r.passage.path != path {
                let s = r.vector.dot(&me);
                let e = best.entry(&r.passage.path).or_insert(f32::MIN);
                *e = e.max(s);
            }
        }
        let mut out: Vec<(NotePath, f32)> = best.into_iter().map(|(p, s)| (p.clone(), s)).filter(|(_, s)| *s > 0.1).collect();
        // Ties by path: the same notes are kept whatever the map's order.
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(k);
        out
    }

    /// Every live row as (path, offset, its vector's bits), sorted: what two
    /// indexes with equal rows give equal (tests).
    #[cfg(test)]
    pub(crate) fn rows_text(&self) -> Vec<(NotePath, usize, Vec<u32>)> {
        let width = self.model.lsa_dims().map_or(0, |d| d + HASH_DIMS);
        let mut v: Vec<(NotePath, usize, Vec<u32>)> = self
            .rows()
            .map(|(_, r)| (r.passage.path.clone(), r.passage.range.start, r.vector.to_f32(width).iter().map(|x| x.to_bits()).collect()))
            .collect();
        v.sort();
        v
    }

    /// The stored vector of row `id`, as bits (tests).
    #[cfg(test)]
    pub(crate) fn row_bits(&self, id: usize) -> Option<Vec<u32>> {
        let width = self.model.lsa_dims().map_or(0, |d| d + HASH_DIMS);
        self.row(id).map(|r| r.vector.to_f32(width).iter().map(|x| x.to_bits()).collect())
    }

    /// The LSA model's own embedding of `text`, as a stored row would hold it (tests).
    #[cfg(test)]
    pub(crate) fn row_bits_for(&self, text: &str) -> Option<Vec<u32>> {
        let Model::Lsa(l) = &self.model else { return None };
        let width = l.dims + HASH_DIMS;
        Some(Vector::of(&l.project(text), Some(l.dims)).to_f32(width).iter().map(|x| x.to_bits()).collect())
    }
}

/// Vectors (unit length) and text keys for `chunks`, from `cache` or the embedder in batches.
fn embed_cached(chunks: &[Chunk], embedder: &dyn Embedder, cache: &mut VectorCache) -> Result<(Vec<Vec<f32>>, Vec<u64>), String> {
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
    Ok((vectors, keys))
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
#[expect(clippy::needless_range_loop, reason = "index-based numeric code reads clearer here")]
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
    #[expect(clippy::needless_range_loop, reason = "matrix indices read clearer than zipped iterators")]
    pub fn fit(texts: &[&str], max_dims: usize) -> Self {
        // Every float sum below runs in a fixed order, so equal passages give equal bits whatever
        // order they arrive in (`docs/design/0.14-agent-index.md` §3.4). Equal texts are
        // interchangeable, so sorting them loses nothing.
        let mut sorted = texts.to_vec();
        sorted.sort_unstable();
        // Terms counted by a number, each word's text kept once: most of a fit's transient
        // memory was a map of words per passage (`docs/design/0.14-index-startup.md` §3.3).
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        let docs: Vec<Vec<(u32, u32)>> = sorted
            .iter()
            .map(|t| {
                let mut tf: Vec<(u32, u32)> = Vec::new();
                for tok in content_tokens(t) {
                    let id = *ids.entry(tok).or_insert_with_key(|k| {
                        names.push(k.clone());
                        u32::try_from(names.len() - 1).unwrap_or(u32::MAX)
                    });
                    tf.push((id, 1));
                }
                tf.sort_unstable_by_key(|e| e.0);
                tf.dedup_by(|b, a| {
                    let same = a.0 == b.0;
                    if same {
                        a.1 += 1;
                    }
                    same
                });
                tf.shrink_to_fit();
                tf
            })
            .collect();
        drop(ids);
        let n = docs.len().max(1);
        let mut df: Vec<u32> = vec![0; names.len()];
        for d in &docs {
            for (t, _) in d {
                df[*t as usize] += 1;
            }
        }
        let min_df = if n >= 40 { 2 } else { 1 };
        let mut terms: Vec<(u32, u32)> = df
            .iter()
            .enumerate()
            .map(|(t, d)| (u32::try_from(t).unwrap_or(u32::MAX), *d))
            .filter(|(_, d)| *d >= min_df && (*d as usize) < n.max(2))
            .collect();
        terms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| names[a.0 as usize].cmp(&names[b.0 as usize])));
        terms.truncate(8000);
        // Term number to its place in the vocabulary.
        let mut place: Vec<Option<usize>> = vec![None; names.len()];
        for (i, (t, _)) in terms.iter().enumerate() {
            place[*t as usize] = Some(i);
        }
        let vocab: HashMap<String, usize> = terms.iter().enumerate().map(|(i, (t, _))| (names[*t as usize].clone(), i)).collect();
        drop(names);
        let idf: Vec<f32> = terms.iter().map(|(_, d)| ((n as f32 + 1.0) / (*d as f32 + 0.5)).ln()).collect();
        let v = vocab.len();

        // Sparse columns (term, weight), unit length.
        let cols: Vec<Vec<(u32, f32)>> = docs
            .iter()
            .map(|d| {
                let mut c: Vec<(u32, f32)> = d
                    .iter()
                    .filter_map(|&(t, f)| {
                        place[t as usize].map(|i| (u32::try_from(i).unwrap_or(u32::MAX), (1.0 + (f as f32).ln()) * idf[i]))
                    })
                    .collect();
                // The norm and the products below add in term order.
                c.sort_unstable_by_key(|&(i, _)| i);
                let norm = c.iter().map(|(_, w)| w * w).sum::<f32>().sqrt().max(1e-9);
                for (_, w) in &mut c {
                    *w /= norm;
                }
                c.shrink_to_fit();
                c
            })
            .collect();

        drop(docs);
        // Generalisation needs far fewer dimensions than passages.
        let k = max_dims.min((1.2 * (n as f32).sqrt()).ceil() as usize).min(v.saturating_sub(1)).min(n.saturating_sub(1)).max(1);
        if v == 0 {
            return Self { vocab, idf, term_vecs: Vec::new(), dims: 0 };
        }
        let l = (k + 10).min(v).min(n.max(1));
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        // Y = A Ω   (v × l)
        let mut y = vec![vec![0.0f32; l]; v];
        let a_times = |y: &mut Vec<Vec<f32>>, right: &[Vec<f32>]| {
            for r in y.iter_mut() {
                r.fill(0.0);
            }
            for (j, col) in cols.iter().enumerate() {
                for &(t, w) in col {
                    let (row, rj) = (&mut y[t as usize], &right[j]);
                    row.iter_mut().zip(rj).for_each(|(a, b)| *a += w * b);
                }
            }
        };
        let at_times = |q: &[Vec<f32>]| -> Vec<Vec<f32>> {
            cols.iter()
                .map(|col| {
                    let mut out = vec![0.0f32; l];
                    for &(t, w) in col {
                        out.iter_mut().zip(&q[t as usize]).for_each(|(a, b)| *a += w * b);
                    }
                    out
                })
                .collect()
        };
        // Y = A Ω, each column of Ω drawn as it is used (the same numbers in the same order as a
        // whole Ω made first, without holding n × l of them).
        let mut omega_j = vec![0.0f32; l];
        for col in &cols {
            for x in &mut omega_j {
                *x = rng.next_f32();
            }
            for &(t, w) in col {
                y[t as usize].iter_mut().zip(&omega_j).for_each(|(a, b)| *a += w * b);
            }
        }
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

    /// A passage's row vector with this model, unit length: folding in (the
    /// vocabulary, IDF and loadings stay as fitted). What a refit gives the
    /// same text, bit for bit.
    pub fn project(&self, text: &str) -> Vec<f32> {
        let mut v = self.embed(text);
        normalize(&mut v);
        v
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
        let mut weights: Vec<(usize, f32)> = tf.into_iter().map(|(i, f)| (i, (1.0 + (f as f32).ln()) * self.idf[i])).collect();
        // Hash order would change the float sums: add in term-index order.
        weights.sort_unstable_by_key(|&(i, _)| i);
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
    SemanticIndex::fit_lsa(chunks)
}

#[cfg(test)]
#[path = "semantic_tests.rs"]
mod per_note_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::chunk_note;

    /// Every text the same vector: every pair of notes ties.
    struct Same;
    impl Embedder for Same {
        fn name(&self) -> &'static str {
            "same"
        }
        fn embed(&self, _: &str) -> Vec<f32> {
            vec![1.0, 1.0]
        }
    }

    /// Ties in `related` are broken by path: the same answer whatever the
    /// order passages came in or the hash order of a run.
    #[test]
    fn related_breaks_ties_by_path() {
        let chunk = |p: &str| Chunk { path: NotePath::new(p), heading: None, range: 0..1, text: p.to_string() };
        let names: Vec<String> = (0..40).map(|i| format!("N{i:02}.md")).collect();
        let want: Vec<NotePath> = names.iter().filter(|n| *n != "N07.md").take(5).map(|n| NotePath::new(n.as_str())).collect();
        for reversed in [false, true] {
            let mut chunks: Vec<Chunk> = names.iter().map(|n| chunk(n)).collect();
            if reversed {
                chunks.reverse();
            }
            let ix = SemanticIndex::try_with_cache(chunks, std::sync::Arc::new(Same), &mut VectorCache::in_memory("same")).unwrap();
            for _ in 0..5 {
                let got: Vec<NotePath> = ix.related(&NotePath::new("N07.md"), 5).into_iter().map(|(p, _)| p).collect();
                assert_eq!(got, want, "reversed: {reversed}");
            }
        }
    }

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
    fn a_cache_that_cannot_be_saved_is_reported_not_printed() {
        let dir = tempfile::tempdir().unwrap();
        // The cache's folder is a file, so the cache can't be written.
        std::fs::write(dir.path().join("blocked"), "x").unwrap();
        let mut cache = VectorCache::open(dir.path().join("blocked/emb.bin"), "m1");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ix = SemanticIndex::try_with_cache(corpus(), std::sync::Arc::new(CountingEmbedder(count)), &mut cache).unwrap();
        assert!(!ix.is_empty());
        assert!(ix.cache_problem().is_some_and(|p| p.contains("embedding cache")), "{:?}", ix.cache_problem());
        assert_eq!(build_lsa_index(corpus()).cache_problem(), None);
    }

    #[test]
    fn cache_skips_unchanged_passages_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emb.bin");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let chunks = corpus();
        let n = chunks.len();
        let mut cache = VectorCache::open(&path, "m1");
        SemanticIndex::try_with_cache(chunks.clone(), std::sync::Arc::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut cache)
            .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), n);
        let mut cache = VectorCache::open(&path, "m1");
        assert_eq!(cache.len(), n);
        let ix =
            SemanticIndex::try_with_cache(chunks.clone(), std::sync::Arc::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut cache)
                .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), n, "second build is fully cached");
        assert_eq!(ix.len(), n);
        let mut other = VectorCache::open(&path, "m2");
        SemanticIndex::try_with_cache(chunks, std::sync::Arc::new(CountingEmbedder(std::sync::Arc::clone(&count))), &mut other).unwrap();
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
        let paths: Vec<&str> = hits.iter().map(|(i, _)| ix.passage(*i).unwrap().path.as_str()).collect();
        assert_eq!(paths[0], "Autos.md");
        let cars = paths.iter().position(|p| *p == "Cars.md").expect("cars found by meaning");
        assert!(paths.iter().position(|p| *p == "Bread.md").is_none_or(|b| cars < b));

        let rel = ix.related(&NotePath::new("Bread.md"), 3);
        assert_eq!(rel[0].0.as_str(), "Baking.md");
        assert!(ix.search("zzzz unknown", 3).is_empty());
    }

    /// 120 passages that share words in overlapping ways, so every sum has many terms.
    fn many_passages() -> Vec<String> {
        let words = [
            "rope", "editor", "tree", "buffer", "undo", "markdown", "note", "link", "search", "index", "quick", "fast", "local", "graph",
            "tag", "plain", "file", "sync", "oven", "flour", "engine", "road", "vehicle", "sugar",
        ];
        (0..120usize)
            .map(|i| {
                (0..18)
                    .map(|j| words[(i * 7 + j * j * 3 + i / 5) % words.len()])
                    .chain(["alpha", "beta", "gamma"].into_iter().skip(i % 3))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn fingerprint(lsa: &Lsa, queries: &[&str]) -> Vec<Vec<u32>> {
        queries.iter().map(|q| lsa.embed(q).iter().map(|x| x.to_bits()).collect()).collect()
    }

    /// `docs/design/0.14-agent-index.md` §3.4: the same passages give the same model, bit for bit, whatever
    /// the order they arrive in and however the hash maps happen to iterate.
    #[test]
    fn lsa_is_deterministic() {
        let owned = many_passages();
        let texts: Vec<&str> = owned.iter().map(String::as_str).collect();
        let queries = ["rope buffer editor undo", "flour oven sugar", "engine road vehicle alpha", "sync graph link tag search index"];
        let first = Lsa::fit(&texts, 24);
        let want = fingerprint(&first, &queries);
        assert!(first.dims() > 1, "the corpus must have latent dimensions");
        // Fresh `HashMap`s seed their hasher anew, so repeated fits already iterate differently.
        for _ in 0..4 {
            assert_eq!(fingerprint(&Lsa::fit(&texts, 24), &queries), want, "same order, another fit");
        }
        let mut shuffled = texts.clone();
        for round in 0..4usize {
            // A fixed permutation per round: rotate by a stride and reverse every other round.
            shuffled.rotate_left(17 + round * 13);
            if round % 2 == 1 {
                shuffled.reverse();
            }
            assert_eq!(fingerprint(&Lsa::fit(&shuffled, 24), &queries), want, "shuffled order, round {round}");
        }
    }
}
