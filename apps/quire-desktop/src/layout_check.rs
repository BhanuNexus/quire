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

use crate::dialogs::{FolderMode, FolderState, HistoryState, ProjectState, PublishState, SettingsState};
use crate::overlays::{Mode, Overlay};
use crate::theme;

/// `n` numbered lines (`{}` in `pattern` is the number).
fn lines(n: u32, pattern: &str) -> String {
    (0..n).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "{}", pattern.replace("{}", &i.to_string()));
        s
    })
}

/// The test workspace, with on-device AI's experimental switch on or off.
fn workspace_with(experimental: bool) -> Workspace {
    let long: String = lines(300, "Line {} of a long note");
    let notes: Vec<(String, String)> = (0..40)
        .map(|i| (format!("Folder {}/Note {i}.md", i % 5), format!("# Note {i}\n\n[[Note {}]] [[Note {}]]\n", (i + 1) % 40, (i + 7) % 40)))
        .collect();
    let mut files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    files.push(("Long.md", &long));
    let mut settings = quire_app::Settings::default();
    settings.ai_local.experimental = experimental;
    let mut ws = Workspace::new(settings);
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

fn overlays(ws: &mut Workspace) -> Vec<(&'static str, Overlay)> {
    let mut v = vec![
        ("palette", Overlay::palette(Mode::Commands)),
        ("quick open", Overlay::palette(Mode::Notes)),
        ("tasks", Overlay::palette(Mode::Tasks)),
        ("rename", Overlay::Rename { input: "Long".into(), focused: false }),
        ("delete", Overlay::ConfirmDelete),
        ("open vault", Overlay::OpenVault { input: String::new() }),
        ("replace", Overlay::VaultReplace { focused: false }),
        ("history", Overlay::History(Box::new(HistoryState::new(ws)))),
        ("history: all devices", Overlay::History(Box::new(HistoryState::all_devices(ws, Ok(sync_versions()))))),
        (
            "history: all devices, error",
            Overlay::History(Box::new(HistoryState::all_devices(ws, Err("The sync server is too old. ".repeat(8))))),
        ),
        ("import: source", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(0)))),
        ("import: preview", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(1)))),
        ("import: folder", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(2)))),
        ("import: running", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(3)))),
        ("import: report", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(4)))),
        ("import: error", Overlay::Feature(Box::new(crate::import_ui::ImportWizard::sample(6)))),
        ("new project", Overlay::NewProject(ProjectState::default())),
        ("publish", Overlay::Publish(PublishState::new(ws))),
        ("new note folder", Overlay::Folder(FolderState::new(ws, FolderMode::NewNote))),
        ("move", Overlay::Folder(FolderState::new(ws, FolderMode::Move))),
        ("conflicts", Overlay::SyncConflicts),
        ("graph", Overlay::Graph(Box::new(crate::graph_view::GraphState::new(ws)))),
    ];
    for (name, st) in SettingsState::every_tab() {
        v.push((name, Overlay::Settings(Box::new(st))));
    }
    // Every feature dialog, from each workstream's block in the registry.
    v.extend(crate::registry::layout_samples(ws));
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
    check_dialogs(false, |_| true);
}

/// With on-device AI's experimental switch on, the dialogs that change
/// with it (Set up AI and Settings) too.
#[test]
fn on_device_dialogs_keep_their_size_and_fit_the_window() {
    check_dialogs(true, |name| name.starts_with("set up AI") || name.starts_with("settings: "));
}

fn check_dialogs(experimental: bool, wanted: impl Fn(&str) -> bool) {
    let mut failures = Vec::new();
    for size in [Vec2::new(1280.0, 820.0), Vec2::new(900.0, 560.0), Vec2::new(760.0, 480.0)] {
        let screen = Rect::from_min_size(Pos2::ZERO, size);
        let mut ws = workspace_with(experimental);
        for (name, mut ov) in overlays(&mut ws) {
            if !wanted(name) {
                continue;
            }
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

/// Every registered panel tab (offered or not) draws without growing from
/// frame to frame, in a narrow and a wide panel; the Settings tabs also with
/// on-device AI's experimental switch on.
#[test]
fn feature_tabs_keep_their_size() {
    let mut failures = Vec::new();
    let tables = [
        ("right", crate::registry::RIGHT_TABS, false),
        ("sidebar", crate::registry::SIDEBAR_TABS, false),
        ("settings", crate::registry::SETTINGS_TABS, false),
        ("settings (on-device on)", crate::registry::SETTINGS_TABS, true),
    ];
    for (table, tabs, experimental) in tables {
        for tab in tabs {
            for width in [220.0_f32, 480.0] {
                let mut ws = workspace_with(experimental);
                let mut fui = crate::registry::FeatureUi::default();
                let ctx = egui::Context::default();
                theme::install_fonts(&ctx);
                ctx.global_style_mut(theme::style);
                let mut sizes = Vec::new();
                for frame in 0..30 {
                    let mut out = ctx.run_ui(input(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 820.0)), frame), |ui| {
                        let r = ui
                            .allocate_ui(Vec2::new(width, 700.0), |ui| {
                                let _ = (tab.show)(ui, &mut ws, &mut fui, &theme::LIGHT);
                            })
                            .response
                            .rect;
                        sizes.push(r.size());
                    });
                    out.textures_delta.clear();
                }
                if sizes[10..].iter().any(|s| (s.y - sizes[10].y).abs() > 0.5 || s.x > width + 0.5) {
                    failures.push(format!("{table} tab {} at width {width}: {:?}", tab.id, &sizes[10..]));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// 0.12: Home draws every section, with a full vault, at three window sizes
/// without changing size from frame to frame or leaving the window.
#[test]
fn home_page_keeps_its_size() {
    use crate::surfaces::Surface;
    let mut failures = Vec::new();
    for (w, h) in [(1280.0_f32, 820.0_f32), (900.0, 560.0), (760.0, 480.0)] {
        let mut ws = workspace_with(false);
        let home = quire_app::switch_state::HomeConfig {
            sections: quire_app::switch_state::HOME_SECTIONS.iter().map(|(id, _)| (*id).to_string()).collect(),
            word_goal: 500,
            pinned: vec!["Long.md".into()],
            view: "from: #project".into(),
            ..Default::default()
        };
        ws.set_home_config(home);
        let mut page = crate::home_ui::sample();
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        ctx.global_style_mut(theme::style);
        let mut sizes = Vec::new();
        for frame in 0..30 {
            let mut out = ctx.run_ui(input(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h)), frame), |ui| {
                let r = ui
                    .allocate_ui(Vec2::new(w, h), |ui| {
                        let _ = page.show(ui, &mut ws, &theme::LIGHT);
                    })
                    .response
                    .rect;
                sizes.push(r.size());
            });
            out.textures_delta.clear();
        }
        if sizes[10..].iter().any(|s| (s.y - sizes[10].y).abs() > 0.5 || s.x > w + 0.5 || s.y > h + 0.5) {
            failures.push(format!("home at {w}x{h}: {:?}", &sizes[10..]));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// 0.12: Settings → Toolbar while the small toolbar's own buttons are
/// edited keeps its size, with the defaults and with a custom list.
#[test]
fn toolbar_settings_for_the_floating_bar_keep_their_size() {
    let tab = crate::registry::SETTINGS_TABS.iter().find(|t| t.id == "toolbar").expect("the Toolbar tab");
    let mut failures = Vec::new();
    for custom in [false, true] {
        for width in [220.0_f32, 480.0] {
            let mut ws = workspace_with(false);
            if custom {
                ws.settings.toolbar.floating_items =
                    Some(vec![quire_app::settings::ToolbarItem::new("Bold"), quire_app::settings::ToolbarItem::new("Save")]);
            }
            let mut fui = crate::registry::FeatureUi::default();
            fui.commands.edit_floating_bar();
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            ctx.global_style_mut(theme::style);
            let mut sizes = Vec::new();
            for frame in 0..30 {
                let mut out = ctx.run_ui(input(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 820.0)), frame), |ui| {
                    let r = ui
                        .allocate_ui(Vec2::new(width, 700.0), |ui| {
                            let _ = (tab.show)(ui, &mut ws, &mut fui, &theme::DARK);
                        })
                        .response
                        .rect;
                    sizes.push(r.size());
                });
                out.textures_delta.clear();
            }
            if sizes[10..].iter().any(|s| (s.y - sizes[10].y).abs() > 0.5 || s.x > width + 0.5) {
                failures.push(format!("custom {custom} at width {width}: {:?}", &sizes[10..]));
            }
            assert_eq!(ws.settings.toolbar.items, None, "editing the small bar leaves the main one alone");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// 0.13: Settings → Links, with Dendron on and several custom types, keeps its size at
/// two widths in both themes; what it saves survives a reload and leaves vault.toml's other text alone.
#[test]
fn links_settings_keep_their_size_and_save_what_they_show() {
    let tab = crate::registry::SETTINGS_TABS.iter().find(|t| t.id == "links").expect("the Links tab");
    let mut failures = Vec::new();
    for pal in [&theme::LIGHT, &theme::DARK] {
        for width in [220.0_f32, 480.0] {
            let mut ws = workspace_with(false);
            let vault = ws.vault().unwrap().clone();
            let hand = "# mine\nformat = 1\n\n[web]\nhide = [\"Journal/\"] # keep\n";
            vault.storage().write_atomic(&NotePath::new(quire_app::vault::prefs::VAULT_PREFS_PATH), hand).unwrap();
            let mut cfg = ws.link_settings();
            cfg.dendron.enabled = true;
            for t in ["supports", "contradicts", "a-rather-long-type-name-for-a-narrow-panel"] {
                cfg.types.insert(t.into(), quire_app::index::TypeStyle { color: Some("#2e7d32".into()), ..Default::default() });
            }
            ws.set_link_settings(&cfg).unwrap();
            let text = vault.storage().read(&NotePath::new(quire_app::vault::prefs::VAULT_PREFS_PATH)).unwrap();
            assert!(text.starts_with(hand), "the user's text is untouched: {text}");
            assert_eq!(quire_app::index::LinkConfig::load(&vault), cfg, "what is saved reads back");
            let mut fui = crate::registry::FeatureUi::default();
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            ctx.global_style_mut(theme::style);
            let mut sizes = Vec::new();
            let mut shown = String::new();
            for frame in 0..30 {
                let mut out = ctx.run_ui(input(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 3000.0)), frame), |ui| {
                    let r = ui
                        .allocate_ui(Vec2::new(width, 2900.0), |ui| {
                            let _ = (tab.show)(ui, &mut ws, &mut fui, pal);
                        })
                        .response
                        .rect;
                    sizes.push(r.size());
                });
                shown = drawn_text(&out);
                out.textures_delta.clear();
            }
            if sizes[10..].iter().any(|s| (s.y - sizes[10].y).abs() > 0.5 || s.x > width + 0.5) {
                failures.push(format!("links settings at width {width}: {:?}", &sizes[10..]));
            }
            for row in ["Hierarchy fields", "Up (parents)", "Dotted note names", "Link types", "supports"] {
                if !shown.contains(row) {
                    failures.push(format!("links settings at width {width} lack {row}: {shown}"));
                }
            }
            assert_eq!(ws.link_settings(), cfg, "drawing the page changes nothing");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Every piece of text drawn in `out`.
fn drawn_text(out: &egui::FullOutput) -> String {
    fn walk(shape: &egui::Shape, into: &mut String) {
        match shape {
            egui::Shape::Text(t) => {
                into.push_str(t.galley.text());
                into.push('\n');
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, into)),
            _ => {}
        }
    }
    let mut text = String::new();
    for c in &out.shapes {
        walk(&c.shape, &mut text);
    }
    text
}

/// What Set up AI and Settings → AI show, with the switch off and on.
fn on_device_screens(experimental: bool) -> (String, String, Workspace) {
    let mut ws = workspace_with(experimental);
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 820.0));
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    ctx.global_style_mut(theme::style);
    let mut ov = Overlay::Feature(crate::onboarding_ui::OnboardingOverlay::make(&mut ws));
    let mut onboarding = String::new();
    for frame in 0..5 {
        let mut out = ctx.run_pass(input(screen, frame), |ctx| {
            let _ = crate::overlays::show(ctx, &mut ov, &mut ws, &theme::LIGHT);
        });
        onboarding = drawn_text(&out);
        out.textures_delta.clear();
    }
    let tab = crate::registry::SETTINGS_TABS.iter().find(|t| t.id == "ai").expect("the AI tab");
    let mut fui = crate::registry::FeatureUi::default();
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    ctx.global_style_mut(theme::style);
    let mut settings = String::new();
    for frame in 0..5 {
        let mut out = ctx.run_ui(input(screen, frame), |ui| {
            let _ = (tab.show)(ui, &mut ws, &mut fui, &theme::LIGHT);
        });
        settings = drawn_text(&out);
        out.textures_delta.clear();
    }
    (onboarding, settings, ws)
}

/// On-device AI is experimental in 0.9.0: by default Set up AI and
/// Settings → AI offer none of it and say it's coming, and nothing starts
/// the engine; the switch brings back the rows, marked Experimental.
#[test]
fn on_device_rows_follow_the_experimental_switch() {
    if std::env::var_os(quire_app::settings::EXPERIMENTAL_ENV).is_some() {
        return; // the switch is on for this process
    }
    let later = quire_app::ai::ON_DEVICE_LATER;
    let (onboarding, settings, ws) = on_device_screens(false);
    assert!(onboarding.contains("No AI") && onboarding.contains("Use a provider"), "{onboarding}");
    assert!(!onboarding.contains("On this computer") && !onboarding.contains("Experimental"), "{onboarding}");
    assert!(onboarding.contains(later), "{onboarding}");
    assert!(settings.contains(later) && settings.contains("Advanced"), "{settings}");
    for row in ["Writing and agent", "Read text in images", "Engine", "Install from file", "Not available in this build", "Experimental"] {
        assert!(!settings.contains(row), "{row} shown while off: {settings}");
    }
    assert!(!quire_app::ai::engine(&ws.settings).is_running(), "nothing started the engine");

    let (onboarding, settings, _) = on_device_screens(true);
    assert!(onboarding.contains("On this computer") && onboarding.contains("Experimental") && !onboarding.contains(later), "{onboarding}");
    for row in ["Meaning search", "Writing and agent", "Read text in images", "Engine", "Not available in this build", "Experimental"] {
        assert!(settings.contains(row), "{row} missing while on: {settings}");
    }
    assert!(!settings.contains(later), "{settings}");
}

/// 0.11: the formatting toolbar keeps its size from frame to frame, wraps
/// in a narrow window instead of overflowing, and the floating toolbar
/// stays on screen wherever the selection is.
#[test]
fn toolbars_keep_their_size_and_fit_the_window() {
    let mut failures = Vec::new();
    let mut ws = workspace_with(false);
    ws.settings.toolbar.show = true;
    let keymap = quire_app::keymap::Keymap::new(&ws.settings.commands);
    for width in [300.0_f32, 760.0, 1280.0] {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 600.0));
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        ctx.global_style_mut(theme::style);
        let mut st = crate::toolbar_ui::ToolbarUi::default();
        let mut sizes = Vec::new();
        for frame in 0..30 {
            let mut out = ctx.run_ui(input(screen, frame), |ui| {
                let r = ui
                    .allocate_ui(Vec2::new(width, 300.0), |ui| {
                        let _ = crate::toolbar_ui::bar(ui, &ws, &keymap, &theme::LIGHT, &mut st);
                    })
                    .response
                    .rect;
                sizes.push(r.size());
            });
            out.textures_delta.clear();
        }
        if sizes[10..].iter().any(|s| (s.y - sizes[10].y).abs() > 0.5 || s.x > width + 0.5) {
            failures.push(format!("toolbar at width {width}: {:?}", &sizes[10..]));
        }
        // The floating toolbar near each corner of the window.
        for at in [Pos2::new(0.0, 0.0), Pos2::new(width - 2.0, 0.0), Pos2::new(width - 2.0, 580.0), Pos2::new(10.0, 300.0)] {
            let sel = Rect::from_min_size(at, Vec2::new(2.0, 18.0));
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            ctx.global_style_mut(theme::style);
            let mut st = crate::toolbar_ui::ToolbarUi::default();
            for frame in 0..5 {
                let mut out = ctx.run_pass(input(screen, frame), |ctx| {
                    let items = quire_app::settings::toolbar::default_floating_items();
                    let _ = crate::toolbar_ui::floating(ctx, sel, &items, &keymap, &theme::DARK, &mut st);
                });
                out.textures_delta.clear();
            }
            let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("quire-floating-toolbar"));
            match ctx.memory(|m| m.area_rect(layer.id)) {
                Some(r) if screen.expand(0.5).contains_rect(r) => {}
                other => failures.push(format!("floating toolbar for a selection at {at:?} in {width}: {other:?}")),
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
