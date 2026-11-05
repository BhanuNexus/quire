// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Excalidraw drawings in exports: `![[Plan.excalidraw]]` becomes an SVG
//! picture (0.10). The app draws them itself (`drawing_ui` in the desktop).

use base64::Engine as _;
use quire_canvas::excalidraw::{self, FileRef};
use quire_vault::{NotePath, Vault};

/// The drawing at `path` as SVG, with its pictures inside as `data:` URLs.
pub fn svg(vault: &Vault, path: &NotePath) -> Option<String> {
    let text = vault.read(path).ok()?;
    let scene = excalidraw::parse(&text).ok()?;
    let href = |id: &str| -> Option<String> {
        match scene.files.get(id)? {
            FileRef::DataUrl(u) => Some(u.clone()),
            FileRef::Vault(rel) => {
                let p = NotePath::new(rel.as_str());
                let bytes = std::fs::read(vault.storage().absolute(&p)?).ok()?;
                let mime = match rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("gif") => "image/gif",
                    Some("webp") => "image/webp",
                    Some("svg") => "image/svg+xml",
                    _ => "image/png",
                };
                Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
            }
        }
    };
    Some(excalidraw::to_svg(&scene, &href))
}

/// SVG as a `data:` URL for an `<img>`.
pub fn data_url(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drawing_exports_as_svg() {
        let md = include_str!("../../quire-canvas/testdata/excalidraw/Plain.excalidraw.md");
        let v = Vault::in_memory(&[("Plan.excalidraw.md", md)]);
        let s = svg(&v, &NotePath::new("Plan.excalidraw.md")).expect("a picture");
        assert!(s.starts_with("<svg") && s.contains(">Idea ^ship</text>"));
        assert!(data_url(&s).starts_with("data:image/svg+xml;base64,PHN2Zy"));
        let mut ws = crate::Workspace::new(crate::Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Plan.excalidraw.md", md), ("Note.md", "# Note\n\n![[Plan.excalidraw]]\n")]));
        ws.index().flush();
        ws.open_note(&NotePath::new("Note.md"));
        let html = ws.export_active_html().and_then(|_| ws.vault()?.read(&NotePath::new("Exports/Note.html")).ok()).unwrap_or_default();
        assert!(html.contains("<img src=\"data:image/svg+xml;base64,"), "{html}");
    }
}
