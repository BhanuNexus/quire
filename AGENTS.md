# AGENTS.md: working in the Quire repository

For an agent or a person starting cold. (Not to be confused with the `AGENTS.md` a user may keep in a *vault*, which tells agents how to work in their notes: `docs/design/0.9.md` §3.A.2.) Everything below was checked against the code and workflows when it was written; if you find it wrong, fix it in the same change.

## What Quire is

- A local-first notes app in Rust (AGPL-3.0-or-later): plain Markdown files in a folder you own, an egui desktop app (`apps/quire-desktop`) and a CLI (`apps/quire-cli`).
- `quire-cli serve` is the web app and the MCP server for agents; `apps/quire-sync-server` is the optional end-to-end encrypted sync server.
- Offline by default; no plugins; no AI without opt-in. Design: `ARCHITECTURE.md`, `docs/design/README.md` (which notes are current).

## Two editions

- **Full** (default features): math, Mermaid and PDF export through Typst, PDF page rendering, voice notes.
- **Lite** (`--no-default-features` on `quire-desktop` and `quire-cli`): the same app and files without those. Both are built, tested and released. Persisted formats never depend on the edition (P5).

## The gate

Run the part your change touches before you commit, and all of it before a release. CI (`.github/workflows/ci.yml`) runs every line below **except `actionlint`**, which no workflow runs: run it by hand when you touch a workflow. Where a line differs from CI's, the comment says so.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p quire-desktop -p quire-cli --no-default-features --all-targets --locked -- -D warnings   # Lite
cargo test --workspace --locked --no-fail-fast
cargo test -p quire-desktop -p quire-cli --no-default-features --locked                                  # Lite
cargo test -p quire-pdf --features render --locked
cargo run -p quire-cli --features eval --locked -- eval synthetic-edits --json                           # agent edit eval
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items --locked
cargo deny check advisories bans licenses sources        # cargo install cargo-deny
typos                                                    # cargo install typos-cli
packaging/test-unpack-engine.sh && packaging/test-release-files.sh && packaging/test-check-ci-green.sh
shellcheck --severity=error packaging/*.sh packaging/linux/*.sh packaging/macos/*.sh packaging/windows/*.sh packaging/flatpak/*.sh apps/quire-sync-server/*.sh
pipx run zizmor==1.31.1 --offline --format=github .github/workflows/   # CI's pinned version
actionlint                                                # NOT run by CI: run it when you touch a workflow
```

CI's `packaging` job also validates the desktop entry and the AppStream file, parses the Flatpak manifest, compiles the Windows installer script with `makensis` and checks `Info.plist`; those need `desktop-file-utils`, `appstream`, `flatpak-builder` and `nsis`, and you only need them when you change those files. CI also has a `perf` job (speed budgets, `packaging/perf-check.py`) and a DCO job on pull requests. `cargo test --workspace` compiles candle for the on-device AI engine: a cold build takes 10+ minutes.

**Small machine** (2 CPUs, about 7 GB RAM): set these on every cargo command, build crate by crate with `-p`, give each worktree its own `target/`, and check `df -h /` before a big build.

```sh
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
# Only if the pinned toolchain (rust-toolchain.toml) cannot be installed on this machine:
export RUSTUP_TOOLCHAIN=stable
```

## Invariants

From `docs/design/0.9.md` §1; code review checks them.

| | Invariant |
|---|---|
| P1 | Markdown files are the truth. Never lose user text; bytes you did not mean to change stay identical. |
| P2 | Nothing proprietary is needed to read notes; deleting `.quire/` never loses note content. |
| P3 | New app state lives under `.quire/` in versioned formats; a reader never rewrites a file of a newer format and keeps unknown fields. |
| P4 | Writes are atomic: `Storage::write_bytes_atomic` for vault files, `quire_app::atomic::write` for config files. |
| P5 | Persisted formats do not depend on the edition. |
| P6 | Offline first: no network, download or engine process unless the user turned the feature on. |
| P7 | No third-party code execution: no plugins, scripts or config that runs commands; shared config that widens agent permissions needs approval on each device. |
| P8 | Agent output is untrusted data, and agents never edit `AGENTS.md`, `CLAUDE.md`, `.quire/**` or skills without a human accepting a proposal. |
| P9 | The UI thread never waits on new work (indexing, AI, PDF, OCR, git and lint run off it). |
| P10 | Tests are deterministic: injected clock, seeded randomness, fake models, no network except loopback servers the test starts. |

A persisted format change needs old data to still load, and a note in `docs/formats/`.

## Workspace

22 members, ordered so that every crate comes after the crates it depends on (libraries first, then the apps). "Depends on" lists direct normal and build dependencies on other workspace crates, any feature, from `cargo metadata --no-deps --format-version 1`; **dev-dependencies are not counted** (for example `quire-cli` also dev-depends on `quire-sync` and `quire-sync-server`). Logic goes in crates with tests beside it; `apps/quire-desktop` only draws and forwards input; every user action is a `Command` in `crates/quire-app/src/commands.rs`, which also gives it a palette entry, a shortcut and the in-app shortcut list.

| Crate | Path | What it is | Depends on (workspace crates) |
|---|---|---|---|
| `quire-canvas` | `crates/quire-canvas` | JSON Canvas model and serializer, ink SVG, Excalidraw | none |
| `quire-spell` | `crates/quire-spell` | Markdown-aware spell checking (Hunspell dictionaries) | none |
| `quire-text` | `crates/quire-text` | rope buffer, changes, transactions, undo tree (no I/O) | none |
| `quire-markdown` | `crates/quire-markdown` | Markdown syntax, front matter, extraction, rendering, smart edits | quire-text |
| `quire-plan` | `crates/quire-plan` | tasks, recurrence, agenda, reminders, periodic notes, flashcards | quire-markdown |
| `quire-sync` | `crates/quire-sync` | end-to-end encrypted sync engine and its folder, S3 and WebDAV transports | quire-text |
| `quire-typeset` | `crates/quire-typeset` | math, Mermaid and PDF export through Typst (the full edition only) | none |
| `quire-vault` | `crates/quire-vault` | storage port, local and in-memory adapters, notes, templates, refactors, history | quire-markdown, quire-sync |
| `quire-import` | `crates/quire-import` | importers (Logseq, Obsidian, Notion, Bear, Evernote, Roam, Keep, Joplin, Day One, HTML) | quire-markdown, quire-vault |
| `quire-index` | `crates/quire-index` | full-text, fuzzy and semantic search, link graph (actor thread) | quire-markdown, quire-plan, quire-vault |
| `quire-intel` | `crates/quire-intel` | related notes, link and tag suggestions, summaries, calculator, LLM port | quire-index, quire-markdown, quire-vault |
| `quire-agent` | `crates/quire-agent` | agent store: actors, zones, policy, proposals, approvals, ledger | quire-intel, quire-markdown, quire-text, quire-vault |
| `quire-ai` | `crates/quire-ai` | on-device AI client: model catalog, downloads, engine process and its protocol | quire-index, quire-intel |
| `quire-pdf` | `crates/quire-pdf` | PDF text with positions, page rendering, annotation notes | quire-markdown, quire-vault |
| `quire-app` | `crates/quire-app` | UI-agnostic application layer: Workspace, documents, commands, settings, sessions, sync glue | quire-agent, quire-ai, quire-canvas, quire-import, quire-index, quire-intel, quire-markdown, quire-pdf, quire-plan, quire-sync, quire-text, quire-typeset, quire-vault |
| `quire-mcp` | `crates/quire-mcp` | note tools and the MCP server (stdio and HTTP) | quire-agent, quire-app, quire-import |
| `quire-assistant` | `crates/quire-assistant` | in-app agent panel loop, ACP bridge, wiki workflows | quire-agent, quire-ai, quire-app, quire-mcp |
| `quire-web` | `crates/quire-web` | the web app of `quire-cli serve` | quire-agent, quire-app, quire-mcp |
| `quire-ai-engine` | `apps/quire-ai-engine` | on-device AI engine: a separate process speaking JSON lines over stdio | quire-ai |
| `quire-cli` | `apps/quire-cli` | headless Quire: search, import, export, tasks, sync, MCP, `serve` | quire-agent, quire-ai, quire-app, quire-assistant, quire-import, quire-mcp, quire-pdf, quire-web |
| `quire-desktop` | `apps/quire-desktop` | egui desktop app (draws and forwards input) | quire-ai, quire-app, quire-assistant, quire-canvas, quire-import, quire-mcp, quire-pdf, quire-spell, quire-typeset |
| `quire-sync-server` | `apps/quire-sync-server` | self-hostable sync server (sees only encrypted data) | quire-sync |

## Files many changes collide in

Most-edited files in the history (as of 0.13.1): `CHANGELOG.md`, `Cargo.lock`, `Cargo.toml`, `README.md`, `crates/quire-app/src/workspace.rs`, `crates/quire-app/src/lib.rs`, `crates/quire-app/src/commands.rs`, `crates/quire-app/src/vault_sync.rs`, `apps/quire-desktop/src/app.rs`, `panels.rs`, `dialogs.rs`, `main.rs` and `registry.rs`. Keep edits to them small and merge often; expect a trivial conflict in `CHANGELOG.md`.

## Commits and the changelog

- Every commit needs a DCO sign-off: `git commit -s`. CI's `dco` job fails a pull request with an unsigned commit. The project requires no other trailer.
- Imperative, focused commits: one change each, with tests for behaviour changes.
- **One changelog rule:** a user-visible change gets one short bullet under `## [Unreleased]` in `CHANGELOG.md` (Added, Changed, Fixed or Security). At release that section is renamed to the version (`RELEASING.md`). There are no changelog fragments and no other changelog file.

## Releasing

Full steps, and the checklist for an agent, in `RELEASING.md`. In short: bump the version and the changelog; wait for CI green on `main`; **dry run** the Release workflow on the branch (`gh api repos/BhanuNexus/quire/actions/workflows/release.yml/dispatches -f ref=<branch>`); run **Tag release** (`gh api repos/BhanuNexus/quire/actions/workflows/tag-release.yml/dispatches -f ref=main -f 'inputs[version]=X.Y.Z'`, add `-f 'inputs[check_only]=true'` first to only check). It refuses to tag unless CI passed on the commit and the Flatpak builder image digest is the recorded one. **A tag is used once**: if a release fails after the tag exists, fix it and release the next version; never delete, move or reuse a tag.

## Never

- Write `unsafe` (`unsafe_code = "forbid"` for the whole workspace) or add an `#[allow]` without a `reason`.
- Add a plugin, script hook or anything that runs third-party code (P7).
- Kill processes by name pattern (`pkill -f`, `killall`): kill only a PID you started.
- Lose or silently rewrite user text (P1); change a persisted format without keeping old data loadable.
- Edit `Cargo.lock` by hand, or add a dependency without a licence check (`deny.toml`) and a reason.
- Reuse a release tag, or publish from a commit CI has not passed.
