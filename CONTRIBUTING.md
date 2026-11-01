# Contributing to Quire

Thanks for helping! Quire is a community project under the [GNU AGPL v3 or later](LICENSE). Everything here is open source: the app, the CLI, the importers and, later, the sync server.

## Ways to help

- **Try it and report what breaks.** Use [bug reports](https://github.com/BhanuNexus/quire/issues/new?template=bug_report.yml). Real-world imports from Logseq, Notion, Bear and Evernote are especially valuable.
- **Pick up an issue** labelled [`good first issue`](https://github.com/BhanuNexus/quire/labels/good%20first%20issue) or [`help wanted`](https://github.com/BhanuNexus/quire/labels/help%20wanted). Comment first so two people don't do the same work.
- **Ideas and questions** go in [Discussions](https://github.com/BhanuNexus/quire/discussions). Open a discussion before a large feature, so the design can be agreed before you write code.
- **Docs and translations** count as much as code.

## Development setup

```bash
git clone https://github.com/BhanuNexus/quire && cd quire
cargo run -p quire-desktop -- ./sample-vault    # rustup installs the pinned toolchain
cargo test --workspace
```

Linux needs the usual X11/Wayland and OpenGL libraries (`libxkbcommon`, Mesa). Read [ARCHITECTURE.md](ARCHITECTURE.md) before a big change. It explains the layers (text → markdown → vault → index → intel → app → UI) and where each kind of code belongs. In short:

- Logic goes in the crates under `crates/`, with tests next to it.
- `apps/quire-desktop` only draws and forwards input.
- Every user action is a `Command` in `crates/quire-app/src/commands.rs`. That gives it a palette entry and a shortcut for free.

## Before you open a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check     # licences and advisories (cargo install cargo-deny)
```

CI runs the same checks on Linux, macOS and Windows.

- Keep PRs focused, one change per PR. Add tests for behaviour changes.
- Put user-visible changes under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
- UI changes: include a before/after screenshot.
- Commit messages: imperative mood, e.g. "Add kanban column limits".

## Sign your commits (DCO)

We use the [Developer Certificate of Origin](https://developercertificate.org/) instead of a CLA. Add a sign-off to each commit:

```bash
git commit -s -m "Fix wikilink rename in tables"
```

This adds `Signed-off-by: Your Name <you@example.com>`, which certifies that you wrote the change or have the right to submit it under the project's licence. A check on each PR verifies the sign-off.

## Dependencies

New dependencies must be AGPL-compatible. `deny.toml` lists the allowed licences, and CI enforces the list. Prefer small, well-maintained crates. Explain any new dependency in the PR.

## Security

Please don't open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

## Code of Conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).
