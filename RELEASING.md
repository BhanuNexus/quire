# Releasing Quire

Every release is built by GitHub Actions from a tag. It's published as an immutable GitHub release with checksums, build-provenance attestations and an SBOM.

| Platform | Files | Built by |
|---|---|---|
| Windows 10/11 x64 | `Quire-<v>-windows-x64-setup.exe` (per-user installer, no admin), `Quire-<v>-windows-x64-portable.zip` | `packaging/windows/package.sh msvc` |
| macOS 11+ (Apple Silicon + Intel) | `Quire-<v>-macos-universal.dmg` | `packaging/macos/package.sh` |
| Linux x86_64 (glibc 2.35+) | `Quire-<v>-linux-x86_64.tar.gz` (reproducible; includes `install.sh` for a no-root install) | `packaging/linux/package.sh` |
| All | `SHA256SUMS`, `Quire-<v>-sbom.spdx.json` | release workflow |

`quire-cli` (search, import, history and the MCP server) ships in every package.

## Cutting a release

1. Bump `version` in the root `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `## [Unreleased]` above it, and update the compare links at the bottom.
3. Commit, then tag and push:
   ```bash
   git tag -s vX.Y.Z -m "Quire X.Y.Z"   # -s signs the tag (recommended); -a works too
   git push origin vX.Y.Z
   ```
   Or, without a local clone (from a phone, say): **Actions → Tag release → Run workflow** on `main`, with the version. It checks the version and changelog, creates the annotated tag on the latest `main` commit (credited to that commit's author), and starts the Release workflow on it.
4. `.github/workflows/release.yml` then:
   1. checks that the tag matches `Cargo.toml` and that the changelog has a section for it, checks that the on-device models are pinned (`packaging/pin-models.py --check`; only a warning while `REQUIRE_MODEL_PINS` at the top of the workflow is `"false"`, as in 0.9.0, where on-device AI ships behind an experimental switch), and runs the tests;
   2. builds the on-device AI engine per target, uploads it as a release asset (`quire-ai-engine-<target>.gz`) and pins its SHA-256 into the app builds (`engine.sha256.<target>`);
   3. builds Linux, macOS and Windows in parallel, from clean (no build caches), both editions. The full edition's packages include the engine next to the app: each job unpacks the engine assets for its target, checks them against their pins (`packaging/unpack-engine.sh`; on macOS the two are joined with `lipo`), and passes the file as `ENGINE_BIN` to the packaging script, which signs it (macOS, Windows), writes its SHA-256 to `crates/quire-ai/engine.bundled.sha256` and builds the app with it, so the app refuses an engine next to it that isn't that file. Quire Lite's packages have no engine; it downloads the asset when on-device AI is turned on. A smoke test checks each package has (or hasn't) the engine and, for the full edition, that `quire-cli ai engine` starts it;
   4. creates a build-provenance attestation for every file;
   5. generates an SPDX SBOM and attests it against the binaries;
   6. writes `SHA256SUMS`;
   7. creates a **draft** release with the changelog section as notes and uploads every file;
   8. publishes the release. With immutable releases on, the tag and assets are locked from this point.

A tag with a suffix (e.g. `v0.4.0-rc.1`) is published as a pre-release. Running the Release workflow by hand on a branch is a dry run: the same builds are uploaded as workflow artifacts, with no attestations and no release.

**If a release goes wrong:** published releases can't be edited. Fix the problem and release the next patch version. Mark the bad release as such in its notes, or delete it, but never reuse its tag.

## One-time repository settings (maintainers)

These can only be changed in the GitHub UI:

- **Settings → General → Releases → Enable release immutability.** Published releases can then never have their assets or tag changed.
- **Settings → Rules → Rulesets → New tag ruleset** for `v*`: restrict creation, update and deletion to maintainers, so release tags can't be moved or deleted.
- **Settings → Environments → `release`** (created by the first release run): add required reviewers to approve each publish. Optionally limit it to `v*` tags.
- **Settings → Rules → Rulesets → New branch ruleset** for `main`: require a pull request, the `CI` checks (`lint`, `test (…)`, `deny`, `typos`, `workflows`) and `CodeQL`; block force-pushes.
- **Settings → Code security:** private vulnerability reporting, Dependabot alerts and security updates, and secret scanning with push protection.
- **Settings → General → Features:** turn on Discussions.

## Verifying a download

```bash
sha256sum -c SHA256SUMS --ignore-missing                       # integrity
gh attestation verify Quire-0.4.0-linux-x86_64.tar.gz --repo BhanuNexus/quire   # built by this repo's release workflow
```

`gh attestation verify` checks a signed statement from GitHub that names the exact workflow, commit and tag that produced the file.

## Signing (recommended before 1.0)

Unsigned builds work, but Windows SmartScreen shows "Windows protected your PC", and macOS Gatekeeper blocks the first launch until the user right-clicks → Open. Add these repository secrets to sign. Each platform is signed only when its secrets are present.

**macOS** needs an Apple Developer Program membership.
- `MACOS_CERTIFICATE`: a "Developer ID Application" certificate exported as .p12, base64-encoded (`base64 -i cert.p12 | pbcopy`).
- `MACOS_CERTIFICATE_PASSWORD`: the .p12 password.
- `MACOS_SIGNING_IDENTITY`: e.g. `Developer ID Application: Your Company (ABCDE12345)`.
- `APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_PASSWORD` (an app-specific password) for notarization. The DMG is notarized and stapled.

**Windows**
- `WINDOWS_CERTIFICATE`: a code-signing certificate as .pfx, base64-encoded.
- `WINDOWS_CERTIFICATE_PASSWORD`: its password.

  Azure Trusted Signing or an EV certificate on an HSM gets SmartScreen reputation fastest. Neither exports as a .pfx; for those, replace `sign()` in `packaging/windows/package.sh` with the provider's tool.

## Building locally

```bash
packaging/linux/package.sh                 # Linux
packaging/windows/package.sh gnu           # Windows, cross from Linux (mingw-w64 + nsis + x86_64-pc-windows-gnu target)
packaging/macos/package.sh                 # macOS (on a Mac)
```

Files land in `dist/`. Without `ENGINE_BIN` the full edition is packaged without the on-device AI engine (it then downloads it, like Lite). To include one, build it and pass it (on macOS, a universal binary made with `lipo`):

```bash
cargo build --release --locked -p quire-ai-engine --bin quire-ai-engine
ENGINE_BIN=target/release/quire-ai-engine packaging/linux/package.sh
```

A local build has no download pins, so it doesn't check an engine it downloads; it does check the bundled one against `engine.bundled.sha256`.

## Toolchain and lints

`rust-toolchain.toml` pins Rust **1.97.0**, so contributors, CI and release builds all use the same compiler and clippy. To move to a newer Rust, change `channel`, run `cargo clippy --workspace --all-targets -- -D warnings` and fix any new lints, then commit both changes together.

Libraries stay on their latest stable releases. Dependabot opens weekly PRs for Cargo and GitHub Actions. As of 0.4.0, `notify` 9 and `ropey` 2 are still pre-releases, so they stay on 8 and 1.

Lint policy lives in `Cargo.toml` (`[workspace.lints]`) and `clippy.toml`. CI runs:

- `rustfmt`;
- clippy with all, pedantic and nursery groups plus selected restriction lints, warnings as errors;
- rustdoc with warnings as errors;
- `typos` spell-checking;
- `cargo-deny` for licences, advisories and sources;
- CodeQL.

Each lint that is switched off has its reason written next to it.

## What the installers do

- **Windows:** installs to `%LOCALAPPDATA%\Programs\Quire` for the current user and adds a Start-menu shortcut (desktop shortcut optional). It registers an uninstaller under *Settings → Apps*, and `/S` installs or uninstalls silently. Uninstalling never touches vaults or settings.
- **macOS:** drag `Quire.app` to Applications. Settings live in `~/Library/Application Support/dev.Quire.Quire/`.
- **Linux:** extract, then run `./install.sh`. It installs to `~/.local/bin` with a menu entry and icon; `./install.sh --uninstall` removes it.

## Not done yet

- **Auto-update:** users download new versions by hand. Planned as an opt-in check against GitHub Releases, in keeping with the offline-by-default promise.
- **Linux:** a tar.gz with a no-root installer for now. AppImage, .deb and Flatpak are tracked in the issues.
