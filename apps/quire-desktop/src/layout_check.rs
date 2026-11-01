// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every dialog keeps its size and stays on screen.
//!
//! An egui `Area` (and so a `Modal`) lays its contents out in the rect it
//! measured last frame. A widget that fills the available height (a vertical
//! separator, a scroll area that doesn't shrink, `available_size()`) then
//! measures at least last frame's size plus spacing, so the dialog grows a
//! little on every repaint: scrolling or moving the mouse made Version
//! history creep off the screen. These tests run each overlay for many frames
//! with scrolling and pointer movement, on a normal and a small window, and
//! fail if any area changes size once settled or doesn't fit the window.

use std::fmt::Write as _;

use egui::{Event, MouseWheelSource, MouseWheelUnit, Pos2, RawInput, Rect, TouchPhase, Vec2};
use quire_app::Workspace;
use quire_app::vault::{NotePath, Vault};

use crate::dialogs::{FolderMode, FolderState, HistoryState, ImportState, ProjectState, PublishState, SettingsState};
use crate::overlays::{Mode, Overlay};
use crate::theme;

/// `n` numbered lines (`{}` in `pattern` is the number).
fn lines(n: u32, pattern: &str) -> String {
    (0..n).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "{}", pattern.replace("{}", &i.to_string()));
        s
    })
}

fn workspace() -> Workspace {
    let long: String = lines(300, "Line {} of a long note");
    let notes: Vec<(String, String)> = (0..40)
        .map(|i| (format!("Folder {}/Note {i}.md", i % 5), format!("# Note {i}\n\n[[Note {}]] [[Note {}]]\n", (i + 1) % 40, (i + 7) % 40)))
        .collect();
    let mut files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    files.push(("Long.md", &long));
    let mut ws = Workspace::new(quire_app::Settings::default());
    ws.attach_vault(Vault::in_memory(&files));
    ws.index().flush();
    // Sixty versions, each further from the current text: a long list and a long diff.
    let path = NotePath::new("Long.md");
    let now = chrono::Utc::now().timestamp_millis();
    for i in 1..=60_u32 {
        let old: String = long.clone() + &lines(i * 4, "Old line {}");
        ws.vault().unwrap().snapshot(&path, &old, now - i64::from(i) * 30_000, true).unwrap();
    }
    ws.open_note(&path).unwrap();
    ws
}

fn sync_versions() -> Vec<quire_app::vault_sync::SyncVersion> {
    (0..80)
        .map(|i| quire_app::vault_sync::SyncVersion {
            rev: quire_app::vault_sync::SyncRevId(format!("r{i}")),
            seq: 80 - i,
            at: 1_790_000_000_000 - i64::try_from(i).unwrap() * 60_000,
            device: if i % 3 == 0 { "Phone".into() } else { "A laptop with a rather long device name".into() },
            path: "Long.md".into(),
            deleted: i == 5,
            text: lines(u32::try_from(i * 5).unwrap(), "Synced line {}"),
        })
        .collect()
}

fn overlays(ws: &Workspace) -> Vec<(&'static str, Overlay)> {
    let mut v = vec![
        ("palette", Overlay::palette(Mode::Commands)),
        ("quick open", Overlay::palette(Mode::Notes)),
        ("tasks", Overlay::palette(Mode::Tasks)),
        ("rename", Overlay::Rename { input: "Long".into(), focused: false }),
        ("delete", Overlay::ConfirmDelete),
        ("open vault", Overlay::OpenVault { input: String::new() }),
        ("replace", Overlay::VaultReplace { focused: false }),
        ("ask", Overlay::Ask { focused: false }),
        ("history", Overlay::History(HistoryState::new(ws))),
        ("history: all devices", Overlay::History(HistoryState::all_devices(ws, Ok(sync_versions())))),
        ("history: all devices, error", Overlay::History(HistoryState::all_devices(ws, Err("The sync server is too old. ".repeat(8))))),
        ("import", Overlay::Import(ImportState::default())),
        ("new project", Overlay::NewProject(ProjectState::default())),
        ("publish", Overlay::Publish(PublishState::new(ws))),
        ("new note folder", Overlay::Folder(FolderState::new(ws, FolderMode::NewNote))),
        ("move", Overlay::Folder(FolderState::new(ws, FolderMode::Move))),
        ("shortcuts", Overlay::Shortcuts { query: String::new(), focused: false }),
        ("conflicts", Overlay::SyncConflicts),
        ("graph", Overlay::Graph(Box::new(crate::graph_view::GraphState::new(ws)))),
    ];
    for (i, st) in SettingsState::every_tab().into_iter().enumerate() {
        v.push((["settings: editor", "settings: ai", "settings: sync", "settings: about"][i], Overlay::Settings(Box::new(st))));
    }
    for step in 0..crate::dialogs::TOUR_STEPS {
        v.push(("tour", Overlay::Tour { step }));
    }
    v
}

fn input(screen: Rect, frame: usize) -> RawInput {
    // Wander the pointer over the dialog and scroll down, then back up.
    let t = frame as f32;
    let pos = Pos2::new(screen.center().x + (t * 37.0) % 300.0 - 150.0, screen.center().y + (t * 23.0) % 200.0 - 100.0);
    let mut events = vec![Event::PointerMoved(pos)];
    if frame.is_multiple_of(2) {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: Vec2::new(0.0, if frame < 60 { -120.0 } else { 120.0 }),
            phase: TouchPhase::Move,
            source: MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::NONE,
        });
    }
    RawInput { screen_rect: Some(screen), events, time: Some(f64::from(t) / 60.0), ..Default::default() }
}

#[test]
fn dialogs_keep_their_size_and_fit_the_window() {
    let mut failures = Vec::new();
    for size in [Vec2::new(1280.0, 820.0), Vec2::new(900.0, 560.0), Vec2::new(760.0, 480.0)] {
        let screen = Rect::from_min_size(Pos2::ZERO, size);
        let mut ws = workspace();
        for (name, mut ov) in overlays(&ws) {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            ctx.set_visuals(theme::visuals(&theme::DARK));
            ctx.global_style_mut(theme::style);
            let mut sizes: Vec<Vec<(egui::LayerId, Rect)>> = Vec::new();
            for frame in 0..120 {
                let mut out = ctx.run_pass(input(screen, frame), |ctx| {
                    let _ = crate::overlays::show(ctx, &mut ov, &mut ws, &theme::DARK);
                });
                out.textures_delta.clear();
                let mut rects: Vec<(egui::LayerId, Rect)> =
                    ctx.memory(|m| m.areas().visible_layer_ids().into_iter().filter_map(|l| m.area_rect(l.id).map(|r| (l, r))).collect());
                rects.sort_by_key(|(l, _)| l.id.value());
                sizes.push(rects);
            }
            // After a few frames to settle, nothing may change size.
            let settled = &sizes[10];
            'frames: for (frame, now) in sizes.iter().enumerate().skip(11) {
                for (layer, r) in now {
                    if let Some((_, before)) = settled.iter().find(|(l, _)| l == layer)
                        && ((r.height() - before.height()).abs() > 0.5 || (r.width() - before.width()).abs() > 0.5)
                    {
                        failures.push(format!(
                            "{name} at {size:?}: went from {:.0}×{:.0} to {:.0}×{:.0} by frame {frame}",
                            before.width(),
                            before.height(),
                            r.width(),
                            r.height()
                        ));
                        break 'frames;
                    }
                }
            }
            // And all of it on screen, so every button can be reached.
            if std::env::var("QUIRE_LAYOUT_DEBUG").is_ok() {
                eprintln!("{name} {size:?} {:?}", sizes.last().unwrap().iter().map(|(_, r)| *r).collect::<Vec<_>>());
            }
            for (_, r) in sizes.last().unwrap() {
                if !screen.expand(0.5).contains_rect(*r) {
                    failures.push(format!("{name} at {size:?}: {r:?} doesn't fit the window"));
                }
            }
        }
    }
    failures.dedup();
    assert!(failures.is_empty(), "{failures:#?}");
}
