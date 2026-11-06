// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Excalidraw drawings in exports: `![[Plan.excalidraw]]` becomes an SVG
//! picture (0.10). The app draws them itself (`drawing_ui` in the desktop).

use base64::Engine as _;
use quire_canvas::excalidraw::{self, FileRef};
use quire_vault::{NotePath, Vault};

/// Picture types a drawing may embed from the vault.
const PICTURES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
];

/// The picture type of `p` by its extension, if it is one a drawing may embed.
pub fn picture_type(p: &str) -> Option<&'static str> {
    let ext = p.rsplit_once('.')?.1.to_ascii_lowercase();
    PICTURES.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// Where a drawing's `[[rel]]` may point, nearest first: next to the
/// drawing, then each folder above it up to the vault (how Excalidraw
/// finds `[[attachments/x.png]]`). Never outside the vault.
pub fn candidates(drawing: &NotePath, rel: &str) -> Vec<NotePath> {
    let mut out = Vec::new();
    let mut folder = drawing.folder();
    loop {
        out.push(NotePath::join(folder, rel));
        if folder.is_empty() {
            break;
        }
        folder = folder.rsplit_once('/').map_or("", |(d, _)| d);
    }
    out
}

/// The file a drawing at `drawing` embeds as `[[rel]]`, if Quire may show it.
///
/// A vault path (0.10.1 security fix). It goes through the same
/// check as every other file the app or a published site shows: a
/// picture type, canonical (relative, no `..`, no drive or root, no
/// control characters), outside hidden folders (`.quire`, `.trash`,
/// `.git`…), and through symlinks it must stay inside the vault and
/// outside hidden folders. `allow` is the caller's own rule (folders hidden
/// from the web), asked for both the path as named and where it leads.
pub fn embedded_file(vault: &Vault, drawing: &NotePath, rel: &str, allow: &dyn Fn(&NotePath) -> bool) -> Option<NotePath> {
    picture_type(rel)?;
    candidates(drawing, rel).into_iter().find_map(|p| {
        let r = quire_agent::vault_file(vault, &p).ok()?;
        (allow(&r.path) && allow(&r.real) && vault.storage().exists(&r.real)).then_some(r.real)
    })
}

/// [`embedded_file`] for a vault folder on disk (the desktop's picture
/// loader, which has paths rather than a [`Vault`]). Returns the file.
pub fn embedded_file_at(root: &std::path::Path, drawing: &NotePath, rel: &str) -> Option<std::path::PathBuf> {
    picture_type(rel)?;
    candidates(drawing, rel).into_iter().find_map(|p| {
        let r = quire_agent::vault_file_at(root, &p).ok()?;
        let abs = root.join(r.real.as_str());
        abs.is_file().then_some(abs)
    })
}

/// The drawing at `path` as SVG, with its pictures inside as `data:` URLs.
/// `allow` is the caller's rule for files it may show ([`embedded_file`]).
pub fn svg(vault: &Vault, path: &NotePath, allow: &dyn Fn(&NotePath) -> bool) -> Option<String> {
    let text = vault.read(path).ok()?;
    let scene = match excalidraw::parse(&text) {
        Ok(s) => s,
        // Over the size limits: a picture that says so (0.10.1 security review #7).
        Err(e) if e == excalidraw::TOO_LARGE => return Some(notice_svg("Drawing too large to show")),
        Err(_) => return None,
    };
    let href = |id: &str| -> Option<String> {
        match scene.files.get(id)? {
            FileRef::DataUrl(u) => Some(u.clone()),
            FileRef::Vault(rel) => {
                let p = embedded_file(vault, path, rel, allow)?;
                let mime = picture_type(p.as_str())?;
                let bytes = vault.storage().read_bytes(&p).ok()?;
                Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
            }
        }
    };
    Some(excalidraw::to_svg(&scene, &href))
}

/// A small picture with a line of text, in place of a drawing.
fn notice_svg(text: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"320\" height=\"48\" viewBox=\"0 0 320 48\">\
<rect x=\"0.5\" y=\"0.5\" width=\"319\" height=\"47\" rx=\"6\" fill=\"#f6f8fa\" stroke=\"#d0d7de\"/>\
<text x=\"160\" y=\"29\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"14\" fill=\"#57606a\">{text}</text></svg>"
    )
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
        let s = svg(&v, &NotePath::new("Plan.excalidraw.md"), &|_| true).expect("a picture");
        assert!(s.starts_with("<svg") && s.contains(">Idea ^ship</text>"));
        assert!(data_url(&s).starts_with("data:image/svg+xml;base64,PHN2Zy"));
        let mut ws = crate::Workspace::new(crate::Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Plan.excalidraw.md", md), ("Note.md", "# Note\n\n![[Plan.excalidraw]]\n")]));
        ws.index().flush();
        ws.open_note(&NotePath::new("Note.md"));
        let html = ws.export_active_html().and_then(|_| ws.vault()?.read(&NotePath::new("Exports/Note.html")).ok()).unwrap_or_default();
        assert!(html.contains("<img src=\"data:image/svg+xml;base64,"), "{html}");
    }

    /// The test drawing with its picture pointing at `target`.
    fn drawing_to(target: &str) -> String {
        include_str!("../../quire-canvas/testdata/excalidraw/Plain.excalidraw.md")
            .replace("[[attachments/dot.png]]", &format!("[[{target}]]"))
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    /// 0.10.1 security review #1: every file a drawing embeds passes the
    /// vault's path checks; hidden folders, `.quire`, symlinks out of the
    /// vault and folders the caller hides are never read.
    #[test]
    fn drawing_embeds_pass_the_path_guard() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.png"), "OUTSIDE-THE-VAULT").unwrap();
        let put = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put("attachments/dot.png", "VISIBLE-PICTURE");
        put("Journal/diary.png", "TOPSECRET-JOURNAL");
        put(".quire/agent/x.png", "QUIRE-INTERNAL");
        put(".hidden/x.png", "HIDDEN-FOLDER");
        put("notes.md", "NOT-A-PICTURE");
        let v = Vault::open_local(&root).unwrap();
        let svg_of = |target: &str, allow: &dyn Fn(&NotePath) -> bool| {
            v.write(&NotePath::new("D.excalidraw.md"), &drawing_to(target)).unwrap();
            svg(&v, &NotePath::new("D.excalidraw.md"), allow).expect("the drawing itself still shows")
        };
        let all = |_: &NotePath| true;
        assert!(svg_of("attachments/dot.png", &all).contains(&b64("VISIBLE-PICTURE")), "an ordinary picture still embeds");
        for (target, marker) in [
            (".quire/agent/x.png", "QUIRE-INTERNAL"),
            (".hidden/x.png", "HIDDEN-FOLDER"),
            ("attachments/../.quire/agent/x.png", "QUIRE-INTERNAL"),
            ("notes.md", "NOT-A-PICTURE"),
        ] {
            assert!(!svg_of(target, &all).contains(&b64(marker)), "{target} must not be embedded");
        }
        // A folder the caller hides (the web's `--hide Journal`), any letter case.
        let no_journal = |p: &NotePath| !p.as_str().to_lowercase().starts_with("journal/");
        assert!(svg_of("attachments/dot.png", &no_journal).contains(&b64("VISIBLE-PICTURE")));
        assert!(!svg_of("Journal/diary.png", &no_journal).contains(&b64("TOPSECRET-JOURNAL")));
        assert!(!svg_of("JOURNAL/diary.png", &no_journal).contains(&b64("TOPSECRET-JOURNAL")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.png"), root.join("link.png")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
            std::os::unix::fs::symlink(root.join("Journal"), root.join("alias")).unwrap();
            assert!(!svg_of("link.png", &all).contains(&b64("OUTSIDE-THE-VAULT")), "a symlink out of the vault");
            assert!(!svg_of("out/secret.png", &all).contains(&b64("OUTSIDE-THE-VAULT")), "a folder symlink out of the vault");
            assert!(!svg_of("alias/diary.png", &no_journal).contains(&b64("TOPSECRET-JOURNAL")), "a symlink into a hidden folder");
            let found = embedded_file_at(&root.canonicalize().unwrap(), &NotePath::new("D.excalidraw.md"), "link.png");
            assert_eq!(found, None, "the desktop's loader refuses it too");
            let found = embedded_file_at(&root.canonicalize().unwrap(), &NotePath::new("Sub/D.excalidraw.md"), "attachments/dot.png");
            assert!(found.is_some(), "the desktop finds a picture in a folder above the drawing");
        }
    }

    #[test]
    fn a_drawing_over_the_limits_says_so() {
        let many = format!("{{\"elements\": [{}]}}", vec!["{\"type\":\"rectangle\"}"; 50_001].join(","));
        let v = Vault::in_memory(&[("Big.excalidraw", many.as_str())]);
        let s = svg(&v, &NotePath::new("Big.excalidraw"), &|_| true).unwrap();
        assert!(s.contains("Drawing too large to show"), "{s}");
    }

    /// Absolute and drive paths never leave the vault (parsing only: on
    /// Windows `root.join("C:/…")` would replace the vault's folder).
    #[test]
    fn drawing_embeds_refuse_absolute_and_drive_paths() {
        let v = Vault::in_memory(&[("C:/Users/me/x.png", "X"), ("a/C:/x.png", "X")]);
        let d = NotePath::new("a/D.excalidraw.md");
        for rel in ["C:/Users/me/x.png", "C:\\Users\\me\\x.png", "c:x.png", "\\\\server\\share\\x.png", "../x.png", "a/../../x.png"] {
            assert_eq!(embedded_file(&v, &d, rel, &|_| true), None, "{rel}");
        }
        assert!(candidates(&d, "x.png").iter().all(|p| !p.as_str().contains("..")));
        assert_eq!(candidates(&d, "x.png"), [NotePath::new("a/x.png"), NotePath::new("x.png")]);
    }
}
