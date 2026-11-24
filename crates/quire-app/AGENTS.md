# AGENTS.md: quire-app

The application layer: `Workspace`, documents, `Command`s and their keys, settings, sessions, jobs, and the glue between the
vault, the index, sync, git and the agent store. It never knows which UI toolkit draws it, and it never blocks the UI thread.
Depends on: quire-agent, quire-ai, quire-base (with `net`), quire-canvas, quire-import, quire-index, quire-intel, quire-markdown,
quire-pdf, quire-plan, quire-sync, quire-text, quire-vault, and quire-typeset (feature `typeset`, on by default; Lite is without it).
Used by: quire-desktop, quire-cli, quire-mcp, quire-web, quire-assistant. Edition: both (P5).

## Invariants
- No UI toolkit: `egui` and `eframe` are allowed only for `quire-desktop`. Handlers for commands live here. [lint] `deny.toml` bans
- Open a vault with `secure_state::open_vault` (or `open_vault_now` before the first frame), never `Vault::open_local`: it keeps a
  locked folder locked when its key file is missing. [lint] `clippy.toml`; each gate carries an `#[expect]`
- Escape, close, quit and lock never drop unsaved text. Locked notes with unsaved edits survive locking and quitting; a note
  that cannot be saved is listed before quit. [test] `unsaved_edits_in_a_locked_folder_survive_locking_and_quitting`,
  `notes_that_cant_be_saved_are_listed_before_quitting` (`secure_state/tests.rs`), `quitting_writes_the_merged_text_a_refused_save_left_behind` (`nav/embed.rs`)
- Settings keep keys they do not know (`#[serde(flatten)]` in `settings/mod.rs`) and an unreadable file is copied to
  `settings.toml.unreadable` before defaults are saved over it. A renamed key needs a migration in `settings/compat.rs`.
  [test] `settings_from_another_version_or_edition_survive_a_save`, `an_unreadable_settings_file_is_kept_not_overwritten`
- Sync glue never turns a failed listing into deletes, and holds mass deletes for the person (`vault_sync/deletes.rs`).
  [test] `a_vault_that_cant_be_listed_sends_no_deletes`, `many_deletes_wait_until_the_person_allows_them` (`vault_sync/tests.rs`)
- Git runs only through `git::run_git` (it checks `git_trust` first): a repository's own config can run programs. [lint] `Command::new`
  is banned in `clippy.toml`; the trust check itself is [review only]
- The UI thread never waits on new work (P9): read the index with `try_*` (`Workspace::try_agenda`), keep what you showed while
  the index worker writes, and read counters from atomics (`IndexService::note_count`). No vault listing before the first frame
  (it ran on the UI thread before: see the 0.13.1 perf note). [test] CI `perf` job (startup only); per frame [review only]
- Default keys must pass `Keymap::conflicts` on the platform that has them (AltGr, Option, reserved keys). A key the system keeps on
  one platform gets that platform's own default in `MOVED` (`keymap.rs`), never a second `COMMANDS` row. [test]
  `every_default_shortcut_passes_the_conflict_checker`, `shortcuts_are_unique` (`commands.rs`)

## Do not
- Write app state with `fs::write` or your own temporary file: `quire_base::fs::write_atomic` with `Options::STATE`, or
  `Options::SECRET` for keys and tokens. `atomic::write` is an old path to the same code. [lint] `clippy.toml`
- Put a vault preference in device settings: `.quire/shared/vault.toml` (`docs/formats/vault-prefs.md`) is the place.
- Call `process::exit`, start another program, or build an HTTP client by hand (`quire_base::net`). [lint] `clippy.toml`

## Where things go
- A command: a `Command` variant and a `COMMANDS` row in `commands.rs`, its handler in `Workspace::run` (`workspace.rs`) or a
  `*_state.rs` module. A setting: a table in `settings/<table>.rs`. Details: `docs/howto.md`.
- Sync glue is `vault_sync/` (`config`, `state`, `store`, `session`, `worker`, `deletes`, `setup`, `workspace`, `harden`). Each
  child does `use super::*;` and `mod.rs` re-exports it, so `quire_app::vault_sync::X` paths hold.
- Background work with budgets (indexing, AI, OCR) is a `Job` in `jobs.rs` (steps of about 20 ms; it pauses while the person types).
  Other threads are named `quire-...` and report back as notices. [lint] `clippy.toml` for the name

## Before you finish
- `cargo test -p quire-app --locked` (a large crate: filter by test name while iterating), then clippy for it, and the Lite pair
  `cargo clippy -p quire-desktop -p quire-cli --no-default-features --all-targets --locked -- -D warnings`.
- A persisted-format change: `docs/formats/<file>.md` and a fixture of the old format. A user-visible change: a changelog bullet.

## Read first (only when changing that area)
- Sync glue: `docs/howto.md` "Change sync", `docs/design/0.12-sync.md` (current), `docs/design/0.14-health-split.md` (the file layout).
- Locked folders: `docs/design/0.11-secure.md` (current). Saves and quit: `docs/design/0.13.2-ui.md` (record: the code wins).
