// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sketches (`*.svg` with Quire ink) as pictures in notes and the preview:
//! an egui image loader that draws them itself, in both editions (no SVG
//! renderer needed). Other SVG files are left to other loaders.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context};

/// Pixels per sketch unit (sharp on high-density screens).
const SCALE: f32 = 2.0;

type Cache = HashMap<String, (Option<SystemTime>, Arc<ColorImage>)>;

#[derive(Default)]
pub(crate) struct SketchLoader {
    cache: Mutex<Cache>,
}

/// Install the loader (once, at start).
pub(crate) fn install(ctx: &Context) {
    ctx.add_image_loader(Arc::new(SketchLoader::default()));
}

/// Make pictures of this sketch be drawn again (after it was saved).
pub(crate) fn forget(ctx: &Context, abs: &std::path::Path) {
    ctx.forget_image(&format!("file://{}", abs.display()));
}

impl ImageLoader for SketchLoader {
    fn id(&self) -> &'static str {
        concat!(module_path!(), "::SketchLoader")
    }

    fn load(&self, _ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        let Some(path) = uri.strip_prefix("file://").filter(|p| p.to_ascii_lowercase().ends_with(".svg")) else {
            return Err(LoadError::NotSupported);
        };
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut cache = self.cache.lock().map_err(|_| LoadError::Loading("the sketch cache is unavailable".into()))?;
        if let Some((m, img)) = cache.get(uri)
            && *m == modified
        {
            return Ok(ImagePoll::Ready { image: Arc::clone(img) });
        }
        let text = std::fs::read_to_string(path).map_err(|e| LoadError::Loading(e.to_string()))?;
        // Only Quire's sketches; other SVGs go to other loaders (or show their name).
        if !text.contains("data-quire-sketch") && !text.contains("data-quire-tool") && !text.contains("data-quire-shape") {
            return Err(LoadError::NotSupported);
        }
        let sk = quire_canvas::ink::parse_svg(&text).map_err(LoadError::Loading)?;
        let (w, h, rgba) = quire_canvas::ink::rasterize(&sk, SCALE);
        let mut img = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        img.source_size = egui::Vec2::new(w as f32 / SCALE, h as f32 / SCALE);
        let img = Arc::new(img);
        cache.insert(uri.to_string(), (modified, Arc::clone(&img)));
        Ok(ImagePoll::Ready { image: img })
    }

    fn forget(&self, uri: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.remove(uri);
        }
    }

    fn forget_all(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }

    fn byte_size(&self) -> usize {
        self.cache.lock().map_or(0, |c| c.values().map(|(_, i)| i.pixels.len() * 4).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_quire_sketches_and_leaves_other_svgs() {
        let dir = std::env::temp_dir().join(format!("quire-sketch-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sk = dir.join("a-ink-1.svg");
        std::fs::write(&sk, quire_canvas::ink::empty_svg(40.0, 20.0)).unwrap();
        let other = dir.join("logo.svg");
        std::fs::write(&other, "<svg viewBox=\"0 0 1 1\"/>").unwrap();
        let l = SketchLoader::default();
        let ctx = Context::default();
        match l.load(&ctx, &format!("file://{}", sk.display()), SizeHint::default()) {
            Ok(ImagePoll::Ready { image }) => assert_eq!(image.size, [80, 40]),
            _ => panic!("a sketch loads"),
        }
        assert!(matches!(l.load(&ctx, &format!("file://{}", other.display()), SizeHint::default()), Err(LoadError::NotSupported)));
        assert!(matches!(l.load(&ctx, "file:///x.png", SizeHint::default()), Err(LoadError::NotSupported)));
        assert!(l.byte_size() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
