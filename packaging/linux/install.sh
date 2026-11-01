#!/usr/bin/env sh
# Install Quire for the current user (no root): ~/.local/bin, menu entry, icon.
# Run from the extracted archive:  ./install.sh      Uninstall:  ./install.sh --uninstall
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin/quire" "$bin/quire-cli" "$data/applications/quire.desktop" "$data/icons/hicolor/256x256/apps/quire.png"
  echo "Quire removed. Your notes were not touched."
  exit 0
fi
mkdir -p "$bin" "$data/applications" "$data/icons/hicolor/256x256/apps"
install -m 755 "$here/quire" "$here/quire-cli" "$bin/"
install -m 644 "$here/quire.desktop" "$data/applications/quire.desktop"
install -m 644 "$here/quire.png" "$data/icons/hicolor/256x256/apps/quire.png"
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$data/applications" || true
fi
echo "Installed to $bin. Start Quire from your app menu, or run: quire"
case ":$PATH:" in *":$bin:"*) ;; *) echo "Note: add $bin to your PATH to run quire from a terminal." ;; esac
