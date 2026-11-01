# Releasing Quire

Release builds come from one script per platform. CI runs the same scripts.

| Platform | Output | Script | Where it runs |
|---|---|---|---|
| macOS 11+ (Apple Silicon + Intel) | `Quire-<v>-macos-universal.dmg` with `Quire.app` | `packaging/macos/package.sh` | macOS only |
| Windows 10/11 x64 | `Quire-<v>-windows-x64-setup.exe` (per-user installer, no admin) and `-portable.zip` | `packaging/windows/package.sh [gnu\|msvc]` | Windows (msvc), or Linux/macOS cross (gnu) |

Both apps include `quire-cli` (search, import, history and the MCP server). On macOS it's inside the bundle: `/Applications/Quire.app/Contents/MacOS/quire-cli`. On Windows it's next to `quire.exe` in `%LOCALAPPDATA%\Programs\Quire`.

## Toolchain and lints

`rust-toolchain.toml` pins Rust **1.97.0**, so contributors, CI and release builds all use the same compiler and clippy. To move to a newer Rust, change `channel`, run `cargo clippy --workspace --all-targets -- -D warnings` and fix any new lints, then commit both changes together.

Libraries stay on their latest stable releases. Dependabot opens weekly PRs for Cargo and GitHub Actions. As of 0.3.1, `notify` 9 and `ropey` 2 are still pre-releases, so they stay on 8 and 1.

Lint policy lives in `Cargo.toml` (`[workspace.lints]`) and `clippy.toml`. CI runs:

- `rustfmt`;
- clippy with all, pedantic and nursery groups plus selected restriction lints, warnings as errors;
- rustdoc with warnings as errors;
- `typos` spell-checking;
- `cargo-deny` for licences, advisories and sources;
- CodeQL.

Each lint that is switched off has its reason written next to it.

## Cutting a release

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo check` so `Cargo.lock` updates, and move the *Unreleased* entries in `CHANGELOG.md` under the new version.
2. Commit, then tag and push: `git tag v0.3.0 && git push origin v0.3.0`.
3. `.github/workflows/release.yml` runs tests, builds macOS and Windows in parallel, and attaches the files and `.sha256` checksums to a **draft** GitHub release.
4. Check the draft, edit the notes and publish it.

Running the workflow by hand from the Actions tab builds the same files as downloadable artifacts, without a release.

## Signing (recommended before a public release)

Unsigned builds work, but Windows SmartScreen shows "Windows protected your PC" and macOS Gatekeeper blocks the first launch until the user right-clicks → Open. Add these repository secrets to sign. Each platform is signed only when its secrets are present.

**macOS** needs an Apple Developer Program membership.
- `MACOS_CERTIFICATE`: a "Developer ID Application" certificate exported as .p12, base64-encoded (`base64 -i cert.p12 | pbcopy`).
- `MACOS_CERTIFICATE_PASSWORD`: the .p12 password.
- `MACOS_SIGNING_IDENTITY`: e.g. `Developer ID Application: Your Company (ABCDE12345)`.
- `APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_PASSWORD` (an app-specific password from appleid.apple.com) for notarization. The DMG is notarized and stapled.

**Windows**
- `WINDOWS_CERTIFICATE`: a code-signing certificate as .pfx, base64-encoded.
- `WINDOWS_CERTIFICATE_PASSWORD`: its password.

  An EV or cloud-HSM certificate (e.g. Azure Trusted Signing) gets SmartScreen reputation fastest, but those can't be exported as a .pfx. For them, replace the `sign()` function in `packaging/windows/package.sh` with the provider's signing tool.

## Building locally

```bash
# Windows, cross from Linux (needs: mingw-w64, nsis, the x86_64-pc-windows-gnu Rust target)
rustup target add x86_64-pc-windows-gnu
packaging/windows/package.sh gnu

# macOS (on a Mac with Xcode command line tools)
rustup target add aarch64-apple-darwin x86_64-apple-darwin
packaging/macos/package.sh
```

Files land in `dist/`.

## What the installers do

- **Windows:** installs to `%LOCALAPPDATA%\Programs\Quire` for the current user and adds a Start-menu shortcut (desktop shortcut optional). It registers an uninstaller under *Settings → Apps*, and `/S` installs or uninstalls silently. Uninstalling never touches vaults or settings.
- **macOS:** drag `Quire.app` to Applications. Settings live in `~/Library/Application Support/dev.Quire.Quire/`.

## Not done yet

- **Auto-update:** users download new versions by hand. Planned as an opt-in check against GitHub Releases, in keeping with the offline-by-default promise.
- **No Linux packages** (AppImage/.deb). The same approach works if wanted.
