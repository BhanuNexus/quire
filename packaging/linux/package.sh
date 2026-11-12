#!/usr/bin/env bash
# Build the Linux downloads for this machine's architecture (x86_64, or
# aarch64 on an ARM64 machine; ARCH=x86_64|aarch64 says so explicitly, and
# the machine must be that architecture: nothing here cross-compiles):
#
#   Quire-<v>-linux-<arch>.tar.gz    portable archive with install.sh
#   Quire-<v>-linux-<deb>.deb        Debian/Ubuntu package (needs dpkg-deb);
#                                    <deb> is amd64 or arm64
#   Quire-<v>-linux-<arch>.AppImage  single-file app (needs APPIMAGETOOL and
#                                    APPIMAGE_RUNTIME; see fetch-appimagetool.sh)
#   quire-sync-server-<v>-linux-x86_64.tar.gz  the self-hosted sync server
#                                    (x86_64 only, for now)
#
#   packaging/linux/package.sh             the full edition (and the sync server)
#   EDITION=lite packaging/linux/package.sh  Quire Lite: no math, diagrams or PDF
#                                            (Quire-Lite-<v>-…; the .deb is
#                                            quire-lite, which replaces quire)
#
# ENGINE_BIN=<path to quire-ai-engine> puts the on-device AI engine in the
# full edition's packages, next to `quire` (/usr/bin in the .deb and the
# AppImage, the archive's top folder in the tar.gz), and builds the app with
# its SHA-256 (crates/quire-ai/engine.bundled.sha256), so the app checks it
# before every start (design §D.3). Quire Lite never includes it: it
# downloads the engine when on-device AI is turned on. Without ENGINE_BIN
# the packages have no engine, as before.
#
# Both editions are the same program over the same files and settings, so
# people can switch either way at any time.
#
# Archives are reproducible where the tools allow: fixed file order, owner
# and timestamps (SOURCE_DATE_EPOCH, defaulting to the last commit time).
set -euo pipefail
cd "$(dirname "$0")/../.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
arch="${ARCH:-$(uname -m)}"
case "$arch" in
  x86_64) deb_arch="amd64" ;;
  aarch64 | arm64) arch="aarch64"; deb_arch="arm64" ;;
  *) echo "unsupported architecture $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
target="$arch-unknown-linux-gnu"
edition="${EDITION:-full}"
case "$edition" in
  full) prefix="Quire"; pkg_name="quire"; other_pkg="quire-lite"; features=() ;;
  lite) prefix="Quire-Lite"; pkg_name="quire-lite"; other_pkg="quire"; features=(--no-default-features) ;;
  *) echo "EDITION must be full or lite" >&2; exit 1 ;;
esac
name="$prefix-$version-linux-$arch"
app_id="io.github.bhanunexus.Quire"
out="dist"
stage="$out/$name"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"
bin="target/$target/release"

# ---------------------------------------------------------------- engine
engine="${ENGINE_BIN:-}"
engine_pin="crates/quire-ai/engine.bundled.sha256"
rm -f "$engine_pin"
if [[ -n "$engine" && "$edition" != full ]]; then
  echo "note: Quire Lite doesn't include the engine; ignoring ENGINE_BIN" >&2
  engine=""
fi
if [[ -n "$engine" ]]; then
  [[ -f "$engine" ]] || { echo "ENGINE_BIN: no file at $engine" >&2; exit 1; }
  # From stdin, so a file name never changes the output (packaging/unpack-engine.sh).
  sha256sum < "$engine" | cut -c1-64 > "$engine_pin"
  # Only these builds embed it; a later development build must not.
  trap 'rm -f "$engine_pin"' EXIT
  echo "Bundling the engine $engine ($(cat "$engine_pin"))"
fi

# Identical code folding: the linker keeps one copy of functions that
# compiled to the same machine code (generic code repeats a lot). About 1 MB
# off each binary (docs/design/0.10-perf.md); Rust promises no unique
# function addresses, so nothing relies on them. Only x86_64 Linux links with
# rust-lld, which has the flag; aarch64 uses the system's GNU ld, which
# doesn't (docs/design/0.13-pack.md §1), so there it is left out.
if [[ "$arch" == x86_64 ]]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--icf=all"
fi
cargo build --release --locked --target "$target" -p quire-desktop -p quire-cli ${features[@]+"${features[@]}"}
[[ "$edition" == full && "$arch" == x86_64 ]] && cargo build --release --locked --target "$target" -p quire-sync-server
mkdir -p "$out"

# ---------------------------------------------------------------- tar.gz
rm -rf "$stage" && mkdir -p "$stage"
install -m 755 "$bin/quire" "$bin/quire-cli" "$stage/"
[[ -n "$engine" ]] && install -m 755 "$engine" "$stage/quire-ai-engine"
install -m 644 packaging/linux/quire.desktop "$stage/"
install -m 644 packaging/icons/quire-256.png "$stage/quire.png"
install -m 755 packaging/linux/install.sh "$stage/"
install -m 644 README.md LICENSE "$stage/"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$out" -cf - "$name" | gzip -9n > "$out/$name.tar.gz"
rm -rf "$stage"

# ---------------------------------------------------------------- sync server
# Once per release, with the full edition (x86_64 only).
server="quire-sync-server-$version-linux-x86_64"
build_server=false
[[ "$edition" == full && "$arch" == x86_64 ]] && build_server=true
if [[ "$build_server" == true ]]; then
  sstage="$out/$server"
  rm -rf "$sstage" && mkdir -p "$sstage"
  install -m 755 "$bin/quire-sync-server" "$sstage/"
  install -m 644 docs/self-hosting.md "$sstage/README.md"
  install -m 644 LICENSE "$sstage/"
  tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -C "$out" -cf - "$server" | gzip -9n > "$out/$server.tar.gz"
  rm -rf "$sstage"
fi

# The AppStream file, with this version's release and notes (from CHANGELOG.md).
metainfo="$out/$app_id.metainfo.xml"
python3 -I packaging/linux/render-metainfo.py "$version" > "$metainfo"
trap 'rm -f "$metainfo" "$engine_pin"' EXIT

# Files every system-wide layout shares (.deb and AppImage).
# $1 = root to install into (e.g. pkgroot or AppDir)
install_tree() {
  local root="$1"
  install -Dm 755 "$bin/quire" "$root/usr/bin/quire"
  install -Dm 755 "$bin/quire-cli" "$root/usr/bin/quire-cli"
  # Next to the app, where it looks first after QUIRE_AI_ENGINE.
  if [[ -n "$engine" ]]; then install -Dm 755 "$engine" "$root/usr/bin/quire-ai-engine"; fi
  sed 's/^Icon=quire$/Icon='"$app_id"'/' packaging/linux/quire.desktop \
    | install -Dm 644 /dev/stdin "$root/usr/share/applications/$app_id.desktop"
  install -Dm 644 packaging/icons/quire-256.png "$root/usr/share/icons/hicolor/256x256/apps/$app_id.png"
  install -Dm 644 "$metainfo" "$root/usr/share/metainfo/$app_id.metainfo.xml"
}

# ---------------------------------------------------------------- .deb
if command -v dpkg-deb > /dev/null; then
  deb="$prefix-$version-linux-$deb_arch.deb"
  pkg="$out/deb-root"
  rm -rf "$pkg" && mkdir -p "$pkg/DEBIAN"
  install_tree "$pkg"
  install -Dm 644 LICENSE "$pkg/usr/share/doc/$pkg_name/copyright"
  size_kb="$(du -sk --exclude=DEBIAN "$pkg" | cut -f1)"
  # The binary links only libc (and, in the full edition, ALSA for voice
  # notes); windowing and GL libraries are loaded at run time, so they're
  # listed here for the package manager to install.
  alsa=""
  [[ "$edition" == full ]] && alsa=", libasound2t64 | libasound2"
  cat > "$pkg/DEBIAN/control" <<EOF
Package: $pkg_name
Version: ${version//-/\~}
Conflicts: $other_pkg
Replaces: $other_pkg
Provides: quire-editor
Architecture: $deb_arch
Maintainer: The Quire contributors <https://github.com/BhanuNexus/quire/issues>
Installed-Size: $size_kb
Depends: libc6 (>= 2.35), libgcc-s1, libgl1 | libegl1, libxkbcommon0, libwayland-client0 | libx11-6$alsa
Recommends: libegl1, libwayland-egl1, libx11-xcb1, libxcursor1, libxi6, libxrender1, libxkbcommon-x11-0, libdbus-1-3
Section: editors
Priority: optional
Homepage: https://github.com/BhanuNexus/quire
Description: Fast, local-first Markdown notes$([[ "$edition" == lite ]] && echo " (Lite edition)")
 Quire is a notepad for plain Markdown files in a folder you choose. It
 styles notes as you type, saves automatically and links them like a wiki,
 with backlinks, related notes and search by words and by meaning.
 Nothing leaves your computer unless you turn on an AI provider.
EOF
  find "$pkg" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
  dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out/$deb" > /dev/null
  rm -rf "$pkg"
fi

# ---------------------------------------------------------------- AppImage
if [[ -n "${APPIMAGETOOL:-}" && -n "${APPIMAGE_RUNTIME:-}" ]]; then
  appimage="$name.AppImage"
  appdir="$out/Quire.AppDir"
  rm -rf "$appdir" && mkdir -p "$appdir"
  install_tree "$appdir"
  cp "$appdir/usr/share/applications/$app_id.desktop" "$appdir/$app_id.desktop"
  cp packaging/icons/quire-256.png "$appdir/$app_id.png"
  cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
case "$(basename "${ARGV0:-quire}")" in
  quire-cli*) exec "$HERE/usr/bin/quire-cli" "$@" ;;
  *) exec "$HERE/usr/bin/quire" "$@" ;;
esac
EOF
  chmod 755 "$appdir/AppRun"
  ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream --runtime-file "$APPIMAGE_RUNTIME" \
    "$appdir" "$out/$appimage" > /dev/null
  rm -rf "$appdir"
fi

if [[ "$build_server" == true ]]; then
  ( cd "$out" && sha256sum "$prefix-$version"-linux-"$arch"* "$prefix-$version"-linux-"$deb_arch".deb "$server.tar.gz" > "$name.sha256" )
else
  ( cd "$out" && sha256sum "$prefix-$version"-linux-"$arch"* "$prefix-$version"-linux-"$deb_arch".deb > "$name.sha256" )
fi
echo "Built:"; ls -la "$out/$prefix-$version"-linux-*
