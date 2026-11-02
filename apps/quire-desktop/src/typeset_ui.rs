// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Math and diagrams as textures, rendered off the UI thread.
//!
//! The editor and preview ask for a formula or a Mermaid diagram every frame;
//! the first ask queues it for a worker thread (Typst for math, the Mermaid
//! renderer for diagrams, then resvg to pixels) and returns `Pending`. When
//! the picture is ready the UI repaints and gets `Ready`. A formula that
//! doesn't parse comes back `Failed` with the reason. Results are kept per
//! (source, size, colour, scale) and dropped when there are too many.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};

use crossbeam_channel::{Receiver, Sender};
use egui::{Color32, ColorImage, TextureHandle, TextureOptions, Vec2};
use resvg::{tiny_skia, usvg};

/// What to draw.
#[derive(Debug, Clone, PartialEq, Hash)]
pub(crate) enum Job {
    /// LaTeX math at `size` points in `color`; `display` for `$$ $$` blocks.
    Math { latex: String, display: bool, size: u32, color: [u8; 4] },
    /// A Mermaid diagram.
    Diagram { source: String, dark: bool },
}

/// A finished picture: its texture and size in points; `baseline` is the
/// distance from the top to the text baseline (math only).
#[derive(Clone)]
pub(crate) struct Picture {
    pub texture: TextureHandle,
    pub size: Vec2,
    pub baseline: f32,
}

#[derive(Clone)]
pub(crate) enum Out {
    Pending,
    Ready(Picture),
    Failed(String),
}

struct Done {
    key: u64,
    result: Result<(ColorImage, Vec2, f32), String>,
}

pub(crate) struct Typesetter {
    ctx: egui::Context,
    cache: HashMap<u64, Out>,
    jobs: Sender<(u64, Job, f32)>,
    done: Receiver<Done>,
}

/// Biggest picture side in pixels (a huge diagram is scaled down to this).
const MAX_PIXELS: f32 = 4096.0;
/// Pictures kept before the cache starts over.
const MAX_CACHED: usize = 600;

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
        Self { ctx: ctx.clone(), cache: HashMap::new(), jobs, done }
    }

    /// Collect finished pictures. Call once per frame.
    pub(crate) fn poll(&mut self) {
        while let Ok(d) = self.done.try_recv() {
            let out = match d.result {
                Ok((image, size, baseline)) => {
                    let texture = self.ctx.load_texture(format!("quire-typeset-{}", d.key), image, TextureOptions::LINEAR);
                    Out::Ready(Picture { texture, size, baseline })
                }
                Err(e) => Out::Failed(e),
            };
            self.cache.insert(d.key, out);
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
        if let Some(out) = self.cache.get(&key) {
            return out.clone();
        }
        if self.cache.len() >= MAX_CACHED {
            self.cache.retain(|_, o| matches!(o, Out::Pending));
        }
        self.cache.insert(key, Out::Pending);
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
    let Ok(mut slot) = shared().lock() else { return Out::Pending };
    let t = slot.get_or_insert_with(|| Typesetter::new(ctx));
    t.poll();
    t.get(job)
}

/// Text colour as RGBA for math.
pub(crate) fn rgba(c: Color32) -> [u8; 4] {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    [r, g, b, a]
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
