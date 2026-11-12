# Flatpak

`io.github.bhanunexus.Quire.yml` builds the full edition of Quire (with the on-device AI engine) as a Flatpak, from source and without a network. The release workflow builds it for x86_64 and aarch64 and attaches `Quire-<version>-linux-<arch>.flatpak` to the release. Quire Lite has no Flatpak (see [docs/design/0.13-pack.md](../../docs/design/0.13-pack.md#2-flatpak)).

## Build it yourself

Needs `flatpak` and `flatpak-builder`, and Python 3 with `venv` for the crate list.

```bash
flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
packaging/flatpak/update-cargo-sources.sh          # writes packaging/flatpak/cargo-sources.json (git-ignored)
flatpak-builder --user --install --install-deps-from=flathub --force-clean build-dir \
  packaging/flatpak/io.github.bhanunexus.Quire.yml
flatpak run io.github.bhanunexus.Quire
```

It is a full Rust build with no cache: expect 30 to 60 minutes and about 10 GB of disk. Check the manifest and the installed AppStream file with Flathub's linter:

```bash
flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest packaging/flatpak/io.github.bhanunexus.Quire.yml
flatpak run --command=flatpak-builder-lint org.flatpak.Builder appstream /path/to/rendered.metainfo.xml
```

(`python3 -I packaging/linux/render-metainfo.py <version>` prints the AppStream file with the release's version, date and notes filled in.)

## What's in the sandbox

| Permission | Why |
|---|---|
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc`, `--device=dri` | the window and OpenGL |
| `--share=network` | Quire Sync, AI providers you configure, the opt-in update check |
| `--socket=pulseaudio` | voice notes |
| `--filesystem=home` | notes live in folders you choose, and Quire watches them for changes by sync tools; a file manager can open a file from anywhere in the home folder |
| `--talk-name=org.freedesktop.Notifications` | reminders |
| `--talk-name=org.freedesktop.secrets` | API keys in the system keyring |

Everything else is closed. `flatpak override --user --filesystem=/path io.github.bhanunexus.Quire` adds a folder outside the home folder (an external drive).

## For a Flathub submission (not done; nothing here is submitted)

Flathub builds from a public git source, offline. What to prepare in a new `flathub/io.github.bhanunexus.Quire` repository, following [the submission guide](https://docs.flathub.org/docs/for-app-authors/submission):

1. **The manifest**: copy `io.github.bhanunexus.Quire.yml` and replace its first source (the `dir` one) with the release:

   ```yaml
       sources:
         - type: git
           url: https://github.com/BhanuNexus/quire.git
           tag: v0.13.0
           commit: <the tag's full commit id>
         - cargo-sources.json
   ```

   `commit` is required so the tag can't move. Nothing else changes.
2. **`cargo-sources.json`**: run `packaging/flatpak/update-cargo-sources.sh` at the tag and commit the file next to the manifest. Regenerate it whenever `Cargo.lock` changes (every release), or Flathub's build fails on a missing or unlisted crate. The script runs `flatpak-cargo-generator.py` from flatpak-builder-tools at a pinned commit, checked by SHA-256.
3. **`flathub.json`** if the app should build only for some architectures; both x86_64 and aarch64 build today.
4. **The linter's findings**: expect `finish-args-home-filesystem-access` (explained in the table above; Flathub treats it as an exception request). Not run here: nothing in 0.13 ran Flathub's linter, so the CI step that does is informational.
   **The app ID** is permanent once published. `io.github.bhanunexus.Quire` is lower case (the validators prefer it, and the GitHub account name is case-insensitive); Flathub verifies ownership of the `io.github.<account>` name, so check its current rules for the case it wants before submitting.
5. **Metadata Flathub checks**: the AppStream file renders from `packaging/linux/io.github.bhanunexus.Quire.metainfo.xml`; its screenshots are at the release's tag (`raw.githubusercontent.com/.../v<version>/docs/screenshots/...`), so the tag has to exist when Flathub fetches them.
6. **What Flathub may question**: the bundled engine (built in the same manifest from the same source, not downloaded), the opt-in update check that points at GitHub releases (it only shows a link), and the `quire-cli` binary in the app.

Open items before submitting are listed in [docs/design/0.13-pack.md](../../docs/design/0.13-pack.md#2-flatpak).
