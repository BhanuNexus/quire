// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The drawing editor driven by input: tools, saving, conflicts, keys,
//! and the frame budget with 2,000 elements.

use egui::{Event, Modifiers, PointerButton, RawInput};
use quire_app::keymap::{Action, Keymap};

use super::*;
use crate::theme;

const RELEASE: &str = include_str!("../../../../crates/quire-canvas/testdata/excalidraw/Release.excalidraw");
const ROADMAP: &str = include_str!("../../../../crates/quire-canvas/testdata/excalidraw/Roadmap sketch.excalidraw.md");

struct Rig {
    ctx: egui::Context,
    ws: Workspace,
    ed: DrawingEditor,
    t: f64,
    _dir: TempDir,
}

/// A folder removed when dropped.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("quire-drawing-edit-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SCREEN: Vec2 = Vec2::new(1280.0, 800.0);

impl Rig {
    fn new(files: &[(&str, &str)], open: &str) -> Self {
        let dir = TempDir::new();
        for (p, t) in files {
            let abs = dir.path().join(p);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, t).unwrap();
        }
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::open_local(dir.path()).unwrap());
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        ctx.global_style_mut(theme::style);
        ws.index().flush();
        let mut r = Self { ctx, ws, ed: DrawingEditor::new(&NotePath::new(open)), t: 0.0, _dir: dir };
        r.frame(vec![]);
        r.frame(vec![]);
        // The pointer over the canvas: its keys go to the editor.
        let c = r.canvas_rect().center();
        r.frame(vec![Event::PointerMoved(c)]);
        r
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.t += 1.0 / 60.0;
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)), events, time: Some(self.t), ..Default::default() };
        let mut out = self.ctx.run_ui(raw, |ui| {
            let _ = self.ed.show(ui, &mut self.ws, &theme::LIGHT);
        });
        out.textures_delta.clear();
    }

    /// A drag with the primary button from `a` to `b` (screen).
    fn drag(&mut self, a: Pos2, b: Pos2, mods: Modifiers) {
        self.frame(vec![Event::PointerMoved(a)]);
        self.frame(vec![Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: true, modifiers: mods }]);
        for i in 1..=6 {
            let p = a + (b - a) * (i as f32 / 6.0);
            self.frame(vec![Event::PointerMoved(p)]);
        }
        self.frame(vec![Event::PointerButton { pos: b, button: PointerButton::Primary, pressed: false, modifiers: mods }]);
        self.frame(vec![]);
    }

    fn click(&mut self, a: Pos2, mods: Modifiers) {
        self.frame(vec![Event::PointerMoved(a)]);
        self.frame(vec![Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: true, modifiers: mods }]);
        self.frame(vec![Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: false, modifiers: mods }]);
        self.frame(vec![]);
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        self.frame(vec![Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers }]);
    }

    /// Wait past the autosave delay, then a frame saves.
    fn autosave(&mut self) {
        self.ed.last_edit = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        self.frame(vec![]);
    }

    fn disk(&self, p: &str) -> String {
        drawings::read_drawing(&self.ws, &NotePath::new(p)).unwrap()
    }

    fn d(&self) -> &Drawing {
        self.ed.d.as_ref().expect("a drawing")
    }

    /// A scene point on screen.
    fn at(&self, x: f64, y: f64) -> Pos2 {
        View { rect: self.canvas_rect(), offset: self.ed.offset, zoom: self.ed.zoom }.to_screen((x, y))
    }

    fn canvas_rect(&self) -> Rect {
        self.ed.last_rect
    }
}

#[test]
fn draws_shapes_and_saves_them_with_every_field_kept() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
    assert!(r.ed.error.is_none(), "{:?}", r.ed.error);
    let before = r.d().state.elements.len();
    r.ed.tool = Tool::Rectangle;
    let c = r.canvas_rect().center();
    r.drag(c, c + Vec2::new(120.0, 80.0), Modifiers::NONE);
    assert_eq!(r.d().state.elements.len(), before + 1, "a rectangle");
    let new = std::sync::Arc::clone(r.d().state.elements.last().unwrap());
    assert_eq!(new.str("type"), Some("rectangle"));
    assert!(new.num("width").unwrap() > 50.0 && new.num("height").unwrap() > 30.0);
    assert_eq!(r.ed.tool, Tool::Select, "back to selecting, as Excalidraw");
    assert_eq!(r.ed.sel, [new.str("id").unwrap().to_string()]);
    r.autosave();
    let disk = r.disk("Release.excalidraw");
    assert!(disk.contains(new.str("id").unwrap()));
    for kept in ["\"customData\"", "\"hexagon-from-the-future\"", "\"lockedMultiSelections\": {}", "\"dataURL\": \"data:image/png;base64,"]
    {
        assert!(disk.contains(kept), "{kept}");
    }
    assert!(Drawing::parse(&disk).is_ok());
    // Undo: the file goes back to the bytes it had.
    r.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(r.d().state.elements.len(), before);
    r.autosave();
    assert_eq!(r.disk("Release.excalidraw"), RELEASE, "byte for byte");
    r.key(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT);
    assert_eq!(r.d().state.elements.len(), before + 1);
}

#[test]
fn arrows_bind_to_the_shapes_at_their_ends() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
    r.ed.tool = Tool::Arrow;
    // From inside the diamond to inside the ellipse.
    let (a, b) = (r.at(620.0, 60.0), r.at(400.0, 50.0));
    r.drag(a, b, Modifiers::NONE);
    let arrow = std::sync::Arc::clone(r.d().state.elements.last().unwrap());
    assert_eq!(arrow.str("type"), Some("arrow"));
    assert_eq!(arrow.obj("startBinding").and_then(|b| b.str("elementId")), Some("dia1"));
    assert_eq!(arrow.obj("endBinding").and_then(|b| b.str("elementId")), Some("ell1"));
    let id = arrow.str("id").unwrap();
    assert!(r.d().get("ell1").unwrap().arr("boundElements").unwrap().iter().any(|b| b.as_obj().and_then(|b| b.str("id")) == Some(id)));
    // Moving the ellipse brings the arrow's end along.
    r.ed.tool = Tool::Select;
    r.ed.sel.clear();
    let end_before = drawing::points(r.d().get(id).unwrap())[1];
    // The ellipse by its outline (it has no fill), at its bottom.
    r.drag(r.at(400.0, 100.0), r.at(400.0, 300.0), Modifiers::NONE);
    assert_eq!(r.d().get("ell1").unwrap().num("y"), Some(200.0), "the ellipse moved");
    let end_after = drawing::points(r.d().get(id).unwrap())[1];
    assert!(end_after.1 > end_before.1 + 100.0, "{end_before:?} → {end_after:?}");
}

#[test]
fn text_tool_types_links_into_a_drawing_note_and_the_index_sees_them() {
    let mut r = Rig::new(&[("Sketch.excalidraw.md", ROADMAP), ("Target.md", "# Target\n")], "Sketch.excalidraw.md");
    r.ed.tool = Tool::Text;
    let p = r.at(0.0, 300.0);
    r.click(p, Modifiers::NONE);
    assert!(r.ed.typing.is_some(), "typing");
    r.frame(vec![Event::Text("see [[Target]]".into())]);
    r.key(Key::Escape, Modifiers::NONE);
    assert!(r.ed.typing.is_none());
    let t = std::sync::Arc::clone(r.d().state.elements.last().unwrap());
    assert_eq!(t.str("rawText"), Some("see [[Target]]"));
    assert_eq!(t.str("text"), Some("see Target"));
    assert_eq!(t.str("id").unwrap().len(), 8, "the plugin's text ids");
    assert!(t.num("width").unwrap() > 40.0, "measured");
    r.autosave();
    let disk = r.disk("Sketch.excalidraw.md");
    assert!(disk.contains(&format!("see [[Target]] ^{}\n\n", t.str("id").unwrap())), "{disk}");
    assert!(disk.contains("```compressed-json\n"), "still compressed");
    r.ws.index().flush();
    let back = r.ws.index().read().backlinks(&NotePath::new("Target.md")).to_vec();
    assert!(back.iter().any(|b| b.from.as_str() == "Sketch.excalidraw.md"), "a backlink from the drawing");
    // An empty new text is dropped without a trace.
    let n = r.d().state.elements.len();
    r.ed.tool = Tool::Text;
    r.click(r.at(0.0, 400.0), Modifiers::NONE);
    r.key(Key::Escape, Modifiers::NONE);
    assert_eq!(r.d().state.elements.len(), n);
}

#[test]
fn a_change_elsewhere_is_never_written_over() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
    // No edits here: it reloads.
    let theirs = RELEASE.replace("\"x\": 560", "\"x\": 561");
    r.ws.vault().unwrap().write(&NotePath::new("Release.excalidraw"), &theirs).unwrap();
    *r.ws.features.canvas.disk_changes.entry(NotePath::new("Release.excalidraw")).or_default() += 1;
    r.frame(vec![]);
    assert_eq!(r.d().get("dia1").unwrap().num("x"), Some(561.0), "reloaded");
    // Edits here and a change there: a conflict, nothing written.
    r.ed.sel = vec!["box1".into()];
    r.key(Key::Delete, Modifiers::NONE);
    let theirs2 = theirs.replace("\"x\": 561", "\"x\": 562");
    r.ws.vault().unwrap().write(&NotePath::new("Release.excalidraw"), &theirs2).unwrap();
    r.autosave();
    assert!(r.ed.conflict.is_some(), "the checked write found their change");
    assert_eq!(r.disk("Release.excalidraw"), theirs2, "theirs is untouched");
    r.autosave();
    assert_eq!(r.disk("Release.excalidraw"), theirs2, "and stays untouched");
    // Keep mine: theirs goes beside it, mine is saved.
    r.ed.resolve(&mut r.ws, true);
    assert!(r.ed.conflict.is_none());
    assert_eq!(r.disk("Release (changed elsewhere).excalidraw"), theirs2);
    assert!(r.disk("Release.excalidraw").contains("\"isDeleted\": true"));
}

#[test]
fn closing_with_unsaved_edits_saves_them() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
    r.ed.sel = vec!["dia1".into()];
    r.key(Key::ArrowRight, Modifiers::NONE);
    assert!(r.ed.dirty);
    let ed = std::mem::replace(&mut r.ed, DrawingEditor::new(&NotePath::new("x.excalidraw")));
    drop(ed);
    assert!(r.disk("Release.excalidraw").contains("\"x\": 561"), "saved on close");
}

#[test]
fn group_lock_duplicate_layers_and_clipboard() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
    r.ed.sel = vec!["ell1".into(), "dia1".into()];
    r.ed.on_sel(|d, s, cx| {
        d.group(s, cx);
    });
    r.ed.sel.clear();
    r.click(r.at(620.0, 60.0), Modifiers::NONE);
    assert_eq!(r.ed.sel.len(), 2, "a click selects the group");
    r.key(Key::D, Modifiers::COMMAND);
    assert_eq!(r.ed.sel.len(), 2);
    assert!(r.ed.sel.iter().all(|s| s != "ell1" && s != "dia1"), "the copies are selected");
    r.key(Key::L, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(r.ed.sel.iter().all(|s| r.d().get(s).unwrap().bool("locked") == Some(true)));
    let n = r.d().state.elements.len();
    let live = drawing::live(&r.d().state.elements).count();
    r.key(Key::Delete, Modifiers::NONE);
    assert_eq!(drawing::live(&r.d().state.elements).count(), live, "locked: not deleted");
    assert_eq!(r.ed.sel.len(), 2, "and still selected");
    let clip = r.ed.copy().unwrap();
    assert!(clip.starts_with("{\"type\":\"excalidraw/clipboard\""));
    r.frame(vec![Event::Paste(clip)]);
    assert_eq!(r.d().state.elements.len(), n + 2);
    // Layers.
    r.ed.sel = vec!["box1".into()];
    r.key(Key::CloseBracket, Modifiers::COMMAND);
    let i = r.d().index_of("box1").unwrap();
    assert_eq!(r.d().state.elements[i - 1].str("id"), Some("lbl1"), "one step up");
}

#[test]
fn links_open_their_notes() {
    let mut r = Rig::new(&[("Release.excalidraw", RELEASE), ("Roadmap.md", "# Roadmap\n")], "Release.excalidraw");
    let mut out = PanelOut::default();
    DrawingEditor::open_link(&mut r.ws, &r.ctx.clone(), "[[Roadmap]]", &mut out);
    assert!(out.jumped);
    assert_eq!(r.ws.active_doc().and_then(|d| d.path.clone()), Some(NotePath::new("Roadmap.md")));
}

/// 2,000 elements: only what's in view is drawn, a frame that changes
/// nothing tessellates nothing, and a frame stays inside its budget.
#[test]
fn two_thousand_elements_stay_smooth() {
    let mut d = Drawing::new_empty(false);
    let mut cx = Cx::new(1, 7);
    let kinds = ["rectangle", "ellipse", "diamond", "arrow", "freedraw", "text"];
    for i in 0..2000 {
        let (x, y) = (f64::from(i % 50) * 120.0, f64::from(i / 50) * 120.0);
        let kind = kinds[i as usize % kinds.len()];
        let mut e = d.new_element(kind, x, y, &Style::default(), &mut cx);
        e.set_num("width", 80.0);
        e.set_num("height", 60.0);
        match kind {
            "arrow" => drawing::set_points(&mut e, &[(0.0, 0.0), (80.0, 60.0)]),
            "freedraw" => {
                let wave: Vec<(f64, f64)> = (0..40).map(|k| (f64::from(k) * 2.0, (f64::from(k) / 3.0).sin() * 20.0)).collect();
                drawing::set_points(&mut e, &wave);
            }
            "text" => {
                e.set_str("text", &format!("Label {i}"));
                e.set_str("originalText", &format!("Label {i}"));
            }
            _ => {}
        }
        d.add(e, &mut cx);
    }
    let text = d.to_text();
    let mut r = Rig::new(&[("Big.excalidraw", &text)], "Big.excalidraw");
    assert_eq!(drawing::live(&r.d().state.elements).count(), 2000);
    // At 100 %, part of the drawing is in view.
    r.ed.zoom = 1.0;
    r.ed.offset = Vec2::ZERO;
    r.ed.zoomed_at = None;
    r.frame(vec![]);
    let drawn = r.ed.drawn;
    assert!(drawn.elements > 50 && drawn.elements < 400, "only what's in view: {drawn:?}");
    r.frame(vec![]);
    assert_eq!(r.ed.drawn.tessellated, 0, "kept meshes are reused");
    // Zoomed out to see everything, then panning: time per frame.
    r.ed.fitted = false;
    r.frame(vec![]);
    r.ed.zoomed_at = None;
    r.frame(vec![]);
    assert_eq!(r.ed.drawn.elements, 2000);
    let start = Instant::now();
    for i in 0..20 {
        r.ed.offset.x += if i % 2 == 0 { 3.0 } else { -3.0 };
        r.frame(vec![]);
    }
    let per = start.elapsed().as_secs_f64() * 1000.0 / 20.0;
    assert_eq!(r.ed.drawn.tessellated, 0, "panning tessellates nothing");
    // A debug build is far slower than a release one; the budget allows that.
    assert!(per < 100.0, "{per:.1} ms a frame");
    eprintln!("2,000 elements, all in view: {per:.1} ms a frame (debug build)");
}

/// The editor's own shortcuts don't take a key Quire's commands use,
/// except editing keys, which Quire leaves to a focused editor.
#[test]
fn keys_dont_conflict_with_quires() {
    let km = Keymap::new(&quire_app::settings::CommandSettings::default());
    let me = Action::Macro("drawing editor".into());
    let mut taken = Vec::new();
    let tool_keys = [
        "V",
        "1",
        "H",
        "R",
        "2",
        "D",
        "3",
        "O",
        "4",
        "A",
        "5",
        "L",
        "6",
        "P",
        "X",
        "7",
        "T",
        "8",
        "9",
        "E",
        "0",
        "Delete",
        "Backspace",
        "Escape",
        "Enter",
    ];
    let combos: Vec<String> = SHORTCUTS.iter().map(|(k, _)| (*k).to_string()).chain(tool_keys.iter().map(|k| (*k).to_string())).collect();
    for spec in combos {
        let combo = quire_app::commands::KeyCombo::parse(&spec).unwrap_or_else(|| panic!("{spec} parses"));
        for c in km.conflicts(&combo, &me) {
            let yields = match &c {
                quire_app::keymap::Conflict::Taken(Action::Command(cmd)) => matches!(cmd.info().category, "Edit" | "Format" | "Review"),
                quire_app::keymap::Conflict::Taken(_) | quire_app::keymap::Conflict::Reserved { .. } => false,
                _ => true,
            };
            if !yields {
                taken.push(format!("{spec}: {}", c.describe()));
            }
        }
    }
    assert!(taken.is_empty(), "{taken:#?}");
}

#[test]
fn drawing_notes_open_in_the_editor_unless_markdown_was_chosen() {
    let mut r = Rig::new(&[("Sketch.excalidraw.md", ROADMAP), ("N.md", "# N\n")], "Sketch.excalidraw.md");
    let mut s = crate::surfaces::Surfaces::default();
    r.ws.open_note(&NotePath::new("Sketch.excalidraw.md")).unwrap();
    route(&mut r.ws, &mut s);
    assert_eq!(s.open.len(), 1, "the editor opened");
    assert!(r.ws.docs().iter().all(|d| d.path.as_ref().is_none_or(|p| p.as_str() != "Sketch.excalidraw.md")), "the Markdown tab closed");
    // "Markdown" in the editor: the note stays a note.
    AS_MARKDOWN.with_borrow_mut(|m| m.insert(NotePath::new("Sketch.excalidraw.md")));
    s.active = None;
    r.ws.open_note(&NotePath::new("Sketch.excalidraw.md")).unwrap();
    route(&mut r.ws, &mut s);
    assert!(r.ws.active_doc().is_some_and(|d| d.path.as_ref().is_some_and(|p| p.as_str() == "Sketch.excalidraw.md")));
    AS_MARKDOWN.with_borrow_mut(HashSet::clear);
    // Embeds name the drawing without `.md`.
    r.ws.index().flush();
    assert_eq!(embed_target(&r.ws, "Sketch.excalidraw").map(|p| p.as_str().to_string()).as_deref(), Some("Sketch.excalidraw.md"));
    assert_eq!(embed_target(&r.ws, "N"), None);
}

#[test]
fn the_editor_keeps_its_layout() {
    for (w, h) in [(1280.0_f32, 820.0_f32), (900.0, 560.0), (760.0, 480.0)] {
        for dark in [false, true] {
            let mut r = Rig::new(&[("Release.excalidraw", RELEASE)], "Release.excalidraw");
            r.ed.sel = vec!["box1".into(), "arr1".into()];
            let mut sizes = Vec::new();
            for f in 0..24 {
                r.t += 1.0 / 60.0;
                let raw = RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
                    events: vec![Event::PointerMoved(Pos2::new(w - 100.0, 200.0 + f as f32 * 7.0))],
                    time: Some(r.t),
                    ..Default::default()
                };
                let pal = if dark { &theme::DARK } else { &theme::LIGHT };
                let mut out = r.ctx.run_ui(raw, |ui| {
                    let rect = ui.allocate_ui(Vec2::new(w, h), |ui| {
                        let _ = r.ed.show(ui, &mut r.ws, pal);
                    });
                    sizes.push(rect.response.rect.size());
                });
                out.textures_delta.clear();
            }
            assert!(
                sizes[8..].iter().all(|s| (s.y - sizes[8].y).abs() < 0.5 && s.x <= w + 0.5 && s.y <= h + 0.5),
                "{w}×{h}: {:?}",
                &sizes[8..]
            );
        }
    }
}
