#!/usr/bin/env bash
# Build the Linux x86_64 downloads:
#
#   Quire-<v>-linux-x86_64.tar.gz    portable archive with install.sh
#   Quire-<v>-linux-amd64.deb        Debian/Ubuntu package (needs dpkg-deb)
#   Quire-<v>-linux-x86_64.AppImage  single-file app (needs APPIMAGETOOL and
#                                    APPIMAGE_RUNTIME; see fetch-appimagetool.sh)
#   quire-sync-server-<v>-linux-x86_64.tar.gz  the self-hosted sync server
#
#   packaging/linux/package.sh
#
# Archives are reproducible where the tools allow: fixed file order, owner
# and timestamps (SOURCE_DATE_EPOCH, defaulting to the last commit time).
set -euo pipefail
cd "$(dirname "$0")/../.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
target="x86_64-unknown-linux-gnu"
name="Quire-$version-linux-x86_64"
app_id="io.github.BhanuNexus.Quire"
out="dist"
stage="$out/$name"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"
release_date="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%d)"
bin="target/$target/release"

cargo build --release --locked --target "$target" -p quire-desktop -p quire-cli -p quire-sync-server
mkdir -p "$out"

# ---------------------------------------------------------------- tar.gz
rm -rf "$stage" && mkdir -p "$stage"
install -m 755 "$bin/quire" "$bin/quire-cli" "$stage/"
install -m 644 packaging/linux/quire.desktop "$stage/"
install -m 644 packaging/icons/quire-256.png "$stage/quire.png"
install -m 755 packaging/linux/install.sh "$stage/"
install -m 644 README.md LICENSE "$stage/"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$out" -cf - "$name" | gzip -9n > "$out/$name.tar.gz"
rm -rf "$stage"

# ---------------------------------------------------------------- sync server
server="quire-sync-server-$version-linux-x86_64"
sstage="$out/$server"
rm -rf "$sstage" && mkdir -p "$sstage"
install -m 755 "$bin/quire-sync-server" "$sstage/"
install -m 644 docs/self-hosting.md "$sstage/README.md"
install -m 644 LICENSE "$sstage/"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$out" -cf - "$server" | gzip -9n > "$out/$server.tar.gz"
rm -rf "$sstage"

# Files every system-wide layout shares (.deb and AppImage).
# $1 = root to install into (e.g. pkgroot or AppDir)
install_tree() {
  local root="$1"
  install -Dm 755 "$bin/quire" "$root/usr/bin/quire"
  install -Dm 755 "$bin/quire-cli" "$root/usr/bin/quire-cli"
  sed 's/^Icon=quire$/Icon='"$app_id"'/' packaging/linux/quire.desktop \
    | install -Dm 644 /dev/stdin "$root/usr/share/applications/$app_id.desktop"
  install -Dm 644 packaging/icons/quire-256.png "$root/usr/share/icons/hicolor/256x256/apps/$app_id.png"
  sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$release_date/" "packaging/linux/$app_id.metainfo.xml" \
    | install -Dm 644 /dev/stdin "$root/usr/share/metainfo/$app_id.metainfo.xml"
}

# ---------------------------------------------------------------- .deb
if command -v dpkg-deb > /dev/null; then
  deb="Quire-$version-linux-amd64.deb"
  pkg="$out/deb-root"
  rm -rf "$pkg" && mkdir -p "$pkg/DEBIAN"
  install_tree "$pkg"
  install -Dm 644 LICENSE "$pkg/usr/share/doc/quire/copyright"
  size_kb="$(du -sk --exclude=DEBIAN "$pkg" | cut -f1)"
  # The binary links only libc; windowing and GL libraries are loaded at run
  # time, so they're listed here for the package manager to install.
  cat > "$pkg/DEBIAN/control" <<EOF
Package: quire
Version: ${version//-/\~}
Architecture: amd64
Maintainer: The Quire contributors <https://github.com/BhanuNexus/quire/issues>
Installed-Size: $size_kb
Depends: libc6 (>= 2.35), libgcc-s1, libgl1 | libegl1, libxkbcommon0, libwayland-client0 | libx11-6
Recommends: libegl1, libwayland-egl1, libx11-xcb1, libxcursor1, libxi6, libxrender1, libxkbcommon-x11-0, libdbus-1-3
Section: editors
Priority: optional
Homepage: https://github.com/BhanuNexus/quire
Description: Fast, local-first Markdown notes
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
  ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream --runtime-file "$APPIMAGE_RUNTIME" \
    "$appdir" "$out/$appimage" > /dev/null
  rm -rf "$appdir"
fi

( cd "$out" && sha256sum Quire-"$version"-linux-* "$server.tar.gz" > "$name.sha256" )
echo "Built:"; ls -la "$out"/Quire-"$version"-linux-* "$out/$server.tar.gz"
