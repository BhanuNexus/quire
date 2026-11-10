// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The renderer behind [`super::request`]: Typst and the Mermaid renderer
//! on a worker thread, resvg to pixels, textures cached by recency.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};

use crossbeam_channel::{Receiver, Sender};
use egui::{ColorImage, TextureOptions, Vec2};
use resvg::{tiny_skia, usvg};

use super::{Job, Out, Picture};

struct Done {
    key: u64,
    result: Result<(ColorImage, Vec2, f32), String>,
}

struct Entry {
    out: Out,
    /// Texture memory, bytes.
    bytes: usize,
    /// When it was last asked for (a counter), for least-recently-used eviction.
    used: u64,
}

pub(crate) struct Typesetter {
    ctx: egui::Context,
    cache: HashMap<u64, Entry>,
    /// 0.12: diagrams drawn again for a zoom level, by (source hash, scale
    /// bucket); a few of them, apart from `cache` so zooming never pushes
    /// the note's other pictures out.
    zoomed: HashMap<u64, Entry>,
    zoomed_bytes: usize,
    clock: u64,
    bytes: usize,
    jobs: Sender<(u64, Job, f32)>,
    done: Receiver<Done>,
}

/// Biggest picture side in pixels (a huge diagram is scaled down to this).
const MAX_PIXELS: f32 = 4096.0;
/// Pictures kept, at most, and the texture memory they may use; past either,
/// the least recently used go.
const MAX_CACHED: usize = 600;
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// Diagrams kept drawn for a zoom level, at most, and their texture memory.
const MAX_ZOOMED: usize = 6;
const MAX_ZOOMED_BYTES: usize = 160 * 1024 * 1024;

/// The scale a zoomed diagram is drawn again at, for `factor` (its size on
/// screen over its size in points): the next step up, so zooming within a
/// step reuses one picture. `None` while the ordinary picture (drawn at
/// 1.5×) is sharp enough.
pub(crate) fn zoom_bucket(factor: f32) -> Option<f32> {
    const STEPS: [f32; 6] = [2.0, 3.0, 4.0, 6.0, 8.0, 12.0];
    if !factor.is_finite() || factor <= 1.5 {
        return None;
    }
    Some(STEPS.iter().copied().find(|s| *s >= factor).unwrap_or(STEPS[STEPS.len() - 1]))
}

fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    Arc::clone(DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        for bytes in [
            quire_typeset::fonts::INTER_REGULAR,
            quire_typeset::fonts::INTER_SEMIBOLD,
            quire_typeset::fonts::INTER_ITALIC,
            quire_typeset::fonts::JETBRAINS_MONO_NL,
            quire_typeset::fonts::STIX_TWO_MATH,
        ] {
            db.load_font_data(bytes.to_vec());
        }
        db.set_sans_serif_family("Inter");
        db.set_monospace_family("JetBrains Mono NL");
        Arc::new(db)
    }))
}

/// Rasterize `svg` at `scale` pixels per point (capped at [`MAX_PIXELS`]).
/// Returns the image and its size in points.
fn rasterize(svg: &str, scale: f32) -> Result<(ColorImage, Vec2), String> {
    let opt = usvg::Options { fontdb: fontdb(), font_family: "Inter".into(), ..usvg::Options::default() };
    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| format!("couldn't read the picture: {e}"))?;
    let size = tree.size();
    let (w, h) = (size.width().max(1.0), size.height().max(1.0));
    let scale = scale.min(MAX_PIXELS / w).min(MAX_PIXELS / h);
    let (pw, ph) = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
    let mut pixmap = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).ok_or("picture too large")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let image = ColorImage::from_rgba_premultiplied([pw.max(1) as usize, ph.max(1) as usize], pixmap.data());
    Ok((image, Vec2::new(w, h)))
}

fn run(job: &Job, scale: f32) -> Result<(ColorImage, Vec2, f32), String> {
    match job {
        Job::Math { latex, display, size, color } => {
            let style = quire_typeset::MathStyle { size_pt: *size as f32, color: *color };
            let r = quire_typeset::math_svg(latex, *display, &style).map_err(|e| e.to_string())?;
            let (img, _) = rasterize(&r.svg, scale)?;
            Ok((img, Vec2::new(r.width_pt, r.height_pt), r.baseline_pt))
        }
        Job::Diagram { source, dark } => {
            let svg = quire_typeset::diagram_svg(source, *dark).map_err(|e| e.to_string())?;
            let (img, size) = rasterize(&svg, scale)?;
            Ok((img, size, size.y))
        }
    }
}

impl Typesetter {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let (jobs, job_rx) = crossbeam_channel::unbounded::<(u64, Job, f32)>();
        let (done_tx, done) = crossbeam_channel::unbounded();
        let wake = ctx.clone();
        std::thread::Builder::new()
            .name("quire-typeset".into())
            .spawn(move || {
                for (key, job, scale) in job_rx {
                    // A panic in a renderer becomes an error for that one picture.
                    let result = catch_unwind(AssertUnwindSafe(|| run(&job, scale)))
                        .unwrap_or_else(|_| Err("the renderer failed on this input".into()));
                    if done_tx.send(Done { key, result }).is_err() {
                        return;
                    }
                    wake.request_repaint();
                }
            })
            .expect("spawn the typesetting thread");
        Self { ctx: ctx.clone(), cache: HashMap::new(), zoomed: HashMap::new(), zoomed_bytes: 0, clock: 0, bytes: 0, jobs, done }
    }

    /// Collect finished pictures. Call once per frame.
    pub(crate) fn poll(&mut self) {
        while let Ok(d) = self.done.try_recv() {
            let (out, bytes) = match d.result {
                Ok((image, size, baseline)) => {
                    let bytes = image.width() * image.height() * 4;
                    let texture = self.ctx.load_texture(format!("quire-typeset-{}", d.key), image, TextureOptions::LINEAR);
                    (Out::Ready(Picture { texture, size, baseline }), bytes)
                }
                Err(e) => (Out::Failed(e), 0),
            };
            // Only if still wanted (it may have been evicted while rendering).
            if let Some(e) = self.cache.get_mut(&d.key) {
                self.bytes = self.bytes - e.bytes + bytes;
                e.out = out;
                e.bytes = bytes;
            } else if let Some(e) = self.zoomed.get_mut(&d.key) {
                self.zoomed_bytes = self.zoomed_bytes - e.bytes + bytes;
                e.out = out;
                e.bytes = bytes;
            }
        }
        self.evict();
        self.evict_zoomed();
    }

    /// Keep at most [`MAX_ZOOMED`] zoomed pictures and [`MAX_ZOOMED_BYTES`]:
    /// the least recently used go (pending ones stay).
    fn evict_zoomed(&mut self) {
        while self.zoomed.len() > MAX_ZOOMED || self.zoomed_bytes > MAX_ZOOMED_BYTES {
            let oldest = self.zoomed.iter().filter(|(_, e)| !matches!(e.out, Out::Pending)).min_by_key(|(_, e)| e.used).map(|(k, _)| *k);
            let Some(k) = oldest else { break };
            if let Some(e) = self.zoomed.remove(&k) {
                self.zoomed_bytes -= e.bytes;
            }
        }
    }

    /// A diagram drawn again at `bucket` times its size ([`zoom_bucket`]),
    /// queued the first time it's asked for.
    pub(crate) fn get_zoomed(&mut self, job: &Job, bucket: f32) -> Out {
        let scale = self.ctx.pixels_per_point().max(1.0) * bucket;
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            ("zoomed", job).hash(&mut h);
            scale.to_bits().hash(&mut h);
            h.finish()
        };
        self.clock += 1;
        if let Some(e) = self.zoomed.get_mut(&key) {
            e.used = self.clock;
            return e.out.clone();
        }
        self.zoomed.insert(key, Entry { out: Out::Pending, bytes: 0, used: self.clock });
        let _ = self.jobs.send((key, job.clone(), scale));
        Out::Pending
    }

    /// Drop the least recently used pictures until under both limits.
    /// Pending ones stay (their results are on the way).
    fn evict(&mut self) {
        if self.cache.len() <= MAX_CACHED && self.bytes <= MAX_BYTES {
            return;
        }
        let mut done: Vec<(u64, u64)> =
            self.cache.iter().filter(|(_, e)| !matches!(e.out, Out::Pending)).map(|(k, e)| (e.used, *k)).collect();
        done.sort_unstable();
        for (_, k) in done {
            if self.cache.len() <= MAX_CACHED * 3 / 4 && self.bytes <= MAX_BYTES * 3 / 4 {
                break;
            }
            if let Some(e) = self.cache.remove(&k) {
                self.bytes -= e.bytes;
            }
        }
    }

    /// The picture for `job`, queuing it the first time it's asked for.
    pub(crate) fn get(&mut self, job: &Job) -> Out {
        let scale = self.ctx.pixels_per_point().max(1.0) * if matches!(job, Job::Math { .. }) { 1.0 } else { 1.5 };
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            job.hash(&mut h);
            scale.to_bits().hash(&mut h);
            h.finish()
        };
        self.clock += 1;
        if let Some(e) = self.cache.get_mut(&key) {
            e.used = self.clock;
            return e.out.clone();
        }
        self.cache.insert(key, Entry { out: Out::Pending, bytes: 0, used: self.clock });
        let _ = self.jobs.send((key, job.clone(), scale));
        Out::Pending
    }
}

fn shared() -> &'static std::sync::Mutex<Option<Typesetter>> {
    static SHARED: OnceLock<std::sync::Mutex<Option<Typesetter>>> = OnceLock::new();
    SHARED.get_or_init(|| std::sync::Mutex::new(None))
}

/// The picture for `job` from the app's one typesetter (editor and preview
/// share it), collecting anything finished since the last call.
pub(crate) fn request(ctx: &egui::Context, job: &Job) -> Out {
    // A panic elsewhere while holding the lock doesn't stop rendering for good.
    let mut slot = shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = slot.get_or_insert_with(|| Typesetter::new(ctx));
    t.poll();
    t.get(job)
}

/// [`Typesetter::get_zoomed`] on the app's one typesetter.
pub(crate) fn request_zoomed(ctx: &egui::Context, job: &Job, bucket: f32) -> Out {
    let mut slot = shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = slot.get_or_insert_with(|| Typesetter::new(ctx));
    t.poll();
    t.get_zoomed(job, bucket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typeset_ui::{Job, Out};

    #[test]
    fn the_cache_forgets_the_least_recently_used() {
        let ctx = egui::Context::default();
        let mut t = Typesetter::new(&ctx);
        let job = |i: usize| Job::Math { latex: format!("x_{{{i}}}"), display: false, size: 14, color: [0, 0, 0, 255] };
        for i in 0..MAX_CACHED + 50 {
            let _ = t.get(&job(i));
        }
        // Everything still pending stays; once rendered, the oldest go first.
        let start = std::time::Instant::now();
        while t.cache.values().any(|e| matches!(e.out, Out::Pending)) {
            t.poll();
            assert!(start.elapsed().as_secs() < 120, "rendering took too long");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        t.poll();
        let _ = t.get(&job(MAX_CACHED + 49));
        t.evict();
        assert!(t.cache.len() <= MAX_CACHED, "{}", t.cache.len());
        assert_eq!(t.cache.values().map(|e| e.bytes).sum::<usize>(), t.bytes);
        assert!(!matches!(t.get(&job(MAX_CACHED + 49)), Out::Pending), "the most recent survives");
    }

    #[test]
    fn math_and_diagrams_come_back_as_textures() {
        let ctx = egui::Context::default();
        let mut t = Typesetter::new(&ctx);
        let math = Job::Math { latex: r"\frac{a}{b} + \sqrt{x^2}".into(), display: true, size: 16, color: [20, 20, 20, 255] };
        let bad = Job::Math { latex: r"\frac{a".into(), display: false, size: 16, color: [20, 20, 20, 255] };
        let diagram = Job::Diagram { source: "graph TD\n  A --> B".into(), dark: false };
        assert!(matches!(t.get(&math), Out::Pending));
        let _ = t.get(&bad);
        let _ = t.get(&diagram);
        let start = std::time::Instant::now();
        loop {
            t.poll();
            let all_done = [&math, &bad, &diagram].iter().all(|j| !matches!(t.get(j), Out::Pending));
            if all_done {
                break;
            }
            assert!(start.elapsed().as_secs() < 60, "rendering took too long");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let Out::Ready(p) = t.get(&math) else { panic!("math should render") };
        assert!(p.size.x > 10.0 && p.size.y > 10.0 && p.baseline > 0.0 && p.baseline <= p.size.y + 0.5, "{:?} {}", p.size, p.baseline);
        assert!(matches!(t.get(&bad), Out::Failed(_)));
        let Out::Ready(d) = t.get(&diagram) else { panic!("diagram should render") };
        assert!(d.size.x > 20.0 && d.size.y > 20.0);
    }

    #[test]
    fn zoomed_diagrams_are_drawn_again_sharper_and_kept_few() {
        assert_eq!(zoom_bucket(1.0), None);
        assert_eq!(zoom_bucket(1.5), None);
        assert_eq!(zoom_bucket(1.6), Some(2.0));
        assert_eq!(zoom_bucket(2.5), Some(3.0));
        assert_eq!(zoom_bucket(7.9), Some(8.0));
        assert_eq!(zoom_bucket(50.0), Some(12.0));
        assert_eq!(zoom_bucket(f32::NAN), None);
        let ctx = egui::Context::default();
        let mut t = Typesetter::new(&ctx);
        let wait = |t: &mut Typesetter, job: &Job, bucket: Option<f32>| {
            let start = std::time::Instant::now();
            loop {
                t.poll();
                let out = match bucket {
                    Some(b) => t.get_zoomed(job, b),
                    None => t.get(job),
                };
                match out {
                    Out::Ready(p) => return p,
                    Out::Failed(e) => panic!("{e}"),
                    Out::Pending => {}
                }
                assert!(start.elapsed().as_secs() < 60, "rendering took too long");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let job = Job::Diagram { source: "graph TD\n  A --> B --> C".into(), dark: false };
        let base = wait(&mut t, &job, None);
        let sharp = wait(&mut t, &job, Some(4.0));
        // The same picture in points, with more pixels.
        assert_eq!(sharp.size, base.size);
        let (bw, sw) = (base.texture.size()[0] as f32, sharp.texture.size()[0] as f32);
        assert!(sw > bw * 2.4, "{bw} → {sw}");
        // Many zoom levels of many diagrams: only a few are kept.
        for i in 0..MAX_ZOOMED + 4 {
            let j = Job::Diagram { source: format!("graph TD\n  A{i} --> B"), dark: false };
            let _ = wait(&mut t, &j, Some(2.0));
        }
        t.evict_zoomed();
        assert!(t.zoomed.len() <= MAX_ZOOMED);
        assert_eq!(t.zoomed.values().map(|e| e.bytes).sum::<usize>(), t.zoomed_bytes);
        assert!(!t.cache.is_empty(), "the ordinary pictures stay");
    }
}
