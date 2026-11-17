---
name: review-change
description: Review a Quire change before it merges, as a checklist of questions (data safety, UI, security, layering, docs). Use when asked to review a diff, a branch or a pull request in this repository.
---

Review the diff against these questions. For each one that applies, look in the file named and say what you found; skip a section the change does not touch. Rules and what enforces them: `AGENTS.md`, "Rules learned the hard way". Where each rule is a test or a lint, run it instead of trusting the author.

## Data (P1, P3, P4)
- Can this lose user text, or change bytes it did not mean to? Look at the round-trip tests beside the editor in `crates/quire-markdown` and at `Storage` in `crates/quire-vault/src/storage.rs`.
- Is every persisted file written atomically (`Storage::write_bytes_atomic`, or the atomic writer from `quire-base`)? Is a bare `fs::write`, `fs::rename`, `File::create` or tempfile `persist` left? Clippy's `disallowed-methods` in `clippy.toml` finds them.
- Do the new bytes depend on what the code read? Then it needs `Storage::write_bytes_if` (compare-and-swap), not read then write.
- Does a failed listing or read turn into "empty" or "all deleted" anywhere, especially in sync (`crates/quire-sync/src/engine.rs`, `crates/quire-app/src/vault_sync/`)?
- Do quit, close, Escape and lock keep unsaved text? Look at the close paths of the changed dialog or editor.
- Persisted format changed? Old data must still load, unknown fields kept, a note in `docs/formats/`, a fixture of the old format in the test.

## UI (P9)
- Is there work in `show`/`ui` that grows with the vault (index locks, `exists()`, listing, formatting per cell)? Cache it. The `perf` job checks startup only.
- Is a new dialog an `OverlayView` listed in `layout_samples` (`apps/quire-desktop/src/registry.rs`), and does `layout_check.rs` pass?
- Does a new default shortcut pass `Keymap::conflicts` for AltGr, Option and reserved keys (`crates/quire-app/src/keymap.rs`)?
- Does code behind the Lite edition still build with `--no-default-features`?
- Are new file names safe on Windows (`quire_sync::names`: reserved names, `:?*`, trailing space or dot)?

## Security (P6, P7, P8)
- New network call, process, plugin or script hook? It must go through the shared HTTP builder or `git::run_git`, be off until the user turns it on, and run no third-party code. `deny.toml` `[bans]` and `clippy.toml` catch some of it.
- Does an MCP tool filter every returned path and title with `common::visible` (`crates/quire-mcp/src/tools/common.rs`) and bound its lists (`bounds.rs`)? `crates/quire-mcp/tests/security.rs` covers only some tools.
- Is agent output treated as untrusted data, and are `AGENTS.md`, `CLAUDE.md`, `.quire/**` and skills still outside what an agent may edit without a person accepting it?
- Does a new random value use the one function that returns an error, never zero bytes?

## Layering and dependencies
- Does a crate now depend on a higher layer, or turn on another crate's feature? `python3 -I packaging/check-layering.py` and the table in `ARCHITECTURE.md` section 3.
- New dependency? Licence in `deny.toml`, a reason in the pull request, and `cargo deny check`.

## Docs
- User-visible change: one `CHANGELOG.md` bullet under `[Unreleased]`, at most two sentences, about what a person sees (CI warns if it is missing).
- Does a design note or format doc the change touches still say what is true, and does the note have its `Status:` block (`packaging/check-design-notes.sh`)?
- Are comments free of workstream labels (`WS-A`) and of "frozen" claims?

Report findings by severity with the file and line, and say which you ran and which you only read.
