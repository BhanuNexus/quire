# AGENTS.md: quire-desktop

The egui window: it draws `quire_app::Workspace` and sends it `Command`s. It never decides what a command does, never owns note
data, and never blocks a frame.
Depends on: quire-app (and quire-ai, quire-assistant, quire-base, quire-canvas, quire-import, quire-mcp, quire-pdf, quire-spell,
quire-typeset). Used by: nothing (a binary). Editions: full, and Lite (`--no-default-features`: no `typeset`, no `voice`).
Features: `typeset`, `voice` (default), `bench` (timing runs for the CI `perf` job; never in a release).

## Invariants
- Only this crate draws: `egui`, `eframe` and `rfd` are allowed here and nowhere else. Logic goes in `quire-app`. [lint] `deny.toml` bans
- A new dialog is an `OverlayView` (`overlays.rs`) in its own `*_ui.rs` file, opened through `ui_target` in `registry.rs`
  (`UiTarget::Overlay`) and listed in `layout_samples` (`registry.rs`) in every interesting state. [test] `layout_check` builds
  the samples on three window sizes: `dialogs_keep_their_size_and_fit_the_window`. A dialog left out of `layout_samples` is
  never laid out in a test.
- A dialog must not grow on every repaint: no vertical separator, no `available_size()` that fills the height (`layout_check.rs`
  header explains). [test] `layout_check`
- Every Settings tab is in `SETTINGS_TABS` (`registry.rs`). [test] `feature_tabs_keep_their_size`
- No disk, index-lock or vault-sized work in `show` or `ui`: they run every frame. Compute once, cache, and read the index without
  waiting (`try_*`). [test] CI `perf` job for startup (`packaging/perf-check.py`); per frame [review only]
- Quit, tab close and lock never drop unsaved text. A `Surface` writes in `close`, reports `unsaved`, and its tab stays open when
  the write failed. [test] `edits_are_saved_when_the_tab_is_hidden_or_closed`, `a_failed_save_keeps_the_edits_and_tells_the_person`,
  `unsaved_canvases_are_listed_after_a_failed_close` (`canvas_ui/mod.rs`), `closing_with_unsaved_edits_saves_them` (`drawing_edit/tests.rs`).
  Other surfaces and the text editor: [review only]
- Both editions build and pass. Code behind `typeset` or `voice` carries `#[cfg(feature = ...)]` and the Lite build must still
  compile. [lint] the Lite clippy line of the gate; [test] the Lite test line

## Do not
- Grow `dialogs.rs` (about 2,200 lines) or `app.rs` for a new dialog or feature; use a `*_ui.rs` file and the registry.
- Run work on the UI thread that can wait on disk, the network or a lock: Settings sync create, join and list were moved off it
  once. Use a thread (named `quire-...`) and send the result back as an event or notice.
- Add a default key without `Keymap::conflicts` (`crates/quire-app/src/keymap.rs`) accepting it on all three platforms:
  AltGr (`Ctrl+Alt+<letter>`), Option on macOS, reserved keys. [review only]
- Use `std::fs::write`, `File::create` or `fs::rename` for what the app saves. [lint] `clippy.toml`

## Where things go
- A command's handler: `quire-app` (`Workspace::run`). Its UI, if any: `registry.rs` `ui_target`. A settings tab: `SETTINGS_TABS`.
- A page in the tab bar or an editor for a file type: a `Surface` (`surfaces.rs`), chosen by extension in `registry.rs`.

## Before you finish
- `cargo test -p quire-desktop --locked`, and `cargo test -p quire-desktop -p quire-cli --no-default-features --locked` for Lite.
- `cargo clippy -p quire-desktop --all-targets --locked -- -D warnings` and the same with `--no-default-features`.
- A user-visible change: a changelog bullet.

## Read first (only when changing that area)
- `docs/howto.md` "Add a dialog" and "Add a command". Editor view: `docs/design/0.11-editor.md` (current).
- Saves and quit: `docs/design/0.13.2-ui.md` section 1 (record: the code wins). Per-frame work: `docs/design/0.13.1-perf.md` (record).
