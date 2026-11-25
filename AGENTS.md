# AGENTS.md: working in the Quire repository

For an agent or a person starting cold. (Not the `AGENTS.md` a user keeps in a *vault*: that one, `docs/agents.md` and `skills/quire/` are for users' notes.) If a line here is wrong, fix it in the same change.

## What Quire is

- A local-first notes app in Rust (AGPL-3.0-or-later): plain Markdown files in a folder you own. An egui desktop app (`apps/quire-desktop`) and a CLI (`apps/quire-cli`); `quire-cli serve` is the web app and the MCP server; `apps/quire-sync-server` is the optional end-to-end encrypted sync server.
- Offline by default; no plugins; no AI without opt-in. **Two editions:** full, and Lite (`--no-default-features` on the desktop and CLI: no math, Mermaid, PDF export or voice). Both are built, tested and released; persisted formats never depend on the edition (P5).
- The crates and their layers: `ARCHITECTURE.md` §3 (checked against `cargo metadata` in CI). Nested `AGENTS.md` files hold the rules of the risky crates: `crates/quire-vault`, `crates/quire-sync`, `crates/quire-app`, `crates/quire-mcp`, `crates/quire-agent` and `apps/quire-desktop`. Read the one next to `Cargo.toml` before changing that crate.

## Where to start

| Task | Read first |
|---|---|
| Add a command, setting, dialog or MCP tool; change a file format or sync | `docs/howto.md` |
| Which design note to trust | `docs/design/README.md`; each note starts with `Status:` (`current` is a rule, `record` and `superseded` are history) |
| Persisted formats | `docs/formats/` |
| Release | `RELEASING.md` (skill: `.claude/skills/release`); a tag is used once |
| Review a change | `.claude/skills/review-change` |

## The gate

Inner loop, one crate: `cargo test -p <crate> --locked`, `cargo clippy -p <crate> --all-targets --locked -- -D warnings`. Before you push, all of this (CI runs it, except `actionlint`: run that by hand when you touch a workflow):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p quire-desktop -p quire-cli --no-default-features --all-targets --locked -- -D warnings   # Lite
cargo test --workspace --locked --no-fail-fast
cargo test -p quire-desktop -p quire-cli --no-default-features --locked                                  # Lite
cargo test -p quire-pdf --features render --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items --locked
cargo deny check advisories bans licenses sources   # cargo install cargo-deny
typos                                                # cargo install typos-cli
python3 -I packaging/check-layering.py && packaging/check-design-notes.sh && packaging/check-comment-labels.sh
packaging/test-unpack-engine.sh && packaging/test-release-files.sh && packaging/test-check-ci-green.sh
python3 -I packaging/test-check-changelog.py && python3 -I packaging/test-ratchet.py && python3 -I packaging/test-pin-models.py
shellcheck --severity=error packaging/*.sh packaging/*/*.sh apps/quire-sync-server/*.sh
pipx run zizmor==1.31.1 --offline --format=github .github/workflows/
```

CI runs the workspace tests with `cargo nextest run --profile ci` (`.config/nextest.toml`: a test over 5 minutes is terminated; JUnit is uploaded) plus `cargo test --doc`, which nextest does not run; `cargo test` stays the local default. Weekly workflows (see each file's header, `docs/design/0.14-health-ci.md`): `real-servers` (the S3 and WebDAV sync test; it fails in CI unless `QUIRE_SYNC_REAL_SERVERS` is set, `skip` to opt out), `proptest` (50,000 cases), `fuzz`, `mutants` (also advisory on pull requests that touch `quire-markdown`, `quire-import` or `quire-canvas`) and `ratchet` (counts that may only go down: `packaging/ratchet.toml`). CI also runs the agent edit eval, the `perf` budgets (`packaging/perf-check.py`), the desktop-entry, AppStream, Flatpak and installer checks, and a DCO job. `cargo test --workspace` compiles candle: a cold build takes 10+ minutes.

**Small machine** (2 CPUs, about 7 GB RAM): build crate by crate with `-p`, give each worktree its own `target/`, check `df -h /` before a big build, and set on every cargo command:

```sh
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export RUSTUP_TOOLCHAIN=stable   # only if the pinned toolchain (rust-toolchain.toml) cannot be installed here
```

## Invariants

From `docs/design/0.9.md` §1; code review checks them.

| | Invariant |
|---|---|
| P1 | Markdown files are the truth. Never lose user text; bytes you did not mean to change stay identical. |
| P2 | Nothing proprietary is needed to read notes; deleting `.quire/` never loses note content. |
| P3 | New app state lives under `.quire/` in versioned formats; a reader never rewrites a file of a newer format and keeps unknown fields. |
| P4 | Writes are atomic (see the first rule below). |
| P5 | Persisted formats do not depend on the edition. |
| P6 | Offline first: no network, download or engine process unless the user turned the feature on. |
| P7 | No third-party code execution: no plugins, scripts or config that runs commands; shared config that widens agent permissions needs approval on each device. |
| P8 | Agent output is untrusted data; agents never edit `AGENTS.md`, `CLAUDE.md`, `.quire/**` or skills without a human accepting a proposal. |
| P9 | The UI thread never waits on new work (indexing, AI, PDF, OCR, git and lint run off it). |
| P10 | Tests are deterministic: injected clock, seeded randomness, fake models, no network except loopback servers the test starts. |

## Rules learned the hard way

Each is tagged by what enforces it: `[test]`, `[lint]` (clippy or cargo-deny; the message says what to use) or `[review only]` (you are the check).

- Write every persisted file atomically: `Storage::write_bytes_atomic` (vault), `quire_base::fs::write_atomic` (app state: `Options::STATE`, secrets: `Options::SECRET`). Never `fs::write`, `fs::rename`, `File::create` or tempfile `persist`. `[lint]` disallowed-methods in `clippy.toml`
- When the new bytes depend on what you read, use `Storage::write_bytes_if` (compare-and-swap), not read then write. `[review only]`
- A failed listing or read is an error, never "empty" or "all deleted": sync turns missing notes into deletes. `[test]` `a_listing_that_lost_a_folder_is_retried_then_refused` and the held-deletes tests
- Never drop the result of a save, snapshot, ledger append or sync-state write with `let _ =`, and never replace an error with `map_err(|_| ..)` (a failed read is not "not found"): pass it up, tell the person, or `#[expect(clippy::let_underscore_must_use, reason = "...")]` saying why losing it is right. `[lint]` in quire-vault, quire-sync, quire-agent and quire-mcp (`lib.rs`); `[review only]` elsewhere (quire-app has about 230 sites left)
- Open a vault with `quire_app::secure_state::open_vault`, never `Vault::open_local` (keeps locked folders locked). `[lint]` disallowed-methods
- Parse YAML only through `quire_markdown::frontmatter::load_yaml` (nesting limit of 64). `[lint]` disallowed-methods
- Network only through `quire_base::net::agent` or `config` (the `net` feature), and only in crates `deny.toml` names for `ureq`. `[lint]` disallowed-methods and cargo-deny bans
- Start a process only with `quire_app::git::run_git` (git) or an `#[expect]` that states the reason; libraries return errors instead of `process::exit`. `[lint]` disallowed-methods
- Random bytes come from `quire_base::rand::fill` or `bytes`, which return an error; never carry on with zeros. `[lint]` disallowed-methods
- A crate depends only on lower layers (`ARCHITECTURE.md` §3); third-party crates stay in their layer. `[test]` `packaging/check-layering.py`; `[lint]` cargo-deny bans
- No work before the first frame, and no per-frame disk, index-lock or vault-sized work in `show`/`ui`: cache it. `[test]` CI `perf` job (startup only); `[review only]` per frame
- Escape, close, quit and lock never drop unsaved text. `[review only]` (tests for canvas and sketch only)
- A new dialog is an `OverlayView` listed in `layout_samples` (`apps/quire-desktop/src/registry.rs`). `[test]` `layout_check`; `every_kind_of_dialog_has_a_sample` fails for an `OverlayView` or `Overlay` variant without a sample
- A default shortcut must pass `Keymap::conflicts` on all three platforms (AltGr, Option, reserved keys). `[test]` `every_default_shortcut_passes_the_conflict_checker` (`crates/quire-app/src/keymap.rs`; 18 older defaults are listed there with their warning; a key the system keeps on one platform gets that platform's own default, `MOVED` in the same file)
- Every MCP tool that returns paths, titles, counts or snippets hides what `common::visible` hides; a ranked list ranks only visible notes. `[test]` `crates/quire-mcp/tests/visibility.rs` (every read tool of the registry); `[review only]` for write tools. Known gap: the index's word statistics still count hidden notes (`docs/design/0.14-health-gates.md`)
- Make file names Windows-safe (reserved names, `:?*`, trailing space or dot): `quire_sync::names`. `[test]` for sync names; `[review only]` for new code
- A persisted-format change keeps old data loadable and gets a note in `docs/formats/`. `[review only]`
- No workstream or phase labels (`WS-B`, `F0`) and no bare `design §4.6` in code comments: name the feature, or write `docs/design/<note>.md` §N for a section that exists. `[test]` `packaging/check-comment-labels.sh`

## Commits and the changelog

- Every commit needs a DCO sign-off: `git commit -s` (CI's `dco` job fails a pull request otherwise). No other trailer is required. Imperative, focused commits, with tests for behaviour changes.
- **One changelog rule:** a user-visible change gets one bullet under `## [Unreleased]` in `CHANGELOG.md` (Added, Changed, Fixed or Security): at most two sentences, about what a person sees, not what the code does. Detail goes in the design note. CI's `changelog` job warns on a pull request that changes source under `crates/` or `apps/` (not `tests/`, fixtures or `*.md`) without `CHANGELOG.md`, unless it is labelled `no-changelog` or comes from Dependabot (`packaging/check-changelog.py`). No fragments, no other changelog file. Expect a trivial `CHANGELOG.md` conflict; keep edits to shared files small.

## Never

- Write `unsafe` (forbidden for the workspace) or an `#[allow]` without a `reason`.
- Add a plugin, script hook or anything that runs third-party code (P7).
- Kill processes by name pattern (`pkill -f`, `killall`): kill only a PID you started.
- Lose or silently rewrite user text (P1).
- Edit `Cargo.lock` by hand, or add a dependency without a licence check (`deny.toml`) and a reason.
- Reuse a release tag, or publish from a commit CI has not passed.
