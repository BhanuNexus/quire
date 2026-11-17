# AGENTS.md: working in the Quire repository

For an agent or a person starting cold. (Not the `AGENTS.md` a user keeps in a *vault*: that one, `docs/agents.md` and `skills/quire/` are for users' notes.) If a line here is wrong, fix it in the same change.

## What Quire is

- A local-first notes app in Rust (AGPL-3.0-or-later): plain Markdown files in a folder you own. An egui desktop app (`apps/quire-desktop`) and a CLI (`apps/quire-cli`); `quire-cli serve` is the web app and the MCP server; `apps/quire-sync-server` is the optional end-to-end encrypted sync server.
- Offline by default; no plugins; no AI without opt-in. **Two editions:** full, and Lite (`--no-default-features` on the desktop and CLI: no math, Mermaid, PDF export or voice). Both are built, tested and released; persisted formats never depend on the edition (P5).
- The crates and their layers: `ARCHITECTURE.md` §3 (checked against `cargo metadata` in CI). Each risky crate has its own `AGENTS.md`: read it before changing that crate.

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
shellcheck --severity=error packaging/*.sh packaging/*/*.sh apps/quire-sync-server/*.sh
pipx run zizmor==1.31.1 --offline --format=github .github/workflows/
```

CI also runs the agent edit eval, the `perf` budgets (`packaging/perf-check.py`), the desktop-entry, AppStream, Flatpak and installer checks, and a DCO job. `cargo test --workspace` compiles candle: a cold build takes 10+ minutes.

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

- Write every persisted file atomically: `Storage::write_bytes_atomic` (vault), `quire_base::fs::write_atomic` (app state). Never `fs::write`, `fs::rename`, `File::create` or tempfile `persist`. `[lint]` disallowed-methods <!-- verify after merge -->
- When the new bytes depend on what you read, use `Storage::write_bytes_if` (compare-and-swap), not read then write. `[review only]`
- A failed listing or read is an error, never "empty" or "all deleted": sync turns missing notes into deletes. `[test]` `a_listing_that_lost_a_folder_is_retried_then_refused` and the held-deletes tests
- Open a vault with `secure_state::open_vault`, never `Vault::open_local` (keeps locked folders locked). `[lint]` disallowed-methods <!-- verify after merge -->
- Parse YAML only through `frontmatter::load_yaml` (nesting limit). `[lint]` disallowed-methods <!-- verify after merge -->
- Network only through the shared HTTP client builder in `quire_base`, and only in crates `deny.toml` names. `[lint]` disallowed-methods and cargo-deny bans <!-- verify after merge -->
- Start a process only with `git::run_git` (git) or a stated reason; libraries return errors instead of `process::exit`. `[lint]` disallowed-methods <!-- verify after merge -->
- Random bytes come from the one function that returns an error; never carry on with zeros. `[lint]` disallowed-methods <!-- verify after merge -->
- A crate depends only on lower layers (`ARCHITECTURE.md` §3); third-party crates stay in their layer. `[test]` `packaging/check-layering.py`; `[lint]` cargo-deny bans
- No work before the first frame, and no per-frame disk, index-lock or vault-sized work in `show`/`ui`: cache it. `[test]` CI `perf` job (startup only); `[review only]` per frame
- Escape, close, quit and lock never drop unsaved text. `[review only]` (tests for canvas and sketch only)
- A new dialog is an `OverlayView` listed in `layout_samples` (`apps/quire-desktop/src/registry.rs`). `[test]` `layout_check` (listed ones only)
- A default shortcut must pass `Keymap::conflicts` on all three platforms (AltGr, Option, reserved keys). `[review only]`
- Every MCP tool that returns paths or titles filters with `common::visible`. `[test]` four named tests in `crates/quire-mcp/tests/security.rs` <!-- verify after merge -->
- Make file names Windows-safe (reserved names, `:?*`, trailing space or dot): `quire_sync::names`. `[test]` for sync names; `[review only]` for new code
- A persisted-format change keeps old data loadable and gets a note in `docs/formats/`. `[review only]`
- No workstream or phase labels (`WS-B`, `F0`) and no bare `design §4.6` in code comments: name the feature, or write `docs/design/<note>.md` §N for a section that exists. `[test]` `packaging/check-comment-labels.sh`

## Commits and the changelog

- Every commit needs a DCO sign-off: `git commit -s` (CI's `dco` job fails a pull request otherwise). No other trailer is required. Imperative, focused commits, with tests for behaviour changes.
- **One changelog rule:** a user-visible change gets one bullet under `## [Unreleased]` in `CHANGELOG.md` (Added, Changed, Fixed or Security): at most two sentences, about what a person sees, not what the code does. Detail goes in the design note. CI warns when a change to `crates/` or `apps/` has no changelog line; label the pull request `no-changelog` when none is due. No fragments, no other changelog file. Expect a trivial `CHANGELOG.md` conflict; keep edits to shared files small.

## Never

- Write `unsafe` (forbidden for the workspace) or an `#[allow]` without a `reason`.
- Add a plugin, script hook or anything that runs third-party code (P7).
- Kill processes by name pattern (`pkill -f`, `killall`): kill only a PID you started.
- Lose or silently rewrite user text (P1).
- Edit `Cargo.lock` by hand, or add a dependency without a licence check (`deny.toml`) and a reason.
- Reuse a release tag, or publish from a commit CI has not passed.
