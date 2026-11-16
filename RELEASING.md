# Releasing Quire

Every release is built by GitHub Actions from a tag. It's published as an immutable GitHub release with checksums, build-provenance attestations and an SBOM.

| Platform | Files | Built by |
|---|---|---|
| Windows 10/11 x64 | `Quire-<v>-windows-x64-setup.exe` (per-user installer, no admin), `Quire-<v>-windows-x64-portable.zip` | `packaging/windows/package.sh msvc` |
| macOS 11+ (Apple Silicon + Intel) | `Quire-<v>-macos-universal.dmg` | `packaging/macos/package.sh` |
| Linux x86_64 and ARM64 (glibc 2.35+) | `Quire-<v>-linux-<arch>.tar.gz` (reproducible; includes `install.sh` for a no-root install), `.AppImage` and `.deb`; `<arch>` is `x86_64` or `aarch64` (the `.deb` says `amd64` or `arm64`). The sync server tarball is x86_64 only | `packaging/linux/package.sh`, natively on each architecture's runner |
| Flatpak, x86_64 and ARM64 | `Quire-<v>-linux-<arch>.flatpak` (the full edition only) | `packaging/flatpak/io.github.bhanunexus.Quire.yml`, with `flatpak-builder`, from source and offline |
| All | `SHA256SUMS`, `Quire-<v>-sbom.spdx.json`, and the on-device AI engine downloads `quire-ai-engine-<target>.gz` | release workflow |

Every edition also has a Quire Lite file (`Quire-Lite-<v>-…`) for Windows, macOS and Linux (not the Flatpak). The exact list of files in a release is one file, [`packaging/release-files.txt`](packaging/release-files.txt): the workflow fails when the files it collected are not exactly those. Change it in the same commit as any change to what is built.

`quire-cli` (search, import, history and the MCP server) ships in every package.

## Cutting a release

1. Bump `version` in the root `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `## [Unreleased]` above it, and update the compare links at the bottom.
3. Commit, then tag and push:
   ```bash
   git tag -s vX.Y.Z -m "Quire X.Y.Z"   # -s signs the tag (recommended); -a works too
   git push origin vX.Y.Z
   ```
   **Pushing a tag yourself skips the Tag release guard** (CI-green check, builder-image digest): only the Release workflow's own checks run. Prefer the workflow below.
   Or, without a local clone (from a phone, say): **Actions → Tag release → Run workflow** on `main`, with the version. Before it creates anything it checks the version and changelog; that CI passed on the commit (the `CI` and `CodeQL` workflows succeeded on its push and every check-run on it finished with success: `packaging/check-ci-green.sh`); that the Flatpak builder image is still the digest recorded in `packaging/flatpak/builder-image.digest`; and the model pins, when they are required. Then it creates the annotated tag on that `main` commit (credited to the commit's author) and starts the Release workflow on it. Tick *check_only* to run only the checks and create nothing. A check-run that is allowed to fail is named, with a reason, in `packaging/ci-exceptions.txt`, which is empty.
4. `.github/workflows/release.yml` then:
   1. `preflight` checks that the tag matches `Cargo.toml` and that the changelog has a section for it, and checks that the on-device models are pinned (`packaging/pin-models.py --check`; only a warning while `REQUIRE_MODEL_PINS` at the top of the workflow is `"false"`, as it is now, with on-device AI behind an experimental switch). `check` runs the workspace tests **beside** the builds, not before them; nothing is assembled or published unless it passed;
   2. builds the on-device AI engine per target, uploads it as a release asset (`quire-ai-engine-<target>.gz`) and pins its SHA-256 into the app builds (`engine.sha256.<target>`);
   3. builds Linux (x86_64 on `ubuntu-22.04`, ARM64 on `ubuntu-22.04-arm`, natively), macOS and Windows in parallel, from clean (no build caches), both editions. The Flatpak is built from source on both architectures (`flatpak-sources`, then `flatpak`; full edition only), in a builder image pinned by digest: `flatpak-image` stops the run, dry run or not, if the tag no longer resolves to the recorded digest. The full edition's packages include the engine next to the app: each job unpacks the engine assets for its target, checks them against their pins (`packaging/unpack-engine.sh`; on macOS the two are joined with `lipo`), and passes the file as `ENGINE_BIN` to the packaging script, which signs it (macOS, Windows), writes its SHA-256 to `crates/quire-ai/engine.bundled.sha256` and builds the app with it, so the app refuses an engine next to it that isn't that file. Quire Lite's packages have no engine; it downloads the asset when on-device AI is turned on. A smoke test checks each package has (or hasn't) the engine and, for the full edition, that `quire-cli ai engine` starts it;
   4. creates a build-provenance attestation for every file (the Flatpak bundles in `flatpak-attest`, which first checks that they are exactly the expected two);
   5. generates an SPDX SBOM;
   6. `assemble` collects every release file, writes `SHA256SUMS` and checks it with `packaging/release-files.sh`: the file names are exactly those of `packaging/release-files.txt`, `sha256sum -c` passes, `SHA256SUMS` has no line for itself and none with the hash of an empty file;
   7. `publish` (tag releases only, in the `release` environment) collects the files again, repeats those checks, attests the SBOM against the binaries, creates a **draft** release with the changelog section as notes and uploads every file;
   8. publishes the release. With immutable releases on, the tag and assets are locked from this point.

A tag with a suffix (e.g. `v0.4.0-rc.1`) is published as a pre-release. Running the Release workflow by hand on a branch is a dry run: the same jobs, tests included, and `assemble`, with its file-set and checksum checks. It differs from a tag release in three ways: no attestation is made (the attest steps are skipped), `publish` does not run (so the `release` environment, the SBOM attestation and the release itself are untried), and the artifacts stay in the workflow run. A dry run does not read a tag, so the version and notes checks of `preflight` run only on a tag.

**If a release goes wrong:** published releases can't be edited. Fix the problem and release the next patch version. Mark the bad release as such in its notes, or delete it, but never reuse its tag. When a run on a tag fails before anything is published, **Re-run failed jobs** on that same run is fine for a flaky runner. The tag itself is never moved, deleted or created again, and a fix to the workflow or the code needs a new version: the workflow is read from the tag. That is why the dry run and the Tag release checks exist.

## Release checklist for an agent

In this order. Do not skip a step because an earlier release worked.

1. Work on a branch. The change is done when the gate in [AGENTS.md](AGENTS.md) passes in both editions.
2. Bump `version` in the root `Cargo.toml`, run `cargo check` (it updates `Cargo.lock`), and make `CHANGELOG.md`'s `[Unreleased]` the `[X.Y.Z]` section as in step 2 above, with the short summary and, when an upgrader must do something, the upgrade note at the top.
3. If a package was added or dropped, edit `packaging/release-files.txt` in the same commit. Packaging scripts are checked by `packaging/test-release-files.sh` and `packaging/test-check-ci-green.sh` (CI's `packaging` job runs them).
4. Merge to `main` and wait for `CI` and `CodeQL` to finish green on the merge commit.
5. **Dry run** on the release branch (or on `main` itself): `gh api repos/BhanuNexus/quire/actions/workflows/release.yml/dispatches -f ref=<branch>`. Read it to the end: every job, including `check`, `flatpak-image` and `assemble`, must be green. About an hour. Fix what it finds and run it again; nothing is lost.
6. Run **Tag release** with *check_only* first: `gh api repos/BhanuNexus/quire/actions/workflows/tag-release.yml/dispatches -f ref=main -f 'inputs[version]=X.Y.Z' -f 'inputs[check_only]=true'`. It must be green.
7. Run it for real, without *check_only*: `gh api repos/BhanuNexus/quire/actions/workflows/tag-release.yml/dispatches -f ref=main -f 'inputs[version]=X.Y.Z'`. It creates the tag and starts the Release workflow on it.
8. Approve the `release` environment when the run reaches `publish`, if reviewers are set.
9. Check the result: `sha256sum -c SHA256SUMS --ignore-missing` on a download, and `gh attestation verify` on one file (see below).

What runs for the first time only on a real tag (a dry run does not enter them):

- the `release` environment (its reviewers and any tag policy);
- `actions/attest-sbom` (its subject globs and the `id-token` and `attestations: write` permissions) and every `attest-build-provenance` step (the engine, Linux, macOS, Windows and `flatpak-attest` jobs), all gated by the tag;
- `gh release create --draft --verify-tag` with all 28 files and `gh release edit --draft=false` (immutable releases);
- `release-notes.sh` in `publish`, and `preflight`'s tag-against-`Cargo.toml` and model-pin checks;
- the whole Tag release workflow (`tag-guard` and `tag`), which can only run on `main`: its first use is with *check_only*. Not covered before: the `GITHUB_TOKEN` reading check-runs and workflow runs, `docker buildx imagetools inspect` there, creating the tag through the API with the commit's author as tagger, and starting the Release workflow.

A failure in any of these burns the version: fix it and release the next patch. **A tag that has been created is never reused**, whether or not the release run failed, and never delete a tag to make a version available again.

**What the guard looks at.** `packaging/check-ci-green.sh` judges the check-runs of the push runs of the `CI` and `CodeQL` workflows on the commit, and requires both runs to exist and to have succeeded. Everything else on the commit is printed as information and does not decide: Dependabot jobs, the weekly Audit run (a failed or unfinished Audit gets a prominent warning, but does not block a release; read it), the Release workflows, and the GitHub Advanced Security check also called "CodeQL". That last one is the code-scanning *pull-request* result ("N new alerts in code changed by this pull request") of the pull request whose head is the commit; it is not the CodeQL workflow. A real alert is fixed or dismissed in Security, Code scanning on its own merits; that is not a precondition of the guard, and the guard does not read it. To make another check-run decide, name it in `packaging/ci-also-required.txt`; to let a failing CI check-run through, name it with a reason in `packaging/ci-exceptions.txt`. Both are empty. The guard looks at the tip of `main` only: a commit pushed together with a newer one has no push runs.

## Owner settings

Only the repository owner can change these (GitHub UI, not a file). Until they are set, nothing enforces the guard:

- **A ruleset for `main`** requiring the CI checks (`lint`, `test (…)` including `ubuntu-24.04-arm`, `test-features`, `deny`, `typos`, `workflows`, `packaging`) and the pull request, and blocking force-pushes. At the last check there was no protection on `main`.
- **The `v*` tag ruleset:** its include list is empty today, so it matches no tag and only has `non_fast_forward`. Make it match `v*` and restrict creation, update and deletion to maintainers.
- **Reviewers on the `release` environment**, so each publish is approved.

Also, once: **Settings → General → Releases → Enable release immutability** (published releases can never change), **Settings → Code security** (private vulnerability reporting, Dependabot alerts and security updates, secret scanning with push protection) and **Settings → General → Features → Discussions**.

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

`rust-toolchain.toml` pins the Rust version (read `channel` there), so contributors, CI and release builds all use the same compiler and clippy. To move to a newer Rust, change `channel`, run `cargo clippy --workspace --all-targets -- -D warnings` and fix any new lints, then commit both changes together.

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

## What is not automated

- **Updates:** Quire can check GitHub Releases for a newer version (Settings → About, off until turned on, at most once a day). It never downloads or installs anything: it shows a link to the release page, so people update by hand.
- **Flathub:** the Flatpak bundle is built and attested here but is not submitted to Flathub (`packaging/flatpak/README.md`).
