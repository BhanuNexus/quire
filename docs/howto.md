# How to do common jobs here

One short section per job: where the code goes, what to register, which test must pass, the one mistake to avoid. Rules that apply to every job are in [AGENTS.md](../AGENTS.md). Lines marked `<!-- verify after merge -->` name files or helpers that the 0.14 health branches are moving; check them against the tree. If a line here is wrong, fix it in the same change.

## Add a command

- Add a variant to `Command` and a row to `COMMANDS` (`crates/quire-app/src/commands.rs`). That gives it a palette entry, a menu item and a line in the shortcut list. Pick its `Owner`.
- Handle it in `Workspace::run` (`workspace.rs`) or in its feature's `*_state.rs` module. The handler goes in `quire-app`, never in `apps/quire-desktop`.
- Give it no default key unless `Keymap::conflicts` (`keymap.rs`) accepts it on all three platforms: AltGr (`Ctrl+Alt+<letter>` is a letter on German keyboards), Option on macOS, and reserved keys.
- Tests that must pass: `shortcuts_are_unique` and `every_command_is_registered_once_with_an_owner` (`commands.rs`), and `every_default_shortcut_passes_the_conflict_checker` (`keymap.rs`), which fails on any conflict a new default key has.
- A CLI subcommand is a different thing: a module in `apps/quire-cli/src/commands/` with a `COMMANDS` table, added to `all()` in its `mod.rs`.
- Add a changelog bullet if a person sees it.

## Add a setting

- A table in `crates/quire-app/src/settings/<table>.rs`, off or unchanged by default. Keep unknown keys when saving (P3) and rename keys only with a migration in `settings/compat.rs`.
- Write the file with the atomic writer (P4): `quire_base::fs::write_atomic`. <!-- verify after merge -->
- List the keys in `docs/formats/settings.md`. A preference that belongs to one vault goes in `.quire/shared/vault.toml` instead (`docs/formats/vault-prefs.md`).
- Its tab goes in `SETTINGS_TABS` (`apps/quire-desktop/src/registry.rs`); `layout_check` builds every tab.
- The mistake to avoid: putting a vault preference in the device settings, or writing `settings.toml` with a raw `fs::write`.

## Add a dialog

- Implement `OverlayView` in its own `*_ui.rs` file under `apps/quire-desktop/src/`, and open it through `ui_target` in `registry.rs` (`UiTarget::Overlay`) from a command's `UiRequest::Feature`. Do not grow `dialogs.rs` for a new dialog.
- Add every interesting state (empty, long text, an error) to `layout_samples` (`registry.rs`). `every_kind_of_dialog_has_a_sample` (`layout_check.rs`) fails while a new `OverlayView` or `Overlay` variant has none.
- Avoid vertical separators and `available_size()` that fill the height: a dialog then grows on every repaint (`layout_check.rs` explains). `cargo test -p quire-desktop layout_check` runs the check.
- No disk, index-lock or vault-sized work in `show`/`ui` (it runs every frame): compute once and cache it.
- Escape, close, quit and lock must not drop unsaved text.
- Code behind the Lite edition (typeset, math, Mermaid, PDF, voice) must compile with `--no-default-features`: run the Lite clippy line of the gate.

## Add an MCP tool

- Add a `ToolSpec` and its handler in `crates/quire-mcp/src/tools/` (one file per tool; the definition sits next to its handler). <!-- verify after merge -->
- Register it in `ToolRegistry::standard()` (`tools/mod.rs`) and add its name to the order test `every_0_9_tool_is_there_in_the_designs_order` in the same file. The test also requires a description of more than 80 characters that contains the word `Example`, and an output schema.
- Bound every list. Take `limit` with its default and maximum stated in the schema; if the result can exceed `DEFAULT_MAX_TOKENS`, also take `max_tokens` and a cursor (`bounds.rs`).
- Filter every returned path or title with `common::visible` (hidden folders, deny zones, locked folders). A count or title from a denied note is a leak. `crates/quire-mcp/tests/visibility.rs` calls every read tool of the registry against a vault with hidden notes; a ranked list must rank only visible notes (`retrieve_where`, `*_where` in `quire-index`), not filter after taking the top k.
- Set the access class: `Annotations::READ`, `WRITE` or `CREATE`; write tools are not listed to read-only clients.
- Update the lists agents read: `skills/quire/SKILL.md`, `docs/agents.md`, `instructions.rs`. Do not add names to `legacy`, whose names are frozen.

## Change a file format

- Old data must still load; keep unknown fields; a reader never rewrites a file of a newer format (P3).
- Update the note in `docs/formats/`. If there is none (for example `.quire/sync/state.json`), write one.
- Add a fixture of the old format to the test, and an "Upgrading" line in the changelog when an older device is affected.
- Formats do not depend on the edition (P5): the Lite build must read and write the same bytes.

## Change sync

- Never drop text. Edits beat deletes. A failed listing or read is an error, never a set of deletes (tests: `a_listing_that_lost_a_folder_is_retried_then_refused` in `crates/quire-vault/src/storage.rs` and the held-deletes tests).
- The wire format is `docs/formats/sync-protocol.md`; the engine is `crates/quire-sync/src/engine.rs` (read its `//!` header first); the app side is `crates/quire-app/src/vault_sync/`. <!-- verify after merge -->
- State files are written with the atomic writer, not `fs::write`. A skip that trusts size and modification time alone can lose an edit on FAT and network drives or after a clock change.
- The randomised folder tests (`crates/quire-sync/tests/folder_sync.rs`) are the guard: 150 seeds by default, `SYNC_SEEDS=<n>` to change. They must pass on Linux, macOS and Windows; do not weaken them to go faster, and do not loop them on a small machine.
- File names Windows cannot store are handled in `quire_sync::names`.
- Attach before and after numbers for a performance change, and add a changelog bullet only if a person sees it.
